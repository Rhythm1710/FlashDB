//! `RPUSH`/`LPUSH`/`RPOP`/`LPOP`/`LLEN`/`LRANGE` — the list commands, the
//! second command group lifted out of `lib.rs` (see the module doc on
//! [`crate::commands`]). Chosen as a warm-up for the bigger stream-command
//! extraction still to come: like `persistence`, this group only reaches a
//! handful of shared helpers in `lib.rs` (`crate::expire_if_due`,
//! `crate::unpack_bulk_str`, `crate::unpack_int`, `crate::wrong_args`,
//! `crate::wrong_type`) and doesn't depend on any other command group.

use crate::resp::Value;
use crate::{
    expire_if_due, unpack_bulk_str, unpack_int, wrong_args, wrong_type, Entry, Store, StoredValue,
};
use std::collections::VecDeque;
use std::time::Instant;

/// Which end of a list a push or pop acts on. `RPUSH`/`RPOP` work the right
/// (tail) end; `LPUSH`/`LPOP` the left (head) end. Passing this as one enum
/// lets a single `push`/`pop` function serve both directions instead of
/// duplicating the body twice with `push_back`/`push_front` swapped.
///
/// `pub(crate)` because `lib.rs`'s `process_command` dispatch table
/// constructs it directly (`Side::Right`, `Side::Left`).
pub(crate) enum Side {
    Left,
    Right,
}

/// `RPUSH key v [v ...]` / `LPUSH key v [v ...]` — append (right) or prepend
/// (left) one or more values, creating the list if the key is absent. Returns
/// the list's new length. A `LPUSH a b c` leaves the list as `[c, b, a]`
/// because each value is pushed onto the head in turn.
pub(crate) fn push(args: &[Value], storage: &Store, side: Side) -> Value {
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
pub(crate) fn pop(args: &[Value], storage: &Store, side: Side) -> Value {
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
pub(crate) fn llen(args: &[Value], storage: &Store) -> Value {
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
pub(crate) fn lrange(args: &[Value], storage: &Store) -> Value {
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
}
