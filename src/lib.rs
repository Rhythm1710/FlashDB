//! FlashDB — a Redis-compatible in-memory database written from scratch.
//!
//! The binary in `main.rs` is a thin wrapper around this library: it binds a
//! listener and hands it to [`run`]. Exposing the server as a library lets the
//! integration tests in `tests/` start a real server on an ephemeral port and
//! talk to it over a genuine TCP socket, exactly as a client would.

use anyhow::Result;
use bytes::Bytes;
use resp::Value;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

pub mod rdb;
pub mod replication;
pub mod resp;

/// The typed payload a key holds.
///
/// Until now every value was a bare `String`. Redis keys, though, come in
/// several shapes — strings, lists, hashes, sets — and a command that meets
/// the wrong shape (e.g. `LPUSH` on a string) must fail with a `WRONGTYPE`
/// error rather than silently misbehave. Modelling the value as an enum makes
/// "what kind of thing is stored here?" a fact the compiler tracks for us:
/// every place that reads a value is forced to say what it does for each
/// shape. `Str`, `List`, and `Hash` exist today; `Set` joins its variant as
/// that command lands, at which point the accessors below grow the matching
/// arms.
#[derive(Debug, PartialEq, Clone)]
pub enum StoredValue {
    Str(String),
    /// A list of strings held in a `VecDeque` so pushes and pops at *both*
    /// ends are O(1) — that is exactly what `LPUSH`/`RPUSH`/`LPOP`/`RPOP` need.
    List(VecDeque<String>),
    /// A hash: an unordered set of field → value pairs, both strings. A
    /// `HashMap` gives O(1) `HGET`/`HSET`/`HDEL`; the trade-off is that field
    /// order isn't preserved, so `HGETALL` returns pairs in an arbitrary order
    /// (real Redis makes no ordering promise for small hashes either).
    Hash(HashMap<String, String>),
}

impl StoredValue {
    /// The Redis type name reported by the `TYPE` command: `string`, `list`,
    /// `hash`, and in future `set`. A missing key is handled by the caller
    /// (Redis reports `none`), so there is no variant for it here.
    fn type_name(&self) -> &'static str {
        match self {
            StoredValue::Str(_) => "string",
            StoredValue::List(_) => "list",
            StoredValue::Hash(_) => "hash",
        }
    }
}

/// One stored value plus its optional expiry deadline.
///
/// Splitting the value from its lifetime this way is what lets a key outlive
/// the request that created it only until a chosen `Instant`. `expires_at` of
/// `None` means "lives forever"; `Some(deadline)` means the key is gone the
/// moment `Instant::now()` passes `deadline`.
pub struct Entry {
    value: StoredValue,
    expires_at: Option<Instant>,
}

impl Entry {
    /// Has this entry's deadline passed as of `now`? Deterministic in `now`
    /// (no hidden `Instant::now()` call) so the logic is unit-testable without
    /// sleeping.
    fn is_expired_at(&self, now: Instant) -> bool {
        matches!(self.expires_at, Some(deadline) if deadline <= now)
    }

    /// Remaining time to live in whole seconds as of `now`, rounded up so a
    /// key just set with `EX 10` reads back as `10`, not `9`. `None` means the
    /// key has no expiry at all (the caller turns that into RESP `-1`).
    fn ttl_secs_at(&self, now: Instant) -> Option<i64> {
        self.expires_at.map(|deadline| {
            if deadline <= now {
                0
            } else {
                let ms = (deadline - now).as_millis();
                ms.div_ceil(1000) as i64
            }
        })
    }
}

/// The shared key/value store. `Arc` lets every connection task hold a handle
/// to the same map; `Mutex` serializes the reads and writes. Each value now
/// carries its own optional expiry (see [`Entry`]).
pub type Store = Arc<Mutex<HashMap<String, Entry>>>;

/// Passive expiry: if `key` is present but its deadline has passed as of
/// `now`, drop it. Called before every read so an expired key is
/// indistinguishable from a missing one, exactly as Redis behaves.
fn expire_if_due(map: &mut HashMap<String, Entry>, key: &str, now: Instant) {
    if matches!(map.get(key), Some(e) if e.is_expired_at(now)) {
        map.remove(key);
    }
}

/// Startup configuration parsed from the command-line arguments.
///
/// These mirror the Redis server flags FlashDB understands so far: `-p`/`--port`
/// chooses the listening port, `--dir` / `--dbfilename` name where the on-disk
/// RDB snapshot lives, and `--replicaof <host> <port>` starts the server as a
/// replica that syncs from a master on boot.
#[derive(Debug, PartialEq, Eq)]
pub struct Config {
    pub port: u16,
    pub dir: String,
    pub dbfilename: String,
    /// If set, this server starts as a *replica* of the master at
    /// `(host, port)`: on startup it dials the master, performs the replication
    /// handshake, and loads the master's snapshot into its own keyspace. `None`
    /// means a normal standalone server. Parsed from `--replicaof <host> <port>`.
    pub replicaof: Option<(String, u16)>,
}

impl Default for Config {
    /// Redis's own defaults: port 6379, the current directory, `dump.rdb`, and
    /// no master (a standalone server).
    fn default() -> Self {
        Config {
            port: 6379,
            dir: ".".to_string(),
            dbfilename: "dump.rdb".to_string(),
            replicaof: None,
        }
    }
}

impl Config {
    /// Parse configuration from an iterator of arguments — typically
    /// `std::env::args().skip(1)`, i.e. the arguments *without* the program
    /// name. Returns the built `Config`, or an `Err(message)` describing the
    /// first problem (an unknown flag, or a flag with no value). Reporting the
    /// error as a value rather than panicking lets `main` print a tidy message
    /// and exit instead of aborting with a backtrace.
    pub fn parse<I>(args: I) -> Result<Config, String>
    where
        I: IntoIterator<Item = String>,
    {
        let mut config = Config::default();
        let mut it = args.into_iter();
        while let Some(flag) = it.next() {
            // A small closure to pull the value that must follow a flag,
            // turning "ran off the end" into a readable error.
            let value_for = |it: &mut I::IntoIter| {
                it.next()
                    .ok_or_else(|| format!("missing value for {flag} option"))
            };
            match flag.as_str() {
                "-p" | "--port" => {
                    let raw = value_for(&mut it)?;
                    config.port = raw
                        .parse::<u16>()
                        .map_err(|_| format!("invalid port number '{raw}'"))?;
                }
                "--dir" => config.dir = value_for(&mut it)?,
                "--dbfilename" => config.dbfilename = value_for(&mut it)?,
                "--replicaof" => {
                    // Redis spells this `--replicaof <masterhost> <masterport>`:
                    // two separate arguments, so we pull both.
                    let host = value_for(&mut it)?;
                    let raw_port = value_for(&mut it)?;
                    let port = raw_port
                        .parse::<u16>()
                        .map_err(|_| format!("invalid master port number '{raw_port}'"))?;
                    config.replicaof = Some((host, port));
                }
                other => return Err(format!("unknown argument '{other}'")),
            }
        }
        Ok(config)
    }

    /// The socket address the server should bind: `127.0.0.1:<port>`.
    pub fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

/// Load the RDB snapshot named by `config` (`<dir>/<dbfilename>`) into a fresh
/// map of store entries.
///
/// A missing file is normal on a first run and yields an empty store rather
/// than an error. A file that exists but can't be parsed *is* an error, so the
/// caller can refuse to serve stale-or-corrupt data instead of silently
/// starting empty.
pub fn load_store_from_rdb(config: &Config) -> Result<HashMap<String, Entry>> {
    let path = std::path::Path::new(&config.dir).join(&config.dbfilename);
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        // No snapshot yet — a clean first boot, not a failure.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e.into()),
    };
    let entries = rdb::parse_rdb(&data)?;
    // Read both clocks once so every key in this load is converted against the
    // same reference instant.
    Ok(entries_to_store(entries, now_unix_ms(), Instant::now()))
}

/// Turn parsed [`rdb::RdbEntry`]s into live [`Entry`]s, resolving expiry.
///
/// The RDB records each expiry as an *absolute wall-clock* deadline in Unix
/// milliseconds, but the store measures expiry on the monotonic `Instant`
/// clock, which has no fixed zero point. So we bridge the two clocks here: given
/// the current wall-clock time (`now_unix_ms`) and the matching `now: Instant`,
/// a key still in the future keeps `now + (deadline − now)`, while a key already
/// past its deadline is dropped — exactly as Redis discards expired keys as it
/// loads a snapshot. Taking both clocks as parameters keeps this logic
/// deterministic and unit-testable without touching the real clock.
fn entries_to_store(
    entries: Vec<rdb::RdbEntry>,
    now_unix_ms: u64,
    now: Instant,
) -> HashMap<String, Entry> {
    let mut map = HashMap::new();
    for entry in entries {
        let expires_at = match entry.expire_at_ms {
            None => None,
            Some(deadline_ms) => {
                if deadline_ms <= now_unix_ms {
                    continue; // already expired at load time — don't resurrect it
                }
                Some(now + Duration::from_millis(deadline_ms - now_unix_ms))
            }
        };
        map.insert(
            entry.key,
            Entry {
                value: entry.value,
                expires_at,
            },
        );
    }
    map
}

/// The current wall-clock time in Unix milliseconds (0 in the impossible case
/// the clock reads before the epoch). Both the loader and the saver take a
/// single reading of this so every key in one operation shares one reference.
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Turn the live store into the [`rdb::RdbEntry`]s a snapshot serializes — the
/// inverse of [`entries_to_store`]. Each value is cloned (the snapshot must
/// outlive the lock), already-expired keys are skipped so dead keys aren't
/// persisted, and every surviving expiry is converted from the store's monotonic
/// `Instant` deadline back into the absolute Unix-millisecond deadline the file
/// records. Both clocks are parameters, so the conversion stays deterministic
/// and unit-testable without touching the real clock.
fn map_to_rdb_entries(
    map: &HashMap<String, Entry>,
    now_unix_ms: u64,
    now: Instant,
) -> Vec<rdb::RdbEntry> {
    let mut entries = Vec::with_capacity(map.len());
    for (key, entry) in map {
        if entry.is_expired_at(now) {
            continue; // a key that has already lapsed is not worth persisting
        }
        let expire_at_ms = entry.expires_at.map(|deadline| {
            // How far the deadline sits ahead of `now` on the monotonic clock,
            // laid back onto the wall clock to get an absolute deadline.
            let remaining = deadline.saturating_duration_since(now);
            now_unix_ms + remaining.as_millis() as u64
        });
        entries.push(rdb::RdbEntry {
            key: key.clone(),
            value: entry.value.clone(),
            expire_at_ms,
        });
    }
    entries
}

/// Take a consistent snapshot of the store and serialize it to the RDB byte
/// image. The lock is held only long enough to copy the keyspace into owned
/// entries; the encoding runs after the lock is released, so a snapshot doesn't
/// block other clients for the whole serialization.
fn snapshot_bytes(store: &Store) -> Vec<u8> {
    let now = Instant::now();
    let unix_ms = now_unix_ms();
    let entries = {
        let map = store.lock().unwrap();
        map_to_rdb_entries(&map, unix_ms, now)
    };
    rdb::write::serialize(&entries)
}

/// Write `bytes` to `path` atomically: write a sibling temp file, then rename it
/// over the target. A crash mid-write leaves the previous snapshot intact rather
/// than a half-written file, the same trick Redis uses with its temp file.
fn write_snapshot(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// `SAVE` — synchronously write the whole keyspace to the configured RDB path,
/// then reply `+OK`. Like Redis's own `SAVE`, this blocks until the file is on
/// disk. An IO failure becomes an error reply rather than a crash.
fn save(args: &[Value], server: &Server) -> Value {
    if !args.is_empty() {
        return wrong_args("save");
    }
    let bytes = snapshot_bytes(&server.store);
    match write_snapshot(server.rdb_path.as_path(), &bytes) {
        Ok(()) => Value::SimpleString("OK".to_string()),
        Err(e) => Value::Error(format!("ERR {}", e)),
    }
}

/// `BGSAVE` — snapshot the keyspace and write it off the request path. We copy a
/// consistent point-in-time snapshot under the lock (Redis forks a child for the
/// same effect), then hand the file IO to a background thread and reply at once
/// so the client isn't blocked on the disk. A write failure is logged rather
/// than reported, since the reply has already gone out.
fn bgsave(args: &[Value], server: &Server) -> Value {
    if !args.is_empty() {
        return wrong_args("bgsave");
    }
    let bytes = snapshot_bytes(&server.store);
    let path = server.rdb_path.clone();
    std::thread::spawn(move || {
        if let Err(e) = write_snapshot(path.as_path(), &bytes) {
            eprintln!("Background save failed: {e}");
        }
    });
    Value::SimpleString("Background saving started".to_string())
}

/// `WAIT numreplicas timeout` — how many replicas have the master's writes.
///
/// In real Redis this *blocks* until at least `numreplicas` replicas have
/// acknowledged the master's current replication offset, or `timeout`
/// milliseconds pass, then returns the number that acked. FlashDB streams
/// writes but doesn't yet track each replica's acknowledged offset, so it can't
/// honour the "up to this offset" part. Instead it returns the number of
/// replicas currently connected — a truthful count of who is receiving the
/// stream — immediately. Offset-accurate, blocking `WAIT` is a follow-up.
///
/// The two arguments are still validated as integers so a malformed `WAIT` is
/// rejected the way Redis rejects it, even though we don't act on their values.
fn wait(args: &[Value], server: &Server) -> Value {
    if args.len() != 2 {
        return wrong_args("wait");
    }
    for arg in args {
        if unpack_int(arg).is_err() {
            return Value::Error("ERR value is not an integer or out of range".to_string());
        }
    }
    Value::Integer(connected_replica_count(&server.replicas) as i64)
}

/// Count the replicas whose link is still live, dropping any whose task has
/// ended (its receiver dropped, so the sender now reports closed). Pruning here
/// keeps a disconnected replica from being counted forever.
fn connected_replica_count(replicas: &Replicas) -> usize {
    let mut list = replicas.lock().unwrap();
    list.retain(|tx| !tx.is_closed());
    list.len()
}

/// The fixed replication ID this master reports in its `+FULLRESYNC` reply.
///
/// Real Redis generates a random 40-hex-char run id each time it starts, and
/// uses it (with an offset) to decide whether a reconnecting replica can do a
/// *partial* resync instead of a full one. FlashDB only does full resyncs, and
/// the replica side doesn't act on this value yet, so a constant is enough — and
/// it keeps the handshake deterministic for tests.
const REPLICATION_ID: &str = "8371b4fb1155b71f4a04d3e1bc3e18c4a990aeeb";

/// The set of currently-connected replicas, as the sending halves of their
/// per-replica channels.
///
/// When a client issues a write, `process_command` re-serializes that command
/// and pushes the bytes into every sender here; each replica's own task
/// ([`serve_replica`]) owns the matching receiver and writes the bytes down its
/// socket. `Arc<Mutex<..>>` because *any* connection task — whichever client ran
/// the write — needs to reach this one shared list.
type Replicas = Arc<Mutex<Vec<mpsc::UnboundedSender<Bytes>>>>;

/// A per-key modification counter, bumped every time a key is successfully
/// written. `WATCH` snapshots the counter of the keys it watches; `EXEC`
/// re-reads them and aborts if any changed — that comparison, and nothing else,
/// is how optimistic locking detects a concurrent write. Shared like the store
/// because a write on *any* connection must be visible to a `WATCH` on another.
/// A key that has never been written simply isn't present, which reads as
/// version 0.
type Versions = Arc<Mutex<HashMap<String, u64>>>;

/// The shared runtime state every connection task holds: the keyspace, the path
/// `SAVE` / `BGSAVE` persist it to, the registry of connected replicas that
/// writes are streamed to, and the per-key version counters `WATCH` compares
/// against. Cloning a `Server` is cheap — every field is an `Arc` handle — so
/// each connection gets its own handle to the same shared state.
#[derive(Clone)]
pub struct Server {
    store: Store,
    rdb_path: Arc<PathBuf>,
    replicas: Replicas,
    versions: Versions,
}

impl Server {
    fn new(store: Store, rdb_path: PathBuf) -> Self {
        Server {
            store,
            rdb_path: Arc::new(rdb_path),
            replicas: Arc::new(Mutex::new(Vec::new())),
            versions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The current version of `key`: the number of writes it has seen, or 0 for
    /// a key never written. Read by `WATCH` (to snapshot) and `EXEC` (to compare).
    fn version_of(&self, key: &str) -> u64 {
        self.versions.lock().unwrap().get(key).copied().unwrap_or(0)
    }
}

/// Per-connection transaction state.
///
/// Everything the server has held so far lived in the shared [`Server`] — one
/// keyspace every client touches. A transaction is the first thing that is
/// *private to one client*: the list of commands queued since `MULTI` belongs to
/// the connection that typed them, not to the map. So this struct lives on the
/// stack of [`handle_conn`], one per connection, and is threaded by `&mut` into
/// command dispatch. It is deliberately not `Clone` and not shared — no `Arc`,
/// no `Mutex` — because no other task ever needs to see it.
#[derive(Default)]
struct Session {
    /// Are we between `MULTI` and `EXEC`/`DISCARD`? While true, ordinary
    /// commands are queued rather than run.
    in_multi: bool,
    /// The commands queued since `MULTI`, replayed in order by `EXEC`.
    queue: Vec<Value>,
    /// A queued command was rejected at queue time (unknown command). Redis
    /// remembers this and makes the eventual `EXEC` abort the *whole*
    /// transaction rather than run a partial one.
    dirty: bool,
    /// Keys this connection is `WATCH`ing, each paired with its version at the
    /// moment it was watched. `EXEC` aborts if any of these has changed since.
    /// Empty means the transaction isn't guarded by optimistic locking.
    watched: Vec<(String, u64)>,
}

impl Session {
    /// Begin a transaction. Nested `MULTI` is an error in Redis (and would be
    /// ambiguous — which `EXEC` closes which?), but it does not abort the
    /// transaction already open.
    fn multi(&mut self) -> Value {
        if self.in_multi {
            return Value::Error("ERR MULTI calls can not be nested".to_string());
        }
        self.in_multi = true;
        Value::SimpleString("OK".to_string())
    }

    /// Queue one command to run at `EXEC`, or taint the transaction if it can
    /// never run. Redis validates enough at queue time to reject an unknown
    /// command up front (returning an error *and* setting the abort flag);
    /// arity and type errors are left to surface inside the `EXEC` reply array,
    /// which matches Redis for those runtime failures.
    fn queue(&mut self, value: Value) -> Value {
        match command_name(&value) {
            Some(name) if is_known_command(&name) => {
                self.queue.push(value);
                Value::SimpleString("QUEUED".to_string())
            }
            Some(name) => {
                self.dirty = true;
                Value::Error(format!("ERR unknown command '{}'", name))
            }
            None => {
                self.dirty = true;
                Value::Error("ERR unknown command".to_string())
            }
        }
    }

    /// Mark keys for optimistic locking. `WATCH` snapshots each key's current
    /// version; if any of them is written before `EXEC`, the transaction aborts.
    /// `WATCH` isn't allowed once a transaction is open (it would be pointless —
    /// the check happens at `EXEC`, and there's nothing to guard mid-queue).
    fn watch(&mut self, args: &[Value], server: &Server) -> Value {
        if self.in_multi {
            return Value::Error("ERR WATCH inside MULTI is not allowed".to_string());
        }
        if args.is_empty() {
            return wrong_args("watch");
        }
        for arg in args {
            let key = match unpack_bulk_str(arg) {
                Ok(k) => k,
                Err(e) => return Value::Error(format!("ERR {}", e)),
            };
            let version = server.version_of(&key);
            self.watched.push((key, version));
        }
        Value::SimpleString("OK".to_string())
    }

    /// Forget every watched key, so the next `EXEC` is unguarded. Always OK,
    /// even with nothing watched.
    fn unwatch(&mut self) -> Value {
        self.watched.clear();
        Value::SimpleString("OK".to_string())
    }

    /// Have any watched keys changed since they were watched? A single mismatch
    /// (or a key written for the first time, moving it off version 0) means a
    /// concurrent writer got in, so `EXEC` must abort.
    fn watch_conflict(&self, server: &Server) -> bool {
        self.watched
            .iter()
            .any(|(key, watched_at)| server.version_of(key) != *watched_at)
    }

    /// Run the queued commands as a batch and end the transaction.
    ///
    /// The transaction ends no matter what, so state is snapshotted and cleared
    /// first. If any queued command was rejected, nothing runs and `EXEC`
    /// reports `EXECABORT`. If a `WATCH`ed key changed underneath us, `EXEC`
    /// aborts by replying with the nil array and running nothing. Otherwise
    /// every queued command runs in order through the normal [`process_command`]
    /// path — so writes still replicate — and their replies come back as one
    /// array. A runtime error (e.g. `WRONGTYPE`) is just one element of that
    /// array; it does not stop the others, because Redis transactions are
    /// batched, not rolled back.
    fn exec(&mut self, server: &Server) -> Value {
        if !self.in_multi {
            return Value::Error("ERR EXEC without MULTI".to_string());
        }
        let queued = std::mem::take(&mut self.queue);
        let dirty = self.dirty;
        let conflict = self.watch_conflict(server);
        self.reset();

        if dirty {
            return Value::Error(
                "EXECABORT Transaction discarded because of previous errors.".to_string(),
            );
        }
        if conflict {
            return Value::NullArray;
        }
        let results = queued
            .into_iter()
            .map(|cmd| process_command(cmd, server))
            .collect();
        Value::Array(results)
    }

    /// Throw away a transaction without running it.
    fn discard(&mut self) -> Value {
        if !self.in_multi {
            return Value::Error("ERR DISCARD without MULTI".to_string());
        }
        self.reset();
        Value::SimpleString("OK".to_string())
    }

    /// Return to the no-transaction state, dropping any queued commands and
    /// clearing the watch set — Redis unwatches after every `EXEC`/`DISCARD`.
    fn reset(&mut self) {
        self.in_multi = false;
        self.queue.clear();
        self.dirty = false;
        self.watched.clear();
    }
}

/// Dispatch one request in the context of this connection's [`Session`].
///
/// The transaction controls (`MULTI`/`EXEC`/`DISCARD`) always act immediately.
/// Everything else runs right away *unless* a transaction is open, in which case
/// it is queued for later. This is the one place per-connection state and the
/// shared server meet.
fn handle_command(value: Value, server: &Server, session: &mut Session) -> Value {
    match command_name(&value).as_deref() {
        Some("multi") => session.multi(),
        Some("exec") => session.exec(server),
        Some("discard") => session.discard(),
        Some("watch") => session.watch(command_args(&value), server),
        Some("unwatch") => session.unwatch(),
        _ if session.in_multi => session.queue(value),
        _ => process_command(value, server),
    }
}

/// Borrow a command frame's argument values (everything after the command name),
/// or an empty slice if the frame isn't a non-empty command array. Lets the
/// transaction controls read their arguments without cloning the frame apart.
fn command_args(value: &Value) -> &[Value] {
    match value {
        Value::Array(items) if !items.is_empty() => &items[1..],
        _ => &[],
    }
}

/// Is `name` (already lowercased) a command this server dispatches? Used to
/// reject an unknown command at `MULTI`-queue time. Kept in lock-step with the
/// dispatch table in [`process_command`]; a command missing here would be
/// queued and then fail at `EXEC` instead of being caught up front.
fn is_known_command(name: &str) -> bool {
    matches!(
        name,
        "ping"
            | "echo"
            | "set"
            | "get"
            | "del"
            | "expire"
            | "ttl"
            | "persist"
            | "type"
            | "rpush"
            | "lpush"
            | "rpop"
            | "lpop"
            | "llen"
            | "lrange"
            | "hset"
            | "hget"
            | "hgetall"
            | "hdel"
            | "save"
            | "bgsave"
            | "replconf"
            | "wait"
            | "multi"
            | "exec"
            | "discard"
            | "watch"
            | "unwatch"
    )
}

/// Accept connections on `listener` forever, serving each on its own task.
///
/// Starts from an empty keyspace, persisting to `dump.rdb` in the working
/// directory. Use [`run_with_config`] to load an existing RDB snapshot first and
/// control where snapshots are written.
pub async fn run(listener: TcpListener) -> Result<()> {
    let server = Server::new(
        Arc::new(Mutex::new(HashMap::new())),
        PathBuf::from("dump.rdb"),
    );
    serve(listener, server).await
}

/// Like [`run`], but first loads the RDB snapshot named by `config` so the
/// server comes up carrying whatever a previous run persisted, and directs
/// `SAVE` / `BGSAVE` back to that same `<dir>/<dbfilename>`. A snapshot that
/// fails to parse aborts startup rather than dropping the data.
pub async fn run_with_config(listener: TcpListener, config: &Config) -> Result<()> {
    let map = load_store_from_rdb(config)?;
    let path = std::path::Path::new(&config.dir).join(&config.dbfilename);
    let server = Server::new(Arc::new(Mutex::new(map)), path);

    // If we were told to replicate, kick off the sync in the background so the
    // handshake with the master runs alongside — not before — us accepting our
    // own clients. The task loads the master's snapshot, then stays connected
    // and applies its live write stream; a failure (or the link ending) is
    // logged and the server keeps serving whatever it already had.
    if let Some((host, master_port)) = &config.replicaof {
        let master_addr = format!("{host}:{master_port}");
        let listening_port = config.port;
        let server_for_repl = server.clone();
        tokio::spawn(async move {
            if let Err(e) =
                replication::sync_from_master(&master_addr, listening_port, server_for_repl).await
            {
                eprintln!("Replication sync from {master_addr} failed: {e:?}");
            } else {
                println!("Replication link to master {master_addr} closed");
            }
        });
    }

    serve(listener, server).await
}

/// The shared accept loop behind both entry points. The [`Server`] is cloned (an
/// `Arc` handle bump) into every connection task. This never returns under
/// normal operation; an error is only produced if `accept` itself fails.
async fn serve(listener: TcpListener, server: Server) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        println!("accepted new connection");
        let server = server.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, server).await {
                eprintln!("Connection error: {:?}", e);
            }
        });
    }
}

async fn handle_conn(stream: TcpStream, server: Server) -> Result<()> {
    let mut handler = resp::RespHandler::new(stream);
    // Transaction state is private to this connection, so it lives here on the
    // task's stack rather than in the shared `Server`.
    let mut session = Session::default();
    loop {
        let value = match handler.read_value().await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => {
                // A protocol violation gets an error reply before we hang
                // up, like real Redis; the parser can't resync after
                // garbage, so keeping the connection open isn't safe.
                // IO errors (client vanished) just propagate.
                if let Some(proto) = e.downcast_ref::<resp::ProtocolError>() {
                    let reply = Value::Error(format!("ERR {}", proto));
                    let _ = handler.write_value(reply).await;
                    return Ok(());
                }
                return Err(e);
            }
        };

        // `PSYNC` turns this connection from a normal client into a replica: it
        // leaves the request/response loop for good and becomes a one-way stream
        // of commands the master pushes. Hand the whole connection off to
        // `serve_replica`, which never returns to this loop.
        if command_name(&value).as_deref() == Some("psync") {
            return serve_replica(handler, server).await;
        }

        // A malformed request produces an error reply, not a dropped
        // connection, so one bad client can't take down its own session.
        // Routing through `handle_command` (rather than `process_command`
        // directly) is what lets `MULTI` queue the commands that follow it.
        let response = handle_command(value, &server, &mut session);
        handler.write_value(response).await?;
    }
    Ok(())
}

/// Peek at the command name of a parsed request without consuming it: the
/// lowercased first bulk string of the array, or `None` if the frame isn't a
/// command array. Used to spot `PSYNC` before dispatch.
fn command_name(value: &Value) -> Option<String> {
    match value {
        Value::Array(items) => match items.first() {
            Some(Value::BulkString(name)) => Some(name.to_lowercase()),
            _ => None,
        },
        _ => None,
    }
}

/// Turn a connection that just issued `PSYNC` into a live replica link.
///
/// Three steps: (1) answer `+FULLRESYNC <replid> 0`; (2) ship a full RDB
/// snapshot of the current keyspace as a bulk payload — framed `$<len>\r\n`
/// then exactly `len` bytes, with **no** trailing CRLF, which is the quirk the
/// replica's `read_rdb_bulk` is written to expect; (3) register a channel and
/// forward every subsequent write command down the socket until the replica
/// disconnects.
///
/// The channel is the bridge between two worlds: `process_command` runs
/// synchronously inside whichever *client's* task issued a write, and pushes the
/// command's bytes into every replica's sender; this async task owns the
/// matching receiver and does the actual socket write. We `select!` between the
/// receiver and a read on the socket so a replica hanging up (a read of zero
/// bytes) is noticed promptly rather than only on the next write.
async fn serve_replica(handler: resp::RespHandler, server: Server) -> Result<()> {
    let mut stream = handler.into_inner();

    // (1) + (2): the FULLRESYNC line, then the initial snapshot.
    stream
        .write_all(format!("+FULLRESYNC {REPLICATION_ID} 0\r\n").as_bytes())
        .await?;
    let snapshot = snapshot_bytes(&server.store);
    stream
        .write_all(format!("${}\r\n", snapshot.len()).as_bytes())
        .await?;
    stream.write_all(&snapshot).await?;
    stream.flush().await?;

    // (3): register so writes begin flowing, then forward them.
    let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
    server.replicas.lock().unwrap().push(tx);
    println!("Replica synced; streaming writes to it");

    let (mut read_half, mut write_half) = stream.split();
    let mut scratch = [0u8; 512];
    loop {
        tokio::select! {
            forwarded = rx.recv() => match forwarded {
                // A write command to relay verbatim to the replica.
                Some(bytes) => {
                    write_half.write_all(&bytes).await?;
                    write_half.flush().await?;
                }
                // Every sender dropped — only happens if the server is tearing
                // down — so there's nothing left to stream.
                None => break,
            },
            read = read_half.read(&mut scratch) => match read {
                Ok(0) => break,       // the replica hung up
                Ok(_) => {}           // a REPLCONF ACK or the like — ignored for now
                Err(e) => return Err(e.into()),
            },
        }
    }
    // Dropping `rx` here (on return) closes our sender's channel, so the next
    // `propagate` prunes this replica from the registry.
    Ok(())
}

/// Route a parsed request to its command handler and produce a reply value.
///
/// Public so integration tests can exercise command dispatch directly, and so
/// the logic lives beside the rest of the server rather than in the binary.
pub fn process_command(value: Value, server: &Server) -> Value {
    // Keep the original command frame around so a write can be forwarded to
    // replicas verbatim after it runs; `extract_command` consumes its argument,
    // so we clone before splitting it into name + args.
    let (command, args) = match extract_command(value.clone()) {
        Ok(parts) => parts,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };

    // Most commands only touch the keyspace; `SAVE`/`BGSAVE` also need the path.
    let storage = &server.store;
    let name = command.to_lowercase();
    let response = match name.as_str() {
        "ping" => match args.first() {
            Some(v) => v.clone(),
            None => Value::SimpleString("PONG".to_string()),
        },
        "echo" => match args.first() {
            Some(v) => v.clone(),
            None => wrong_args("echo"),
        },
        "set" => set(&args, storage),
        "get" => get(&args, storage),
        "del" => del(&args, storage),
        "expire" => expire(&args, storage),
        "ttl" => ttl(&args, storage),
        "persist" => persist(&args, storage),
        "type" => type_cmd(&args, storage),
        "rpush" => push(&args, storage, Side::Right),
        "lpush" => push(&args, storage, Side::Left),
        "rpop" => pop(&args, storage, Side::Right),
        "lpop" => pop(&args, storage, Side::Left),
        "llen" => llen(&args, storage),
        "lrange" => lrange(&args, storage),
        "hset" => hset(&args, storage),
        "hget" => hget(&args, storage),
        "hgetall" => hgetall(&args, storage),
        "hdel" => hdel(&args, storage),
        "save" => save(&args, server),
        "bgsave" => bgsave(&args, server),
        // A replica announces its listening port and capabilities with REPLCONF
        // during the handshake; the master just acknowledges each one. (When the
        // master later sends `REPLCONF GETACK` the *replica* answers — that
        // direction is the replica's job, not handled here.)
        "replconf" => Value::SimpleString("OK".to_string()),
        "wait" => wait(&args, server),
        c => Value::Error(format!("ERR unknown command '{}'", c)),
    };

    // Replication: after a write command runs, stream it to every connected
    // replica so their keyspaces track this master's. A command that *failed*
    // (returned an error — bad arity, WRONGTYPE, a syntax error) changed
    // nothing, so it isn't propagated. This is a simplification of Redis's
    // exact "did the dataset actually change?" rule, but it never propagates a
    // rejected command.
    if is_write_command(&name) && !matches!(response, Value::Error(_)) {
        propagate(&server.replicas, &value);
        // Bump the version of every key this write touched so a `WATCH` on it
        // elsewhere notices the change and aborts its `EXEC`.
        bump_versions(&server.versions, &name, &args);
    }
    response
}

/// The keys a successful write command modified, so their versions can be
/// bumped for `WATCH`. Every write here keys off its first argument — except
/// `DEL`, which takes a whole list of keys. Kept alongside [`is_write_command`]:
/// a new write command must appear in both.
fn written_keys(name: &str, args: &[Value]) -> Vec<String> {
    let key_args: &[Value] = match name {
        "del" => args,
        // set/expire/persist/rpush/lpush/rpop/lpop/hset/hdel all take the key
        // first and touch only that one key.
        _ => &args[..args.len().min(1)],
    };
    key_args
        .iter()
        .filter_map(|v| unpack_bulk_str(v).ok())
        .collect()
}

/// Increment the version counter of each key a write touched. This is the write
/// side of optimistic locking: `WATCH` snapshots these counters and `EXEC`
/// compares against them.
fn bump_versions(versions: &Versions, name: &str, args: &[Value]) {
    let keys = written_keys(name, args);
    if keys.is_empty() {
        return;
    }
    let mut map = versions.lock().unwrap();
    for key in keys {
        *map.entry(key).or_insert(0) += 1;
    }
}

/// Does this (already-lowercased) command mutate the keyspace? Only writes are
/// streamed to replicas; reads stay local to whichever server received them.
/// Kept as one list so it's obvious which commands replicate — extend it in
/// lock-step with the dispatch table above whenever a new write lands.
fn is_write_command(name: &str) -> bool {
    matches!(
        name,
        "set"
            | "del"
            | "expire"
            | "persist"
            | "rpush"
            | "lpush"
            | "rpop"
            | "lpop"
            | "hset"
            | "hdel"
    )
}

/// Fan one write command out to every connected replica.
///
/// The command is re-serialized to its RESP array and the same bytes are pushed
/// into each replica's channel. Pushing is non-blocking — the unbounded channel
/// takes the bytes immediately and each replica's own task does the socket write
/// — so a slow replica never stalls the client that issued the write. A send
/// fails only when that replica's task has ended (its receiver was dropped), so
/// `retain` doubles as the place dead replicas are pruned from the registry.
fn propagate(replicas: &Replicas, command: &Value) {
    let mut list = replicas.lock().unwrap();
    if list.is_empty() {
        return;
    }
    let bytes = Bytes::from(command.serialize().into_bytes());
    list.retain(|tx| tx.send(bytes.clone()).is_ok());
}

fn set(args: &[Value], storage: &Store) -> Value {
    if args.len() < 2 {
        return wrong_args("set");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let value = match unpack_bulk_str(&args[1]) {
        Ok(v) => v,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };

    // Optional trailing options: EX <seconds> or PX <milliseconds>. Anything
    // else is a syntax error, matching how real Redis rejects unknown tokens.
    let mut expires_at = None;
    let mut i = 2;
    while i < args.len() {
        let opt = match unpack_bulk_str(&args[i]) {
            Ok(o) => o.to_lowercase(),
            Err(e) => return Value::Error(format!("ERR {}", e)),
        };
        match opt.as_str() {
            "ex" | "px" => {
                let amount = match args.get(i + 1) {
                    Some(v) => match unpack_int(v) {
                        Ok(n) => n,
                        Err(e) => return Value::Error(format!("ERR {}", e)),
                    },
                    None => return Value::Error("ERR syntax error".to_string()),
                };
                if amount <= 0 {
                    return Value::Error("ERR invalid expire time in 'set' command".to_string());
                }
                let dur = if opt == "ex" {
                    Duration::from_secs(amount as u64)
                } else {
                    Duration::from_millis(amount as u64)
                };
                expires_at = Some(Instant::now() + dur);
                i += 2;
            }
            _ => return Value::Error("ERR syntax error".to_string()),
        }
    }

    // A plain SET clears any previous TTL because we insert a fresh Entry.
    // SET always stores a string value, overwriting any prior type.
    storage.lock().unwrap().insert(
        key,
        Entry {
            value: StoredValue::Str(value),
            expires_at,
        },
    );
    Value::SimpleString("OK".to_string())
}

fn get(args: &[Value], storage: &Store) -> Value {
    if args.len() != 1 {
        return wrong_args("get");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get(&key) {
        Some(e) => match &e.value {
            StoredValue::Str(s) => Value::BulkString(s.clone()),
            // GET only understands strings; a list or hash (or any future
            // type) here is a client bug, and Redis answers it with WRONGTYPE.
            _ => wrong_type(),
        },
        None => Value::Null,
    }
}

/// `TYPE key` — report the kind of value stored at `key` as a simple string:
/// `string` today (and `list` / `hash` / `set` once those exist), or `none`
/// if the key is missing or has expired.
fn type_cmd(args: &[Value], storage: &Store) -> Value {
    if args.len() != 1 {
        return wrong_args("type");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get(&key) {
        Some(e) => Value::SimpleString(e.value.type_name().to_string()),
        None => Value::SimpleString("none".to_string()),
    }
}

fn del(args: &[Value], storage: &Store) -> Value {
    if args.is_empty() {
        return wrong_args("del");
    }
    let now = Instant::now();
    let mut removed = 0i64;
    let mut store = storage.lock().unwrap();
    for arg in args {
        let key = match unpack_bulk_str(arg) {
            Ok(k) => k,
            Err(e) => return Value::Error(format!("ERR {}", e)),
        };
        // Expire first so DELeting an already-expired key counts as 0, the
        // key being logically gone already.
        expire_if_due(&mut store, &key, now);
        if store.remove(&key).is_some() {
            removed += 1;
        }
    }
    Value::Integer(removed)
}

/// `EXPIRE key seconds` — set a relative timeout on an existing key. Returns
/// `:1` if the timeout was set, `:0` if the key doesn't exist. A non-positive
/// timeout deletes the key immediately (and still reports `:1`), as in Redis.
fn expire(args: &[Value], storage: &Store) -> Value {
    if args.len() != 2 {
        return wrong_args("expire");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let secs = match unpack_int(&args[1]) {
        Ok(n) => n,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    if !store.contains_key(&key) {
        return Value::Integer(0);
    }
    if secs <= 0 {
        store.remove(&key);
        return Value::Integer(1);
    }
    match store.get_mut(&key) {
        Some(e) => {
            e.expires_at = Some(now + Duration::from_secs(secs as u64));
            Value::Integer(1)
        }
        None => Value::Integer(0),
    }
}

/// `TTL key` — remaining time to live in seconds. `-2` if the key is missing,
/// `-1` if it exists but has no expiry, otherwise the seconds left.
fn ttl(args: &[Value], storage: &Store) -> Value {
    if args.len() != 1 {
        return wrong_args("ttl");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get(&key) {
        None => Value::Integer(-2),
        Some(e) => match e.ttl_secs_at(now) {
            None => Value::Integer(-1),
            Some(secs) => Value::Integer(secs),
        },
    }
}

/// `PERSIST key` — remove a key's expiry so it lives forever. Returns `:1` if
/// an expiry was removed, `:0` if the key is missing or had no expiry.
fn persist(args: &[Value], storage: &Store) -> Value {
    if args.len() != 1 {
        return wrong_args("persist");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get_mut(&key) {
        Some(e) if e.expires_at.is_some() => {
            e.expires_at = None;
            Value::Integer(1)
        }
        _ => Value::Integer(0),
    }
}

/// Which end of a list a push or pop acts on. `RPUSH`/`RPOP` work the right
/// (tail) end; `LPUSH`/`LPOP` the left (head) end. Passing this as one enum
/// lets a single `push`/`pop` function serve both directions instead of
/// duplicating the body twice with `push_back`/`push_front` swapped.
enum Side {
    Left,
    Right,
}

/// The error Redis returns when a command meets a key holding a different kind
/// of value than it operates on — e.g. `LPUSH` on a string, or `GET` on a
/// list. The reply is a plain error frame beginning with the word `WRONGTYPE`.
fn wrong_type() -> Value {
    Value::Error("WRONGTYPE Operation against a key holding the wrong kind of value".to_string())
}

/// `RPUSH key v [v ...]` / `LPUSH key v [v ...]` — append (right) or prepend
/// (left) one or more values, creating the list if the key is absent. Returns
/// the list's new length. A `LPUSH a b c` leaves the list as `[c, b, a]`
/// because each value is pushed onto the head in turn.
fn push(args: &[Value], storage: &Store, side: Side) -> Value {
    let name = match side {
        Side::Left => "lpush",
        Side::Right => "rpush",
    };
    if args.len() < 2 {
        return wrong_args(name);
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let mut values = Vec::with_capacity(args.len() - 1);
    for arg in &args[1..] {
        match unpack_bulk_str(arg) {
            Ok(v) => values.push(v),
            Err(e) => return Value::Error(format!("ERR {}", e)),
        }
    }

    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    // `entry(..).or_insert_with(..)` inserts a fresh empty list only when the
    // key is absent; if it already exists we get the existing entry back and
    // never overwrite it — so a wrong-typed key falls through to WRONGTYPE
    // below without being clobbered. A brand-new list has no expiry.
    let entry = store.entry(key).or_insert_with(|| Entry {
        value: StoredValue::List(VecDeque::new()),
        expires_at: None,
    });
    match &mut entry.value {
        StoredValue::List(list) => {
            for v in values {
                match side {
                    Side::Left => list.push_front(v),
                    Side::Right => list.push_back(v),
                }
            }
            Value::Integer(list.len() as i64)
        }
        _ => wrong_type(),
    }
}

/// `RPOP key` / `LPOP key` — remove and return one element from the tail
/// (right) or head (left). Replies with the element as a bulk string, or null
/// if the key is missing or the list is empty. When the pop empties the list
/// the key is deleted, matching Redis: an empty list never lingers in the
/// keyspace.
fn pop(args: &[Value], storage: &Store, side: Side) -> Value {
    let name = match side {
        Side::Left => "lpop",
        Side::Right => "rpop",
    };
    if args.len() != 1 {
        return wrong_args(name);
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };

    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    // Borrow the entry just long enough to pop and note whether the list is now
    // empty, then let that borrow end before touching the map again — Rust
    // won't let us `store.remove(..)` while a `&mut` into the same map is live.
    let (popped, now_empty) = match store.get_mut(&key) {
        None => return Value::Null,
        Some(e) => match &mut e.value {
            StoredValue::List(list) => {
                let popped = match side {
                    Side::Left => list.pop_front(),
                    Side::Right => list.pop_back(),
                };
                (popped, list.is_empty())
            }
            _ => return wrong_type(),
        },
    };
    if now_empty {
        store.remove(&key);
    }
    match popped {
        Some(v) => Value::BulkString(v),
        None => Value::Null,
    }
}

/// `LLEN key` — the number of elements in the list, `0` if the key is missing,
/// or WRONGTYPE if the key holds something that isn't a list.
fn llen(args: &[Value], storage: &Store) -> Value {
    if args.len() != 1 {
        return wrong_args("llen");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get(&key) {
        None => Value::Integer(0),
        Some(e) => match &e.value {
            StoredValue::List(list) => Value::Integer(list.len() as i64),
            _ => wrong_type(),
        },
    }
}

/// `LRANGE key start stop` — return the elements between `start` and `stop`
/// inclusive, as an array of bulk strings. Indices are zero-based and may be
/// negative to count back from the end (`-1` is the last element). An
/// out-of-range or inverted range yields an empty array, and a missing key is
/// treated as an empty list.
fn lrange(args: &[Value], storage: &Store) -> Value {
    if args.len() != 3 {
        return wrong_args("lrange");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let start = match unpack_int(&args[1]) {
        Ok(n) => n,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let stop = match unpack_int(&args[2]) {
        Ok(n) => n,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get(&key) {
        None => Value::Array(Vec::new()),
        Some(e) => match &e.value {
            StoredValue::List(list) => {
                let (from, to) = normalize_range(start, stop, list.len() as i64);
                if from > to {
                    return Value::Array(Vec::new());
                }
                let items = list
                    .iter()
                    .skip(from as usize)
                    .take((to - from + 1) as usize)
                    .map(|v| Value::BulkString(v.clone()))
                    .collect();
                Value::Array(items)
            }
            _ => wrong_type(),
        },
    }
}

/// Turn Redis's user-facing `start`/`stop` (which allow negatives and overshoot
/// the ends) into a concrete `[from, to]` pair of clamped, non-negative
/// indices. A negative index counts back from the end; anything past an edge is
/// pulled to the edge. If the result has `from > to` the caller returns an
/// empty range.
fn normalize_range(start: i64, stop: i64, len: i64) -> (i64, i64) {
    let from = if start < 0 {
        (len + start).max(0)
    } else {
        start
    };
    let to = if stop < 0 { len + stop } else { stop };
    (from, to.min(len - 1))
}

/// `HSET key field value [field value ...]` — set one or more field/value
/// pairs on the hash at `key`, creating the hash if the key is absent. Returns
/// the number of fields that were *newly added* (updates to existing fields
/// don't count), matching Redis. The trailing arguments must form whole
/// field/value pairs, so the argument count after the key must be even.
fn hset(args: &[Value], storage: &Store) -> Value {
    // key + at least one field/value pair, and the pairs must be complete:
    // that means an odd total (key + even number of field/value tokens).
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return wrong_args("hset");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    // Collect the pairs up front so a bad argument fails before we mutate the
    // store — a rejected HSET leaves the keyspace untouched.
    let mut pairs = Vec::with_capacity((args.len() - 1) / 2);
    let mut i = 1;
    while i < args.len() {
        let field = match unpack_bulk_str(&args[i]) {
            Ok(f) => f,
            Err(e) => return Value::Error(format!("ERR {}", e)),
        };
        let value = match unpack_bulk_str(&args[i + 1]) {
            Ok(v) => v,
            Err(e) => return Value::Error(format!("ERR {}", e)),
        };
        pairs.push((field, value));
        i += 2;
    }

    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    // Like `push`, `or_insert_with` only creates a fresh hash when the key is
    // absent; an existing wrong-typed key is returned untouched and falls
    // through to the WRONGTYPE arm below without being clobbered.
    let entry = store.entry(key).or_insert_with(|| Entry {
        value: StoredValue::Hash(HashMap::new()),
        expires_at: None,
    });
    match &mut entry.value {
        StoredValue::Hash(map) => {
            let mut added = 0i64;
            for (field, value) in pairs {
                // `insert` returns the old value if the field already existed;
                // `None` means this field is brand new, so it counts.
                if map.insert(field, value).is_none() {
                    added += 1;
                }
            }
            Value::Integer(added)
        }
        _ => wrong_type(),
    }
}

/// `HGET key field` — the value stored at `field` in the hash at `key`, as a
/// bulk string, or null if the key or the field is missing. WRONGTYPE if the
/// key holds something other than a hash.
fn hget(args: &[Value], storage: &Store) -> Value {
    if args.len() != 2 {
        return wrong_args("hget");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let field = match unpack_bulk_str(&args[1]) {
        Ok(f) => f,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get(&key) {
        None => Value::Null,
        Some(e) => match &e.value {
            StoredValue::Hash(map) => match map.get(&field) {
                Some(v) => Value::BulkString(v.clone()),
                None => Value::Null,
            },
            _ => wrong_type(),
        },
    }
}

/// `HGETALL key` — every field and value in the hash, flattened into one array
/// as `[field1, value1, field2, value2, ...]`. An empty array if the key is
/// missing. The pair order is unspecified because the backing store is a
/// `HashMap`; clients that need order sort client-side, as they must with
/// Redis too.
fn hgetall(args: &[Value], storage: &Store) -> Value {
    if args.len() != 1 {
        return wrong_args("hgetall");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get(&key) {
        None => Value::Array(Vec::new()),
        Some(e) => match &e.value {
            StoredValue::Hash(map) => {
                let mut out = Vec::with_capacity(map.len() * 2);
                for (field, value) in map {
                    out.push(Value::BulkString(field.clone()));
                    out.push(Value::BulkString(value.clone()));
                }
                Value::Array(out)
            }
            _ => wrong_type(),
        },
    }
}

/// `HDEL key field [field ...]` — remove one or more fields from the hash,
/// returning how many were actually present and removed. A missing key removes
/// nothing and returns `0`; when the last field goes the key is deleted so an
/// empty hash never lingers, mirroring how list pops clean up.
fn hdel(args: &[Value], storage: &Store) -> Value {
    if args.len() < 2 {
        return wrong_args("hdel");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let mut fields = Vec::with_capacity(args.len() - 1);
    for arg in &args[1..] {
        match unpack_bulk_str(arg) {
            Ok(f) => fields.push(f),
            Err(e) => return Value::Error(format!("ERR {}", e)),
        }
    }

    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    // Borrow the entry only long enough to delete the fields and note whether
    // the hash is now empty, then drop that borrow before removing the key —
    // the same borrow dance the list pop does.
    let (removed, now_empty) = match store.get_mut(&key) {
        None => return Value::Integer(0),
        Some(e) => match &mut e.value {
            StoredValue::Hash(map) => {
                let mut removed = 0i64;
                for field in fields {
                    if map.remove(&field).is_some() {
                        removed += 1;
                    }
                }
                (removed, map.is_empty())
            }
            _ => return wrong_type(),
        },
    };
    if now_empty {
        store.remove(&key);
    }
    Value::Integer(removed)
}

fn wrong_args(cmd: &str) -> Value {
    Value::Error(format!(
        "ERR wrong number of arguments for '{}' command",
        cmd
    ))
}

fn extract_command(value: Value) -> Result<(String, Vec<Value>)> {
    match value {
        Value::Array(a) => {
            let mut it = a.into_iter();
            let cmd = it.next().ok_or_else(|| anyhow::anyhow!("empty command"))?;
            Ok((unpack_bulk_str(&cmd)?, it.collect()))
        }
        _ => Err(anyhow::anyhow!("expected an array of bulk strings")),
    }
}

fn unpack_bulk_str(value: &Value) -> Result<String> {
    match value {
        Value::BulkString(s) => Ok(s.clone()),
        _ => Err(anyhow::anyhow!("expected a bulk string argument")),
    }
}

/// Parse a bulk-string argument as a signed 64-bit integer, mirroring the
/// error text Redis returns for a non-numeric argument.
fn unpack_int(value: &Value) -> Result<i64> {
    let s = unpack_bulk_str(value)?;
    s.parse::<i64>()
        .map_err(|_| anyhow::anyhow!("value is not an integer or out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.to_string())
    }

    // Small helper: turn a slice of &str into the owned String iterator
    // `Config::parse` expects, so tests read like a command line.
    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn server() -> Server {
        Server::new(store(), PathBuf::from("dump.rdb"))
    }

    #[test]
    fn wait_reports_zero_replicas_when_none_are_connected() {
        let srv = server();
        assert_eq!(wait(&[bulk("0"), bulk("100")], &srv), Value::Integer(0));
    }

    #[test]
    fn wait_counts_connected_replicas_and_prunes_dead_ones() {
        let srv = server();
        // Two live replicas: keep their receivers alive so the senders stay open.
        let (tx1, _rx1) = mpsc::unbounded_channel::<Bytes>();
        let (tx2, _rx2) = mpsc::unbounded_channel::<Bytes>();
        // A third whose receiver is dropped immediately, so its sender is closed.
        let (tx3, rx3) = mpsc::unbounded_channel::<Bytes>();
        drop(rx3);
        srv.replicas.lock().unwrap().extend([tx1, tx2, tx3]);

        // WAIT counts the two live links and prunes the dead one.
        assert_eq!(wait(&[bulk("2"), bulk("50")], &srv), Value::Integer(2));
        assert_eq!(srv.replicas.lock().unwrap().len(), 2);
    }

    #[test]
    fn wait_validates_its_arguments() {
        let srv = server();
        assert_eq!(wait(&[bulk("0")], &srv), wrong_args("wait"));
        assert_eq!(
            wait(&[bulk("x"), bulk("100")], &srv),
            Value::Error("ERR value is not an integer or out of range".to_string())
        );
    }

    #[test]
    fn write_commands_are_classified_for_replication() {
        for w in [
            "set", "del", "expire", "persist", "rpush", "lpush", "rpop", "lpop", "hset", "hdel",
        ] {
            assert!(is_write_command(w), "{w} should replicate");
        }
        for r in [
            "get", "ttl", "type", "llen", "lrange", "hget", "hgetall", "ping", "echo", "save",
            "bgsave", "wait", "replconf", "psync",
        ] {
            assert!(!is_write_command(r), "{r} should not replicate");
        }
    }

    #[test]
    fn propagate_sends_the_command_bytes_to_every_replica() {
        let replicas: Replicas = Arc::new(Mutex::new(Vec::new()));
        let (tx1, mut rx1) = mpsc::unbounded_channel::<Bytes>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<Bytes>();
        replicas.lock().unwrap().extend([tx1, tx2]);

        let cmd = Value::Array(vec![bulk("SET"), bulk("k"), bulk("v")]);
        propagate(&replicas, &cmd);

        let want = Bytes::from(cmd.serialize().into_bytes());
        assert_eq!(rx1.try_recv().unwrap(), want);
        assert_eq!(rx2.try_recv().unwrap(), want);
    }

    #[test]
    fn propagate_prunes_replicas_whose_receiver_is_gone() {
        let replicas: Replicas = Arc::new(Mutex::new(Vec::new()));
        let (tx_live, mut rx_live) = mpsc::unbounded_channel::<Bytes>();
        let (tx_dead, rx_dead) = mpsc::unbounded_channel::<Bytes>();
        drop(rx_dead); // this replica's task has ended
        replicas.lock().unwrap().extend([tx_live, tx_dead]);

        propagate(&replicas, &Value::Array(vec![bulk("DEL"), bulk("k")]));

        // The dead sender was dropped from the registry; the live one delivered.
        assert_eq!(replicas.lock().unwrap().len(), 1);
        assert!(rx_live.try_recv().is_ok());
    }

    #[test]
    fn a_write_through_process_command_reaches_a_replica() {
        let srv = server();
        let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
        srv.replicas.lock().unwrap().push(tx);

        let set = Value::Array(vec![bulk("SET"), bulk("k"), bulk("v")]);
        assert_eq!(
            process_command(set.clone(), &srv),
            Value::SimpleString("OK".to_string())
        );
        // The SET was streamed to the replica verbatim...
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from(set.serialize().into_bytes())
        );

        // ...but a read (GET) is not, and neither is a rejected write.
        let _ = process_command(Value::Array(vec![bulk("GET"), bulk("k")]), &srv);
        let bad_set = Value::Array(vec![bulk("SET")]); // too few args -> error
        assert!(matches!(process_command(bad_set, &srv), Value::Error(_)));
        assert!(
            rx.try_recv().is_err(),
            "no read or failed write should stream"
        );
    }

    // A command frame built from a command name plus string arguments, the
    // shape `handle_command`/`process_command` expect.
    fn cmd(parts: &[&str]) -> Value {
        Value::Array(parts.iter().map(|p| bulk(p)).collect())
    }

    #[test]
    fn multi_opens_a_transaction_and_queues_following_commands() {
        let srv = server();
        let mut s = Session::default();
        assert_eq!(
            handle_command(cmd(&["MULTI"]), &srv, &mut s),
            Value::SimpleString("OK".to_string())
        );
        // Commands after MULTI are answered +QUEUED, not run.
        assert_eq!(
            handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s),
            Value::SimpleString("QUEUED".to_string())
        );
        // The write has not touched the store yet.
        assert_eq!(get(&[bulk("k")], &srv.store), Value::Null);
    }

    #[test]
    fn exec_runs_the_queue_in_order_and_returns_an_array_of_replies() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s);
        handle_command(cmd(&["GET", "k"]), &srv, &mut s);

        let reply = handle_command(cmd(&["EXEC"]), &srv, &mut s);
        assert_eq!(
            reply,
            Value::Array(vec![
                Value::SimpleString("OK".to_string()),
                Value::BulkString("v".to_string()),
            ])
        );
        // The transaction is closed: a plain command runs immediately again.
        assert_eq!(
            handle_command(cmd(&["GET", "k"]), &srv, &mut s),
            Value::BulkString("v".to_string())
        );
    }

    #[test]
    fn discard_drops_the_queue_without_running_it() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s);
        assert_eq!(
            handle_command(cmd(&["DISCARD"]), &srv, &mut s),
            Value::SimpleString("OK".to_string())
        );
        // Nothing ran, and we are no longer in a transaction.
        assert_eq!(get(&[bulk("k")], &srv.store), Value::Null);
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut s),
            Value::Error("ERR EXEC without MULTI".to_string())
        );
    }

    #[test]
    fn exec_and_discard_without_multi_are_errors() {
        let srv = server();
        let mut s = Session::default();
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut s),
            Value::Error("ERR EXEC without MULTI".to_string())
        );
        assert_eq!(
            handle_command(cmd(&["DISCARD"]), &srv, &mut s),
            Value::Error("ERR DISCARD without MULTI".to_string())
        );
    }

    #[test]
    fn nested_multi_is_rejected_but_keeps_the_transaction_open() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        assert_eq!(
            handle_command(cmd(&["MULTI"]), &srv, &mut s),
            Value::Error("ERR MULTI calls can not be nested".to_string())
        );
        // Still queuing after the rejected nested MULTI.
        assert_eq!(
            handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s),
            Value::SimpleString("QUEUED".to_string())
        );
    }

    #[test]
    fn an_unknown_queued_command_aborts_the_whole_transaction() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s);
        // A bogus command is rejected at queue time and taints the transaction.
        assert!(matches!(
            handle_command(cmd(&["NOPE"]), &srv, &mut s),
            Value::Error(_)
        ));
        // EXEC now aborts wholesale; the earlier SET must NOT have run.
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut s),
            Value::Error("EXECABORT Transaction discarded because of previous errors.".to_string())
        );
        assert_eq!(get(&[bulk("k")], &srv.store), Value::Null);
    }

    #[test]
    fn a_runtime_error_inside_exec_does_not_stop_the_other_commands() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s);
        // LPUSH on a string key is a WRONGTYPE error at run time, not queue time.
        handle_command(cmd(&["LPUSH", "k", "x"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k2", "v2"]), &srv, &mut s);

        let reply = handle_command(cmd(&["EXEC"]), &srv, &mut s);
        match reply {
            Value::Array(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Value::SimpleString("OK".to_string()));
                assert!(matches!(items[1], Value::Error(_)));
                assert_eq!(items[2], Value::SimpleString("OK".to_string()));
            }
            other => panic!("expected an array reply, got {other:?}"),
        }
        // The command after the error still ran.
        assert_eq!(
            get(&[bulk("k2")], &srv.store),
            Value::BulkString("v2".to_string())
        );
    }

    #[test]
    fn queued_writes_propagate_to_replicas_when_exec_runs() {
        let srv = server();
        let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
        srv.replicas.lock().unwrap().push(tx);
        let mut s = Session::default();

        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s);
        // Queuing alone streams nothing to the replica.
        assert!(rx.try_recv().is_err());

        handle_command(cmd(&["EXEC"]), &srv, &mut s);
        // Running the queue on EXEC replicates the write like any other.
        let set = cmd(&["SET", "k", "v"]);
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from(set.serialize().into_bytes())
        );
    }

    #[test]
    fn watch_lets_exec_run_when_no_watched_key_changed() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["SET", "k", "1"]), &srv, &mut s);
        assert_eq!(
            handle_command(cmd(&["WATCH", "k"]), &srv, &mut s),
            Value::SimpleString("OK".to_string())
        );
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k", "2"]), &srv, &mut s);
        // Nobody else touched k, so EXEC goes through.
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut s),
            Value::Array(vec![Value::SimpleString("OK".to_string())])
        );
        assert_eq!(
            get(&[bulk("k")], &srv.store),
            Value::BulkString("2".to_string())
        );
    }

    #[test]
    fn exec_aborts_with_nil_when_a_watched_key_changed() {
        let srv = server();
        let mut watcher = Session::default();
        handle_command(cmd(&["SET", "k", "1"]), &srv, &mut watcher);
        handle_command(cmd(&["WATCH", "k"]), &srv, &mut watcher);
        handle_command(cmd(&["MULTI"]), &srv, &mut watcher);
        handle_command(cmd(&["SET", "k", "from-watcher"]), &srv, &mut watcher);

        // A different connection writes k while the transaction is queued.
        let mut other = Session::default();
        handle_command(cmd(&["SET", "k", "from-other"]), &srv, &mut other);

        // EXEC sees the version moved and aborts: nil array, nothing run.
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut watcher),
            Value::NullArray
        );
        assert_eq!(
            get(&[bulk("k")], &srv.store),
            Value::BulkString("from-other".to_string())
        );
    }

    #[test]
    fn watching_a_missing_key_that_then_appears_aborts_exec() {
        let srv = server();
        let mut watcher = Session::default();
        // k does not exist yet -> watched at version 0.
        handle_command(cmd(&["WATCH", "k"]), &srv, &mut watcher);
        handle_command(cmd(&["MULTI"]), &srv, &mut watcher);
        handle_command(cmd(&["GET", "k"]), &srv, &mut watcher);

        let mut other = Session::default();
        handle_command(cmd(&["SET", "k", "v"]), &srv, &mut other);

        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut watcher),
            Value::NullArray
        );
    }

    #[test]
    fn unwatch_clears_the_guard_so_exec_runs() {
        let srv = server();
        let mut watcher = Session::default();
        handle_command(cmd(&["SET", "k", "1"]), &srv, &mut watcher);
        handle_command(cmd(&["WATCH", "k"]), &srv, &mut watcher);
        assert_eq!(
            handle_command(cmd(&["UNWATCH"]), &srv, &mut watcher),
            Value::SimpleString("OK".to_string())
        );

        let mut other = Session::default();
        handle_command(cmd(&["SET", "k", "2"]), &srv, &mut other);

        handle_command(cmd(&["MULTI"]), &srv, &mut watcher);
        handle_command(cmd(&["SET", "k", "3"]), &srv, &mut watcher);
        // Watch was cleared, so the concurrent write doesn't abort us.
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut watcher),
            Value::Array(vec![Value::SimpleString("OK".to_string())])
        );
    }

    #[test]
    fn watch_is_rejected_once_a_transaction_is_open() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        assert_eq!(
            handle_command(cmd(&["WATCH", "k"]), &srv, &mut s),
            Value::Error("ERR WATCH inside MULTI is not allowed".to_string())
        );
    }

    #[test]
    fn exec_unwatches_so_a_later_transaction_is_unguarded() {
        let srv = server();
        let mut watcher = Session::default();
        handle_command(cmd(&["SET", "k", "1"]), &srv, &mut watcher);
        handle_command(cmd(&["WATCH", "k"]), &srv, &mut watcher);
        handle_command(cmd(&["MULTI"]), &srv, &mut watcher);
        handle_command(cmd(&["EXEC"]), &srv, &mut watcher);

        // A concurrent write to k after the first EXEC must not haunt the second
        // transaction — EXEC should have cleared the watch set.
        let mut other = Session::default();
        handle_command(cmd(&["SET", "k", "2"]), &srv, &mut other);

        handle_command(cmd(&["MULTI"]), &srv, &mut watcher);
        handle_command(cmd(&["SET", "k", "3"]), &srv, &mut watcher);
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut watcher),
            Value::Array(vec![Value::SimpleString("OK".to_string())])
        );
    }

    #[test]
    fn del_bumps_the_version_of_every_key_it_removes() {
        let srv = server();
        handle_command(cmd(&["SET", "a", "1"]), &srv, &mut Session::default());
        handle_command(cmd(&["SET", "b", "1"]), &srv, &mut Session::default());
        let (va, vb) = (srv.version_of("a"), srv.version_of("b"));

        handle_command(cmd(&["DEL", "a", "b"]), &srv, &mut Session::default());
        assert!(srv.version_of("a") > va);
        assert!(srv.version_of("b") > vb);
    }

    #[test]
    fn config_defaults_to_standalone() {
        let config = Config::parse(args(&[])).unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.replicaof, None);
    }

    #[test]
    fn config_parses_replicaof_host_and_port() {
        let config = Config::parse(args(&["--replicaof", "127.0.0.1", "6379"])).unwrap();
        assert_eq!(config.replicaof, Some(("127.0.0.1".to_string(), 6379)));
    }

    #[test]
    fn config_rejects_a_bad_master_port() {
        let err = Config::parse(args(&["--replicaof", "localhost", "notaport"])).unwrap_err();
        assert!(err.contains("invalid master port"));
    }

    #[test]
    fn config_reports_a_missing_replicaof_value() {
        // Host given but the port ran off the end of the argument list.
        let err = Config::parse(args(&["--replicaof", "localhost"])).unwrap_err();
        assert!(err.contains("missing value for --replicaof"));
    }

    #[test]
    fn entry_without_expiry_never_expires_and_has_no_ttl() {
        let e = Entry {
            value: StoredValue::Str("v".to_string()),
            expires_at: None,
        };
        let now = Instant::now();
        assert!(!e.is_expired_at(now));
        assert_eq!(e.ttl_secs_at(now), None);
    }

    #[test]
    fn entry_ttl_rounds_up_and_expires_on_deadline() {
        let now = Instant::now();
        let e = Entry {
            value: StoredValue::Str("v".to_string()),
            expires_at: Some(now + Duration::from_millis(1500)),
        };
        // 1.5s left rounds up to 2, and it is not yet expired.
        assert_eq!(e.ttl_secs_at(now), Some(2));
        assert!(!e.is_expired_at(now));
        // At the deadline it is expired and reports 0 seconds left.
        let at_deadline = now + Duration::from_millis(1500);
        assert!(e.is_expired_at(at_deadline));
        assert_eq!(e.ttl_secs_at(at_deadline), Some(0));
    }

    #[test]
    fn set_with_ex_records_a_positive_ttl() {
        let s = store();
        let reply = set(&[bulk("k"), bulk("v"), bulk("EX"), bulk("100")], &s);
        assert_eq!(reply, Value::SimpleString("OK".to_string()));
        assert_eq!(ttl(&[bulk("k")], &s), Value::Integer(100));
    }

    #[test]
    fn set_with_px_in_the_past_is_gone_on_next_get() {
        let s = store();
        // 1ms TTL: by the time GET runs it has already lapsed.
        set(&[bulk("k"), bulk("v"), bulk("PX"), bulk("1")], &s);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(get(&[bulk("k")], &s), Value::Null);
        // And TTL now reports the key as missing entirely.
        assert_eq!(ttl(&[bulk("k")], &s), Value::Integer(-2));
    }

    #[test]
    fn set_rejects_non_positive_and_non_numeric_expiry() {
        let s = store();
        assert_eq!(
            set(&[bulk("k"), bulk("v"), bulk("EX"), bulk("0")], &s),
            Value::Error("ERR invalid expire time in 'set' command".to_string())
        );
        assert_eq!(
            set(&[bulk("k"), bulk("v"), bulk("EX"), bulk("abc")], &s),
            Value::Error("ERR value is not an integer or out of range".to_string())
        );
        assert_eq!(
            set(&[bulk("k"), bulk("v"), bulk("NOPE"), bulk("5")], &s),
            Value::Error("ERR syntax error".to_string())
        );
        assert_eq!(
            set(&[bulk("k"), bulk("v"), bulk("EX")], &s),
            Value::Error("ERR syntax error".to_string())
        );
    }

    #[test]
    fn plain_set_clears_a_previous_ttl() {
        let s = store();
        set(&[bulk("k"), bulk("v"), bulk("EX"), bulk("100")], &s);
        set(&[bulk("k"), bulk("w")], &s);
        // No expiry any more, so TTL is -1 (exists, lives forever).
        assert_eq!(ttl(&[bulk("k")], &s), Value::Integer(-1));
    }

    #[test]
    fn expire_sets_ttl_only_on_existing_keys() {
        let s = store();
        assert_eq!(
            expire(&[bulk("missing"), bulk("10")], &s),
            Value::Integer(0)
        );
        set(&[bulk("k"), bulk("v")], &s);
        assert_eq!(expire(&[bulk("k"), bulk("10")], &s), Value::Integer(1));
        assert_eq!(ttl(&[bulk("k")], &s), Value::Integer(10));
    }

    #[test]
    fn expire_with_non_positive_deletes_the_key() {
        let s = store();
        set(&[bulk("k"), bulk("v")], &s);
        assert_eq!(expire(&[bulk("k"), bulk("-1")], &s), Value::Integer(1));
        assert_eq!(get(&[bulk("k")], &s), Value::Null);
    }

    #[test]
    fn ttl_distinguishes_missing_no_expiry_and_expiring() {
        let s = store();
        assert_eq!(ttl(&[bulk("missing")], &s), Value::Integer(-2));
        set(&[bulk("k"), bulk("v")], &s);
        assert_eq!(ttl(&[bulk("k")], &s), Value::Integer(-1));
        expire(&[bulk("k"), bulk("50")], &s);
        assert_eq!(ttl(&[bulk("k")], &s), Value::Integer(50));
    }

    #[test]
    fn persist_removes_expiry_and_reports_correctly() {
        let s = store();
        assert_eq!(persist(&[bulk("missing")], &s), Value::Integer(0));
        set(&[bulk("k"), bulk("v")], &s);
        // No expiry to remove yet.
        assert_eq!(persist(&[bulk("k")], &s), Value::Integer(0));
        expire(&[bulk("k"), bulk("100")], &s);
        assert_eq!(persist(&[bulk("k")], &s), Value::Integer(1));
        assert_eq!(ttl(&[bulk("k")], &s), Value::Integer(-1));
    }

    #[test]
    fn type_reports_string_for_a_set_key() {
        let s = store();
        set(&[bulk("k"), bulk("v")], &s);
        assert_eq!(
            type_cmd(&[bulk("k")], &s),
            Value::SimpleString("string".to_string())
        );
    }

    #[test]
    fn type_reports_none_for_a_missing_key() {
        let s = store();
        assert_eq!(
            type_cmd(&[bulk("missing")], &s),
            Value::SimpleString("none".to_string())
        );
    }

    #[test]
    fn type_reports_none_after_a_key_expires() {
        let s = store();
        set(&[bulk("k"), bulk("v"), bulk("PX"), bulk("1")], &s);
        std::thread::sleep(Duration::from_millis(5));
        // Passive expiry runs on TYPE too, so a lapsed key looks absent.
        assert_eq!(
            type_cmd(&[bulk("k")], &s),
            Value::SimpleString("none".to_string())
        );
    }

    #[test]
    fn type_checks_arity() {
        let s = store();
        assert_eq!(
            type_cmd(&[], &s),
            Value::Error("ERR wrong number of arguments for 'type' command".to_string())
        );
    }

    // Convenience: read the whole list back as an array of bulk strings.
    fn all(s: &Store, key: &str) -> Value {
        lrange(&[bulk(key), bulk("0"), bulk("-1")], s)
    }

    fn arr(items: &[&str]) -> Value {
        Value::Array(items.iter().map(|x| bulk(x)).collect())
    }

    #[test]
    fn rpush_appends_and_returns_the_new_length() {
        let s = store();
        assert_eq!(
            push(&[bulk("l"), bulk("a")], &s, Side::Right),
            Value::Integer(1)
        );
        assert_eq!(
            push(&[bulk("l"), bulk("b"), bulk("c")], &s, Side::Right),
            Value::Integer(3)
        );
        // RPUSH keeps insertion order: a, b, c.
        assert_eq!(all(&s, "l"), arr(&["a", "b", "c"]));
    }

    #[test]
    fn lpush_prepends_so_order_reverses() {
        let s = store();
        // LPUSH a b c pushes each onto the head, leaving [c, b, a].
        assert_eq!(
            push(
                &[bulk("l"), bulk("a"), bulk("b"), bulk("c")],
                &s,
                Side::Left
            ),
            Value::Integer(3)
        );
        assert_eq!(all(&s, "l"), arr(&["c", "b", "a"]));
    }

    #[test]
    fn llen_counts_and_reports_zero_for_missing() {
        let s = store();
        assert_eq!(llen(&[bulk("missing")], &s), Value::Integer(0));
        push(&[bulk("l"), bulk("a"), bulk("b")], &s, Side::Right);
        assert_eq!(llen(&[bulk("l")], &s), Value::Integer(2));
    }

    #[test]
    fn type_reports_list_for_a_pushed_key() {
        let s = store();
        push(&[bulk("l"), bulk("a")], &s, Side::Right);
        assert_eq!(
            type_cmd(&[bulk("l")], &s),
            Value::SimpleString("list".to_string())
        );
    }

    #[test]
    fn lpop_and_rpop_take_from_the_right_ends() {
        let s = store();
        push(
            &[bulk("l"), bulk("a"), bulk("b"), bulk("c")],
            &s,
            Side::Right,
        );
        // List is [a, b, c]. LPOP takes the head, RPOP the tail.
        assert_eq!(
            pop(&[bulk("l")], &s, Side::Left),
            Value::BulkString("a".to_string())
        );
        assert_eq!(
            pop(&[bulk("l")], &s, Side::Right),
            Value::BulkString("c".to_string())
        );
        assert_eq!(all(&s, "l"), arr(&["b"]));
    }

    #[test]
    fn popping_the_last_element_deletes_the_key() {
        let s = store();
        push(&[bulk("l"), bulk("only")], &s, Side::Right);
        assert_eq!(
            pop(&[bulk("l")], &s, Side::Left),
            Value::BulkString("only".to_string())
        );
        // The now-empty list is gone entirely, so TYPE sees nothing.
        assert_eq!(
            type_cmd(&[bulk("l")], &s),
            Value::SimpleString("none".to_string())
        );
        assert_eq!(llen(&[bulk("l")], &s), Value::Integer(0));
    }

    #[test]
    fn popping_a_missing_key_is_null() {
        let s = store();
        assert_eq!(pop(&[bulk("nope")], &s, Side::Left), Value::Null);
        assert_eq!(pop(&[bulk("nope")], &s, Side::Right), Value::Null);
    }

    #[test]
    fn lrange_handles_negative_and_out_of_range_indices() {
        let s = store();
        push(
            &[bulk("l"), bulk("a"), bulk("b"), bulk("c"), bulk("d")],
            &s,
            Side::Right,
        );
        // Whole list via 0..-1.
        assert_eq!(all(&s, "l"), arr(&["a", "b", "c", "d"]));
        // A middle slice.
        assert_eq!(
            lrange(&[bulk("l"), bulk("1"), bulk("2")], &s),
            arr(&["b", "c"])
        );
        // Negative start counts from the end; stop past the end is clamped.
        assert_eq!(
            lrange(&[bulk("l"), bulk("-2"), bulk("100")], &s),
            arr(&["c", "d"])
        );
        // An inverted range is empty, not an error.
        assert_eq!(
            lrange(&[bulk("l"), bulk("3"), bulk("1")], &s),
            Value::Array(Vec::new())
        );
    }

    #[test]
    fn lrange_on_a_missing_key_is_an_empty_array() {
        let s = store();
        assert_eq!(
            lrange(&[bulk("missing"), bulk("0"), bulk("-1")], &s),
            Value::Array(Vec::new())
        );
    }

    #[test]
    fn list_commands_reject_a_string_key_with_wrongtype() {
        let s = store();
        set(&[bulk("k"), bulk("v")], &s);
        let wt = wrong_type();
        assert_eq!(push(&[bulk("k"), bulk("x")], &s, Side::Right), wt);
        assert_eq!(pop(&[bulk("k")], &s, Side::Left), wt);
        assert_eq!(llen(&[bulk("k")], &s), wt);
        assert_eq!(lrange(&[bulk("k"), bulk("0"), bulk("-1")], &s), wt);
        // ...and GET on a list is symmetric.
        push(&[bulk("l"), bulk("a")], &s, Side::Right);
        assert_eq!(get(&[bulk("l")], &s), wt);
    }

    #[test]
    fn push_leaves_a_wrong_typed_key_untouched() {
        let s = store();
        set(&[bulk("k"), bulk("v")], &s);
        push(&[bulk("k"), bulk("x")], &s, Side::Right);
        // The failed RPUSH must not have replaced or corrupted the string.
        assert_eq!(get(&[bulk("k")], &s), Value::BulkString("v".to_string()));
    }

    #[test]
    fn push_checks_arity() {
        let s = store();
        assert_eq!(
            push(&[bulk("l")], &s, Side::Right),
            Value::Error("ERR wrong number of arguments for 'rpush' command".to_string())
        );
    }

    // Read a hash back as a sorted (field, value) list so assertions don't
    // depend on the HashMap's arbitrary iteration order.
    fn hash_pairs(v: Value) -> Vec<(String, String)> {
        let items = match v {
            Value::Array(items) => items,
            other => panic!("expected an array, got {:?}", other),
        };
        let mut pairs = Vec::new();
        let mut it = items.into_iter();
        while let (Some(f), Some(val)) = (it.next(), it.next()) {
            match (f, val) {
                (Value::BulkString(f), Value::BulkString(val)) => pairs.push((f, val)),
                other => panic!("expected bulk strings, got {:?}", other),
            }
        }
        pairs.sort();
        pairs
    }

    #[test]
    fn hset_adds_fields_and_counts_only_new_ones() {
        let s = store();
        // Two brand-new fields → 2.
        assert_eq!(
            hset(&[bulk("h"), bulk("a"), bulk("1"), bulk("b"), bulk("2")], &s),
            Value::Integer(2)
        );
        // Updating `a` and adding `c` → only `c` is new, so 1.
        assert_eq!(
            hset(&[bulk("h"), bulk("a"), bulk("9"), bulk("c"), bulk("3")], &s),
            Value::Integer(1)
        );
        assert_eq!(
            hash_pairs(hgetall(&[bulk("h")], &s)),
            vec![
                ("a".to_string(), "9".to_string()),
                ("b".to_string(), "2".to_string()),
                ("c".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn hget_returns_value_or_null() {
        let s = store();
        hset(&[bulk("h"), bulk("field"), bulk("val")], &s);
        assert_eq!(
            hget(&[bulk("h"), bulk("field")], &s),
            Value::BulkString("val".to_string())
        );
        // Missing field on an existing hash → null.
        assert_eq!(hget(&[bulk("h"), bulk("nope")], &s), Value::Null);
        // Missing key entirely → null.
        assert_eq!(hget(&[bulk("missing"), bulk("field")], &s), Value::Null);
    }

    #[test]
    fn hgetall_on_missing_key_is_empty() {
        let s = store();
        assert_eq!(hgetall(&[bulk("missing")], &s), Value::Array(Vec::new()));
    }

    #[test]
    fn hdel_removes_fields_and_deletes_the_emptied_key() {
        let s = store();
        hset(&[bulk("h"), bulk("a"), bulk("1"), bulk("b"), bulk("2")], &s);
        // Removing a present and an absent field counts only the present one.
        assert_eq!(
            hdel(&[bulk("h"), bulk("a"), bulk("gone")], &s),
            Value::Integer(1)
        );
        // Removing the last field empties the hash, so the key disappears.
        assert_eq!(hdel(&[bulk("h"), bulk("b")], &s), Value::Integer(1));
        assert_eq!(
            type_cmd(&[bulk("h")], &s),
            Value::SimpleString("none".to_string())
        );
        // HDEL on a missing key removes nothing.
        assert_eq!(hdel(&[bulk("h"), bulk("a")], &s), Value::Integer(0));
    }

    #[test]
    fn type_reports_hash_for_a_hset_key() {
        let s = store();
        hset(&[bulk("h"), bulk("f"), bulk("v")], &s);
        assert_eq!(
            type_cmd(&[bulk("h")], &s),
            Value::SimpleString("hash".to_string())
        );
    }

    #[test]
    fn hash_commands_reject_a_string_key_with_wrongtype() {
        let s = store();
        set(&[bulk("k"), bulk("v")], &s);
        let wt = wrong_type();
        assert_eq!(hset(&[bulk("k"), bulk("f"), bulk("v")], &s), wt);
        assert_eq!(hget(&[bulk("k"), bulk("f")], &s), wt);
        assert_eq!(hgetall(&[bulk("k")], &s), wt);
        assert_eq!(hdel(&[bulk("k"), bulk("f")], &s), wt);
        // The failed HSET must not have clobbered the string value.
        assert_eq!(get(&[bulk("k")], &s), Value::BulkString("v".to_string()));
    }

    #[test]
    fn hset_checks_arity_including_dangling_field() {
        let s = store();
        // No pairs at all.
        assert_eq!(
            hset(&[bulk("h")], &s),
            Value::Error("ERR wrong number of arguments for 'hset' command".to_string())
        );
        // A field with no value (even arg count) is rejected.
        assert_eq!(
            hset(&[bulk("h"), bulk("field")], &s),
            Value::Error("ERR wrong number of arguments for 'hset' command".to_string())
        );
    }

    // Build an owned argument list the way `std::env::args().skip(1)` would
    // hand it to `Config::parse`.
    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn config_defaults_when_no_args_are_given() {
        let cfg = Config::parse(argv(&[])).unwrap();
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.port, 6379);
        assert_eq!(cfg.dir, ".");
        assert_eq!(cfg.dbfilename, "dump.rdb");
        assert_eq!(cfg.addr(), "127.0.0.1:6379");
    }

    #[test]
    fn config_parses_all_flags() {
        let cfg = Config::parse(argv(&[
            "-p",
            "7000",
            "--dir",
            "/var/lib/flashdb",
            "--dbfilename",
            "snapshot.rdb",
        ]))
        .unwrap();
        assert_eq!(cfg.port, 7000);
        assert_eq!(cfg.dir, "/var/lib/flashdb");
        assert_eq!(cfg.dbfilename, "snapshot.rdb");
        assert_eq!(cfg.addr(), "127.0.0.1:7000");
    }

    #[test]
    fn config_accepts_the_long_port_flag() {
        let cfg = Config::parse(argv(&["--port", "6380"])).unwrap();
        assert_eq!(cfg.port, 6380);
    }

    #[test]
    fn config_rejects_unknown_flags() {
        let err = Config::parse(argv(&["--nope", "1"])).unwrap_err();
        assert_eq!(err, "unknown argument '--nope'");
    }

    #[test]
    fn config_rejects_a_flag_with_no_value() {
        let err = Config::parse(argv(&["-p"])).unwrap_err();
        assert_eq!(err, "missing value for -p option");
    }

    fn rdb_entry(key: &str, val: &str, expire_at_ms: Option<u64>) -> rdb::RdbEntry {
        rdb::RdbEntry {
            key: key.to_string(),
            value: StoredValue::Str(val.to_string()),
            expire_at_ms,
        }
    }

    #[test]
    fn loading_keeps_unexpiring_keys_verbatim() {
        let now = Instant::now();
        let map = entries_to_store(vec![rdb_entry("k", "v", None)], 1_000, now);
        let entry = map.get("k").expect("key should be loaded");
        assert!(entry.expires_at.is_none());
        match &entry.value {
            StoredValue::Str(s) => assert_eq!(s, "v"),
            other => panic!("expected a string, got {:?}", other),
        }
    }

    #[test]
    fn loading_drops_a_key_whose_deadline_already_passed() {
        let now = Instant::now();
        // Deadline 500ms, but "now" on the wall clock is already 1000ms.
        let map = entries_to_store(vec![rdb_entry("stale", "v", Some(500))], 1_000, now);
        assert!(!map.contains_key("stale"), "expired key must not load");
    }

    #[test]
    fn loading_converts_a_future_deadline_into_a_live_ttl() {
        let now = Instant::now();
        // Deadline is 10s past the load moment on the wall clock.
        let map = entries_to_store(vec![rdb_entry("k", "v", Some(10_000))], 0, now);
        let entry = map.get("k").expect("future-dated key should load");
        // Its Instant deadline should sit ~10s ahead of the reference instant,
        // so TTL reads back as 10 seconds.
        assert_eq!(entry.ttl_secs_at(now), Some(10));
        assert!(!entry.is_expired_at(now));
    }

    #[test]
    fn a_missing_rdb_file_loads_an_empty_store() {
        let cfg = Config {
            port: 6379,
            dir: "/nonexistent-flashdb-dir".to_string(),
            dbfilename: "nope.rdb".to_string(),
            replicaof: None,
        };
        let map = load_store_from_rdb(&cfg).expect("a missing file is not an error");
        assert!(map.is_empty());
    }

    #[test]
    fn config_rejects_a_non_numeric_or_out_of_range_port() {
        assert_eq!(
            Config::parse(argv(&["-p", "abc"])).unwrap_err(),
            "invalid port number 'abc'"
        );
        // 70000 doesn't fit in a u16, so it's rejected the same way.
        assert_eq!(
            Config::parse(argv(&["-p", "70000"])).unwrap_err(),
            "invalid port number '70000'"
        );
    }

    // Build the [`rdb::RdbEntry`]s for a store, then convert the same expiry the
    // other way with `entries_to_store`, and confirm a future deadline survives
    // the monotonic → wall-clock → monotonic round trip and an expired key is
    // dropped on save.
    #[test]
    fn map_to_rdb_entries_drops_expired_and_converts_future_deadlines() {
        let now = Instant::now();
        let mut map = HashMap::new();
        map.insert(
            "live".to_string(),
            Entry {
                value: StoredValue::Str("v".to_string()),
                expires_at: Some(now + Duration::from_secs(10)),
            },
        );
        map.insert(
            "dead".to_string(),
            Entry {
                value: StoredValue::Str("v".to_string()),
                expires_at: Some(now - Duration::from_secs(1)),
            },
        );
        map.insert(
            "forever".to_string(),
            Entry {
                value: StoredValue::Str("v".to_string()),
                expires_at: None,
            },
        );

        let entries = map_to_rdb_entries(&map, 1_000_000, now);
        // The already-lapsed key is not persisted.
        assert!(!entries.iter().any(|e| e.key == "dead"));
        assert_eq!(entries.len(), 2);
        // The live key's deadline is now-plus-10s on the wall clock.
        let live = entries.iter().find(|e| e.key == "live").unwrap();
        assert_eq!(live.expire_at_ms, Some(1_000_000 + 10_000));
        // The immortal key carries no deadline.
        let forever = entries.iter().find(|e| e.key == "forever").unwrap();
        assert_eq!(forever.expire_at_ms, None);
    }

    #[test]
    fn save_writes_a_snapshot_that_reloads_every_value_type() {
        let dir = std::env::temp_dir().join(format!("flashdb-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.rdb");

        let s = store();
        set(&[bulk("greeting"), bulk("hello world")], &s);
        push(&[bulk("l"), bulk("a"), bulk("b")], &s, Side::Right);
        hset(&[bulk("h"), bulk("f"), bulk("v")], &s);
        let server = Server::new(s.clone(), path.clone());
        assert_eq!(save(&[], &server), Value::SimpleString("OK".to_string()));

        // Reload the file exactly as startup would.
        let cfg = Config {
            port: 6379,
            dir: dir.to_string_lossy().into_owned(),
            dbfilename: "dump.rdb".to_string(),
            replicaof: None,
        };
        let reloaded = load_store_from_rdb(&cfg).unwrap();

        match &reloaded.get("greeting").unwrap().value {
            StoredValue::Str(v) => assert_eq!(v, "hello world"),
            other => panic!("expected a string, got {other:?}"),
        }
        match &reloaded.get("l").unwrap().value {
            StoredValue::List(list) => {
                assert_eq!(
                    list.iter().cloned().collect::<Vec<_>>(),
                    vec!["a".to_string(), "b".to_string()]
                );
            }
            other => panic!("expected a list, got {other:?}"),
        }
        match &reloaded.get("h").unwrap().value {
            StoredValue::Hash(map) => assert_eq!(map.get("f").map(String::as_str), Some("v")),
            other => panic!("expected a hash, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_and_bgsave_reject_arguments() {
        let server = Server::new(store(), PathBuf::from("unused.rdb"));
        assert_eq!(
            save(&[bulk("x")], &server),
            Value::Error("ERR wrong number of arguments for 'save' command".to_string())
        );
        assert_eq!(
            bgsave(&[bulk("x")], &server),
            Value::Error("ERR wrong number of arguments for 'bgsave' command".to_string())
        );
    }
}
