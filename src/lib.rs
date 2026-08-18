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
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};

mod commands;
pub mod rdb;
pub mod replication;
pub mod resp;
pub mod stream;

use commands::lists::Side;
use commands::pubsub::Subscribers;
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
    let client_id = commands::pubsub::NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
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
            commands::pubsub::serve_subscriber(&mut stream, &mut buffer, &server, client_id, value)
                .await?;
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
            if let Some(req) = commands::streams::as_blocking_xread(&value) {
                let response = commands::streams::xread_blocking(req, &server).await;
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
        "xadd" => commands::streams::xadd(&args, storage),
        "xlen" => commands::streams::xlen(&args, storage),
        "xrange" => commands::streams::xrange(&args, storage),
        "xrevrange" => commands::streams::xrevrange(&args, storage),
        "xdel" => commands::streams::xdel(&args, storage),
        "xtrim" => commands::streams::xtrim(&args, storage),
        "xread" => commands::streams::xread(&args, storage),
        "save" => commands::persistence::save(&args, server),
        "bgsave" => commands::persistence::bgsave(&args, server),
        // A replica announces its listening port and capabilities with REPLCONF
        // during the handshake; the master just acknowledges each one. (When the
        // master later sends `REPLCONF GETACK` the *replica* answers — that
        // direction is the replica's job, not handled here.)
        "replconf" => Value::SimpleString("OK".to_string()),
        "wait" => commands::persistence::wait(&args, server),
        // Pub/sub, in `commands::pubsub`. `PUBLISH` runs from an ordinary
        // connection, so it's dispatched here; `SUBSCRIBE`/`UNSUBSCRIBE` are
        // handled by the dedicated subscriber loop (`serve_subscriber`,
        // reached from `handle_conn`) because they change the connection's
        // mode.
        "publish" => commands::pubsub::publish(&args, server),
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

    // Pub/sub (`SUBSCRIBE`/`UNSUBSCRIBE`/`PUBLISH`, the routing table, and the
    // subscribe-mode connection loop) tests moved to `commands::pubsub`
    // alongside the code they exercise. This one stays: it exercises the
    // dispatch tables that still live in `lib.rs`, not `pubsub` itself.
    #[test]
    fn publish_is_a_known_but_non_write_command() {
        // Queueable inside MULTI, but never streamed to replicas.
        assert!(is_known_command("publish"));
        assert!(!is_write_command("publish"));
    }
}
