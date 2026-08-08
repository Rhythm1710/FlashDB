//! `SAVE`, `BGSAVE`, `WAIT`, and the snapshot helpers underneath them — the
//! first command group split out of `lib.rs` (see the module doc on
//! [`crate::commands`] for why).
//!
//! Everything here reaches the rest of the crate through `crate::` paths:
//! [`crate::Server`], [`crate::Store`], [`crate::rdb`], and a couple of small
//! shared helpers (`crate::now_unix_ms`, `crate::wrong_args`,
//! `crate::unpack_int`) that stay put in `lib.rs` because other command
//! groups still awaiting extraction use them too.

use crate::resp::Value;
use crate::{rdb, unpack_int, wrong_args, Entry, Replicas, Server, Store, StoredValue};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

/// Turn the live store into the [`rdb::RdbEntry`]s a snapshot serializes —
/// the inverse of [`crate::entries_to_store`]. Each value is cloned (the
/// snapshot must outlive the lock), already-expired keys are skipped so dead
/// keys aren't persisted, and every surviving expiry is converted from the
/// store's monotonic `Instant` deadline back into the absolute
/// Unix-millisecond deadline the file records. Both clocks are parameters, so
/// the conversion stays deterministic and unit-testable without touching the
/// real clock.
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
        if matches!(entry.value, StoredValue::Stream(_)) {
            // Real Redis persists streams with a listpack/radix-tree encoding we
            // neither write nor read yet, so a stream is skipped rather than
            // emitted in a format a real `redis-server` couldn't load. (This
            // filter is the single source of truth; the exhaustive arms in
            // `rdb::write` exist only for the type system and never run.)
            continue;
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
/// entries; the encoding runs after the lock is released, so a snapshot
/// doesn't block other clients for the whole serialization.
///
/// `pub(crate)` because `lib.rs`'s `serve_replica` also needs a snapshot to
/// answer a replica's initial `PSYNC`.
pub(crate) fn snapshot_bytes(store: &Store) -> Vec<u8> {
    let now = Instant::now();
    let unix_ms = crate::now_unix_ms();
    let entries = {
        let map = store.lock().unwrap();
        map_to_rdb_entries(&map, unix_ms, now)
    };
    rdb::write::serialize(&entries)
}

/// Write `bytes` to `path` atomically: write a sibling temp file, then rename
/// it over the target. A crash mid-write leaves the previous snapshot intact
/// rather than a half-written file, the same trick Redis uses with its temp
/// file.
fn write_snapshot(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// `SAVE` — synchronously write the whole keyspace to the configured RDB
/// path, then reply `+OK`. Like Redis's own `SAVE`, this blocks until the
/// file is on disk. An IO failure becomes an error reply rather than a
/// crash.
pub(crate) fn save(args: &[Value], server: &Server) -> Value {
    if !args.is_empty() {
        return wrong_args("save");
    }
    let bytes = snapshot_bytes(&server.store);
    match write_snapshot(server.rdb_path.as_path(), &bytes) {
        Ok(()) => Value::SimpleString("OK".to_string()),
        Err(e) => Value::Error(format!("ERR {}", e)),
    }
}

/// `BGSAVE` — snapshot the keyspace and write it off the request path. We
/// copy a consistent point-in-time snapshot under the lock (Redis forks a
/// child for the same effect), then hand the file IO to a background thread
/// and reply at once so the client isn't blocked on the disk. A write
/// failure is logged rather than reported, since the reply has already gone
/// out.
pub(crate) fn bgsave(args: &[Value], server: &Server) -> Value {
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
/// writes but doesn't yet track each replica's acknowledged offset, so it
/// can't honour the "up to this offset" part. Instead it returns the number
/// of replicas currently connected — a truthful count of who is receiving
/// the stream — immediately. Offset-accurate, blocking `WAIT` is a
/// follow-up.
///
/// The two arguments are still validated as integers so a malformed `WAIT`
/// is rejected the way Redis rejects it, even though we don't act on their
/// values.
pub(crate) fn wait(args: &[Value], server: &Server) -> Value {
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
/// ended (its receiver dropped, so the sender now reports closed). Pruning
/// here keeps a disconnected replica from being counted forever.
fn connected_replica_count(replicas: &Replicas) -> usize {
    let mut list = replicas.lock().unwrap();
    list.retain(|tx| !tx.is_closed());
    list.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hset, load_store_from_rdb, push, set, Config, Side};
    use bytes::Bytes;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn store() -> Store {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.to_string())
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

    // Build the [`rdb::RdbEntry`]s for a store, then convert the same expiry
    // the other way with `entries_to_store`, and confirm a future deadline
    // survives the monotonic → wall-clock → monotonic round trip and an
    // expired key is dropped on save.
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
