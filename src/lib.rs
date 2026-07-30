//! FlashDB — a Redis-compatible in-memory database written from scratch.
//!
//! The binary in `main.rs` is a thin wrapper around this library: it binds a
//! listener and hands it to [`run`]. Exposing the server as a library lets the
//! integration tests in `tests/` start a real server on an ephemeral port and
//! talk to it over a genuine TCP socket, exactly as a client would.

use anyhow::Result;
use resp::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};

pub mod rdb;
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
#[derive(Debug, PartialEq)]
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
/// chooses the listening port, and `--dir` / `--dbfilename` name where an RDB
/// snapshot lives on disk. The two RDB fields aren't consulted yet (persistence
/// is Phase 3) but are parsed and held now so the wiring is ready the day
/// `SAVE`/load arrive.
#[derive(Debug, PartialEq, Eq)]
pub struct Config {
    pub port: u16,
    pub dir: String,
    pub dbfilename: String,
}

impl Default for Config {
    /// Redis's own defaults: port 6379, the current directory, `dump.rdb`.
    fn default() -> Self {
        Config {
            port: 6379,
            dir: ".".to_string(),
            dbfilename: "dump.rdb".to_string(),
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

/// Accept connections on `listener` forever, serving each on its own task.
///
/// The store is created here and shared (cloned `Arc`) into every connection.
/// This never returns under normal operation; an error is only produced if
/// `accept` itself fails.
pub async fn run(listener: TcpListener) -> Result<()> {
    let storage: Store = Arc::new(Mutex::new(HashMap::new()));

    loop {
        let (stream, _) = listener.accept().await?;
        println!("accepted new connection");
        let storage = storage.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, storage).await {
                eprintln!("Connection error: {:?}", e);
            }
        });
    }
}

async fn handle_conn(stream: TcpStream, storage: Store) -> Result<()> {
    let mut handler = resp::RespHandler::new(stream);
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

        // A malformed request produces an error reply, not a dropped
        // connection, so one bad client can't take down its own session.
        let response = process_command(value, &storage);
        handler.write_value(response).await?;
    }
    Ok(())
}

/// Route a parsed request to its command handler and produce a reply value.
///
/// Public so integration tests can exercise command dispatch directly, and so
/// the logic lives beside the rest of the server rather than in the binary.
pub fn process_command(value: Value, storage: &Store) -> Value {
    let (command, args) = match extract_command(value) {
        Ok(parts) => parts,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };

    match command.to_lowercase().as_str() {
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
        c => Value::Error(format!("ERR unknown command '{}'", c)),
    }
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
}
