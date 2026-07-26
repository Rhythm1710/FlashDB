//! FlashDB — a Redis-compatible in-memory database written from scratch.
//!
//! The binary in `main.rs` is a thin wrapper around this library: it binds a
//! listener and hands it to [`run`]. Exposing the server as a library lets the
//! integration tests in `tests/` start a real server on an ephemeral port and
//! talk to it over a genuine TCP socket, exactly as a client would.

use anyhow::Result;
use resp::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};

pub mod resp;

/// One stored value plus its optional expiry deadline.
///
/// Splitting the value from its lifetime this way is what lets a key outlive
/// the request that created it only until a chosen `Instant`. `expires_at` of
/// `None` means "lives forever"; `Some(deadline)` means the key is gone the
/// moment `Instant::now()` passes `deadline`.
pub struct Entry {
    value: String,
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
    storage
        .lock()
        .unwrap()
        .insert(key, Entry { value, expires_at });
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
        Some(e) => Value::BulkString(e.value.clone()),
        None => Value::Null,
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
            value: "v".to_string(),
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
            value: "v".to_string(),
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
}
