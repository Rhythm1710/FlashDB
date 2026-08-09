//! FlashDB — a Redis-compatible in-memory database written from scratch.
//!
//! The binary in `main.rs` is a thin wrapper around this library: it binds a
//! listener and hands it to [`run`]. Exposing the server as a library lets the
//! integration tests in `tests/` start a real server on an ephemeral port and
//! talk to it over a genuine TCP socket, exactly as a client would.

use anyhow::Result;
use bytes::{Buf, Bytes, BytesMut};
use resp::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;

mod commands;
pub mod rdb;
pub mod replication;
pub mod resp;
pub mod stream;

use commands::lists::Side;
use stream::Stream;

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
    /// A stream: an append-only log of entries keyed by a monotonically
    /// increasing `<ms>-<seq>` ID (see [`crate::stream`]). This is the fourth
    /// value type; `Set` is still to come.
    Stream(Stream),
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
            StoredValue::Stream(_) => "stream",
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

/// The pub/sub routing table: for each channel name, the set of connections
/// currently subscribed to it, keyed by a per-connection id so a single
/// connection can be removed cleanly when it unsubscribes or disconnects.
///
/// Each subscriber is represented by the *sending* half of an `mpsc` channel —
/// the exact same trick as the replica registry ([`Replicas`]). `PUBLISH` runs
/// inside the *publisher's* task: it looks up the channel here and pushes the
/// message bytes into every subscriber's sender. Each subscriber's own task
/// (see [`serve_subscriber`]) owns the matching receiver and writes the bytes
/// down its socket. `Arc<Mutex<..>>` because any connection may publish to a
/// channel any other connection is subscribed to.
type Subscribers = Arc<Mutex<HashMap<String, HashMap<u64, mpsc::UnboundedSender<Bytes>>>>>;

/// Hands out a unique id to each connection so it can be tracked in the pub/sub
/// routing table and removed again by id. A plain monotonic counter — the value
/// only has to be unique for the life of the process, not meaningful.
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

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
    subscribers: Subscribers,
    /// Wakes any client parked in a blocking `XREAD ... BLOCK`. Every successful
    /// `XADD` calls `notify_waiters()` on it; a blocked reader re-checks its
    /// streams each time it fires. A single shared `Notify` (rather than one per
    /// stream) is enough because a spurious wake just costs one extra re-check.
    stream_notify: Arc<Notify>,
}

impl Server {
    fn new(store: Store, rdb_path: PathBuf) -> Self {
        Server {
            store,
            rdb_path: Arc::new(rdb_path),
            replicas: Arc::new(Mutex::new(Vec::new())),
            versions: Arc::new(Mutex::new(HashMap::new())),
            subscribers: Arc::new(Mutex::new(HashMap::new())),
            stream_notify: Arc::new(Notify::new()),
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
            | "publish"
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
    // A stable id for this connection, used to track and later remove its
    // subscriptions from the shared pub/sub routing table.
    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
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

        // `SUBSCRIBE` puts the connection into pub/sub mode, where the server
        // may push messages to it at any time — not just in reply to a command.
        // That needs a loop that watches the socket and a message channel at
        // once, so hand the connection to `serve_subscriber`. It runs until the
        // client unsubscribes from everything (then we resume the normal loop)
        // or disconnects. A `SUBSCRIBE` mid-transaction is left to flow into
        // `handle_command` instead, which rejects it like any other command that
        // can't be queued.
        if command_name(&value).as_deref() == Some("subscribe") && !session.in_multi {
            let (mut stream, mut buffer) = handler.into_parts();
            serve_subscriber(&mut stream, &mut buffer, &server, client_id, value).await?;
            // Back to a plain request/response session, carrying over any bytes
            // the client pipelined after its final `UNSUBSCRIBE`.
            handler = resp::RespHandler::from_parts(stream, buffer);
            continue;
        }

        // A blocking `XREAD ... BLOCK` must be handled here, at the async layer,
        // so it can actually park the task on a timer/notification instead of
        // spinning inside the synchronous dispatch. Inside a transaction we skip
        // this: Redis runs a queued blocking command as if it were non-blocking,
        // so it falls through to `handle_command` like anything else.
        if !session.in_multi {
            if let Some(req) = as_blocking_xread(&value) {
                let response = xread_blocking(req, &server).await;
                handler.write_value(response).await?;
                continue;
            }
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
    let snapshot = commands::persistence::snapshot_bytes(&server.store);
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

/// Build the RESP frame Redis pushes to a subscriber when a message arrives:
/// the three-element array `["message", channel, payload]`.
fn message_frame(channel: &str, payload: &str) -> Value {
    Value::Array(vec![
        Value::BulkString("message".to_string()),
        Value::BulkString(channel.to_string()),
        Value::BulkString(payload.to_string()),
    ])
}

/// Build a subscribe/unsubscribe *confirmation* frame: `[kind, channel, count]`,
/// where `count` is how many channels the connection is subscribed to *after*
/// this action. `channel` is `None` only for an `UNSUBSCRIBE` that had nothing
/// to unsubscribe, which Redis answers with a nil channel field.
fn subscription_reply(kind: &str, channel: Option<&str>, count: usize) -> Value {
    let channel_field = match channel {
        Some(c) => Value::BulkString(c.to_string()),
        None => Value::Null,
    };
    Value::Array(vec![
        Value::BulkString(kind.to_string()),
        channel_field,
        Value::Integer(count as i64),
    ])
}

/// `PUBLISH channel message` — deliver `message` to every connection subscribed
/// to `channel`, and reply with how many received it.
///
/// This runs in the *publisher's* task. For each subscriber we push the encoded
/// message frame into that subscriber's channel; the subscriber's own task does
/// the socket write. A send fails only if that subscriber's task has already
/// gone, so `retain` delivers and prunes dead subscribers in one pass — the same
/// pattern `propagate` uses for replicas. An empty channel is removed so the map
/// doesn't accumulate keys for channels no one listens on any more.
fn publish(args: &[Value], server: &Server) -> Value {
    if args.len() != 2 {
        return wrong_args("publish");
    }
    let channel = match unpack_bulk_str(&args[0]) {
        Ok(c) => c,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let message = match unpack_bulk_str(&args[1]) {
        Ok(m) => m,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };

    let frame = Bytes::from(message_frame(&channel, &message).serialize().into_bytes());
    let mut table = server.subscribers.lock().unwrap();
    let mut delivered: i64 = 0;
    if let Some(subs) = table.get_mut(&channel) {
        subs.retain(|_id, tx| match tx.send(frame.clone()) {
            Ok(()) => {
                delivered += 1;
                true
            }
            Err(_) => false,
        });
        if subs.is_empty() {
            table.remove(&channel);
        }
    }
    Value::Integer(delivered)
}

/// Add `client_id` (reachable via `tx`) to each named channel's subscriber set,
/// recording the channels locally in `subscribed`, and return the confirmation
/// frames — one per channel, as Redis sends. Re-subscribing to a channel already
/// held is harmless: the set membership is idempotent and the confirmation still
/// goes out with the current count.
///
/// The return is the raw wire bytes rather than a `Value`, because `SUBSCRIBE`
/// with several channels replies with several *independent* top-level frames
/// back to back — not one array wrapping them. Concatenating each frame's own
/// serialization is exactly that; a `Value::Array` would prepend a spurious
/// `*N\r\n` header and turn it into an array of arrays.
fn do_subscribe(
    args: &[Value],
    server: &Server,
    client_id: u64,
    tx: &mpsc::UnboundedSender<Bytes>,
    subscribed: &mut HashSet<String>,
) -> String {
    if args.is_empty() {
        return wrong_args("subscribe").serialize();
    }
    let mut out = String::new();
    for arg in args {
        let channel = match unpack_bulk_str(arg) {
            Ok(c) => c,
            Err(e) => return Value::Error(format!("ERR {}", e)).serialize(),
        };
        server
            .subscribers
            .lock()
            .unwrap()
            .entry(channel.clone())
            .or_default()
            .insert(client_id, tx.clone());
        subscribed.insert(channel.clone());
        out.push_str(
            &subscription_reply("subscribe", Some(&channel), subscribed.len()).serialize(),
        );
    }
    out
}

/// Remove `client_id` from the named channels (or from *all* the connection's
/// channels when `args` is empty, which is Redis's "unsubscribe from
/// everything"), updating `subscribed`, and return one confirmation frame per
/// channel as raw wire bytes (see [`do_subscribe`] on why not a `Value`). An
/// `UNSUBSCRIBE` with nothing to remove still gets a single reply with a nil
/// channel and count 0, matching Redis.
fn do_unsubscribe(
    args: &[Value],
    server: &Server,
    client_id: u64,
    subscribed: &mut HashSet<String>,
) -> String {
    // Which channels to drop: the named ones, or every one we hold if none named.
    let targets: Vec<String> = if args.is_empty() {
        subscribed.iter().cloned().collect()
    } else {
        let mut names = Vec::with_capacity(args.len());
        for arg in args {
            match unpack_bulk_str(arg) {
                Ok(c) => names.push(c),
                Err(e) => return Value::Error(format!("ERR {}", e)).serialize(),
            }
        }
        names
    };

    if targets.is_empty() {
        return subscription_reply("unsubscribe", None, 0).serialize();
    }

    let mut out = String::new();
    for channel in targets {
        remove_subscriber(server, client_id, &channel);
        subscribed.remove(&channel);
        out.push_str(
            &subscription_reply("unsubscribe", Some(&channel), subscribed.len()).serialize(),
        );
    }
    out
}

/// Drop `client_id` from one channel's subscriber set, removing the channel
/// entirely once it has no subscribers left.
fn remove_subscriber(server: &Server, client_id: u64, channel: &str) {
    let mut table = server.subscribers.lock().unwrap();
    if let Some(subs) = table.get_mut(channel) {
        subs.remove(&client_id);
        if subs.is_empty() {
            table.remove(channel);
        }
    }
}

/// Serve a connection that has entered pub/sub mode.
///
/// While subscribed, a connection is half server-push, half request/response:
/// the server may send it a message at any moment (someone else `PUBLISH`ed),
/// and the client may still send `SUBSCRIBE`/`UNSUBSCRIBE`/`PING`. Handling both
/// at once is why this can't live in the plain read-reply loop: we `split` the
/// socket and `select!` between a message arriving on our channel and a command
/// arriving on the wire.
///
/// Returns when the client has unsubscribed from everything (control goes back
/// to the normal loop) or the socket closes. Either way, every remaining
/// subscription for this connection is torn out of the shared table on the way
/// out so a vanished client leaves nothing behind.
async fn serve_subscriber(
    stream: &mut TcpStream,
    buffer: &mut BytesMut,
    server: &Server,
    client_id: u64,
    first: Value,
) -> Result<()> {
    // Our end of the push channel. The sending halves handed to the routing
    // table are clones; this receiver is the one socket-writer for them all.
    // Holding `tx` here for the whole loop keeps the channel open even after we
    // hand clones out, so `rx.recv()` never ends just because a `PUBLISH`er
    // pruned a stale clone.
    let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
    let mut subscribed: HashSet<String> = HashSet::new();

    let (mut read_half, mut write_half) = stream.split();

    // The command that got us here is the first SUBSCRIBE; run it before looping.
    let reply = do_subscribe(
        command_args(&first),
        server,
        client_id,
        &tx,
        &mut subscribed,
    );
    write_half.write_all(reply.as_bytes()).await?;
    write_half.flush().await?;

    let result = 'conn: loop {
        // Drain any complete commands already sitting in the buffer (a client
        // may have pipelined several) before waiting on the socket again.
        loop {
            match resp::parse_message(buffer) {
                Ok(Some((value, consumed))) => {
                    buffer.advance(consumed);
                    let reply = handle_sub_command(value, server, client_id, &tx, &mut subscribed);
                    write_half.write_all(reply.as_bytes()).await?;
                    write_half.flush().await?;
                    // Unsubscribed from the last channel: leave pub/sub mode.
                    if subscribed.is_empty() {
                        break 'conn Ok(());
                    }
                }
                Ok(None) => break, // need more bytes
                Err(proto) => {
                    // Garbage on the wire: reply once and close, as the main loop does.
                    let reply = Value::Error(format!("ERR {}", proto)).serialize();
                    let _ = write_half.write_all(reply.as_bytes()).await;
                    let _ = write_half.flush().await;
                    break 'conn Ok(());
                }
            }
        }

        tokio::select! {
            // A message someone else published on a channel we're subscribed to.
            pushed = rx.recv() => match pushed {
                Some(bytes) => {
                    write_half.write_all(&bytes).await?;
                    write_half.flush().await?;
                }
                None => break 'conn Ok(()), // every sender gone (can't happen while `tx` lives)
            },
            // More bytes from the client — loop back up to parse them.
            read = read_half.read_buf(buffer) => match read {
                Ok(0) => break 'conn Ok(()), // client hung up
                Ok(_) => {}
                Err(e) => break 'conn Err(e.into()),
            },
        }
    };

    // Whatever ended us, make sure this connection owns no subscriptions any
    // more — otherwise a `PUBLISH` would try to push to a socket that's gone.
    for channel in &subscribed {
        remove_subscriber(server, client_id, channel);
    }
    result
}

/// Dispatch one command received while the connection is in pub/sub mode.
///
/// RESP2 only permits a subscribed client to (un)subscribe or `PING`; anything
/// else is refused with Redis's own error, rather than run. This keeps a
/// subscribed connection from, say, issuing a blocking read that would strand
/// the pushed-message stream.
fn handle_sub_command(
    value: Value,
    server: &Server,
    client_id: u64,
    tx: &mpsc::UnboundedSender<Bytes>,
    subscribed: &mut HashSet<String>,
) -> String {
    match command_name(&value).as_deref() {
        Some("subscribe") => do_subscribe(command_args(&value), server, client_id, tx, subscribed),
        Some("unsubscribe") => do_unsubscribe(command_args(&value), server, client_id, subscribed),
        Some("ping") => Value::SimpleString("PONG".to_string()).serialize(),
        Some(other) => Value::Error(format!(
            "ERR Can't execute '{}': only (P)SUBSCRIBE / (P)UNSUBSCRIBE / PING / QUIT are allowed in this context",
            other
        ))
        .serialize(),
        None => Value::Error("ERR unknown command".to_string()).serialize(),
    }
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
        "rpush" => commands::lists::push(&args, storage, Side::Right),
        "lpush" => commands::lists::push(&args, storage, Side::Left),
        "rpop" => commands::lists::pop(&args, storage, Side::Right),
        "lpop" => commands::lists::pop(&args, storage, Side::Left),
        "llen" => commands::lists::llen(&args, storage),
        "lrange" => commands::lists::lrange(&args, storage),
        "hset" => commands::hashes::hset(&args, storage),
        "hget" => commands::hashes::hget(&args, storage),
        "hgetall" => commands::hashes::hgetall(&args, storage),
        "hdel" => commands::hashes::hdel(&args, storage),
        "xadd" => xadd(&args, storage),
        "xlen" => xlen(&args, storage),
        "xrange" => xrange(&args, storage),
        "xrevrange" => xrevrange(&args, storage),
        "xdel" => xdel(&args, storage),
        "xtrim" => xtrim(&args, storage),
        "xread" => xread(&args, storage),
        "save" => commands::persistence::save(&args, server),
        "bgsave" => commands::persistence::bgsave(&args, server),
        // A replica announces its listening port and capabilities with REPLCONF
        // during the handshake; the master just acknowledges each one. (When the
        // master later sends `REPLCONF GETACK` the *replica* answers — that
        // direction is the replica's job, not handled here.)
        "replconf" => Value::SimpleString("OK".to_string()),
        "wait" => commands::persistence::wait(&args, server),
        // Pub/sub. `PUBLISH` runs from an ordinary connection, so it lives here;
        // `SUBSCRIBE`/`UNSUBSCRIBE` are handled by the dedicated subscriber loop
        // (`serve_subscriber`) because they change the connection's mode.
        "publish" => publish(&args, server),
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
        // A successful `XADD` may satisfy a client parked in a blocking
        // `XREAD ... BLOCK`; wake them all so each re-checks its streams.
        if name == "xadd" {
            server.stream_notify.notify_waiters();
        }
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
            | "xadd"
            | "xdel"
            | "xtrim"
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

/// The error Redis returns when a command meets a key holding a different kind
/// of value than it operates on — e.g. `LPUSH` on a string, or `GET` on a
/// list. The reply is a plain error frame beginning with the word `WRONGTYPE`.
fn wrong_type() -> Value {
    Value::Error("WRONGTYPE Operation against a key holding the wrong kind of value".to_string())
}

/// `XADD key [MAXLEN [=|~] threshold] <id> field value [field value ...]` —
/// append an entry to a stream, creating the stream if the key is absent.
/// `<id>` is `*` (fully auto), `<ms>-*` / `<ms>` (auto sequence), or an
/// explicit `<ms>-<seq>`; the resolved ID must be strictly greater than the
/// stream's current top ID. If `MAXLEN` is given, the stream is trimmed down
/// to `threshold` entries (via the same [`stream::Stream::trim`] primitive
/// `XTRIM` uses) *after* the new entry is appended, so `MAXLEN 0` still keeps
/// the entry just added out of the deletion — trim runs after, not before.
/// Replies with the ID actually stored (a bulk string), or an error on a
/// wrong-typed key, a bad ID, an ID that isn't large enough, or a malformed
/// `MAXLEN` clause.
fn xadd(args: &[Value], storage: &Store) -> Value {
    // key + id + at least one field/value pair.
    if args.len() < 4 {
        return wrong_args("xadd");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let (maxlen, consumed) = match parse_maxlen_clause(&args[1..]) {
        Ok(Some((threshold, consumed))) => (Some(threshold), consumed),
        Ok(None) => (None, 0),
        Err(e) => return Value::Error(e),
    };
    let rest = &args[1 + consumed..];
    if rest.len() < 3 {
        return wrong_args("xadd");
    }
    let field_value_args = &rest[1..];
    if !field_value_args.len().is_multiple_of(2) {
        return wrong_args("xadd");
    }
    let id_arg = match unpack_bulk_str(&rest[0]) {
        Ok(s) => s,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let request = match stream::parse_xadd_id(&id_arg) {
        Ok(req) => req,
        Err(e) => return Value::Error(e),
    };
    let mut fields = Vec::with_capacity(field_value_args.len() / 2);
    for pair in field_value_args.chunks(2) {
        let field = match unpack_bulk_str(&pair[0]) {
            Ok(f) => f,
            Err(e) => return Value::Error(format!("ERR {}", e)),
        };
        let value = match unpack_bulk_str(&pair[1]) {
            Ok(v) => v,
            Err(e) => return Value::Error(format!("ERR {}", e)),
        };
        fields.push((field, value));
    }

    let now = Instant::now();
    let now_ms = now_unix_ms();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);

    // Look before inserting: resolving an auto ID needs the stream's current
    // `last_id`, and validation may reject the request — in which case we must
    // not have created an empty stream. So we borrow any existing entry first,
    // compute and check the ID, and only insert a fresh stream once the ID is
    // known good.
    let last_id = match store.get(&key) {
        None => stream::StreamId::MIN,
        Some(e) => match &e.value {
            StoredValue::Stream(s) => s.last_id,
            _ => return wrong_type(),
        },
    };
    let id = match stream::resolve_id(request, last_id, now_ms) {
        Ok(id) => id,
        Err(e) => return Value::Error(e),
    };

    let entry = store.entry(key).or_insert_with(|| Entry {
        value: StoredValue::Stream(Stream::default()),
        expires_at: None,
    });
    match &mut entry.value {
        StoredValue::Stream(s) => {
            s.append(id, fields);
            if let Some(threshold) = maxlen {
                s.trim(threshold);
            }
            Value::BulkString(id.to_string())
        }
        // The pre-check above already returned WRONGTYPE for a non-stream key,
        // so this arm is only reachable for a key we just created as a stream.
        _ => wrong_type(),
    }
}

/// `XLEN key` — the number of entries in the stream at `key`, `0` if the key is
/// missing, or `WRONGTYPE` if it holds another type.
fn xlen(args: &[Value], storage: &Store) -> Value {
    if args.len() != 1 {
        return wrong_args("xlen");
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
            StoredValue::Stream(s) => Value::Integer(s.entries.len() as i64),
            _ => wrong_type(),
        },
    }
}

/// Render one stream entry as the RESP shape Redis uses: a two-element array of
/// `[id, [field, value, field, value, ...]]`. Shared by `XRANGE` and `XREAD`.
fn entry_to_value(entry: &stream::StreamEntry) -> Value {
    let mut flat = Vec::with_capacity(entry.fields.len() * 2);
    for (field, value) in &entry.fields {
        flat.push(Value::BulkString(field.clone()));
        flat.push(Value::BulkString(value.clone()));
    }
    Value::Array(vec![
        Value::BulkString(entry.id.to_string()),
        Value::Array(flat),
    ])
}

/// `XRANGE key start end [COUNT n]` — the entries whose IDs fall in the
/// inclusive range `[start, end]`. `start`/`end` accept `-`/`+` (the extremes),
/// a bare `<ms>` (whole millisecond), or a full `<ms>-<seq>`. Replies with an
/// array of `[id, [field, value, ...]]` entries (empty array for a missing
/// key), or an error on a bad bound, bad `COUNT`, or a wrong-typed key.
fn xrange(args: &[Value], storage: &Store) -> Value {
    if args.len() != 3 && args.len() != 5 {
        return wrong_args("xrange");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let start = match unpack_bulk_str(&args[1]).map_err(|e| format!("ERR {}", e)) {
        Ok(s) => match stream::parse_range_start(&s) {
            Ok(id) => id,
            Err(e) => return Value::Error(e),
        },
        Err(e) => return Value::Error(e),
    };
    let end = match unpack_bulk_str(&args[2]).map_err(|e| format!("ERR {}", e)) {
        Ok(s) => match stream::parse_range_end(&s) {
            Ok(id) => id,
            Err(e) => return Value::Error(e),
        },
        Err(e) => return Value::Error(e),
    };
    let count = match parse_optional_count(&args[3..]) {
        Ok(c) => c,
        Err(e) => return Value::Error(e),
    };

    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get(&key) {
        None => Value::Array(Vec::new()),
        Some(e) => match &e.value {
            StoredValue::Stream(s) => {
                let entries = s.range(start, end, count);
                Value::Array(entries.into_iter().map(entry_to_value).collect())
            }
            _ => wrong_type(),
        },
    }
}

/// `XREVRANGE key end start [COUNT n]` — the same entries as `XRANGE` but in
/// reverse (highest ID first). Note the bound order is flipped: `end` comes
/// before `start`, matching Redis. Bounds accept `-`/`+`, a bare `<ms>`, a full
/// `<ms>-<seq>`, or an exclusive `(<id>`. Replies with an array of
/// `[id, [field, value, ...]]` entries newest-first, an empty array for a
/// missing key, or an error on a bad bound/`COUNT`/wrong-typed key.
fn xrevrange(args: &[Value], storage: &Store) -> Value {
    if args.len() != 3 && args.len() != 5 {
        return wrong_args("xrevrange");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    // Argument order is `end start` — the reverse of XRANGE.
    let end = match unpack_bulk_str(&args[1]).map_err(|e| format!("ERR {}", e)) {
        Ok(s) => match stream::parse_range_end(&s) {
            Ok(id) => id,
            Err(e) => return Value::Error(e),
        },
        Err(e) => return Value::Error(e),
    };
    let start = match unpack_bulk_str(&args[2]).map_err(|e| format!("ERR {}", e)) {
        Ok(s) => match stream::parse_range_start(&s) {
            Ok(id) => id,
            Err(e) => return Value::Error(e),
        },
        Err(e) => return Value::Error(e),
    };
    let count = match parse_optional_count(&args[3..]) {
        Ok(c) => c,
        Err(e) => return Value::Error(e),
    };

    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get(&key) {
        None => Value::Array(Vec::new()),
        Some(e) => match &e.value {
            StoredValue::Stream(s) => {
                let entries = s.rev_range(start, end, count);
                Value::Array(entries.into_iter().map(entry_to_value).collect())
            }
            _ => wrong_type(),
        },
    }
}

/// `XDEL key id [id ...]` — remove the named entries from the stream, replying
/// with the count actually deleted (IDs that weren't present simply don't
/// count). A missing key deletes nothing (`0`); a wrong-typed key is
/// `WRONGTYPE`. The stream's `last_id` is left untouched even if every entry
/// goes, so a future `XADD` can never reuse a deleted ID.
fn xdel(args: &[Value], storage: &Store) -> Value {
    if args.len() < 2 {
        return wrong_args("xdel");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    // Parse every ID up front so a malformed one rejects the whole command
    // before we mutate anything.
    let mut ids = Vec::with_capacity(args.len() - 1);
    for arg in &args[1..] {
        let s = match unpack_bulk_str(arg) {
            Ok(s) => s,
            Err(e) => return Value::Error(format!("ERR {}", e)),
        };
        match stream::parse_entry_id(&s) {
            Ok(id) => ids.push(id),
            Err(e) => return Value::Error(e),
        }
    }

    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get_mut(&key) {
        None => Value::Integer(0),
        Some(e) => match &mut e.value {
            StoredValue::Stream(s) => {
                let deleted = ids.iter().filter(|id| s.delete(**id)).count();
                Value::Integer(deleted as i64)
            }
            _ => wrong_type(),
        },
    }
}

/// `XTRIM key MAXLEN [=|~] threshold` — drop the oldest entries until at most
/// `threshold` remain, replying with the number actually removed. The `=`/`~`
/// exactness marker Redis accepts before the threshold is parsed but not acted
/// on differently — like the RDB writer's plain-string encoding, approximate
/// trimming is a size/performance tweak, not a correctness difference, so
/// FlashDB always trims exactly. A missing key trims nothing (`0`); a
/// wrong-typed key is `WRONGTYPE`. Like `XDEL`, trimming never lowers the
/// stream's high-water mark, so a future `XADD` still can't reuse a
/// just-trimmed ID.
fn xtrim(args: &[Value], storage: &Store) -> Value {
    if args.len() < 2 {
        return wrong_args("xtrim");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let threshold = match parse_maxlen_clause(&args[1..]) {
        Ok(Some((threshold, consumed))) if consumed == args.len() - 1 => threshold,
        Ok(_) => return Value::Error("ERR syntax error".to_string()),
        Err(e) => return Value::Error(e),
    };

    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get_mut(&key) {
        None => Value::Integer(0),
        Some(e) => match &mut e.value {
            StoredValue::Stream(s) => Value::Integer(s.trim(threshold) as i64),
            _ => wrong_type(),
        },
    }
}

/// Try to parse a `MAXLEN [=|~] threshold` clause from the front of `args`.
/// `Ok(None)` means `args` is empty or doesn't start with the `MAXLEN`
/// keyword — "no clause here", not an error, so callers like `XADD` can treat
/// it as "trim not requested" and keep parsing whatever comes next (the
/// entry ID). `Ok(Some((threshold, consumed)))` gives the parsed threshold
/// and how many argument slots the clause ate — 2 without the optional
/// `=`/`~` exactness marker, 3 with it — so a caller with more arguments
/// after the clause (like `XADD`'s id/field/value tail) knows where its own
/// parsing resumes. `Err` only fires once we're committed to `MAXLEN` having
/// started: a missing threshold or one that doesn't parse as a `usize`.
/// Shared by `XADD`'s optional trim-on-add clause and `XTRIM`'s dedicated
/// command so the exactness marker and error messages can't drift apart.
fn parse_maxlen_clause(args: &[Value]) -> Result<Option<(usize, usize)>, String> {
    let Some(first) = args.first() else {
        return Ok(None);
    };
    let keyword = unpack_bulk_str(first).map_err(|e| format!("ERR {}", e))?;
    if !keyword.eq_ignore_ascii_case("maxlen") {
        return Ok(None);
    }
    let mut idx = 1;
    let modifier = args.get(idx).and_then(|v| unpack_bulk_str(v).ok());
    if matches!(modifier.as_deref(), Some("=") | Some("~")) {
        idx += 1;
    }
    let threshold_arg = match args.get(idx) {
        Some(v) => v,
        None => return Err("ERR syntax error".to_string()),
    };
    let threshold = unpack_bulk_str(threshold_arg)
        .map_err(|e| format!("ERR {}", e))?
        .parse::<usize>()
        .map_err(|_| "ERR value is not an integer or out of range".to_string())?;
    Ok(Some((threshold, idx + 1)))
}

/// Parse an optional trailing `COUNT n` clause (the slice after the fixed
/// arguments). An empty slice means no count; `COUNT n` yields `Some(n)`;
/// anything else is a syntax error.
fn parse_optional_count(rest: &[Value]) -> Result<Option<usize>, String> {
    if rest.is_empty() {
        return Ok(None);
    }
    if rest.len() != 2 {
        return Err("ERR syntax error".to_string());
    }
    let keyword = unpack_bulk_str(&rest[0]).map_err(|e| format!("ERR {}", e))?;
    if !keyword.eq_ignore_ascii_case("count") {
        return Err("ERR syntax error".to_string());
    }
    let n = unpack_bulk_str(&rest[1]).map_err(|e| format!("ERR {}", e))?;
    let n = n
        .parse::<usize>()
        .map_err(|_| "ERR value is not an integer or out of range".to_string())?;
    Ok(Some(n))
}

/// A parsed `XREAD` request: the optional `COUNT`/`BLOCK` modifiers plus the
/// paired stream keys and their id specifications (kept as raw strings because a
/// `$` can only be resolved against live stream state, and that resolution
/// happens at a different moment for a blocking vs. a non-blocking read).
#[derive(Debug)]
struct XReadRequest {
    /// `BLOCK <ms>` if present. `Some(0)` means block forever; `None` means the
    /// classic non-blocking read.
    block: Option<u64>,
    /// `COUNT <n>` cap on entries returned per stream, if given.
    count: Option<usize>,
    /// The stream keys, in order.
    keys: Vec<String>,
    /// The id specifications paired with `keys` — a `$` or a concrete `<ms>`/
    /// `<ms>-<seq>` bound, still unparsed.
    id_specs: Vec<String>,
}

/// Parse the whole `XREAD [COUNT n] [BLOCK ms] STREAMS key... id...` argument
/// list. `COUNT` and `BLOCK` may appear in either order before the mandatory
/// `STREAMS` keyword; after it comes an equal, non-empty count of keys and ids.
/// Returns the Redis error string on any malformed input.
fn parse_xread(args: &[Value]) -> Result<XReadRequest, String> {
    let mut idx = 0;
    let mut count: Option<usize> = None;
    let mut block: Option<u64> = None;

    // Consume leading option words until we reach STREAMS. Each option takes one
    // following value argument.
    loop {
        let word = match args.get(idx).and_then(|v| unpack_bulk_str(v).ok()) {
            Some(w) => w,
            None => return Err("ERR syntax error".to_string()),
        };
        if word.eq_ignore_ascii_case("streams") {
            idx += 1;
            break;
        } else if word.eq_ignore_ascii_case("count") {
            let n = args
                .get(idx + 1)
                .ok_or_else(|| "ERR syntax error".to_string())
                .and_then(|v| unpack_bulk_str(v).map_err(|e| format!("ERR {}", e)))?
                .parse::<usize>()
                .map_err(|_| "ERR value is not an integer or out of range".to_string())?;
            count = Some(n);
            idx += 2;
        } else if word.eq_ignore_ascii_case("block") {
            let ms = args
                .get(idx + 1)
                .ok_or_else(|| "ERR syntax error".to_string())
                .and_then(|v| unpack_bulk_str(v).map_err(|e| format!("ERR {}", e)))?
                .parse::<u64>()
                .map_err(|_| "ERR timeout is not an integer or out of range".to_string())?;
            block = Some(ms);
            idx += 2;
        } else {
            return Err("ERR syntax error, expected STREAMS in XREAD".to_string());
        }
    }

    // The remainder is N keys followed by N ids — non-empty and even.
    let rest = &args[idx..];
    if rest.is_empty() || !rest.len().is_multiple_of(2) {
        return Err(
            "ERR Unbalanced XREAD list of streams: for each stream key an ID or '$' must be specified."
                .to_string(),
        );
    }
    let n = rest.len() / 2;
    let (key_vals, id_vals) = rest.split_at(n);
    let mut keys = Vec::with_capacity(n);
    let mut id_specs = Vec::with_capacity(n);
    for (k, i) in key_vals.iter().zip(id_vals) {
        keys.push(unpack_bulk_str(k).map_err(|e| format!("ERR {}", e))?);
        id_specs.push(unpack_bulk_str(i).map_err(|e| format!("ERR {}", e))?);
    }
    Ok(XReadRequest {
        block,
        count,
        keys,
        id_specs,
    })
}

/// Turn each `(key, id_spec)` into the concrete exclusive lower bound `XREAD`
/// reads past. A `$` resolves to the stream's current `last_id` (the empty
/// stream's `0-0` if it doesn't exist yet), which is what makes `$` mean "only
/// what arrives after this moment". Any other spec is a range-start bound. A
/// wrong-typed key aborts the whole read with `WRONGTYPE`. Needs `&mut` only to
/// run passive expiry before reading.
fn resolve_after_ids(
    store: &mut HashMap<String, Entry>,
    req: &XReadRequest,
) -> Result<Vec<stream::StreamId>, Value> {
    let now = Instant::now();
    let mut afters = Vec::with_capacity(req.keys.len());
    for (key, spec) in req.keys.iter().zip(&req.id_specs) {
        expire_if_due(store, key, now);
        let last_id = match store.get(key) {
            None => stream::StreamId::MIN,
            Some(e) => match &e.value {
                StoredValue::Stream(s) => s.last_id,
                _ => return Err(wrong_type()),
            },
        };
        let after = if spec == "$" {
            last_id
        } else {
            stream::parse_range_start(spec).map_err(Value::Error)?
        };
        afters.push(after);
    }
    Ok(afters)
}

/// Gather the reply for a set of already-resolved `after` bounds: for each
/// stream that has entries strictly greater than its bound, a `[key, [entries]]`
/// pair. Returns the nil array (`Value::NullArray`) when nothing is new, exactly
/// as Redis does. Read-only — the caller holds the lock.
fn xread_collect(
    store: &HashMap<String, Entry>,
    req: &XReadRequest,
    afters: &[stream::StreamId],
) -> Value {
    let mut results = Vec::new();
    for (key, after) in req.keys.iter().zip(afters) {
        let stream_ref = match store.get(key) {
            None => continue,
            Some(e) => match &e.value {
                StoredValue::Stream(s) => s,
                // A key that was a stream at resolution time but isn't now would
                // be unusual, but treat any non-stream as no contribution here;
                // the WRONGTYPE was already reported during resolution.
                _ => continue,
            },
        };
        let entries = stream_ref.entries_after(*after, req.count);
        if entries.is_empty() {
            continue;
        }
        let entry_values: Vec<Value> = entries.into_iter().map(entry_to_value).collect();
        results.push(Value::Array(vec![
            Value::BulkString(key.clone()),
            Value::Array(entry_values),
        ]));
    }
    if results.is_empty() {
        Value::NullArray
    } else {
        Value::Array(results)
    }
}

/// `XREAD [COUNT n] [BLOCK ms] STREAMS key [key ...] id [id ...]` — for each
/// named stream, the entries with an ID strictly greater than the paired `id`.
/// A `$` id means "only entries added after now". This is the synchronous entry
/// point used for a non-blocking read (and for a `BLOCK` read replayed inside
/// `EXEC`, where blocking is not allowed): it does a single pass and returns
/// immediately, ignoring any `BLOCK`. The actual waiting lives in
/// [`xread_blocking`], reached from the async connection loop.
fn xread(args: &[Value], storage: &Store) -> Value {
    let req = match parse_xread(args) {
        Ok(r) => r,
        Err(e) => return Value::Error(e),
    };
    let mut store = storage.lock().unwrap();
    let afters = match resolve_after_ids(&mut store, &req) {
        Ok(a) => a,
        Err(v) => return v,
    };
    xread_collect(&store, &req, &afters)
}

/// The blocking variant of `XREAD`, run from the async connection loop so it can
/// actually `.await`. The `$` bounds are resolved once, up front, against the
/// stream state at the moment blocking begins — so we only see entries that
/// arrive *after* that. Then we park until a write wakes us or the deadline
/// passes, re-checking on each wake.
async fn xread_blocking(req: XReadRequest, server: &Server) -> Value {
    // Snapshot the bounds while holding the lock exactly once.
    let afters = {
        let mut store = server.store.lock().unwrap();
        match resolve_after_ids(&mut store, &req) {
            Ok(a) => a,
            Err(v) => return v,
        }
    };

    // `BLOCK 0` waits forever; otherwise compute an absolute deadline.
    let deadline = match req.block {
        Some(0) | None => None,
        Some(ms) => Some(Instant::now() + Duration::from_millis(ms)),
    };

    loop {
        // Register interest *before* checking the store. `notify_waiters()` does
        // not store a permit, so a wake that lands between our check and our
        // await would be lost — enabling the future now closes that race.
        let notified = server.stream_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        {
            let store = server.store.lock().unwrap();
            let result = xread_collect(&store, &req, &afters);
            if !matches!(result, Value::NullArray) {
                return result;
            }
        }

        match deadline {
            None => notified.await,
            Some(dl) => {
                let now = Instant::now();
                if now >= dl {
                    return Value::NullArray;
                }
                // Woken (Ok) → loop and re-check; elapsed (Err) → timed out.
                if timeout(dl - now, notified).await.is_err() {
                    return Value::NullArray;
                }
            }
        }
    }
}

/// If `value` is an `XREAD` carrying a `BLOCK` clause, parse it into a request to
/// be handled by the async blocking path. Returns `None` when it isn't an
/// `XREAD`, when it has no `BLOCK`, or when it fails to parse — in those cases
/// the caller lets the ordinary synchronous dispatch handle it (producing a
/// non-blocking reply or the proper error).
fn as_blocking_xread(value: &Value) -> Option<XReadRequest> {
    if command_name(value).as_deref() != Some("xread") {
        return None;
    }
    let req = parse_xread(command_args(value)).ok()?;
    req.block.map(|_| req)
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

    // WAIT and SAVE/BGSAVE tests moved to `commands::persistence` alongside
    // the code they exercise.

    #[test]
    fn write_commands_are_classified_for_replication() {
        for w in [
            "set", "del", "expire", "persist", "rpush", "lpush", "rpop", "lpop", "hset", "hdel",
            "xadd", "xdel", "xtrim",
        ] {
            assert!(is_write_command(w), "{w} should replicate");
        }
        for r in [
            "get",
            "ttl",
            "type",
            "llen",
            "lrange",
            "hget",
            "hgetall",
            "ping",
            "echo",
            "save",
            "bgsave",
            "wait",
            "replconf",
            "psync",
            "xlen",
            "xrange",
            "xrevrange",
            "xread",
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
    fn xadd_with_explicit_ids_stores_and_returns_them() {
        let st = store();
        assert_eq!(
            xadd(&[bulk("s"), bulk("1-1"), bulk("f"), bulk("v")], &st),
            Value::BulkString("1-1".to_string())
        );
        assert_eq!(
            xadd(&[bulk("s"), bulk("1-2"), bulk("f"), bulk("v")], &st),
            Value::BulkString("1-2".to_string())
        );
        assert_eq!(xlen(&[bulk("s")], &st), Value::Integer(2));
        assert_eq!(
            type_cmd(&[bulk("s")], &st),
            Value::SimpleString("stream".to_string())
        );
    }

    #[test]
    fn xadd_rejects_a_non_increasing_id() {
        let st = store();
        xadd(&[bulk("s"), bulk("5-5"), bulk("f"), bulk("v")], &st);
        // Equal or smaller than the top item is refused...
        assert!(matches!(
            xadd(&[bulk("s"), bulk("5-5"), bulk("f"), bulk("v")], &st),
            Value::Error(_)
        ));
        assert!(matches!(
            xadd(&[bulk("s"), bulk("5-4"), bulk("f"), bulk("v")], &st),
            Value::Error(_)
        ));
        // ...and the failed adds didn't change the stream.
        assert_eq!(xlen(&[bulk("s")], &st), Value::Integer(1));
    }

    #[test]
    fn xadd_auto_sequence_increments_within_a_millisecond() {
        let st = store();
        assert_eq!(
            xadd(&[bulk("s"), bulk("5-*"), bulk("f"), bulk("v")], &st),
            Value::BulkString("5-0".to_string())
        );
        assert_eq!(
            xadd(&[bulk("s"), bulk("5-*"), bulk("f"), bulk("v")], &st),
            Value::BulkString("5-1".to_string())
        );
    }

    #[test]
    fn xadd_rejects_zero_id_and_wrong_type() {
        let st = store();
        assert!(matches!(
            xadd(&[bulk("s"), bulk("0-0"), bulk("f"), bulk("v")], &st),
            Value::Error(_)
        ));
        // A string key can't take XADD, and the string is left intact.
        set(&[bulk("str"), bulk("hi")], &st);
        assert_eq!(
            xadd(&[bulk("str"), bulk("1-1"), bulk("f"), bulk("v")], &st),
            wrong_type()
        );
        assert_eq!(
            get(&[bulk("str")], &st),
            Value::BulkString("hi".to_string())
        );
    }

    #[test]
    fn xadd_with_maxlen_trims_after_appending() {
        let st = store();
        for id in ["1-0", "2-0", "3-0"] {
            xadd(
                &[
                    bulk("s"),
                    bulk("MAXLEN"),
                    bulk("2"),
                    bulk(id),
                    bulk("k"),
                    bulk(id),
                ],
                &st,
            );
        }
        // Each add is followed by a trim to 2, so only the newest two survive
        // — including the entry that was *just* added, proving trim runs
        // after the append rather than before.
        assert_eq!(xlen(&[bulk("s")], &st), Value::Integer(2));
        let reply = xrange(&[bulk("s"), bulk("-"), bulk("+")], &st);
        assert_eq!(
            reply,
            Value::Array(vec![
                Value::Array(vec![
                    Value::BulkString("2-0".into()),
                    Value::Array(vec![
                        Value::BulkString("k".into()),
                        Value::BulkString("2-0".into())
                    ]),
                ]),
                Value::Array(vec![
                    Value::BulkString("3-0".into()),
                    Value::Array(vec![
                        Value::BulkString("k".into()),
                        Value::BulkString("3-0".into())
                    ]),
                ]),
            ])
        );

        // The `=` exactness marker is accepted, same as XTRIM.
        assert_eq!(
            xadd(
                &[
                    bulk("s"),
                    bulk("MAXLEN"),
                    bulk("="),
                    bulk("1"),
                    bulk("4-0"),
                    bulk("k"),
                    bulk("4-0")
                ],
                &st
            ),
            Value::BulkString("4-0".to_string())
        );
        assert_eq!(xlen(&[bulk("s")], &st), Value::Integer(1));

        // Without MAXLEN, XADD behaves exactly as before — no trimming.
        for id in ["5-0", "6-0"] {
            xadd(&[bulk("s"), bulk(id), bulk("k"), bulk(id)], &st);
        }
        assert_eq!(xlen(&[bulk("s")], &st), Value::Integer(3));

        // A malformed MAXLEN clause is a syntax/parse error and never
        // creates the key.
        assert_eq!(
            xadd(
                &[
                    bulk("nope"),
                    bulk("MAXLEN"),
                    bulk("*"),
                    bulk("f"),
                    bulk("v")
                ],
                &st
            ),
            Value::Error("ERR value is not an integer or out of range".to_string())
        );
        assert_eq!(xlen(&[bulk("nope")], &st), Value::Integer(0));
    }

    #[test]
    fn xrange_returns_entries_in_the_inclusive_range() {
        let st = store();
        for id in ["1-0", "2-0", "3-0"] {
            xadd(&[bulk("s"), bulk(id), bulk("k"), bulk(id)], &st);
        }
        // A bounded range picks up 2-0 and 3-0.
        let reply = xrange(&[bulk("s"), bulk("2"), bulk("+")], &st);
        let Value::Array(items) = reply else {
            panic!("expected array");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            Value::Array(vec![
                Value::BulkString("2-0".to_string()),
                Value::Array(vec![bulk("k"), bulk("2-0")]),
            ])
        );

        // COUNT caps the full range.
        let reply = xrange(
            &[bulk("s"), bulk("-"), bulk("+"), bulk("COUNT"), bulk("1")],
            &st,
        );
        assert!(matches!(reply, Value::Array(ref v) if v.len() == 1));

        // A missing key is an empty array, not an error.
        assert_eq!(
            xrange(&[bulk("nope"), bulk("-"), bulk("+")], &st),
            Value::Array(vec![])
        );
    }

    #[test]
    fn xrange_exclusive_bounds_drop_the_endpoints() {
        let st = store();
        for id in ["1-0", "2-0", "3-0"] {
            xadd(&[bulk("s"), bulk(id), bulk("k"), bulk(id)], &st);
        }
        // `(1-0` .. `(3-0` excludes both ends, leaving only 2-0.
        let reply = xrange(&[bulk("s"), bulk("(1-0"), bulk("(3-0")], &st);
        let Value::Array(items) = reply else {
            panic!("expected array");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0],
            Value::Array(vec![
                Value::BulkString("2-0".to_string()),
                Value::Array(vec![bulk("k"), bulk("2-0")]),
            ])
        );
    }

    #[test]
    fn xrevrange_returns_entries_newest_first() {
        let st = store();
        for id in ["1-0", "2-0", "3-0"] {
            xadd(&[bulk("s"), bulk(id), bulk("k"), bulk(id)], &st);
        }
        // Bounds are `end start` here: `+` down to `-`.
        let reply = xrevrange(&[bulk("s"), bulk("+"), bulk("-")], &st);
        let Value::Array(items) = reply else {
            panic!("expected array");
        };
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], entry_to_value_id("3-0"));
        assert_eq!(items[2], entry_to_value_id("1-0"));

        // COUNT keeps the two highest IDs.
        let reply = xrevrange(
            &[bulk("s"), bulk("+"), bulk("-"), bulk("COUNT"), bulk("2")],
            &st,
        );
        let Value::Array(items) = reply else {
            panic!("expected array");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], entry_to_value_id("3-0"));
        assert_eq!(items[1], entry_to_value_id("2-0"));
    }

    // Build the RESP shape `xadd(&["s", id, "k", id])` above stores for `id`.
    fn entry_to_value_id(id: &str) -> Value {
        Value::Array(vec![
            Value::BulkString(id.to_string()),
            Value::Array(vec![bulk("k"), bulk(id)]),
        ])
    }

    #[test]
    fn xdel_removes_named_entries_and_counts_them() {
        let st = store();
        for id in ["1-0", "2-0", "3-0"] {
            xadd(&[bulk("s"), bulk(id), bulk("k"), bulk(id)], &st);
        }
        // Delete two present IDs plus one that isn't there: count is 2.
        assert_eq!(
            xdel(&[bulk("s"), bulk("1-0"), bulk("3-0"), bulk("9-9")], &st),
            Value::Integer(2)
        );
        assert_eq!(xlen(&[bulk("s")], &st), Value::Integer(1));
        // The high-water mark is preserved: re-adding 3-0 is still refused.
        assert!(matches!(
            xadd(&[bulk("s"), bulk("3-0"), bulk("f"), bulk("v")], &st),
            Value::Error(_)
        ));
        // A missing key deletes nothing; a wrong-typed key is WRONGTYPE.
        assert_eq!(xdel(&[bulk("nope"), bulk("1-0")], &st), Value::Integer(0));
        set(&[bulk("str"), bulk("hi")], &st);
        assert_eq!(xdel(&[bulk("str"), bulk("1-0")], &st), wrong_type());
    }

    #[test]
    fn xtrim_drops_the_oldest_entries_and_reports_the_count() {
        let st = store();
        for id in ["1-0", "2-0", "3-0", "4-0"] {
            xadd(&[bulk("s"), bulk(id), bulk("k"), bulk(id)], &st);
        }
        // MAXLEN 2 drops the two oldest entries.
        assert_eq!(
            xtrim(&[bulk("s"), bulk("MAXLEN"), bulk("2")], &st),
            Value::Integer(2)
        );
        assert_eq!(xlen(&[bulk("s")], &st), Value::Integer(2));
        let reply = xrange(&[bulk("s"), bulk("-"), bulk("+")], &st);
        assert_eq!(
            reply,
            Value::Array(vec![
                Value::Array(vec![
                    Value::BulkString("3-0".into()),
                    Value::Array(vec![
                        Value::BulkString("k".into()),
                        Value::BulkString("3-0".into())
                    ]),
                ]),
                Value::Array(vec![
                    Value::BulkString("4-0".into()),
                    Value::Array(vec![
                        Value::BulkString("k".into()),
                        Value::BulkString("4-0".into())
                    ]),
                ]),
            ])
        );

        // The optional `=` exactness marker is accepted; a threshold at or above
        // the current length trims nothing.
        assert_eq!(
            xtrim(&[bulk("s"), bulk("MAXLEN"), bulk("="), bulk("10")], &st),
            Value::Integer(0)
        );

        // The high-water mark stayed at 4-0: re-adding it is refused even though
        // the entry itself is long gone.
        assert!(matches!(
            xadd(&[bulk("s"), bulk("4-0"), bulk("f"), bulk("v")], &st),
            Value::Error(_)
        ));

        // An unknown strategy is a syntax error. A bad modifier isn't treated
        // as `=`/`~` at all — it's read as the threshold itself, so it fails
        // to parse as a number instead.
        assert_eq!(
            xtrim(&[bulk("s"), bulk("MINLEN"), bulk("2")], &st),
            Value::Error("ERR syntax error".to_string())
        );
        assert_eq!(
            xtrim(&[bulk("s"), bulk("MAXLEN"), bulk("?"), bulk("2")], &st),
            Value::Error("ERR value is not an integer or out of range".to_string())
        );
        // Trailing garbage after a valid clause is still a syntax error.
        assert_eq!(
            xtrim(&[bulk("s"), bulk("MAXLEN"), bulk("2"), bulk("extra")], &st),
            Value::Error("ERR syntax error".to_string())
        );

        // A missing key trims nothing; a wrong-typed key is WRONGTYPE.
        assert_eq!(
            xtrim(&[bulk("nope"), bulk("MAXLEN"), bulk("5")], &st),
            Value::Integer(0)
        );
        set(&[bulk("str"), bulk("hi")], &st);
        assert_eq!(
            xtrim(&[bulk("str"), bulk("MAXLEN"), bulk("5")], &st),
            wrong_type()
        );
    }

    #[test]
    fn xread_returns_only_entries_after_the_given_id() {
        let st = store();
        for id in ["1-0", "2-0", "3-0"] {
            xadd(&[bulk("s"), bulk(id), bulk("k"), bulk(id)], &st);
        }
        // Everything strictly after 1-0.
        let reply = xread(&[bulk("STREAMS"), bulk("s"), bulk("1-0")], &st);
        let Value::Array(streams) = reply else {
            panic!("expected array");
        };
        assert_eq!(streams.len(), 1);
        let Value::Array(ref pair) = streams[0] else {
            panic!("expected [key, entries]");
        };
        assert_eq!(pair[0], Value::BulkString("s".to_string()));
        let Value::Array(ref entries) = pair[1] else {
            panic!("expected entries array");
        };
        assert_eq!(entries.len(), 2); // 2-0 and 3-0

        // `$` means "only newer than now" — nothing, so a nil array.
        assert_eq!(
            xread(&[bulk("STREAMS"), bulk("s"), bulk("$")], &st),
            Value::NullArray
        );
    }

    #[test]
    fn parse_xread_reads_block_and_count_in_any_order() {
        // BLOCK before COUNT.
        let req = parse_xread(&[
            bulk("BLOCK"),
            bulk("100"),
            bulk("COUNT"),
            bulk("2"),
            bulk("STREAMS"),
            bulk("s"),
            bulk("$"),
        ])
        .unwrap();
        assert_eq!(req.block, Some(100));
        assert_eq!(req.count, Some(2));
        assert_eq!(req.keys, vec!["s".to_string()]);
        assert_eq!(req.id_specs, vec!["$".to_string()]);

        // COUNT before BLOCK, and BLOCK 0 (wait forever) parses to Some(0).
        let req = parse_xread(&[
            bulk("COUNT"),
            bulk("5"),
            bulk("BLOCK"),
            bulk("0"),
            bulk("STREAMS"),
            bulk("a"),
            bulk("b"),
            bulk("0"),
            bulk("0"),
        ])
        .unwrap();
        assert_eq!(req.block, Some(0));
        assert_eq!(req.count, Some(5));
        assert_eq!(req.keys, vec!["a".to_string(), "b".to_string()]);

        // A non-numeric BLOCK timeout is rejected with Redis's message.
        let err = parse_xread(&[
            bulk("BLOCK"),
            bulk("soon"),
            bulk("STREAMS"),
            bulk("s"),
            bulk("$"),
        ])
        .unwrap_err();
        assert!(err.contains("timeout is not an integer"));
    }

    #[test]
    fn as_blocking_xread_only_fires_for_a_block_clause() {
        // A plain XREAD is not routed to the blocking path.
        assert!(as_blocking_xread(&cmd(&["XREAD", "STREAMS", "s", "$"])).is_none());
        // One with BLOCK is.
        let req = as_blocking_xread(&cmd(&["XREAD", "BLOCK", "50", "STREAMS", "s", "$"]))
            .expect("should be a blocking request");
        assert_eq!(req.block, Some(50));
        // A different command is ignored entirely.
        assert!(as_blocking_xread(&cmd(&["GET", "k"])).is_none());
    }

    #[tokio::test]
    async fn xread_block_returns_at_once_when_data_is_already_present() {
        let srv = server();
        process_command(cmd(&["XADD", "s", "1-0", "f", "v"]), &srv);
        // The entry predates the read and the id bound is 0-0, so the blocking
        // read should find it immediately without waiting out the timeout.
        let req = parse_xread(&[
            bulk("BLOCK"),
            bulk("5000"),
            bulk("STREAMS"),
            bulk("s"),
            bulk("0"),
        ])
        .unwrap();
        let reply = xread_blocking(req, &srv).await;
        let streams = match reply {
            Value::Array(s) => s,
            other => panic!("expected a stream array, got {:?}", other),
        };
        assert_eq!(streams.len(), 1);
    }

    #[tokio::test]
    async fn xread_block_times_out_to_nil_when_nothing_arrives() {
        let srv = server();
        // No writer, short timeout: the read parks and then gives up with nil.
        let req = parse_xread(&[
            bulk("BLOCK"),
            bulk("20"),
            bulk("STREAMS"),
            bulk("s"),
            bulk("$"),
        ])
        .unwrap();
        assert_eq!(xread_blocking(req, &srv).await, Value::NullArray);
    }

    #[tokio::test]
    async fn xread_block_wakes_when_an_entry_arrives() {
        let srv = server();
        // A second task appends after a short delay; the blocked reader should
        // be woken by the XADD notification and return the new entry, well
        // before its generous timeout.
        let writer = srv.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            process_command(cmd(&["XADD", "s", "1-1", "f", "v"]), &writer);
        });
        let req = parse_xread(&[
            bulk("BLOCK"),
            bulk("5000"),
            bulk("STREAMS"),
            bulk("s"),
            bulk("$"),
        ])
        .unwrap();
        let reply = xread_blocking(req, &srv).await;
        let streams = match reply {
            Value::Array(s) => s,
            other => panic!("expected a stream array, got {:?}", other),
        };
        assert_eq!(streams.len(), 1);
        let pair = match &streams[0] {
            Value::Array(p) => p,
            other => panic!("expected [key, entries], got {:?}", other),
        };
        assert_eq!(pair[0], Value::BulkString("s".to_string()));
        let entries = match &pair[1] {
            Value::Array(e) => e,
            other => panic!("expected entries array, got {:?}", other),
        };
        assert_eq!(entries.len(), 1);
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

    // `map_to_rdb_entries`/`save`/`bgsave` tests moved to
    // `commands::persistence` alongside the code they exercise.

    // ----- Pub/Sub -----------------------------------------------------------

    #[test]
    fn message_and_confirmation_frames_serialize_as_redis_expects() {
        // The push frame a subscriber receives.
        assert_eq!(
            message_frame("news", "hi").serialize(),
            "*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$2\r\nhi\r\n"
        );
        // A subscribe confirmation: [kind, channel, running-count].
        assert_eq!(
            subscription_reply("subscribe", Some("news"), 1).serialize(),
            "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n"
        );
        // An "unsubscribed from everything" reply carries a nil channel.
        assert_eq!(
            subscription_reply("unsubscribe", None, 0).serialize(),
            "*3\r\n$11\r\nunsubscribe\r\n$-1\r\n:0\r\n"
        );
    }

    #[test]
    fn publish_with_no_subscribers_delivers_to_zero() {
        let srv = server();
        assert_eq!(
            publish(&[bulk("news"), bulk("hello")], &srv),
            Value::Integer(0)
        );
    }

    #[test]
    fn publish_delivers_the_message_frame_to_every_subscriber() {
        let srv = server();
        let mut subscribed = HashSet::new();
        // Two subscribers on "news", registered through the real subscribe path.
        let (tx1, mut rx1) = mpsc::unbounded_channel::<Bytes>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<Bytes>();
        do_subscribe(&[bulk("news")], &srv, 1, &tx1, &mut subscribed.clone());
        do_subscribe(&[bulk("news")], &srv, 2, &tx2, &mut subscribed);

        assert_eq!(
            publish(&[bulk("news"), bulk("hello")], &srv),
            Value::Integer(2)
        );

        let want = Bytes::from(message_frame("news", "hello").serialize().into_bytes());
        assert_eq!(rx1.try_recv().unwrap(), want);
        assert_eq!(rx2.try_recv().unwrap(), want);
    }

    #[test]
    fn publish_only_reaches_the_named_channel() {
        let srv = server();
        let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
        let mut subscribed = HashSet::new();
        do_subscribe(&[bulk("sports")], &srv, 1, &tx, &mut subscribed);

        // A publish to a different channel reaches nobody.
        assert_eq!(
            publish(&[bulk("news"), bulk("hi")], &srv),
            Value::Integer(0)
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn publish_prunes_a_subscriber_whose_receiver_is_gone() {
        let srv = server();
        let (tx_live, _rx_live) = mpsc::unbounded_channel::<Bytes>();
        let (tx_dead, rx_dead) = mpsc::unbounded_channel::<Bytes>();
        drop(rx_dead); // its sender is now closed
        srv.subscribers
            .lock()
            .unwrap()
            .entry("news".to_string())
            .or_default()
            .extend([(1, tx_live), (2, tx_dead)]);

        // Only the live subscriber counts, and the dead one is pruned.
        assert_eq!(
            publish(&[bulk("news"), bulk("hi")], &srv),
            Value::Integer(1)
        );
        assert_eq!(srv.subscribers.lock().unwrap()["news"].len(), 1);
    }

    #[test]
    fn publish_validates_its_arity() {
        let srv = server();
        assert_eq!(publish(&[bulk("news")], &srv), wrong_args("publish"));
        assert_eq!(
            publish(&[bulk("a"), bulk("b"), bulk("c")], &srv),
            wrong_args("publish")
        );
    }

    #[test]
    fn subscribe_registers_the_client_and_confirms_each_channel() {
        let srv = server();
        let (tx, _rx) = mpsc::unbounded_channel::<Bytes>();
        let mut subscribed = HashSet::new();

        let reply = do_subscribe(&[bulk("a"), bulk("b")], &srv, 7, &tx, &mut subscribed);
        // Two independent confirmation frames, counts growing 1 then 2.
        assert_eq!(
            reply,
            "*3\r\n$9\r\nsubscribe\r\n$1\r\na\r\n:1\r\n\
             *3\r\n$9\r\nsubscribe\r\n$1\r\nb\r\n:2\r\n"
        );
        assert_eq!(
            subscribed,
            HashSet::from(["a".to_string(), "b".to_string()])
        );
        let table = srv.subscribers.lock().unwrap();
        assert!(table["a"].contains_key(&7));
        assert!(table["b"].contains_key(&7));
    }

    #[test]
    fn unsubscribe_by_name_removes_only_that_channel() {
        let srv = server();
        let (tx, _rx) = mpsc::unbounded_channel::<Bytes>();
        let mut subscribed = HashSet::new();
        do_subscribe(&[bulk("a"), bulk("b")], &srv, 7, &tx, &mut subscribed);

        let reply = do_unsubscribe(&[bulk("a")], &srv, 7, &mut subscribed);
        assert_eq!(reply, "*3\r\n$11\r\nunsubscribe\r\n$1\r\na\r\n:1\r\n");
        assert_eq!(subscribed, HashSet::from(["b".to_string()]));
        // "a" had its only subscriber removed, so the channel is gone entirely.
        let table = srv.subscribers.lock().unwrap();
        assert!(!table.contains_key("a"));
        assert!(table["b"].contains_key(&7));
    }

    #[test]
    fn bare_unsubscribe_drops_every_channel() {
        let srv = server();
        let (tx, _rx) = mpsc::unbounded_channel::<Bytes>();
        let mut subscribed = HashSet::new();
        do_subscribe(&[bulk("a"), bulk("b")], &srv, 7, &tx, &mut subscribed);

        // No arguments = leave all channels; ends with count 0.
        let reply = do_unsubscribe(&[], &srv, 7, &mut subscribed);
        assert!(reply.contains(":0\r\n"));
        assert!(subscribed.is_empty());
        assert!(srv.subscribers.lock().unwrap().is_empty());
    }

    #[test]
    fn unsubscribe_with_nothing_subscribed_replies_with_nil_channel() {
        let srv = server();
        let mut subscribed = HashSet::new();
        let reply = do_unsubscribe(&[], &srv, 7, &mut subscribed);
        assert_eq!(reply, "*3\r\n$11\r\nunsubscribe\r\n$-1\r\n:0\r\n");
    }

    #[test]
    fn a_subscribed_connection_rejects_non_pubsub_commands() {
        let srv = server();
        let (tx, _rx) = mpsc::unbounded_channel::<Bytes>();
        let mut subscribed = HashSet::from(["news".to_string()]);
        let get = Value::Array(vec![bulk("GET"), bulk("k")]);
        let reply = handle_sub_command(get, &srv, 7, &tx, &mut subscribed);
        assert!(reply.starts_with("-ERR Can't execute 'get'"));
        // PING is still allowed while subscribed.
        let ping = Value::Array(vec![bulk("PING")]);
        assert_eq!(
            handle_sub_command(ping, &srv, 7, &tx, &mut subscribed),
            "+PONG\r\n"
        );
    }

    #[test]
    fn publish_is_a_known_but_non_write_command() {
        // Queueable inside MULTI, but never streamed to replicas.
        assert!(is_known_command("publish"));
        assert!(!is_write_command("publish"));
    }
}
