//! `MULTI` / `EXEC` / `DISCARD` / `WATCH` / `UNWATCH` — the transaction
//! dispatcher and its per-connection [`Session`] (see the module doc on
//! [`crate::commands`] for why this file exists).
//!
//! Every group moved so far carried *shared* state — a registry every
//! connection reaches through the same [`crate::Server`] handle. `Session` is
//! the opposite: it belongs to exactly one connection, lives on that
//! connection's stack in `lib.rs`'s `handle_conn`, and is threaded through by
//! `&mut` rather than cloned or locked. That flips the usual privacy
//! direction from every earlier split. `commands::streams`/`pubsub` only ever
//! needed to reach *up* into `lib.rs`'s private items (free, since they're
//! descendants of the crate root); this module needs `lib.rs` to reach back
//! *down* into `Session` — construct one, and read whether it's mid-transaction
//! — which is not free. That's why [`Session`] itself and its `in_multi` field
//! are `pub(crate)` while `queue`/`dirty`/`watched` stay private, reached only
//! from the methods in this file. Everything else reaches the rest of the
//! crate through `crate::` paths: [`crate::Server`], the [`crate::resp`]
//! module, [`crate::process_command`], and the small shared helpers
//! (`crate::command_name`, `crate::unpack_bulk_str`, `crate::wrong_args`) that
//! stay in `lib.rs` because other command groups use them too.

use crate::resp::Value;
use crate::{command_args, command_name, process_command, unpack_bulk_str, wrong_args, Server};

/// Per-connection transaction state.
///
/// Everything the server has held until now lived in the shared
/// [`crate::Server`] — one keyspace every client touches. A transaction is
/// the first thing that is *private to one client*: the list of commands
/// queued since `MULTI` belongs to the connection that typed them, not to the
/// map. So this struct lives on the stack of `lib.rs`'s `handle_conn`, one per
/// connection, and is threaded by `&mut` into command dispatch. It is
/// deliberately not `Clone` and not shared — no `Arc`, no `Mutex` — because no
/// other task ever needs to see it.
#[derive(Default)]
pub(crate) struct Session {
    /// Are we between `MULTI` and `EXEC`/`DISCARD`? While true, ordinary
    /// commands are queued rather than run. `handle_conn` reads this directly
    /// (to decide whether `SUBSCRIBE`/a blocking `XREAD` may run at all), so
    /// it's the one field exposed outside this module.
    pub(crate) in_multi: bool,
    /// The commands queued since `MULTI`, replayed in order by `EXEC`.
    queue: Vec<Value>,
    /// A queued command was rejected at queue time (unknown command). Redis
    /// remembers this and makes the eventual `EXEC` abort the *whole*
    /// transaction rather than run a partial one.
    dirty: bool,
    /// Keys this connection is `WATCH`ing, each paired with its version at the
    /// moment it was watched. `EXEC` aborts if any of these has changed since.
    /// Empty means the transaction isn't guarded by optimistic locking.
    watched: Vec<(String, u64)>,
}

impl Session {
    /// Begin a transaction. Nested `MULTI` is an error in Redis (and would be
    /// ambiguous — which `EXEC` closes which?), but it does not abort the
    /// transaction already open.
    fn multi(&mut self) -> Value {
        if self.in_multi {
            return Value::Error("ERR MULTI calls can not be nested".to_string());
        }
        self.in_multi = true;
        Value::SimpleString("OK".to_string())
    }

    /// Queue one command to run at `EXEC`, or taint the transaction if it can
    /// never run. Redis validates enough at queue time to reject an unknown
    /// command up front (returning an error *and* setting the abort flag);
    /// arity and type errors are left to surface inside the `EXEC` reply array,
    /// which matches Redis for those runtime failures.
    fn queue(&mut self, value: Value) -> Value {
        match command_name(&value) {
            Some(name) if is_known_command(&name) => {
                self.queue.push(value);
                Value::SimpleString("QUEUED".to_string())
            }
            Some(name) => {
                self.dirty = true;
                Value::Error(format!("ERR unknown command '{}'", name))
            }
            None => {
                self.dirty = true;
                Value::Error("ERR unknown command".to_string())
            }
        }
    }

    /// Mark keys for optimistic locking. `WATCH` snapshots each key's current
    /// version; if any of them is written before `EXEC`, the transaction aborts.
    /// `WATCH` isn't allowed once a transaction is open (it would be pointless —
    /// the check happens at `EXEC`, and there's nothing to guard mid-queue).
    fn watch(&mut self, args: &[Value], server: &Server) -> Value {
        if self.in_multi {
            return Value::Error("ERR WATCH inside MULTI is not allowed".to_string());
        }
        if args.is_empty() {
            return wrong_args("watch");
        }
        for arg in args {
            let key = match unpack_bulk_str(arg) {
                Ok(k) => k,
                Err(e) => return Value::Error(format!("ERR {}", e)),
            };
            let version = server.version_of(&key);
            self.watched.push((key, version));
        }
        Value::SimpleString("OK".to_string())
    }

    /// Forget every watched key, so the next `EXEC` is unguarded. Always OK,
    /// even with nothing watched.
    fn unwatch(&mut self) -> Value {
        self.watched.clear();
        Value::SimpleString("OK".to_string())
    }

    /// Have any watched keys changed since they were watched? A single mismatch
    /// (or a key written for the first time, moving it off version 0) means a
    /// concurrent writer got in, so `EXEC` must abort.
    fn watch_conflict(&self, server: &Server) -> bool {
        self.watched
            .iter()
            .any(|(key, watched_at)| server.version_of(key) != *watched_at)
    }

    /// Run the queued commands as a batch and end the transaction.
    ///
    /// The transaction ends no matter what, so state is snapshotted and cleared
    /// first. If any queued command was rejected, nothing runs and `EXEC`
    /// reports `EXECABORT`. If a `WATCH`ed key changed underneath us, `EXEC`
    /// aborts by replying with the nil array and running nothing. Otherwise
    /// every queued command runs in order through the normal
    /// [`crate::process_command`] path — so writes still replicate — and their
    /// replies come back as one array. A runtime error (e.g. `WRONGTYPE`) is
    /// just one element of that array; it does not stop the others, because
    /// Redis transactions are batched, not rolled back.
    fn exec(&mut self, server: &Server) -> Value {
        if !self.in_multi {
            return Value::Error("ERR EXEC without MULTI".to_string());
        }
        let queued = std::mem::take(&mut self.queue);
        let dirty = self.dirty;
        let conflict = self.watch_conflict(server);
        self.reset();

        if dirty {
            return Value::Error(
                "EXECABORT Transaction discarded because of previous errors.".to_string(),
            );
        }
        if conflict {
            return Value::NullArray;
        }
        let results = queued
            .into_iter()
            .map(|cmd| process_command(cmd, server))
            .collect();
        Value::Array(results)
    }

    /// Throw away a transaction without running it.
    fn discard(&mut self) -> Value {
        if !self.in_multi {
            return Value::Error("ERR DISCARD without MULTI".to_string());
        }
        self.reset();
        Value::SimpleString("OK".to_string())
    }

    /// Return to the no-transaction state, dropping any queued commands and
    /// clearing the watch set — Redis unwatches after every `EXEC`/`DISCARD`.
    fn reset(&mut self) {
        self.in_multi = false;
        self.queue.clear();
        self.dirty = false;
        self.watched.clear();
    }
}

/// Dispatch one request in the context of this connection's [`Session`].
///
/// The transaction controls (`MULTI`/`EXEC`/`DISCARD`) always act immediately.
/// Everything else runs right away *unless* a transaction is open, in which case
/// it is queued for later. This is the one place per-connection state and the
/// shared server meet.
pub(crate) fn handle_command(value: Value, server: &Server, session: &mut Session) -> Value {
    match command_name(&value).as_deref() {
        Some("multi") => session.multi(),
        Some("exec") => session.exec(server),
        Some("discard") => session.discard(),
        Some("watch") => session.watch(command_args(&value), server),
        Some("unwatch") => session.unwatch(),
        _ if session.in_multi => session.queue(value),
        _ => process_command(value, server),
    }
}

/// Is `name` (already lowercased) a command this server dispatches? Used to
/// reject an unknown command at `MULTI`-queue time. Kept in lock-step with the
/// dispatch table in [`crate::process_command`]; a command missing here would
/// be queued and then fail at `EXEC` instead of being caught up front.
fn is_known_command(name: &str) -> bool {
    matches!(
        name,
        "ping"
            | "echo"
            | "set"
            | "get"
            | "del"
            | "expire"
            | "ttl"
            | "persist"
            | "type"
            | "rpush"
            | "lpush"
            | "rpop"
            | "lpop"
            | "llen"
            | "lrange"
            | "hset"
            | "hget"
            | "hgetall"
            | "hdel"
            | "zadd"
            | "zscore"
            | "zrank"
            | "zrange"
            | "save"
            | "bgsave"
            | "replconf"
            | "wait"
            | "multi"
            | "exec"
            | "discard"
            | "watch"
            | "unwatch"
            | "publish"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{get, Store};
    use bytes::Bytes;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
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

    // A command frame built from a command name plus string arguments, the
    // shape `handle_command`/`process_command` expect.
    fn cmd(parts: &[&str]) -> Value {
        Value::Array(parts.iter().map(|p| bulk(p)).collect())
    }

    #[test]
    fn multi_opens_a_transaction_and_queues_following_commands() {
        let srv = server();
        let mut s = Session::default();
        assert_eq!(
            handle_command(cmd(&["MULTI"]), &srv, &mut s),
            Value::SimpleString("OK".to_string())
        );
        // Commands after MULTI are answered +QUEUED, not run.
        assert_eq!(
            handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s),
            Value::SimpleString("QUEUED".to_string())
        );
        // The write has not touched the store yet.
        assert_eq!(get(&[bulk("k")], &srv.store), Value::Null);
    }

    #[test]
    fn exec_runs_the_queue_in_order_and_returns_an_array_of_replies() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s);
        handle_command(cmd(&["GET", "k"]), &srv, &mut s);

        let reply = handle_command(cmd(&["EXEC"]), &srv, &mut s);
        assert_eq!(
            reply,
            Value::Array(vec![
                Value::SimpleString("OK".to_string()),
                Value::BulkString("v".to_string()),
            ])
        );
        // The transaction is closed: a plain command runs immediately again.
        assert_eq!(
            handle_command(cmd(&["GET", "k"]), &srv, &mut s),
            Value::BulkString("v".to_string())
        );
    }

    #[test]
    fn discard_drops_the_queue_without_running_it() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s);
        assert_eq!(
            handle_command(cmd(&["DISCARD"]), &srv, &mut s),
            Value::SimpleString("OK".to_string())
        );
        // Nothing ran, and we are no longer in a transaction.
        assert_eq!(get(&[bulk("k")], &srv.store), Value::Null);
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut s),
            Value::Error("ERR EXEC without MULTI".to_string())
        );
    }

    #[test]
    fn exec_and_discard_without_multi_are_errors() {
        let srv = server();
        let mut s = Session::default();
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut s),
            Value::Error("ERR EXEC without MULTI".to_string())
        );
        assert_eq!(
            handle_command(cmd(&["DISCARD"]), &srv, &mut s),
            Value::Error("ERR DISCARD without MULTI".to_string())
        );
    }

    #[test]
    fn nested_multi_is_rejected_but_keeps_the_transaction_open() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        assert_eq!(
            handle_command(cmd(&["MULTI"]), &srv, &mut s),
            Value::Error("ERR MULTI calls can not be nested".to_string())
        );
        // Still queuing after the rejected nested MULTI.
        assert_eq!(
            handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s),
            Value::SimpleString("QUEUED".to_string())
        );
    }

    #[test]
    fn an_unknown_queued_command_aborts_the_whole_transaction() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s);
        // A bogus command is rejected at queue time and taints the transaction.
        assert!(matches!(
            handle_command(cmd(&["NOPE"]), &srv, &mut s),
            Value::Error(_)
        ));
        // EXEC now aborts wholesale; the earlier SET must NOT have run.
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut s),
            Value::Error("EXECABORT Transaction discarded because of previous errors.".to_string())
        );
        assert_eq!(get(&[bulk("k")], &srv.store), Value::Null);
    }

    #[test]
    fn a_runtime_error_inside_exec_does_not_stop_the_other_commands() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s);
        // LPUSH on a string key is a WRONGTYPE error at run time, not queue time.
        handle_command(cmd(&["LPUSH", "k", "x"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k2", "v2"]), &srv, &mut s);

        let reply = handle_command(cmd(&["EXEC"]), &srv, &mut s);
        match reply {
            Value::Array(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Value::SimpleString("OK".to_string()));
                assert!(matches!(items[1], Value::Error(_)));
                assert_eq!(items[2], Value::SimpleString("OK".to_string()));
            }
            other => panic!("expected an array reply, got {other:?}"),
        }
        // The command after the error still ran.
        assert_eq!(
            get(&[bulk("k2")], &srv.store),
            Value::BulkString("v2".to_string())
        );
    }

    #[test]
    fn queued_writes_propagate_to_replicas_when_exec_runs() {
        let srv = server();
        let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
        srv.replicas.lock().unwrap().push(tx);
        let mut s = Session::default();

        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k", "v"]), &srv, &mut s);
        // Queuing alone streams nothing to the replica.
        assert!(rx.try_recv().is_err());

        handle_command(cmd(&["EXEC"]), &srv, &mut s);
        // Running the queue on EXEC replicates the write like any other.
        let set = cmd(&["SET", "k", "v"]);
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from(set.serialize().into_bytes())
        );
    }

    #[test]
    fn watch_lets_exec_run_when_no_watched_key_changed() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["SET", "k", "1"]), &srv, &mut s);
        assert_eq!(
            handle_command(cmd(&["WATCH", "k"]), &srv, &mut s),
            Value::SimpleString("OK".to_string())
        );
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        handle_command(cmd(&["SET", "k", "2"]), &srv, &mut s);
        // Nobody else touched k, so EXEC goes through.
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut s),
            Value::Array(vec![Value::SimpleString("OK".to_string())])
        );
        assert_eq!(
            get(&[bulk("k")], &srv.store),
            Value::BulkString("2".to_string())
        );
    }

    #[test]
    fn exec_aborts_with_nil_when_a_watched_key_changed() {
        let srv = server();
        let mut watcher = Session::default();
        handle_command(cmd(&["SET", "k", "1"]), &srv, &mut watcher);
        handle_command(cmd(&["WATCH", "k"]), &srv, &mut watcher);
        handle_command(cmd(&["MULTI"]), &srv, &mut watcher);
        handle_command(cmd(&["SET", "k", "from-watcher"]), &srv, &mut watcher);

        // A different connection writes k while the transaction is queued.
        let mut other = Session::default();
        handle_command(cmd(&["SET", "k", "from-other"]), &srv, &mut other);

        // EXEC sees the version moved and aborts: nil array, nothing run.
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut watcher),
            Value::NullArray
        );
        assert_eq!(
            get(&[bulk("k")], &srv.store),
            Value::BulkString("from-other".to_string())
        );
    }

    #[test]
    fn watching_a_missing_key_that_then_appears_aborts_exec() {
        let srv = server();
        let mut watcher = Session::default();
        // k does not exist yet -> watched at version 0.
        handle_command(cmd(&["WATCH", "k"]), &srv, &mut watcher);
        handle_command(cmd(&["MULTI"]), &srv, &mut watcher);
        handle_command(cmd(&["GET", "k"]), &srv, &mut watcher);

        let mut other = Session::default();
        handle_command(cmd(&["SET", "k", "v"]), &srv, &mut other);

        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut watcher),
            Value::NullArray
        );
    }

    #[test]
    fn unwatch_clears_the_guard_so_exec_runs() {
        let srv = server();
        let mut watcher = Session::default();
        handle_command(cmd(&["SET", "k", "1"]), &srv, &mut watcher);
        handle_command(cmd(&["WATCH", "k"]), &srv, &mut watcher);
        assert_eq!(
            handle_command(cmd(&["UNWATCH"]), &srv, &mut watcher),
            Value::SimpleString("OK".to_string())
        );

        let mut other = Session::default();
        handle_command(cmd(&["SET", "k", "2"]), &srv, &mut other);

        handle_command(cmd(&["MULTI"]), &srv, &mut watcher);
        handle_command(cmd(&["SET", "k", "3"]), &srv, &mut watcher);
        // Watch was cleared, so the concurrent write doesn't abort us.
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut watcher),
            Value::Array(vec![Value::SimpleString("OK".to_string())])
        );
    }

    #[test]
    fn watch_is_rejected_once_a_transaction_is_open() {
        let srv = server();
        let mut s = Session::default();
        handle_command(cmd(&["MULTI"]), &srv, &mut s);
        assert_eq!(
            handle_command(cmd(&["WATCH", "k"]), &srv, &mut s),
            Value::Error("ERR WATCH inside MULTI is not allowed".to_string())
        );
    }

    #[test]
    fn exec_unwatches_so_a_later_transaction_is_unguarded() {
        let srv = server();
        let mut watcher = Session::default();
        handle_command(cmd(&["SET", "k", "1"]), &srv, &mut watcher);
        handle_command(cmd(&["WATCH", "k"]), &srv, &mut watcher);
        handle_command(cmd(&["MULTI"]), &srv, &mut watcher);
        handle_command(cmd(&["EXEC"]), &srv, &mut watcher);

        // A concurrent write to k after the first EXEC must not haunt the second
        // transaction — EXEC should have cleared the watch set.
        let mut other = Session::default();
        handle_command(cmd(&["SET", "k", "2"]), &srv, &mut other);

        handle_command(cmd(&["MULTI"]), &srv, &mut watcher);
        handle_command(cmd(&["SET", "k", "3"]), &srv, &mut watcher);
        assert_eq!(
            handle_command(cmd(&["EXEC"]), &srv, &mut watcher),
            Value::Array(vec![Value::SimpleString("OK".to_string())])
        );
    }

    #[test]
    fn del_bumps_the_version_of_every_key_it_removes() {
        let srv = server();
        handle_command(cmd(&["SET", "a", "1"]), &srv, &mut Session::default());
        handle_command(cmd(&["SET", "b", "1"]), &srv, &mut Session::default());
        let (va, vb) = (srv.version_of("a"), srv.version_of("b"));

        handle_command(cmd(&["DEL", "a", "b"]), &srv, &mut Session::default());
        assert!(srv.version_of("a") > va);
        assert!(srv.version_of("b") > vb);
    }

    // This test used to live in `lib.rs` alongside `is_write_command`; it
    // moved here with `is_known_command`, the thing it's really about.
    #[test]
    fn publish_is_a_known_but_non_write_command() {
        // Queueable inside MULTI, but never streamed to replicas.
        assert!(is_known_command("publish"));
        assert!(!crate::is_write_command("publish"));
    }
}
