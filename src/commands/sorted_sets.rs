//! `ZADD`/`ZSCORE`/`ZRANK`/`ZRANGE`/`ZCARD`/`ZREM`/`ZINCRBY`/`ZRANGEBYSCORE`/
//! `ZCOUNT` — the sorted set commands, following the
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

/// Parse a `ZRANGEBYSCORE`/`ZCOUNT` min/max bound: a plain score, `-inf` /
/// `+inf` (or bare `inf` — `f64::from_str` already accepts all three), or the
/// same score prefixed with `(` to make that end of the range *exclusive* —
/// Redis's score-range grammar, distinct from `ZADD`'s plain score (which has
/// no exclusivity marker at all; the `(` here plays the same role
/// `crate::stream`'s range-bound parsers give it for `XRANGE`/`XREVRANGE`).
/// Returns `(score, exclusive)`; rejects `nan` the same way `parse_score`
/// does, but with Redis's own error text for a bad bound (`"min or max is
/// not a float"`, vs. `parse_score`'s `"value is not a valid float"`).
fn parse_score_bound(value: &Value) -> Result<(f64, bool), String> {
    let s = unpack_bulk_str(value).map_err(|e| e.to_string())?;
    let (rest, exclusive) = match s.strip_prefix('(') {
        Some(inner) => (inner, true),
        None => (s.as_str(), false),
    };
    let score: f64 = rest
        .parse()
        .map_err(|_| "min or max is not a float".to_string())?;
    if score.is_nan() {
        return Err("min or max is not a float".to_string());
    }
    Ok((score, exclusive))
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

/// `ZCARD key` — the number of members in the sorted set, or `0` for a
/// missing key. WRONGTYPE if the key holds something other than a sorted set.
pub(crate) fn zcard(args: &[Value], storage: &Store) -> Value {
    if args.len() != 1 {
        return wrong_args("zcard");
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
            StoredValue::ZSet(zset) => Value::Integer(zset.len() as i64),
            _ => wrong_type(),
        },
    }
}

/// `ZREM key member [member ...]` — remove the named members and return how
/// many were actually present. Like `HDEL`, an emptied set is removed from
/// the keyspace entirely rather than left behind as an empty value.
pub(crate) fn zrem(args: &[Value], storage: &Store) -> Value {
    if args.len() < 2 {
        return wrong_args("zrem");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let mut members = Vec::with_capacity(args.len() - 1);
    for arg in &args[1..] {
        match unpack_bulk_str(arg) {
            Ok(m) => members.push(m),
            Err(e) => return Value::Error(format!("ERR {}", e)),
        }
    }

    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    let Some(entry) = store.get_mut(&key) else {
        return Value::Integer(0);
    };
    let zset = match &mut entry.value {
        StoredValue::ZSet(zset) => zset,
        _ => return wrong_type(),
    };
    let removed = members.iter().filter(|m| zset.remove(m)).count();
    if zset.is_empty() {
        store.remove(&key);
    }
    Value::Integer(removed as i64)
}

/// `ZINCRBY key increment member` — add `increment` to `member`'s score
/// (treating a missing member as starting at `0`), creating the set on first
/// use, and return the new score as a bulk string. Rejects a non-numeric
/// increment the same way `ZADD` rejects a non-numeric score, plus a
/// Redis-matching error if the arithmetic itself produces `NaN` (possible
/// from `inf + -inf`, even though neither operand alone is `NaN`).
pub(crate) fn zincrby(args: &[Value], storage: &Store) -> Value {
    if args.len() != 3 {
        return wrong_args("zincrby");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let increment = match parse_score(&args[1]) {
        Ok(s) => s,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let member = match unpack_bulk_str(&args[2]) {
        Ok(m) => m,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };

    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    let entry = store.entry(key).or_insert_with(|| Entry {
        value: StoredValue::ZSet(ZSet::default()),
        expires_at: None,
    });
    let zset = match &mut entry.value {
        StoredValue::ZSet(zset) => zset,
        _ => return wrong_type(),
    };
    let new_score = zset.score(&member).unwrap_or(0.0) + increment;
    if new_score.is_nan() {
        return Value::Error("ERR resulting score is not a number (NaN)".to_string());
    }
    zset.insert(member, new_score);
    Value::BulkString(format_score(new_score))
}

/// `ZRANGEBYSCORE key min max [WITHSCORES]` — the members whose *scores* fall
/// in `[min, max]` (ascending order). `min`/`max` accept `-inf`/`+inf` for an
/// open-ended scan and a leading `(` to make that end exclusive (see
/// [`parse_score_bound`]) — unlike `ZRANGE`, which ranges over rank, not
/// score. `WITHSCORES` and the missing-key/WRONGTYPE handling mirror `ZRANGE`
/// exactly.
pub(crate) fn zrangebyscore(args: &[Value], storage: &Store) -> Value {
    if args.len() != 3 && args.len() != 4 {
        return wrong_args("zrangebyscore");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let (min, min_exclusive) = match parse_score_bound(&args[1]) {
        Ok(v) => v,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let (max, max_exclusive) = match parse_score_bound(&args[2]) {
        Ok(v) => v,
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
                let items = zset.range_by_score(min, min_exclusive, max, max_exclusive);
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

/// `ZCOUNT key min max` — how many members' scores fall in `[min, max]`,
/// using the same bound grammar as `ZRANGEBYSCORE` (`-inf`/`+inf`, exclusive
/// `(`). `0` for a missing key.
pub(crate) fn zcount(args: &[Value], storage: &Store) -> Value {
    if args.len() != 3 {
        return wrong_args("zcount");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let (min, min_exclusive) = match parse_score_bound(&args[1]) {
        Ok(v) => v,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let (max, max_exclusive) = match parse_score_bound(&args[2]) {
        Ok(v) => v,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };

    let now = Instant::now();
    let mut store = storage.lock().unwrap();
    expire_if_due(&mut store, &key, now);
    match store.get(&key) {
        None => Value::Integer(0),
        Some(e) => match &e.value {
            StoredValue::ZSet(zset) => {
                Value::Integer(zset.count_by_score(min, min_exclusive, max, max_exclusive) as i64)
            }
            _ => wrong_type(),
        },
    }
}

/// Append a sorted set in Redis's `ZSET_2` payload layout: a count, then each
/// member as a string followed by its score as an 8-byte little-endian IEEE
/// double. Members are written in ascending `(score, member)` order. The type
/// byte itself is written by `rdb::write`; this is only the payload
/// [`read_zset`] consumes when `binary_scores` is set.
pub(crate) fn write_zset(buf: &mut Vec<u8>, zset: &ZSet) {
    use crate::rdb::write::{write_length, write_string};
    let members = zset.range(0, -1);
    write_length(buf, members.len() as u64);
    for (member, score) in members {
        write_string(buf, member);
        buf.extend_from_slice(&score.to_le_bytes());
    }
}

/// Inverse of [`write_zset`], plus the older `ZSET` score encoding (a length
/// byte, then decimal text, with `254`/`255` meaning `+inf`/`-inf`).
/// `binary_scores` selects `ZSET_2`. A NaN score or a repeated member is an
/// error, matching what Redis refuses to load.
pub(crate) fn read_zset(
    cur: &mut crate::rdb::Cursor,
    binary_scores: bool,
) -> Result<ZSet, crate::rdb::RdbError> {
    use crate::rdb::{read_count, read_string, RdbError};
    let count = read_count(cur)?;
    let mut zset = ZSet::default();
    for _ in 0..count {
        let member = read_string(cur)?;
        let score = if binary_scores {
            read_binary_score(cur)?
        } else {
            read_legacy_score(cur)?
        };
        if score.is_nan() {
            return Err(RdbError::InvalidScore);
        }
        if !zset.insert(member, score) {
            return Err(RdbError::DuplicateZSetMember);
        }
    }
    Ok(zset)
}

fn read_binary_score(cur: &mut crate::rdb::Cursor) -> Result<f64, crate::rdb::RdbError> {
    let bytes = cur.read_bytes(8)?;
    let mut arr = [0u8; 8];
    arr.copy_from_slice(bytes);
    Ok(f64::from_le_bytes(arr))
}

/// Redis's pre-`ZSET_2` score: one length byte, then that many bytes of
/// decimal text. `253` is NaN, `254` is `+inf`, `255` is `-inf`.
fn read_legacy_score(cur: &mut crate::rdb::Cursor) -> Result<f64, crate::rdb::RdbError> {
    use crate::rdb::RdbError;
    match cur.read_u8()? {
        255 => Ok(f64::NEG_INFINITY),
        254 => Ok(f64::INFINITY),
        253 => Err(RdbError::InvalidScore),
        len => {
            let bytes = cur.read_bytes(len as usize)?;
            let text = std::str::from_utf8(bytes).map_err(|_| RdbError::InvalidScore)?;
            text.parse::<f64>().map_err(|_| RdbError::InvalidScore)
        }
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
        assert_eq!(zcard(&[bulk("k")], &s), wt);
        assert_eq!(zrem(&[bulk("k"), bulk("m")], &s), wt);
        assert_eq!(zincrby(&[bulk("k"), bulk("1"), bulk("m")], &s), wt);
        assert_eq!(zrangebyscore(&[bulk("k"), bulk("0"), bulk("1")], &s), wt);
        assert_eq!(zcount(&[bulk("k"), bulk("0"), bulk("1")], &s), wt);
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

    #[test]
    fn zcard_counts_members_and_is_zero_for_a_missing_key() {
        let s = store();
        assert_eq!(zcard(&[bulk("missing")], &s), Value::Integer(0));
        zadd(&[bulk("z"), bulk("1"), bulk("a"), bulk("2"), bulk("b")], &s);
        assert_eq!(zcard(&[bulk("z")], &s), Value::Integer(2));
    }

    #[test]
    fn zrem_removes_named_members_and_counts_only_ones_present() {
        let s = store();
        zadd(
            &[
                bulk("z"),
                bulk("1"),
                bulk("a"),
                bulk("2"),
                bulk("b"),
                bulk("3"),
                bulk("c"),
            ],
            &s,
        );
        assert_eq!(
            zrem(&[bulk("z"), bulk("a"), bulk("nope"), bulk("c")], &s),
            Value::Integer(2)
        );
        assert_eq!(
            zrange(&[bulk("z"), bulk("0"), bulk("-1")], &s),
            Value::Array(vec![bulk("b")])
        );
    }

    #[test]
    fn zrem_deletes_the_key_once_the_set_is_emptied() {
        let s = store();
        zadd(&[bulk("z"), bulk("1"), bulk("a")], &s);
        assert_eq!(zrem(&[bulk("z"), bulk("a")], &s), Value::Integer(1));
        assert_eq!(
            type_cmd(&[bulk("z")], &s),
            Value::SimpleString("none".to_string())
        );
        assert_eq!(zcard(&[bulk("z")], &s), Value::Integer(0));
    }

    #[test]
    fn zrem_on_a_missing_key_removes_nothing() {
        let s = store();
        assert_eq!(zrem(&[bulk("missing"), bulk("a")], &s), Value::Integer(0));
    }

    #[test]
    fn zincrby_creates_the_member_starting_from_zero() {
        let s = store();
        assert_eq!(
            zincrby(&[bulk("z"), bulk("2.5"), bulk("m")], &s),
            Value::BulkString("2.5".to_string())
        );
        assert_eq!(
            zscore(&[bulk("z"), bulk("m")], &s),
            Value::BulkString("2.5".to_string())
        );
    }

    #[test]
    fn zincrby_adds_to_an_existing_score_and_can_go_negative() {
        let s = store();
        zadd(&[bulk("z"), bulk("5"), bulk("m")], &s);
        assert_eq!(
            zincrby(&[bulk("z"), bulk("-8"), bulk("m")], &s),
            Value::BulkString("-3".to_string())
        );
    }

    #[test]
    fn zincrby_rejects_a_non_numeric_increment() {
        let s = store();
        assert_eq!(
            zincrby(&[bulk("z"), bulk("notanumber"), bulk("m")], &s),
            Value::Error("ERR value is not a valid float".to_string())
        );
    }

    #[test]
    fn zincrby_rejects_an_increment_that_produces_nan() {
        let s = store();
        zadd(&[bulk("z"), bulk("inf"), bulk("m")], &s);
        assert_eq!(
            zincrby(&[bulk("z"), bulk("-inf"), bulk("m")], &s),
            Value::Error("ERR resulting score is not a number (NaN)".to_string())
        );
        // The member's score must be untouched by the rejected increment.
        assert_eq!(
            zscore(&[bulk("z"), bulk("m")], &s),
            Value::BulkString("inf".to_string())
        );
    }

    fn seed_zrangebyscore_set(s: &Store) {
        zadd(
            &[
                bulk("z"),
                bulk("1"),
                bulk("a"),
                bulk("2"),
                bulk("b"),
                bulk("2"),
                bulk("c"), // ties "b" on score
                bulk("3"),
                bulk("d"),
            ],
            s,
        );
    }

    #[test]
    fn zrangebyscore_is_inclusive_by_default() {
        let s = store();
        seed_zrangebyscore_set(&s);
        assert_eq!(
            zrangebyscore(&[bulk("z"), bulk("2"), bulk("2")], &s),
            Value::Array(vec![bulk("b"), bulk("c")])
        );
        assert_eq!(
            zrangebyscore(&[bulk("z"), bulk("1"), bulk("3")], &s),
            Value::Array(vec![bulk("a"), bulk("b"), bulk("c"), bulk("d")])
        );
    }

    #[test]
    fn zrangebyscore_supports_infinities_and_exclusive_bounds() {
        let s = store();
        seed_zrangebyscore_set(&s);
        assert_eq!(
            zrangebyscore(&[bulk("z"), bulk("-inf"), bulk("+inf")], &s),
            Value::Array(vec![bulk("a"), bulk("b"), bulk("c"), bulk("d")])
        );
        // Exclusive min of 1 drops "a"; exclusive max of 3 drops "d".
        assert_eq!(
            zrangebyscore(&[bulk("z"), bulk("(1"), bulk("(3")], &s),
            Value::Array(vec![bulk("b"), bulk("c")])
        );
    }

    #[test]
    fn zrangebyscore_withscores_interleaves_score_after_each_member() {
        let s = store();
        zadd(
            &[bulk("z"), bulk("1"), bulk("a"), bulk("2.5"), bulk("b")],
            &s,
        );
        assert_eq!(
            zrangebyscore(
                &[bulk("z"), bulk("-inf"), bulk("+inf"), bulk("WITHSCORES")],
                &s
            ),
            Value::Array(vec![bulk("a"), bulk("1"), bulk("b"), bulk("2.5")])
        );
    }

    #[test]
    fn zrangebyscore_on_missing_key_is_empty() {
        let s = store();
        assert_eq!(
            zrangebyscore(&[bulk("missing"), bulk("-inf"), bulk("+inf")], &s),
            Value::Array(Vec::new())
        );
    }

    #[test]
    fn zrangebyscore_rejects_a_bad_bound_or_trailing_option() {
        let s = store();
        seed_zrangebyscore_set(&s);
        assert_eq!(
            zrangebyscore(&[bulk("z"), bulk("notanumber"), bulk("3")], &s),
            Value::Error("ERR min or max is not a float".to_string())
        );
        assert_eq!(
            zrangebyscore(&[bulk("z"), bulk("1"), bulk("3"), bulk("NOPE")], &s),
            Value::Error("ERR syntax error".to_string())
        );
    }

    #[test]
    fn zcount_counts_members_in_the_score_window() {
        let s = store();
        seed_zrangebyscore_set(&s);
        assert_eq!(
            zcount(&[bulk("z"), bulk("2"), bulk("2")], &s),
            Value::Integer(2)
        );
        assert_eq!(
            zcount(&[bulk("z"), bulk("-inf"), bulk("+inf")], &s),
            Value::Integer(4)
        );
        assert_eq!(
            zcount(&[bulk("z"), bulk("(1"), bulk("(3")], &s),
            Value::Integer(2)
        );
    }

    #[test]
    fn zcount_is_zero_for_a_missing_key_or_empty_window() {
        let s = store();
        seed_zrangebyscore_set(&s);
        assert_eq!(
            zcount(&[bulk("missing"), bulk("-inf"), bulk("+inf")], &s),
            Value::Integer(0)
        );
        assert_eq!(
            zcount(&[bulk("z"), bulk("10"), bulk("20")], &s),
            Value::Integer(0)
        );
    }

    #[test]
    fn rdb_payload_round_trips_scores_including_infinity() {
        let mut zset = ZSet::default();
        zset.insert("c".to_string(), -2.5);
        zset.insert("a".to_string(), 1.0);
        zset.insert("top".to_string(), f64::INFINITY);
        let mut buf = Vec::new();
        write_zset(&mut buf, &zset);
        let mut cur = crate::rdb::Cursor::new(&buf);
        assert_eq!(read_zset(&mut cur, true).unwrap(), zset);
    }

    #[test]
    fn legacy_string_scores_load_infinities_and_decimals() {
        use crate::rdb::write::write_string;
        use crate::rdb::RdbError;
        let mut buf = Vec::new();
        crate::rdb::write::write_length(&mut buf, 2);
        write_string(&mut buf, "low");
        buf.push(255); // -inf
        write_string(&mut buf, "n");
        buf.push(1);
        buf.push(b'2');
        let mut cur = crate::rdb::Cursor::new(&buf);
        let zset = read_zset(&mut cur, false).unwrap();
        assert_eq!(zset.score("low"), Some(f64::NEG_INFINITY));
        assert_eq!(zset.score("n"), Some(2.0));

        let mut nan = Vec::new();
        crate::rdb::write::write_length(&mut nan, 1);
        write_string(&mut nan, "bad");
        nan.push(253);
        let mut cur = crate::rdb::Cursor::new(&nan);
        assert_eq!(read_zset(&mut cur, false), Err(RdbError::InvalidScore));
    }
}
