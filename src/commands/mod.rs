//! Command implementations, pulled out of the ever-growing `lib.rs` one
//! self-contained group at a time.
//!
//! `lib.rs` started as the whole server and, command by command, grew past
//! four thousand lines with every value type's handlers, the connection loop,
//! and the RDB/replication/pub-sub plumbing all in one file. `persistence` is
//! the first slice out: `SAVE`, `BGSAVE`, `WAIT`, and the snapshot helpers
//! they share, which only ever talk to a [`crate::Server`] and the `rdb`
//! module — nothing about them depends on any other command group. Future
//! sessions can keep peeling groups out the same way (strings/lists/hashes,
//! streams, pub/sub, transactions) until `lib.rs` is just wiring: the
//! connection loop and the `process_command` dispatch table.

pub(crate) mod persistence;
