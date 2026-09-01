//! Sorted set value type: the pure `ZSet` data structure the `Z*` commands
//! share.
//!
//! A Redis *sorted set* holds unique string members, each tagged with a
//! floating-point **score**. Members are ordered by score (ties broken by
//! comparing the member strings themselves), and that order is what `ZRANGE`
//! walks and `ZRANK` reports a position in. Mirroring [`crate::stream`]'s
//! split, the ordering rules live here — apart from the RESP argument parsing
//! in `commands::sorted_sets` — so they can be unit-tested on their own.

use std::cmp::Ordering;
use std::collections::HashMap;

/// One member at its current score, as kept in ascending sorted order.
#[derive(Debug, Clone, PartialEq)]
struct Ranked {
    score: f64,
    member: String,
}

/// Compare two (score, member) pairs the way the sorted order does: score
/// first, then member as a tiebreaker. `f64::partial_cmp` only returns `None`
/// for NaN, and [`ZSet::insert`] never stores a NaN score (rejected by the
/// caller before it reaches here — see `commands::sorted_sets::parse_score`),
/// so `unwrap` is safe: every score actually stored has a total order.
fn cmp_pairs(a_score: f64, a_member: &str, b_score: f64, b_member: &str) -> Ordering {
    a_score
        .partial_cmp(&b_score)
        .unwrap()
        .then_with(|| a_member.cmp(b_member))
}

/// A whole sorted set: every member's score for O(1) lookup, plus the same
/// members kept in ascending order for O(log n) ranking and range scans —
/// the same two-views trick a database index uses, traded for simplicity over
/// a real skip list (which is what real Redis uses internally).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ZSet {
    scores: HashMap<String, f64>,
    ranked: Vec<Ranked>,
}

impl ZSet {
    /// How many members this set holds.
    pub fn len(&self) -> usize {
        self.scores.len()
    }

    pub fn is_empty(&self) -> bool {
        self.scores.is_empty()
    }

    /// Set `member`'s score, inserting it if new. Returns `true` if `member`
    /// was not previously a member of the set (Redis's "newly added" count for
    /// `ZADD`), `false` if it already existed and only its score changed.
    ///
    /// An existing member's old `Ranked` entry has to be located and removed
    /// before the new one is inserted at its (possibly different) sorted
    /// position — a plain overwrite would leave a stale entry from before the
    /// score changed sitting in the wrong place in `ranked`.
    pub fn insert(&mut self, member: String, score: f64) -> bool {
        if let Some(&old_score) = self.scores.get(&member) {
            let old_pos = self.ranked.partition_point(|r| {
                cmp_pairs(r.score, &r.member, old_score, &member) == Ordering::Less
            });
            self.ranked.remove(old_pos);
            let new_pos = self.ranked.partition_point(|r| {
                cmp_pairs(r.score, &r.member, score, &member) == Ordering::Less
            });
            self.ranked.insert(
                new_pos,
                Ranked {
                    score,
                    member: member.clone(),
                },
            );
            self.scores.insert(member, score);
            false
        } else {
            let pos = self.ranked.partition_point(|r| {
                cmp_pairs(r.score, &r.member, score, &member) == Ordering::Less
            });
            self.ranked.insert(
                pos,
                Ranked {
                    score,
                    member: member.clone(),
                },
            );
            self.scores.insert(member, score);
            true
        }
    }

    /// `member`'s current score, or `None` if it isn't in the set.
    pub fn score(&self, member: &str) -> Option<f64> {
        self.scores.get(member).copied()
    }

    /// `member`'s 0-based rank in ascending score order, or `None` if it isn't
    /// in the set. Binary-searches `ranked` for the member's known score
    /// (from `scores`) rather than scanning, since `ranked` is always sorted.
    pub fn rank(&self, member: &str) -> Option<usize> {
        let score = *self.scores.get(member)?;
        let pos = self
            .ranked
            .partition_point(|r| cmp_pairs(r.score, &r.member, score, member) == Ordering::Less);
        // `pos` is the first entry that is not < (score, member); since the
        // member is known to be present, that entry must *be* it.
        debug_assert_eq!(self.ranked[pos].member, member);
        Some(pos)
    }

    /// The `(member, score)` pairs whose ranks fall in `[start, stop]`
    /// (inclusive, ascending order), with Redis's negative-index and
    /// out-of-range clamping rules: `-1` is the last rank, an out-of-range
    /// bound clamps to the nearest valid one, and an inverted span (after
    /// clamping) yields an empty result. Same shape as
    /// `commands::lists::normalize_range` for `LRANGE`, applied here to rank
    /// instead of list index (kept as its own copy rather than shared, like
    /// each command group's own small helpers elsewhere in the codebase).
    pub fn range(&self, start: i64, stop: i64) -> Vec<(&str, f64)> {
        let len = self.ranked.len() as i64;
        let from = if start < 0 {
            (len + start).max(0)
        } else {
            start
        };
        let to = if stop < 0 { len + stop } else { stop };
        let to = to.min(len - 1);
        if from > to {
            return Vec::new();
        }
        self.ranked[from as usize..=to as usize]
            .iter()
            .map(|r| (r.member.as_str(), r.score))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_reports_new_vs_updated() {
        let mut z = ZSet::default();
        assert!(z.insert("a".to_string(), 1.0));
        assert!(!z.insert("a".to_string(), 2.0), "re-adding is an update");
        assert_eq!(z.score("a"), Some(2.0));
        assert_eq!(z.len(), 1);
    }

    #[test]
    fn members_are_ordered_by_score_then_by_name() {
        let mut z = ZSet::default();
        z.insert("charlie".to_string(), 3.0);
        z.insert("alice".to_string(), 1.0);
        z.insert("bob".to_string(), 1.0); // ties alice on score
        let names: Vec<&str> = z.range(0, -1).into_iter().map(|(m, _)| m).collect();
        assert_eq!(names, vec!["alice", "bob", "charlie"]);
    }

    #[test]
    fn updating_a_score_moves_the_member_to_its_new_position() {
        let mut z = ZSet::default();
        z.insert("a".to_string(), 1.0);
        z.insert("b".to_string(), 2.0);
        z.insert("c".to_string(), 3.0);
        // Move "a" past "c".
        z.insert("a".to_string(), 5.0);
        let names: Vec<&str> = z.range(0, -1).into_iter().map(|(m, _)| m).collect();
        assert_eq!(names, vec!["b", "c", "a"]);
    }

    #[test]
    fn rank_reflects_position_and_missing_members_are_none() {
        let mut z = ZSet::default();
        z.insert("a".to_string(), 1.0);
        z.insert("b".to_string(), 2.0);
        assert_eq!(z.rank("a"), Some(0));
        assert_eq!(z.rank("b"), Some(1));
        assert_eq!(z.rank("nope"), None);
    }

    #[test]
    fn score_of_a_missing_member_is_none() {
        let z = ZSet::default();
        assert_eq!(z.score("nope"), None);
    }

    #[test]
    fn range_supports_negative_indices_like_lrange() {
        let mut z = ZSet::default();
        for (m, s) in [("a", 1.0), ("b", 2.0), ("c", 3.0), ("d", 4.0)] {
            z.insert(m.to_string(), s);
        }
        let names = |v: Vec<(&str, f64)>| {
            v.into_iter()
                .map(|(m, _)| m.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(z.range(0, -1)), vec!["a", "b", "c", "d"]);
        assert_eq!(names(z.range(-2, -1)), vec!["c", "d"]);
        assert_eq!(names(z.range(1, 2)), vec!["b", "c"]);
        // Out-of-range end clamps rather than erroring.
        assert_eq!(names(z.range(0, 100)), vec!["a", "b", "c", "d"]);
        // An inverted span (after clamping) is empty.
        assert_eq!(names(z.range(3, 1)), Vec::<String>::new());
    }

    #[test]
    fn range_on_an_empty_set_is_empty() {
        let z = ZSet::default();
        assert_eq!(z.range(0, -1), Vec::new());
    }
}
