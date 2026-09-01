//! Command implementations, pulled out of the ever-growing `lib.rs` one
//! self-contained group at a time.
//!
//! `lib.rs` started as the whole server and, command by command, grew past
//! four thousand lines with every value type's handlers, the connection loop,
//! and the RDB/replication/pub-sub plumbing all in one file. `persistence` is
//! the first slice out: `SAVE`, `BGSAVE`, `WAIT`, and the snapshot helpers
//! they share, which only ever talk to a [`crate::Server`] and the `rdb`
//! module — nothing about them depends on any other command group. `lists`
//! and `hashes` follow the same pattern for `RPUSH`/`LPUSH`/`RPOP`/`LPOP`/
//! `LLEN`/`LRANGE` and `HSET`/`HGET`/`HGETALL`/`HDEL`. `streams` is the
//! biggest group yet — `XADD`/`XLEN`/`XRANGE`/`XREVRANGE`/`XDEL`/`XTRIM`/
//! `XREAD` plus the async blocking `XREAD ... BLOCK` path and its
//! `XReadRequest` request type. `pubsub` follows — `SUBSCRIBE`/`UNSUBSCRIBE`/
//! `PUBLISH`, the routing table, and the subscribe-mode connection loop.
//! `transactions` followed — `MULTI`/`EXEC`/`DISCARD`/`WATCH`/`UNWATCH` and the
//! per-connection `Session` they dispatch through, the first group whose state
//! is private to one connection rather than a crate-wide registry. With it
//! out, `lib.rs` settled into wiring: the connection loop and the
//! `process_command` dispatch table. `sorted_sets` is the first genuinely new
//! command group added after the split rather than lifted out of `lib.rs` —
//! `ZADD`/`ZSCORE`/`ZRANK`/`ZRANGE`, following the same pattern from day one.

pub(crate) mod hashes;
pub(crate) mod lists;
pub(crate) mod persistence;
pub(crate) mod pubsub;
pub(crate) mod sorted_sets;
pub(crate) mod streams;
pub(crate) mod transactions;
