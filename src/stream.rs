//! Stream value type: IDs, entries, and the parsing rules the `X*` commands
//! share.
//!
//! A Redis *stream* is an append-only log of entries. Each entry has a unique,
//! monotonically increasing **ID** of the form `<ms>-<seq>` (a millisecond
//! timestamp and a per-millisecond sequence counter) and a set of field/value
//! pairs. The ordering rules — auto-generating IDs, rejecting an ID that isn't
//! strictly larger than the last one, and interpreting the `-`/`+`/partial
//! range bounds — are fiddly enough that they live here, apart from the command
//! plumbing in `lib.rs`, and can be unit-tested on their own.

use std::fmt;

/// A stream entry ID: a millisecond timestamp plus a sequence number that
/// disambiguates entries added within the same millisecond.
///
/// The field order (`ms` then `seq`) is deliberate: `#[derive(Ord)]` compares
/// structs field-by-field in declaration order, so the derived ordering is
/// exactly the stream ordering — compare timestamps first, break ties by
/// sequence. That single derive is what lets us `sort`, binary-search, and
/// range-scan entries without writing any comparison code by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct StreamId {
    pub ms: u64,
    pub seq: u64,
}

impl StreamId {
    /// The smallest possible ID, `0-0`. Redis forbids actually *adding* an
    /// entry with this ID, but it's the natural "empty stream" sentinel and the
    /// lower bound `-` resolves to in a range query.
    pub const MIN: StreamId = StreamId { ms: 0, seq: 0 };
    /// The largest possible ID, used as the upper bound `+` resolves to.
    pub const MAX: StreamId = StreamId {
        ms: u64::MAX,
        seq: u64::MAX,
    };
}

impl fmt::Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.ms, self.seq)
    }
}

/// One entry in a stream: its ID plus its field/value pairs, kept in insertion
/// order (a `Vec` of pairs rather than a map, because Redis preserves the order
/// fields were given to `XADD` and allows repeated fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEntry {
    pub id: StreamId,
    pub fields: Vec<(String, String)>,
}

/// A whole stream: its entries in ascending ID order, plus the last ID handed
/// out. `last_id` is tracked explicitly rather than read off the final entry so
/// it stays correct even once entry deletion (`XDEL`, a later feature) can
/// leave the tail empty — Redis never reuses an ID below the high-water mark.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stream {
    pub entries: Vec<StreamEntry>,
    pub last_id: StreamId,
}

impl Stream {
    /// Append an already-resolved entry. The caller has validated the ID is
    /// strictly greater than `last_id`, so pushing keeps `entries` sorted and we
    /// just advance the high-water mark.
    pub fn append(&mut self, id: StreamId, fields: Vec<(String, String)>) {
        self.entries.push(StreamEntry { id, fields });
        self.last_id = id;
    }

    /// The entries whose IDs fall in the inclusive range `[start, end]`.
    ///
    /// Because `entries` is always sorted, `partition_point` binary-searches for
    /// the first index at or after `start` in O(log n); we then walk forward
    /// while the ID stays `<= end`. `count` caps how many are returned (`None`
    /// = no cap), which is what `XRANGE ... COUNT n` needs.
    pub fn range(&self, start: StreamId, end: StreamId, count: Option<usize>) -> Vec<&StreamEntry> {
        if start > end {
            return Vec::new();
        }
        let from = self.entries.partition_point(|e| e.id < start);
        let mut out = Vec::new();
        for entry in &self.entries[from..] {
            if entry.id > end {
                break;
            }
            out.push(entry);
            if count.is_some_and(|c| out.len() >= c) {
                break;
            }
        }
        out
    }

    /// The entries with an ID strictly greater than `after`, capped by `count`.
    /// This is the read `XREAD` performs: "everything new since the ID I last
    /// saw." A strictly-greater bound is an inclusive range starting just past
    /// `after`, so we reuse `range` with `after.next()` as the low bound.
    pub fn entries_after(&self, after: StreamId, count: Option<usize>) -> Vec<&StreamEntry> {
        match after.next() {
            Some(start) => self.range(start, StreamId::MAX, count),
            // `after` was already the maximum possible ID: nothing can follow it.
            None => Vec::new(),
        }
    }

    /// The entries in the inclusive range `[start, end]` but yielded **highest
    /// ID first**, capped by `count`. This is what `XREVRANGE` returns: the same
    /// window as [`range`], reversed, with the cap applied from the top so a
    /// `COUNT n` keeps the `n` largest IDs rather than the `n` smallest.
    pub fn rev_range(
        &self,
        start: StreamId,
        end: StreamId,
        count: Option<usize>,
    ) -> Vec<&StreamEntry> {
        // Collect the whole window ascending (no cap yet), then reverse and cap:
        // capping before the reverse would keep the wrong end of the window.
        let mut out = self.range(start, end, None);
        out.reverse();
        if let Some(c) = count {
            out.truncate(c);
        }
        out
    }

    /// Remove the entry with exactly this ID, reporting whether one was there.
    /// `entries` stays sorted, so a binary search (`partition_point`) finds the
    /// slot in O(log n); a hit is removed with an O(n) shift. `last_id` is left
    /// untouched on purpose — Redis never lowers a stream's high-water mark, so a
    /// future ID can't collide with a just-deleted one even if the tail is gone.
    pub fn delete(&mut self, id: StreamId) -> bool {
        let idx = self.entries.partition_point(|e| e.id < id);
        if idx < self.entries.len() && self.entries[idx].id == id {
            self.entries.remove(idx);
            true
        } else {
            false
        }
    }
}

impl StreamId {
    /// The next representable ID after `self`, or `None` if `self` is already
    /// the maximum. Incrementing the sequence is enough until it saturates, at
    /// which point the next ID rolls into the following millisecond at seq 0.
    pub fn next(self) -> Option<StreamId> {
        if self.seq < u64::MAX {
            Some(StreamId {
                ms: self.ms,
                seq: self.seq + 1,
            })
        } else if self.ms < u64::MAX {
            Some(StreamId {
                ms: self.ms + 1,
                seq: 0,
            })
        } else {
            None
        }
    }

    /// The previous representable ID before `self`, or `None` if `self` is
    /// already `0-0`. The mirror of [`next`](StreamId::next): decrement the
    /// sequence, and when it underflows drop into the previous millisecond at
    /// the maximum sequence. Used to turn an *exclusive* end bound into the
    /// inclusive one the range scan works with.
    pub fn prev(self) -> Option<StreamId> {
        if self.seq > 0 {
            Some(StreamId {
                ms: self.ms,
                seq: self.seq - 1,
            })
        } else if self.ms > 0 {
            Some(StreamId {
                ms: self.ms - 1,
                seq: u64::MAX,
            })
        } else {
            None
        }
    }
}

/// What an `XADD` ID argument asked for, before it's resolved against the
/// stream's current state. Modelling the three shapes as an enum keeps the
/// parsing (pure string work) separate from the resolution (which needs the
/// clock and the last ID) — and makes the `match` in [`resolve_id`] spell out
/// every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdRequest {
    /// `*` — both the millisecond and the sequence are auto-generated.
    AutoAll,
    /// `<ms>-*` (or a bare `<ms>`) — the millisecond is fixed, the sequence is
    /// auto-generated.
    AutoSeq(u64),
    /// `<ms>-<seq>` — a fully specified ID.
    Explicit(StreamId),
}

/// The error message Redis returns for a malformed stream ID.
const INVALID_ID: &str = "ERR Invalid stream ID specified as stream command argument";

/// Parse the ID argument of an `XADD`. Accepts `*`, `<ms>`, `<ms>-*`, and
/// `<ms>-<seq>`; anything else is a client error.
pub fn parse_xadd_id(s: &str) -> Result<IdRequest, String> {
    if s == "*" {
        return Ok(IdRequest::AutoAll);
    }
    match s.split_once('-') {
        None => {
            // A bare millisecond means "this ms, auto sequence" — same as `ms-*`.
            let ms = s.parse::<u64>().map_err(|_| INVALID_ID.to_string())?;
            Ok(IdRequest::AutoSeq(ms))
        }
        Some((ms, "*")) => {
            let ms = ms.parse::<u64>().map_err(|_| INVALID_ID.to_string())?;
            Ok(IdRequest::AutoSeq(ms))
        }
        Some((ms, seq)) => {
            let ms = ms.parse::<u64>().map_err(|_| INVALID_ID.to_string())?;
            let seq = seq.parse::<u64>().map_err(|_| INVALID_ID.to_string())?;
            Ok(IdRequest::Explicit(StreamId { ms, seq }))
        }
    }
}

/// Turn a parsed [`IdRequest`] into the concrete ID the new entry will carry,
/// enforcing Redis's monotonicity rules against `last` (the stream's current
/// top ID) and the wall clock `now_ms`.
///
/// The invariant every branch upholds: the returned ID is strictly greater than
/// `last` and strictly greater than `0-0`. A request that can't satisfy that —
/// an explicit ID that isn't large enough, or an auto-sequence that would
/// overflow — becomes the matching Redis error string.
pub fn resolve_id(req: IdRequest, last: StreamId, now_ms: u64) -> Result<StreamId, String> {
    match req {
        IdRequest::AutoAll => {
            // Prefer the clock, but never go backwards: if time hasn't advanced
            // past the last entry's millisecond, stay on it and bump the seq.
            let ms = now_ms.max(last.ms);
            let seq = if ms == last.ms {
                last.seq.checked_add(1).ok_or_else(overflow_err)?
            } else {
                0
            };
            Ok(StreamId { ms, seq })
        }
        IdRequest::AutoSeq(ms) => {
            let id = if ms < last.ms {
                return Err(smaller_err());
            } else if ms == last.ms {
                let seq = last.seq.checked_add(1).ok_or_else(overflow_err)?;
                StreamId { ms, seq }
            } else {
                StreamId { ms, seq: 0 }
            };
            Ok(id)
        }
        IdRequest::Explicit(id) => {
            if id == StreamId::MIN {
                return Err("ERR The ID specified in XADD must be greater than 0-0".to_string());
            }
            if id <= last {
                return Err(smaller_err());
            }
            Ok(id)
        }
    }
}

fn smaller_err() -> String {
    "ERR The ID specified in XADD is equal or smaller than the target stream top item".to_string()
}

fn overflow_err() -> String {
    // The sequence counter for this millisecond is exhausted (2^64 entries in
    // one ms is not reachable in practice, but we refuse rather than wrap).
    "ERR The stream has exhausted the last possible ID, unable to add more items".to_string()
}

/// Parse a fully specified entry ID as used by `XDEL`: a bare `<ms>` fills the
/// sequence with `0`, a full `<ms>-<seq>` is taken exactly. Unlike the range
/// parsers this rejects `-`/`+`/`(` — `XDEL` names concrete entries, not ranges.
pub fn parse_entry_id(s: &str) -> Result<StreamId, String> {
    parse_bound(s, 0)
}

/// Parse the *start* bound of an `XRANGE`/`XREVRANGE`/`XREAD`. `-` is the
/// minimum ID; a bare `<ms>` means "the first entry in that millisecond", i.e.
/// seq 0; a full `<ms>-<seq>` is taken exactly. A leading `(` makes the bound
/// **exclusive**: the first ID strictly greater than the one written, which we
/// express as the inclusive bound one step up (`id.next()`).
pub fn parse_range_start(s: &str) -> Result<StreamId, String> {
    if let Some(inner) = s.strip_prefix('(') {
        let id = parse_bound(inner, 0)?;
        // Exclusive lower bound one past the maximum ID means the window is
        // empty; `MAX` as the inclusive start makes any real `end` invert it.
        return Ok(id.next().unwrap_or(StreamId::MAX));
    }
    if s == "-" {
        return Ok(StreamId::MIN);
    }
    parse_bound(s, 0)
}

/// Parse the *end* bound of an `XRANGE`/`XREVRANGE`. `+` is the maximum ID; a
/// bare `<ms>` means "the last entry in that millisecond", i.e. the maximum
/// sequence. A leading `(` makes the bound **exclusive**: the last ID strictly
/// smaller than the one written, expressed as the inclusive bound one step down
/// (`id.prev()`).
pub fn parse_range_end(s: &str) -> Result<StreamId, String> {
    if let Some(inner) = s.strip_prefix('(') {
        let id = parse_bound(inner, u64::MAX)?;
        // Exclusive upper bound below `0-0` means the window is empty; `MIN` as
        // the inclusive end makes any real `start` invert it.
        return Ok(id.prev().unwrap_or(StreamId::MIN));
    }
    if s == "+" {
        return Ok(StreamId::MAX);
    }
    parse_bound(s, u64::MAX)
}

/// Shared body of the two range parsers: a full `ms-seq` is exact, while a bare
/// `ms` takes `default_seq` for the missing sequence (0 for a start bound so it
/// includes the whole millisecond from the top; `u64::MAX` for an end bound so
/// it includes it to the bottom).
fn parse_bound(s: &str, default_seq: u64) -> Result<StreamId, String> {
    match s.split_once('-') {
        None => {
            let ms = s.parse::<u64>().map_err(|_| INVALID_ID.to_string())?;
            Ok(StreamId {
                ms,
                seq: default_seq,
            })
        }
        Some((ms, seq)) => {
            let ms = ms.parse::<u64>().map_err(|_| INVALID_ID.to_string())?;
            let seq = seq.parse::<u64>().map_err(|_| INVALID_ID.to_string())?;
            Ok(StreamId { ms, seq })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_order_by_ms_then_seq() {
        assert!(StreamId { ms: 1, seq: 0 } < StreamId { ms: 1, seq: 1 });
        assert!(StreamId { ms: 1, seq: 9 } < StreamId { ms: 2, seq: 0 });
        assert_eq!(StreamId::MIN, StreamId { ms: 0, seq: 0 });
        assert!(StreamId::MIN < StreamId::MAX);
    }

    #[test]
    fn id_displays_as_ms_dash_seq() {
        assert_eq!(StreamId { ms: 5, seq: 3 }.to_string(), "5-3");
    }

    #[test]
    fn parse_xadd_id_forms() {
        assert_eq!(parse_xadd_id("*").unwrap(), IdRequest::AutoAll);
        assert_eq!(parse_xadd_id("7").unwrap(), IdRequest::AutoSeq(7));
        assert_eq!(parse_xadd_id("7-*").unwrap(), IdRequest::AutoSeq(7));
        assert_eq!(
            parse_xadd_id("7-2").unwrap(),
            IdRequest::Explicit(StreamId { ms: 7, seq: 2 })
        );
        assert!(parse_xadd_id("nope").is_err());
        assert!(parse_xadd_id("7-x").is_err());
    }

    #[test]
    fn resolve_auto_all_prefers_clock_but_never_goes_back() {
        // Clock ahead of last: new millisecond, seq resets to 0.
        let id = resolve_id(IdRequest::AutoAll, StreamId { ms: 10, seq: 4 }, 20).unwrap();
        assert_eq!(id, StreamId { ms: 20, seq: 0 });
        // Clock not ahead: stay on last ms, bump the sequence.
        let id = resolve_id(IdRequest::AutoAll, StreamId { ms: 30, seq: 4 }, 20).unwrap();
        assert_eq!(id, StreamId { ms: 30, seq: 5 });
    }

    #[test]
    fn resolve_auto_seq_rules() {
        // Same ms as last: sequence advances.
        let id = resolve_id(IdRequest::AutoSeq(10), StreamId { ms: 10, seq: 2 }, 0).unwrap();
        assert_eq!(id, StreamId { ms: 10, seq: 3 });
        // Newer ms: sequence starts at 0.
        let id = resolve_id(IdRequest::AutoSeq(11), StreamId { ms: 10, seq: 2 }, 0).unwrap();
        assert_eq!(id, StreamId { ms: 11, seq: 0 });
        // Older ms is rejected.
        assert!(resolve_id(IdRequest::AutoSeq(9), StreamId { ms: 10, seq: 2 }, 0).is_err());
    }

    #[test]
    fn resolve_explicit_must_increase_and_beat_zero() {
        let last = StreamId { ms: 5, seq: 5 };
        // Strictly greater is fine.
        assert_eq!(
            resolve_id(IdRequest::Explicit(StreamId { ms: 5, seq: 6 }), last, 0).unwrap(),
            StreamId { ms: 5, seq: 6 }
        );
        // Equal or smaller is rejected.
        assert!(resolve_id(IdRequest::Explicit(last), last, 0).is_err());
        assert!(resolve_id(IdRequest::Explicit(StreamId { ms: 5, seq: 4 }), last, 0).is_err());
        // 0-0 is specifically rejected on an empty stream.
        let err = resolve_id(IdRequest::Explicit(StreamId::MIN), StreamId::MIN, 0).unwrap_err();
        assert!(err.contains("greater than 0-0"));
    }

    #[test]
    fn range_bounds_fill_defaults() {
        assert_eq!(parse_range_start("-").unwrap(), StreamId::MIN);
        assert_eq!(parse_range_end("+").unwrap(), StreamId::MAX);
        assert_eq!(parse_range_start("5").unwrap(), StreamId { ms: 5, seq: 0 });
        assert_eq!(
            parse_range_end("5").unwrap(),
            StreamId {
                ms: 5,
                seq: u64::MAX
            }
        );
        assert_eq!(
            parse_range_start("5-2").unwrap(),
            StreamId { ms: 5, seq: 2 }
        );
    }

    #[test]
    fn stream_range_and_after() {
        let mut s = Stream::default();
        s.append(StreamId { ms: 1, seq: 0 }, vec![("a".into(), "1".into())]);
        s.append(StreamId { ms: 2, seq: 0 }, vec![("b".into(), "2".into())]);
        s.append(StreamId { ms: 2, seq: 1 }, vec![("c".into(), "3".into())]);
        s.append(StreamId { ms: 3, seq: 0 }, vec![("d".into(), "4".into())]);

        // Inclusive range over the two ms=2 entries.
        let got = s.range(
            StreamId { ms: 2, seq: 0 },
            StreamId {
                ms: 2,
                seq: u64::MAX,
            },
            None,
        );
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, StreamId { ms: 2, seq: 0 });
        assert_eq!(got[1].id, StreamId { ms: 2, seq: 1 });

        // COUNT caps the result.
        let got = s.range(StreamId::MIN, StreamId::MAX, Some(2));
        assert_eq!(got.len(), 2);

        // Everything strictly after 2-0 is 2-1 and 3-0.
        let got = s.entries_after(StreamId { ms: 2, seq: 0 }, None);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, StreamId { ms: 2, seq: 1 });

        // Inverted range yields nothing.
        assert!(s
            .range(StreamId { ms: 3, seq: 0 }, StreamId { ms: 1, seq: 0 }, None)
            .is_empty());
    }

    #[test]
    fn id_prev_is_the_inverse_of_next() {
        let id = StreamId { ms: 5, seq: 3 };
        assert_eq!(id.prev(), Some(StreamId { ms: 5, seq: 2 }));
        // Sequence underflow borrows from the millisecond.
        assert_eq!(
            StreamId { ms: 5, seq: 0 }.prev(),
            Some(StreamId {
                ms: 4,
                seq: u64::MAX
            })
        );
        // Nothing precedes 0-0.
        assert_eq!(StreamId::MIN.prev(), None);
    }

    #[test]
    fn exclusive_bounds_nudge_one_step_inward() {
        // `(5-5` as a start is the first ID strictly greater than 5-5.
        assert_eq!(
            parse_range_start("(5-5").unwrap(),
            StreamId { ms: 5, seq: 6 }
        );
        // `(5-5` as an end is the last ID strictly smaller than 5-5.
        assert_eq!(parse_range_end("(5-5").unwrap(), StreamId { ms: 5, seq: 4 });
        // A bad inner ID is still rejected.
        assert!(parse_range_start("(nope").is_err());
    }

    #[test]
    fn rev_range_yields_highest_first_and_caps_from_the_top() {
        let mut s = Stream::default();
        s.append(StreamId { ms: 1, seq: 0 }, vec![("a".into(), "1".into())]);
        s.append(StreamId { ms: 2, seq: 0 }, vec![("b".into(), "2".into())]);
        s.append(StreamId { ms: 3, seq: 0 }, vec![("c".into(), "3".into())]);

        let got = s.rev_range(StreamId::MIN, StreamId::MAX, None);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].id, StreamId { ms: 3, seq: 0 });
        assert_eq!(got[2].id, StreamId { ms: 1, seq: 0 });

        // COUNT keeps the two *highest* IDs, not the two lowest.
        let got = s.rev_range(StreamId::MIN, StreamId::MAX, Some(2));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, StreamId { ms: 3, seq: 0 });
        assert_eq!(got[1].id, StreamId { ms: 2, seq: 0 });
    }

    #[test]
    fn delete_removes_only_an_exact_match_and_keeps_last_id() {
        let mut s = Stream::default();
        s.append(StreamId { ms: 1, seq: 0 }, vec![("a".into(), "1".into())]);
        s.append(StreamId { ms: 2, seq: 0 }, vec![("b".into(), "2".into())]);

        assert!(!s.delete(StreamId { ms: 9, seq: 9 })); // no such entry
        assert!(s.delete(StreamId { ms: 1, seq: 0 }));
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].id, StreamId { ms: 2, seq: 0 });
        // Deleting the tail does not lower the high-water mark.
        assert!(s.delete(StreamId { ms: 2, seq: 0 }));
        assert!(s.entries.is_empty());
        assert_eq!(s.last_id, StreamId { ms: 2, seq: 0 });
    }

    #[test]
    fn parse_entry_id_fills_seq_and_rejects_ranges() {
        assert_eq!(parse_entry_id("5").unwrap(), StreamId { ms: 5, seq: 0 });
        assert_eq!(parse_entry_id("5-2").unwrap(), StreamId { ms: 5, seq: 2 });
        assert!(parse_entry_id("-").is_err());
        assert!(parse_entry_id("+").is_err());
    }
}
