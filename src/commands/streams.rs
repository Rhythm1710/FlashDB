//! `XADD`/`XLEN`/`XRANGE`/`XREVRANGE`/`XDEL`/`XTRIM`/`XREAD` (including the
//! blocking `XREAD ... BLOCK` path) — the stream commands, the largest group
//! lifted out of `lib.rs` yet (see the module doc on [`crate::commands`]).
//! Unlike `persistence`/`lists`/`hashes`, this group brings a local type
//! along with it: [`XReadRequest`], the parsed shape of an `XREAD` command,
//! is only ever constructed and consumed in here — both the synchronous
//! [`xread`] and the async [`xread_blocking`] (reached from `lib.rs`'s
//! connection loop so it can actually `.await`) share it. Everything else
//! reaches the rest of the crate through `crate::` paths: [`crate::Server`],
//! [`crate::Store`], [`crate::Entry`], [`crate::StoredValue`], the
//! [`crate::stream`] module, and the small shared helpers (`crate::wrong_type`,
//! `crate::wrong_args`, `crate::unpack_bulk_str`, `crate::expire_if_due`,
//! `crate::now_unix_ms`, `crate::command_name`, `crate::command_args`) that
//! stay in `lib.rs` because other command groups use them too.

use crate::resp::Value;
use crate::{
    command_args, command_name, expire_if_due, now_unix_ms, stream, unpack_bulk_str, wrong_args,
    wrong_type, Entry, Server, Store, StoredValue,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use stream::Stream;
use tokio::time::timeout;

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
pub(crate) fn xadd(args: &[Value], storage: &Store) -> Value {
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
pub(crate) fn xlen(args: &[Value], storage: &Store) -> Value {
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
pub(crate) fn xrange(args: &[Value], storage: &Store) -> Value {
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
pub(crate) fn xrevrange(args: &[Value], storage: &Store) -> Value {
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
pub(crate) fn xdel(args: &[Value], storage: &Store) -> Value {
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
pub(crate) fn xtrim(args: &[Value], storage: &Store) -> Value {
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
pub(crate) struct XReadRequest {
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
pub(crate) fn xread(args: &[Value], storage: &Store) -> Value {
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
pub(crate) async fn xread_blocking(req: XReadRequest, server: &Server) -> Value {
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
pub(crate) fn as_blocking_xread(value: &Value) -> Option<XReadRequest> {
    if command_name(value).as_deref() != Some("xread") {
        return None;
    }
    let req = parse_xread(command_args(value)).ok()?;
    req.block.map(|_| req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{get, process_command, set, type_cmd};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn store() -> Store {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.to_string())
    }

    fn server() -> Server {
        Server::new(store(), std::path::PathBuf::from("dump.rdb"))
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
}
