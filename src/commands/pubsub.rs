//! `SUBSCRIBE` / `UNSUBSCRIBE` / `PUBLISH` — the pub/sub routing table, the
//! per-connection subscribe-mode loop, and the client-id counter that keys it
//! (see the module doc on [`crate::commands`] for why this file exists).
//!
//! This is the second group (after `streams`) to reach into [`crate::Server`]'s
//! private fields directly (`server.subscribers`) rather than only calling free
//! functions — same descendant-of-the-defining-module privacy rule as always,
//! just exercised on a different field. It's also the first group to bring an
//! *async connection-mode handler* along with its command logic: `SUBSCRIBE`
//! doesn't just answer once, it turns the connection into something the server
//! can push to at any time, so [`serve_subscriber`] — the loop `lib.rs`'s
//! `handle_conn` hands the whole socket to — lives here too, not just the
//! `PUBLISH` side. Everything else reaches the rest of the crate through
//! `crate::` paths: [`crate::Server`], the [`crate::resp`] module, and the
//! small shared helpers (`crate::command_args`, `crate::command_name`,
//! `crate::unpack_bulk_str`, `crate::wrong_args`) that stay in `lib.rs`
//! because other command groups use them too.

use crate::resp::{self, Value};
use crate::{command_args, command_name, unpack_bulk_str, wrong_args, Server};
use anyhow::Result;
use bytes::{Buf, Bytes, BytesMut};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// The pub/sub routing table: for each channel name, the set of connections
/// currently subscribed to it, keyed by a per-connection id so a single
/// connection can be removed cleanly when it unsubscribes or disconnects.
///
/// Each subscriber is represented by the *sending* half of an `mpsc` channel —
/// the exact same trick as the replica registry (`crate::Replicas`). `PUBLISH`
/// runs inside the *publisher's* task: it looks up the channel here and pushes
/// the message bytes into every subscriber's sender. Each subscriber's own
/// task (see [`serve_subscriber`]) owns the matching receiver and writes the
/// bytes down its socket. `Arc<Mutex<..>>` because any connection may publish
/// to a channel any other connection is subscribed to.
pub(crate) type Subscribers =
    Arc<Mutex<HashMap<String, HashMap<u64, mpsc::UnboundedSender<Bytes>>>>>;

/// Hands out a unique id to each connection so it can be tracked in the
/// pub/sub routing table and removed again by id. A plain monotonic counter —
/// the value only has to be unique for the life of the process, not
/// meaningful.
pub(crate) static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

/// Build the RESP frame Redis pushes to a subscriber when a message arrives:
/// the three-element array `["message", channel, payload]`.
fn message_frame(channel: &str, payload: &str) -> Value {
    Value::Array(vec![
        Value::BulkString("message".to_string()),
        Value::BulkString(channel.to_string()),
        Value::BulkString(payload.to_string()),
    ])
}

/// Build a subscribe/unsubscribe *confirmation* frame: `[kind, channel, count]`,
/// where `count` is how many channels the connection is subscribed to *after*
/// this action. `channel` is `None` only for an `UNSUBSCRIBE` that had nothing
/// to unsubscribe, which Redis answers with a nil channel field.
fn subscription_reply(kind: &str, channel: Option<&str>, count: usize) -> Value {
    let channel_field = match channel {
        Some(c) => Value::BulkString(c.to_string()),
        None => Value::Null,
    };
    Value::Array(vec![
        Value::BulkString(kind.to_string()),
        channel_field,
        Value::Integer(count as i64),
    ])
}

/// `PUBLISH channel message` — deliver `message` to every connection subscribed
/// to `channel`, and reply with how many received it.
///
/// This runs in the *publisher's* task. For each subscriber we push the encoded
/// message frame into that subscriber's channel; the subscriber's own task does
/// the socket write. A send fails only if that subscriber's task has already
/// gone, so `retain` delivers and prunes dead subscribers in one pass — the same
/// pattern `crate::propagate` uses for replicas. An empty channel is removed so
/// the map doesn't accumulate keys for channels no one listens on any more.
pub(crate) fn publish(args: &[Value], server: &Server) -> Value {
    if args.len() != 2 {
        return wrong_args("publish");
    }
    let channel = match unpack_bulk_str(&args[0]) {
        Ok(c) => c,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let message = match unpack_bulk_str(&args[1]) {
        Ok(m) => m,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };

    let frame = Bytes::from(message_frame(&channel, &message).serialize().into_bytes());
    let mut table = server.subscribers.lock().unwrap();
    let mut delivered: i64 = 0;
    if let Some(subs) = table.get_mut(&channel) {
        subs.retain(|_id, tx| match tx.send(frame.clone()) {
            Ok(()) => {
                delivered += 1;
                true
            }
            Err(_) => false,
        });
        if subs.is_empty() {
            table.remove(&channel);
        }
    }
    Value::Integer(delivered)
}

/// Add `client_id` (reachable via `tx`) to each named channel's subscriber set,
/// recording the channels locally in `subscribed`, and return the confirmation
/// frames — one per channel, as Redis sends. Re-subscribing to a channel already
/// held is harmless: the set membership is idempotent and the confirmation still
/// goes out with the current count.
///
/// The return is the raw wire bytes rather than a `Value`, because `SUBSCRIBE`
/// with several channels replies with several *independent* top-level frames
/// back to back — not one array wrapping them. Concatenating each frame's own
/// serialization is exactly that; a `Value::Array` would prepend a spurious
/// `*N\r\n` header and turn it into an array of arrays.
fn do_subscribe(
    args: &[Value],
    server: &Server,
    client_id: u64,
    tx: &mpsc::UnboundedSender<Bytes>,
    subscribed: &mut HashSet<String>,
) -> String {
    if args.is_empty() {
        return wrong_args("subscribe").serialize();
    }
    let mut out = String::new();
    for arg in args {
        let channel = match unpack_bulk_str(arg) {
            Ok(c) => c,
            Err(e) => return Value::Error(format!("ERR {}", e)).serialize(),
        };
        server
            .subscribers
            .lock()
            .unwrap()
            .entry(channel.clone())
            .or_default()
            .insert(client_id, tx.clone());
        subscribed.insert(channel.clone());
        out.push_str(
            &subscription_reply("subscribe", Some(&channel), subscribed.len()).serialize(),
        );
    }
    out
}

/// Remove `client_id` from the named channels (or from *all* the connection's
/// channels when `args` is empty, which is Redis's "unsubscribe from
/// everything"), updating `subscribed`, and return one confirmation frame per
/// channel as raw wire bytes (see [`do_subscribe`] on why not a `Value`). An
/// `UNSUBSCRIBE` with nothing to remove still gets a single reply with a nil
/// channel and count 0, matching Redis.
fn do_unsubscribe(
    args: &[Value],
    server: &Server,
    client_id: u64,
    subscribed: &mut HashSet<String>,
) -> String {
    // Which channels to drop: the named ones, or every one we hold if none named.
    let targets: Vec<String> = if args.is_empty() {
        subscribed.iter().cloned().collect()
    } else {
        let mut names = Vec::with_capacity(args.len());
        for arg in args {
            match unpack_bulk_str(arg) {
                Ok(c) => names.push(c),
                Err(e) => return Value::Error(format!("ERR {}", e)).serialize(),
            }
        }
        names
    };

    if targets.is_empty() {
        return subscription_reply("unsubscribe", None, 0).serialize();
    }

    let mut out = String::new();
    for channel in targets {
        remove_subscriber(server, client_id, &channel);
        subscribed.remove(&channel);
        out.push_str(
            &subscription_reply("unsubscribe", Some(&channel), subscribed.len()).serialize(),
        );
    }
    out
}

/// Drop `client_id` from one channel's subscriber set, removing the channel
/// entirely once it has no subscribers left.
fn remove_subscriber(server: &Server, client_id: u64, channel: &str) {
    let mut table = server.subscribers.lock().unwrap();
    if let Some(subs) = table.get_mut(channel) {
        subs.remove(&client_id);
        if subs.is_empty() {
            table.remove(channel);
        }
    }
}

/// Serve a connection that has entered pub/sub mode.
///
/// While subscribed, a connection is half server-push, half request/response:
/// the server may send it a message at any moment (someone else `PUBLISH`ed),
/// and the client may still send `SUBSCRIBE`/`UNSUBSCRIBE`/`PING`. Handling both
/// at once is why this can't live in the plain read-reply loop: we `split` the
/// socket and `select!` between a message arriving on our channel and a command
/// arriving on the wire.
///
/// Returns when the client has unsubscribed from everything (control goes back
/// to the normal loop) or the socket closes. Either way, every remaining
/// subscription for this connection is torn out of the shared table on the way
/// out so a vanished client leaves nothing behind.
pub(crate) async fn serve_subscriber(
    stream: &mut TcpStream,
    buffer: &mut BytesMut,
    server: &Server,
    client_id: u64,
    first: Value,
) -> Result<()> {
    // Our end of the push channel. The sending halves handed to the routing
    // table are clones; this receiver is the one socket-writer for them all.
    // Holding `tx` here for the whole loop keeps the channel open even after we
    // hand clones out, so `rx.recv()` never ends just because a `PUBLISH`er
    // pruned a stale clone.
    let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
    let mut subscribed: HashSet<String> = HashSet::new();

    let (mut read_half, mut write_half) = stream.split();

    // The command that got us here is the first SUBSCRIBE; run it before looping.
    let reply = do_subscribe(
        command_args(&first),
        server,
        client_id,
        &tx,
        &mut subscribed,
    );
    write_half.write_all(reply.as_bytes()).await?;
    write_half.flush().await?;

    let result = 'conn: loop {
        // Drain any complete commands already sitting in the buffer (a client
        // may have pipelined several) before waiting on the socket again.
        loop {
            match resp::parse_message(buffer) {
                Ok(Some((value, consumed))) => {
                    buffer.advance(consumed);
                    let reply = handle_sub_command(value, server, client_id, &tx, &mut subscribed);
                    write_half.write_all(reply.as_bytes()).await?;
                    write_half.flush().await?;
                    // Unsubscribed from the last channel: leave pub/sub mode.
                    if subscribed.is_empty() {
                        break 'conn Ok(());
                    }
                }
                Ok(None) => break, // need more bytes
                Err(proto) => {
                    // Garbage on the wire: reply once and close, as the main loop does.
                    let reply = Value::Error(format!("ERR {}", proto)).serialize();
                    let _ = write_half.write_all(reply.as_bytes()).await;
                    let _ = write_half.flush().await;
                    break 'conn Ok(());
                }
            }
        }

        tokio::select! {
            // A message someone else published on a channel we're subscribed to.
            pushed = rx.recv() => match pushed {
                Some(bytes) => {
                    write_half.write_all(&bytes).await?;
                    write_half.flush().await?;
                }
                None => break 'conn Ok(()), // every sender gone (can't happen while `tx` lives)
            },
            // More bytes from the client — loop back up to parse them.
            read = read_half.read_buf(buffer) => match read {
                Ok(0) => break 'conn Ok(()), // client hung up
                Ok(_) => {}
                Err(e) => break 'conn Err(e.into()),
            },
        }
    };

    // Whatever ended us, make sure this connection owns no subscriptions any
    // more — otherwise a `PUBLISH` would try to push to a socket that's gone.
    for channel in &subscribed {
        remove_subscriber(server, client_id, channel);
    }
    result
}

/// Dispatch one command received while the connection is in pub/sub mode.
///
/// RESP2 only permits a subscribed client to (un)subscribe or `PING`; anything
/// else is refused with Redis's own error, rather than run. This keeps a
/// subscribed connection from, say, issuing a blocking read that would strand
/// the pushed-message stream.
fn handle_sub_command(
    value: Value,
    server: &Server,
    client_id: u64,
    tx: &mpsc::UnboundedSender<Bytes>,
    subscribed: &mut HashSet<String>,
) -> String {
    match command_name(&value).as_deref() {
        Some("subscribe") => do_subscribe(command_args(&value), server, client_id, tx, subscribed),
        Some("unsubscribe") => do_unsubscribe(command_args(&value), server, client_id, subscribed),
        Some("ping") => Value::SimpleString("PONG".to_string()).serialize(),
        Some(other) => Value::Error(format!(
            "ERR Can't execute '{}': only (P)SUBSCRIBE / (P)UNSUBSCRIBE / PING / QUIT are allowed in this context",
            other
        ))
        .serialize(),
        None => Value::Error("ERR unknown command".to_string()).serialize(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use std::collections::HashMap as StdHashMap;
    use std::path::PathBuf;

    fn store() -> Store {
        Arc::new(Mutex::new(StdHashMap::new()))
    }

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.to_string())
    }

    fn server() -> Server {
        Server::new(store(), PathBuf::from("dump.rdb"))
    }

    #[test]
    fn message_and_confirmation_frames_serialize_as_redis_expects() {
        // The push frame a subscriber receives.
        assert_eq!(
            message_frame("news", "hi").serialize(),
            "*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$2\r\nhi\r\n"
        );
        // A subscribe confirmation: [kind, channel, running-count].
        assert_eq!(
            subscription_reply("subscribe", Some("news"), 1).serialize(),
            "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n"
        );
        // An "unsubscribed from everything" reply carries a nil channel.
        assert_eq!(
            subscription_reply("unsubscribe", None, 0).serialize(),
            "*3\r\n$11\r\nunsubscribe\r\n$-1\r\n:0\r\n"
        );
    }

    #[test]
    fn publish_with_no_subscribers_delivers_to_zero() {
        let srv = server();
        assert_eq!(
            publish(&[bulk("news"), bulk("hello")], &srv),
            Value::Integer(0)
        );
    }

    #[test]
    fn publish_delivers_the_message_frame_to_every_subscriber() {
        let srv = server();
        let mut subscribed = HashSet::new();
        // Two subscribers on "news", registered through the real subscribe path.
        let (tx1, mut rx1) = mpsc::unbounded_channel::<Bytes>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<Bytes>();
        do_subscribe(&[bulk("news")], &srv, 1, &tx1, &mut subscribed.clone());
        do_subscribe(&[bulk("news")], &srv, 2, &tx2, &mut subscribed);

        assert_eq!(
            publish(&[bulk("news"), bulk("hello")], &srv),
            Value::Integer(2)
        );

        let want = Bytes::from(message_frame("news", "hello").serialize().into_bytes());
        assert_eq!(rx1.try_recv().unwrap(), want);
        assert_eq!(rx2.try_recv().unwrap(), want);
    }

    #[test]
    fn publish_only_reaches_the_named_channel() {
        let srv = server();
        let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
        let mut subscribed = HashSet::new();
        do_subscribe(&[bulk("sports")], &srv, 1, &tx, &mut subscribed);

        // A publish to a different channel reaches nobody.
        assert_eq!(
            publish(&[bulk("news"), bulk("hi")], &srv),
            Value::Integer(0)
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn publish_prunes_a_subscriber_whose_receiver_is_gone() {
        let srv = server();
        let (tx_live, _rx_live) = mpsc::unbounded_channel::<Bytes>();
        let (tx_dead, rx_dead) = mpsc::unbounded_channel::<Bytes>();
        drop(rx_dead); // its sender is now closed
        srv.subscribers
            .lock()
            .unwrap()
            .entry("news".to_string())
            .or_default()
            .extend([(1, tx_live), (2, tx_dead)]);

        // Only the live subscriber counts, and the dead one is pruned.
        assert_eq!(
            publish(&[bulk("news"), bulk("hi")], &srv),
            Value::Integer(1)
        );
        assert_eq!(srv.subscribers.lock().unwrap()["news"].len(), 1);
    }

    #[test]
    fn publish_validates_its_arity() {
        let srv = server();
        assert_eq!(publish(&[bulk("news")], &srv), wrong_args("publish"));
        assert_eq!(
            publish(&[bulk("a"), bulk("b"), bulk("c")], &srv),
            wrong_args("publish")
        );
    }

    #[test]
    fn subscribe_registers_the_client_and_confirms_each_channel() {
        let srv = server();
        let (tx, _rx) = mpsc::unbounded_channel::<Bytes>();
        let mut subscribed = HashSet::new();

        let reply = do_subscribe(&[bulk("a"), bulk("b")], &srv, 7, &tx, &mut subscribed);
        // Two independent confirmation frames, counts growing 1 then 2.
        assert_eq!(
            reply,
            "*3\r\n$9\r\nsubscribe\r\n$1\r\na\r\n:1\r\n\
             *3\r\n$9\r\nsubscribe\r\n$1\r\nb\r\n:2\r\n"
        );
        assert_eq!(
            subscribed,
            HashSet::from(["a".to_string(), "b".to_string()])
        );
        let table = srv.subscribers.lock().unwrap();
        assert!(table["a"].contains_key(&7));
        assert!(table["b"].contains_key(&7));
    }

    #[test]
    fn unsubscribe_by_name_removes_only_that_channel() {
        let srv = server();
        let (tx, _rx) = mpsc::unbounded_channel::<Bytes>();
        let mut subscribed = HashSet::new();
        do_subscribe(&[bulk("a"), bulk("b")], &srv, 7, &tx, &mut subscribed);

        let reply = do_unsubscribe(&[bulk("a")], &srv, 7, &mut subscribed);
        assert_eq!(reply, "*3\r\n$11\r\nunsubscribe\r\n$1\r\na\r\n:1\r\n");
        assert_eq!(subscribed, HashSet::from(["b".to_string()]));
        // "a" had its only subscriber removed, so the channel is gone entirely.
        let table = srv.subscribers.lock().unwrap();
        assert!(!table.contains_key("a"));
        assert!(table["b"].contains_key(&7));
    }

    #[test]
    fn bare_unsubscribe_drops_every_channel() {
        let srv = server();
        let (tx, _rx) = mpsc::unbounded_channel::<Bytes>();
        let mut subscribed = HashSet::new();
        do_subscribe(&[bulk("a"), bulk("b")], &srv, 7, &tx, &mut subscribed);

        // No arguments = leave all channels; ends with count 0.
        let reply = do_unsubscribe(&[], &srv, 7, &mut subscribed);
        assert!(reply.contains(":0\r\n"));
        assert!(subscribed.is_empty());
        assert!(srv.subscribers.lock().unwrap().is_empty());
    }

    #[test]
    fn unsubscribe_with_nothing_subscribed_replies_with_nil_channel() {
        let srv = server();
        let mut subscribed = HashSet::new();
        let reply = do_unsubscribe(&[], &srv, 7, &mut subscribed);
        assert_eq!(reply, "*3\r\n$11\r\nunsubscribe\r\n$-1\r\n:0\r\n");
    }

    #[test]
    fn a_subscribed_connection_rejects_non_pubsub_commands() {
        let srv = server();
        let (tx, _rx) = mpsc::unbounded_channel::<Bytes>();
        let mut subscribed = HashSet::from(["news".to_string()]);
        let get = Value::Array(vec![bulk("GET"), bulk("k")]);
        let reply = handle_sub_command(get, &srv, 7, &tx, &mut subscribed);
        assert!(reply.starts_with("-ERR Can't execute 'get'"));
        // PING is still allowed while subscribed.
        let ping = Value::Array(vec![bulk("PING")]);
        assert_eq!(
            handle_sub_command(ping, &srv, 7, &tx, &mut subscribed),
            "+PONG\r\n"
        );
    }
}
