//! `HSET`/`HGET`/`HGETALL`/`HDEL` — the hash commands, the third command
//! group lifted out of `lib.rs` (see the module doc on [`crate::commands`]).
//! Reaches the same small set of shared `lib.rs` helpers as
//! [`crate::commands::lists`] (`crate::expire_if_due`,
//! `crate::unpack_bulk_str`, `crate::wrong_args`, `crate::wrong_type`) and
//! nothing from any other command group.

use crate::resp::Value;
use crate::{expire_if_due, unpack_bulk_str, wrong_args, wrong_type, Entry, Store, StoredValue};
use std::collections::HashMap;
use std::time::Instant;

/// `HSET key field value [field value ...]` — set one or more field/value
/// pairs on the hash at `key`, creating the hash if the key is absent. Returns
/// the number of fields that were *newly added* (updates to existing fields
/// don't count), matching Redis. The trailing arguments must form whole
/// field/value pairs, so the argument count after the key must be even.
pub(crate) fn hset(args: &[Value], storage: &Store) -> Value {
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
pub(crate) fn hget(args: &[Value], storage: &Store) -> Value {
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
pub(crate) fn hgetall(args: &[Value], storage: &Store) -> Value {
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
pub(crate) fn hdel(args: &[Value], storage: &Store) -> Value {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resp::Value;
    use crate::{get, set, type_cmd, wrong_type};
    use std::sync::{Arc, Mutex};

    fn store() -> Store {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.to_string())
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
}
