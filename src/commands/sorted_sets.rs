//! `ZADD`/`ZSCORE`/`ZRANK`/`ZRANGE` — the sorted set commands, following the
//! same `crate::`-reaching pattern as every other group in
//! [`crate::commands`]. The ordering logic itself lives in
//! [`crate::sorted_set`]; this module is just RESP argument parsing plus
//! store access, mirroring [`crate::commands::hashes`] closely (`ZADD`
//! parses variadic pairs exactly like `HSET`).

use crate::resp::Value;
use crate::sorted_set::ZSet;
use crate::{
    expire_if_due, unpack_bulk_str, unpack_int, wrong_args, wrong_type, Entry, Store, StoredValue,
};
use std::time::Instant;

/// Parse a bulk-string argument as the floating-point score `ZADD` expects.
/// Redis accepts `inf`/`-inf` (Rust's `f64::from_str` already does) but
/// rejects `nan` — a member can't be ordered against a score that doesn't
/// compare to anything, which is exactly what [`crate::sorted_set::ZSet`]
/// relies on to keep its ranked order total.
fn parse_score(value: &Value) -> Result<f64, String> {
    let s = unpack_bulk_str(value).map_err(|e| e.to_string())?;
    let score: f64 = s
        .parse()
        .map_err(|_| "value is not a valid float".to_string())?;
    if score.is_nan() {
        return Err("value is not a valid float".to_string());
    }
    Ok(score)
}

/// Render a score the way `ZSCORE`/`ZRANGE ... WITHSCORES` report it: a whole
/// number prints with no trailing `.0` (matching Redis, which trims trailing
/// zeros off its `%.17g` formatting), anything fractional — or `inf`/`-inf` —
/// prints via `f64`'s own shortest round-tripping `Display`.
fn format_score(score: f64) -> String {
    if score.is_finite() && score.fract() == 0.0 {
        format!("{}", score as i64)
    } else {
        format!("{}", score)
    }
}

/// `ZADD key score member [score member ...]` — set one or more members'
/// scores in the sorted set at `key`, creating it if the key is absent.
/// Returns the number of members that were *newly added* (an update to an
/// existing member's score doesn't count), matching `HSET`'s and Redis's own
/// counting rule. The trailing arguments must form whole score/member pairs.
pub(crate) fn zadd(args: &[Value], storage: &Store) -> Value {
    // key + at least one score/member pair, and the pairs must be complete:
    // that means an odd total (key + an even number of score/member tokens).
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return wrong_args("zadd");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    // Parse every pair up front so a bad score or a malformed member fails
    // before anything is mutated — a rejected ZADD leaves the keyspace alone,
    // the same rule `hset` follows.
    let mut pairs = Vec::with_capacity((args.len() - 1) / 2);
    let mut i = 1;
    while i < args.len() {
        let score = match parse_score(&args[i]) {
            Ok(s) => s,
            Err(e) => return Value::Error(format!("ERR {}", e)),
        };
        let member = match unpack_bulk_str(&args[i + 1]) {
            Ok(m) => m,
            Err(e) => return Value::Error(format!("ERR {}", e)),
        };
        pairs.push((score, member));
        i += 2;
    }

    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    // Like `hset`'s `or_insert_with`, this only creates a fresh sorted set
    // when the key is absent; an existing wrong-typed key is left untouched
    // and falls through to the WRONGTYPE arm below.
    let entry = store.entry(key).or_insert_with(|| Entry {
        value: StoredValue::ZSet(ZSet::default()),
        expires_at: None,
    });
    match &mut entry.value {
        StoredValue::ZSet(zset) => {
            let mut added = 0i64;
            for (score, member) in pairs {
                if zset.insert(member, score) {
                    added += 1;
                }
            }
            Value::Integer(added)
        }
        _ => wrong_type(),
    }
}

/// `ZSCORE key member` — the member's score as a bulk string, or null if the
/// key or the member is missing. WRONGTYPE if the key holds something other
/// than a sorted set.
pub(crate) fn zscore(args: &[Value], storage: &Store) -> Value {
    if args.len() != 2 {
        return wrong_args("zscore");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let member = match unpack_bulk_str(&args[1]) {
        Ok(m) => m,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get(&key) {
        None => Value::Null,
        Some(e) => match &e.value {
            StoredValue::ZSet(zset) => match zset.score(&member) {
                Some(score) => Value::BulkString(format_score(score)),
                None => Value::Null,
            },
            _ => wrong_type(),
        },
    }
}

/// `ZRANK key member` — the member's 0-based rank in ascending score order, or
/// null if the key or the member is missing.
pub(crate) fn zrank(args: &[Value], storage: &Store) -> Value {
    if args.len() != 2 {
        return wrong_args("zrank");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let member = match unpack_bulk_str(&args[1]) {
        Ok(m) => m,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get(&key) {
        None => Value::Null,
        Some(e) => match &e.value {
            StoredValue::ZSet(zset) => match zset.rank(&member) {
                Some(rank) => Value::Integer(rank as i64),
                None => Value::Null,
            },
            _ => wrong_type(),
        },
    }
}

/// `ZRANGE key start stop [WITHSCORES]` — the members whose ranks fall in
/// `[start, stop]` (inclusive, ascending order, negative indices count back
/// from the end — same clamping rule as `LRANGE`). Without `WITHSCORES` the
/// reply is a flat array of members; with it, each member is followed by its
/// score as a bulk string, matching Redis. An empty array for a missing key.
pub(crate) fn zrange(args: &[Value], storage: &Store) -> Value {
    if args.len() != 3 && args.len() != 4 {
        return wrong_args("zrange");
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
    let with_scores = match args.get(3) {
        None => false,
        Some(v) => match unpack_bulk_str(v) {
            Ok(s) if s.eq_ignore_ascii_case("withscores") => true,
            Ok(_) => return Value::Error("ERR syntax error".to_string()),
            Err(e) => return Value::Error(format!("ERR {}", e)),
        },
    };

    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get(&key) {
        None => Value::Array(Vec::new()),
        Some(e) => match &e.value {
            StoredValue::ZSet(zset) => {
                let items = zset.range(start, stop);
                let mut out = Vec::with_capacity(if with_scores {
                    items.len() * 2
                } else {
                    items.len()
                });
                for (member, score) in items {
                    out.push(Value::BulkString(member.to_string()));
                    if with_scores {
                        out.push(Value::BulkString(format_score(score)));
                    }
                }
                Value::Array(out)
            }
            _ => wrong_type(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resp::Value;
    use crate::{get, set, type_cmd, wrong_type};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn store() -> Store {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.to_string())
    }

    #[test]
    fn zadd_adds_and_counts_only_new_members() {
        let s = store();
        assert_eq!(
            zadd(&[bulk("z"), bulk("1"), bulk("a"), bulk("2"), bulk("b")], &s),
            Value::Integer(2)
        );
        // Updating "a"'s score and adding "c" -> only "c" is new.
        assert_eq!(
            zadd(&[bulk("z"), bulk("9"), bulk("a"), bulk("3"), bulk("c")], &s),
            Value::Integer(1)
        );
        assert_eq!(
            zscore(&[bulk("z"), bulk("a")], &s),
            Value::BulkString("9".to_string())
        );
    }

    #[test]
    fn zscore_returns_value_or_null() {
        let s = store();
        zadd(&[bulk("z"), bulk("1.5"), bulk("m")], &s);
        assert_eq!(
            zscore(&[bulk("z"), bulk("m")], &s),
            Value::BulkString("1.5".to_string())
        );
        assert_eq!(zscore(&[bulk("z"), bulk("nope")], &s), Value::Null);
        assert_eq!(zscore(&[bulk("missing"), bulk("m")], &s), Value::Null);
    }

    #[test]
    fn zrank_reflects_ascending_order() {
        let s = store();
        zadd(
            &[
                bulk("z"),
                bulk("3"),
                bulk("c"),
                bulk("1"),
                bulk("a"),
                bulk("2"),
                bulk("b"),
            ],
            &s,
        );
        assert_eq!(zrank(&[bulk("z"), bulk("a")], &s), Value::Integer(0));
        assert_eq!(zrank(&[bulk("z"), bulk("b")], &s), Value::Integer(1));
        assert_eq!(zrank(&[bulk("z"), bulk("c")], &s), Value::Integer(2));
        assert_eq!(zrank(&[bulk("z"), bulk("nope")], &s), Value::Null);
        assert_eq!(zrank(&[bulk("missing"), bulk("a")], &s), Value::Null);
    }

    #[test]
    fn zrange_returns_members_in_order_with_negative_indices() {
        let s = store();
        zadd(
            &[
                bulk("z"),
                bulk("3"),
                bulk("c"),
                bulk("1"),
                bulk("a"),
                bulk("2"),
                bulk("b"),
            ],
            &s,
        );
        assert_eq!(
            zrange(&[bulk("z"), bulk("0"), bulk("-1")], &s),
            Value::Array(vec![bulk("a"), bulk("b"), bulk("c")])
        );
        assert_eq!(
            zrange(&[bulk("z"), bulk("-2"), bulk("-1")], &s),
            Value::Array(vec![bulk("b"), bulk("c")])
        );
    }

    #[test]
    fn zrange_withscores_interleaves_score_after_each_member() {
        let s = store();
        zadd(
            &[bulk("z"), bulk("1"), bulk("a"), bulk("2.5"), bulk("b")],
            &s,
        );
        assert_eq!(
            zrange(&[bulk("z"), bulk("0"), bulk("-1"), bulk("WITHSCORES")], &s),
            Value::Array(vec![bulk("a"), bulk("1"), bulk("b"), bulk("2.5"),])
        );
    }

    #[test]
    fn zrange_on_missing_key_is_empty() {
        let s = store();
        assert_eq!(
            zrange(&[bulk("missing"), bulk("0"), bulk("-1")], &s),
            Value::Array(Vec::new())
        );
    }

    #[test]
    fn zadd_rejects_a_non_numeric_score() {
        let s = store();
        assert_eq!(
            zadd(&[bulk("z"), bulk("notanumber"), bulk("m")], &s),
            Value::Error("ERR value is not a valid float".to_string())
        );
        assert_eq!(
            zadd(&[bulk("z"), bulk("nan"), bulk("m")], &s),
            Value::Error("ERR value is not a valid float".to_string())
        );
    }

    #[test]
    fn zadd_checks_arity_including_dangling_member() {
        let s = store();
        assert_eq!(
            zadd(&[bulk("z")], &s),
            Value::Error("ERR wrong number of arguments for 'zadd' command".to_string())
        );
        assert_eq!(
            zadd(&[bulk("z"), bulk("1")], &s),
            Value::Error("ERR wrong number of arguments for 'zadd' command".to_string())
        );
    }

    #[test]
    fn zrange_rejects_a_bad_trailing_option() {
        let s = store();
        zadd(&[bulk("z"), bulk("1"), bulk("a")], &s);
        assert_eq!(
            zrange(&[bulk("z"), bulk("0"), bulk("-1"), bulk("NOPE")], &s),
            Value::Error("ERR syntax error".to_string())
        );
    }

    #[test]
    fn sorted_set_commands_reject_a_string_key_with_wrongtype() {
        let s = store();
        set(&[bulk("k"), bulk("v")], &s);
        let wt = wrong_type();
        assert_eq!(zadd(&[bulk("k"), bulk("1"), bulk("m")], &s), wt);
        assert_eq!(zscore(&[bulk("k"), bulk("m")], &s), wt);
        assert_eq!(zrank(&[bulk("k"), bulk("m")], &s), wt);
        assert_eq!(zrange(&[bulk("k"), bulk("0"), bulk("-1")], &s), wt);
        // The failed ZADD must not have clobbered the string value.
        assert_eq!(get(&[bulk("k")], &s), Value::BulkString("v".to_string()));
    }

    #[test]
    fn type_reports_zset_for_a_zadd_key() {
        let s = store();
        zadd(&[bulk("z"), bulk("1"), bulk("m")], &s);
        assert_eq!(
            type_cmd(&[bulk("z")], &s),
            Value::SimpleString("zset".to_string())
        );
    }
}
