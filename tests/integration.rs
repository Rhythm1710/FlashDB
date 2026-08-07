//! End-to-end tests that start a real FlashDB server on an ephemeral port and
//! drive it over a genuine TCP socket, sending raw RESP bytes and asserting on
//! the raw RESP bytes that come back. Where the parser unit tests prove single
//! frames parse correctly in isolation, these prove the whole pipeline —
//! accept, read, dispatch, reply — behaves against a live client.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Start a server on 127.0.0.1:0 (the OS picks a free port) and return its
/// address. The server runs on a background task for the life of the test; the
/// task is dropped when the test ends, which is fine for a short-lived test.
async fn start_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = flashdb::run(listener).await;
    });
    addr
}

/// Connect a fresh client to the running server.
async fn connect(addr: std::net::SocketAddr) -> TcpStream {
    TcpStream::connect(addr).await.unwrap()
}

/// Send raw bytes to the server.
async fn send(stream: &mut TcpStream, bytes: &[u8]) {
    stream.write_all(bytes).await.unwrap();
}

/// Read until we have accumulated at least `expected.len()` bytes, then assert
/// the reply matches exactly. TCP may deliver a reply in several chunks, so we
/// keep reading until the whole expected reply has arrived.
async fn expect_reply(stream: &mut TcpStream, expected: &str) {
    let want = expected.as_bytes();
    let mut got = Vec::new();
    let mut chunk = [0u8; 256];
    while got.len() < want.len() {
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk))
            .await
            .expect("timed out waiting for reply")
            .expect("read failed");
        assert_ne!(n, 0, "server closed connection early; got so far: {got:?}");
        got.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(
        String::from_utf8_lossy(&got),
        expected,
        "unexpected reply bytes"
    );
}

#[tokio::test]
async fn ping_returns_pong() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    send(&mut client, b"*1\r\n$4\r\nPING\r\n").await;
    expect_reply(&mut client, "+PONG\r\n").await;
}

#[tokio::test]
async fn ping_echoes_its_argument() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    send(&mut client, b"*2\r\n$4\r\nPING\r\n$5\r\nhello\r\n").await;
    expect_reply(&mut client, "$5\r\nhello\r\n").await;
}

#[tokio::test]
async fn echo_returns_its_argument() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    send(&mut client, b"*2\r\n$4\r\nECHO\r\n$3\r\nhey\r\n").await;
    expect_reply(&mut client, "$3\r\nhey\r\n").await;
}

#[tokio::test]
async fn set_then_get_round_trips_the_value() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    send(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n",
    )
    .await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n").await;
    expect_reply(&mut client, "$3\r\nbar\r\n").await;
}

#[tokio::test]
async fn get_missing_key_returns_null() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$7\r\nmissing\r\n").await;
    expect_reply(&mut client, "$-1\r\n").await;
}

#[tokio::test]
async fn del_reports_the_number_of_keys_removed() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    send(&mut client, b"*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(&mut client, b"*3\r\n$3\r\nSET\r\n$1\r\nb\r\n$1\r\n2\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;
    // Two of the three keys exist, so DEL should count 2.
    send(
        &mut client,
        b"*4\r\n$3\r\nDEL\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n",
    )
    .await;
    expect_reply(&mut client, ":2\r\n").await;
}

#[tokio::test]
async fn pipelined_commands_each_get_a_reply_in_order() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // Two commands written in a single segment; both replies must come back
    // in order, proving the server drains a pipelined buffer one frame at a
    // time rather than only handling the first.
    send(
        &mut client,
        b"*1\r\n$4\r\nPING\r\n*2\r\n$4\r\nECHO\r\n$2\r\nhi\r\n",
    )
    .await;
    expect_reply(&mut client, "+PONG\r\n$2\r\nhi\r\n").await;
}

#[tokio::test]
async fn a_frame_split_across_writes_is_reassembled() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // Deliberately split one PING frame across two writes with a pause; the
    // server must buffer the first half and wait rather than error.
    send(&mut client, b"*1\r\n$4\r\nPI").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    send(&mut client, b"NG\r\n").await;
    expect_reply(&mut client, "+PONG\r\n").await;
}

#[tokio::test]
async fn unknown_command_gets_an_error_reply() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    send(&mut client, b"*1\r\n$7\r\nNOSUCH0\r\n").await;
    expect_reply(&mut client, "-ERR unknown command 'nosuch0'\r\n").await;
}

#[tokio::test]
async fn wrong_arity_gets_an_error_but_keeps_the_connection() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    send(&mut client, b"*1\r\n$3\r\nGET\r\n").await;
    expect_reply(
        &mut client,
        "-ERR wrong number of arguments for 'get' command\r\n",
    )
    .await;
    // The connection should still be usable after an application-level error.
    send(&mut client, b"*1\r\n$4\r\nPING\r\n").await;
    expect_reply(&mut client, "+PONG\r\n").await;
}

#[tokio::test]
async fn garbage_input_gets_a_protocol_error_then_the_server_closes() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // A byte that can never begin a valid RESP frame. The server replies with
    // a protocol error and then hangs up, because it can't resync mid-stream.
    send(&mut client, b"!oops\r\n").await;
    let mut got = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut chunk))
            .await
            .expect("timed out")
            .expect("read failed");
        if n == 0 {
            break; // server closed the connection, as expected
        }
        got.extend_from_slice(&chunk[..n]);
    }
    assert!(
        got.starts_with(b"-ERR Protocol error:"),
        "expected a protocol error reply, got: {}",
        String::from_utf8_lossy(&got)
    );
}

#[tokio::test]
async fn set_with_ex_is_readable_and_reports_its_ttl() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // SET k v EX 100
    send(
        &mut client,
        b"*5\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n$2\r\nEX\r\n$3\r\n100\r\n",
    )
    .await;
    expect_reply(&mut client, "+OK\r\n").await;
    // The value is still there...
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n").await;
    expect_reply(&mut client, "$1\r\nv\r\n").await;
    // ...and TTL reports the full 100 seconds (rounded up from just under).
    send(&mut client, b"*2\r\n$3\r\nTTL\r\n$1\r\nk\r\n").await;
    expect_reply(&mut client, ":100\r\n").await;
}

#[tokio::test]
async fn a_key_set_with_px_expires_and_reads_back_null() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // SET k v PX 30 — a 30ms lifetime.
    send(
        &mut client,
        b"*5\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n$2\r\nPX\r\n$2\r\n30\r\n",
    )
    .await;
    expect_reply(&mut client, "+OK\r\n").await;
    // Wait past the deadline, then GET must see the key as gone.
    tokio::time::sleep(Duration::from_millis(60)).await;
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n").await;
    expect_reply(&mut client, "$-1\r\n").await;
    // TTL now reports -2: the key does not exist at all.
    send(&mut client, b"*2\r\n$3\r\nTTL\r\n$1\r\nk\r\n").await;
    expect_reply(&mut client, ":-2\r\n").await;
}

#[tokio::test]
async fn expire_then_persist_round_trips_a_ttl() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // A plain key with no expiry: TTL is -1.
    send(&mut client, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(&mut client, b"*2\r\n$3\r\nTTL\r\n$1\r\nk\r\n").await;
    expect_reply(&mut client, ":-1\r\n").await;
    // EXPIRE sets a timeout and reports 1; a missing key reports 0.
    send(
        &mut client,
        b"*3\r\n$6\r\nEXPIRE\r\n$1\r\nk\r\n$3\r\n500\r\n",
    )
    .await;
    expect_reply(&mut client, ":1\r\n").await;
    send(
        &mut client,
        b"*3\r\n$6\r\nEXPIRE\r\n$7\r\nnokey00\r\n$2\r\n10\r\n",
    )
    .await;
    expect_reply(&mut client, ":0\r\n").await;
    // PERSIST strips the timeout back off, and TTL returns to -1.
    send(&mut client, b"*2\r\n$7\r\nPERSIST\r\n$1\r\nk\r\n").await;
    expect_reply(&mut client, ":1\r\n").await;
    send(&mut client, b"*2\r\n$3\r\nTTL\r\n$1\r\nk\r\n").await;
    expect_reply(&mut client, ":-1\r\n").await;
}

#[tokio::test]
async fn type_reports_string_for_a_set_key_and_none_otherwise() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // A missing key is 'none'.
    send(&mut client, b"*2\r\n$4\r\nTYPE\r\n$1\r\nk\r\n").await;
    expect_reply(&mut client, "+none\r\n").await;
    // After a SET the key reports 'string'.
    send(&mut client, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(&mut client, b"*2\r\n$4\r\nTYPE\r\n$1\r\nk\r\n").await;
    expect_reply(&mut client, "+string\r\n").await;
}

#[tokio::test]
async fn type_reports_none_once_a_key_has_expired() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // Set a key with a very short TTL, let it lapse, then TYPE sees 'none'.
    send(
        &mut client,
        b"*5\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n$2\r\nPX\r\n$2\r\n20\r\n",
    )
    .await;
    expect_reply(&mut client, "+OK\r\n").await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    send(&mut client, b"*2\r\n$4\r\nTYPE\r\n$1\r\nk\r\n").await;
    expect_reply(&mut client, "+none\r\n").await;
}

#[tokio::test]
async fn rpush_then_lrange_returns_the_list_in_order() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // RPUSH mylist a b c  -> length 3
    send(
        &mut client,
        b"*5\r\n$5\r\nRPUSH\r\n$6\r\nmylist\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n",
    )
    .await;
    expect_reply(&mut client, ":3\r\n").await;
    // LRANGE mylist 0 -1  -> the whole list as an array of bulk strings.
    send(
        &mut client,
        b"*4\r\n$6\r\nLRANGE\r\n$6\r\nmylist\r\n$1\r\n0\r\n$2\r\n-1\r\n",
    )
    .await;
    expect_reply(&mut client, "*3\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n").await;
    // LLEN reports the same count.
    send(&mut client, b"*2\r\n$4\r\nLLEN\r\n$6\r\nmylist\r\n").await;
    expect_reply(&mut client, ":3\r\n").await;
    // TYPE now reports 'list'.
    send(&mut client, b"*2\r\n$4\r\nTYPE\r\n$6\r\nmylist\r\n").await;
    expect_reply(&mut client, "+list\r\n").await;
}

#[tokio::test]
async fn lpush_prepends_and_pops_drain_from_both_ends() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // LPUSH q a b c -> pushed onto the head in turn, so the list is [c, b, a].
    send(
        &mut client,
        b"*5\r\n$5\r\nLPUSH\r\n$1\r\nq\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n",
    )
    .await;
    expect_reply(&mut client, ":3\r\n").await;
    // LPOP takes the head 'c'...
    send(&mut client, b"*2\r\n$4\r\nLPOP\r\n$1\r\nq\r\n").await;
    expect_reply(&mut client, "$1\r\nc\r\n").await;
    // ...RPOP takes the tail 'a'.
    send(&mut client, b"*2\r\n$4\r\nRPOP\r\n$1\r\nq\r\n").await;
    expect_reply(&mut client, "$1\r\na\r\n").await;
    // Only 'b' remains.
    send(
        &mut client,
        b"*4\r\n$6\r\nLRANGE\r\n$1\r\nq\r\n$1\r\n0\r\n$2\r\n-1\r\n",
    )
    .await;
    expect_reply(&mut client, "*1\r\n$1\r\nb\r\n").await;
}

#[tokio::test]
async fn popping_the_last_element_makes_the_key_disappear() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    send(
        &mut client,
        b"*3\r\n$5\r\nRPUSH\r\n$1\r\nl\r\n$4\r\nonly\r\n",
    )
    .await;
    expect_reply(&mut client, ":1\r\n").await;
    send(&mut client, b"*2\r\n$4\r\nLPOP\r\n$1\r\nl\r\n").await;
    expect_reply(&mut client, "$4\r\nonly\r\n").await;
    // The list is empty, so the key is gone: LPOP is null and TYPE is none.
    send(&mut client, b"*2\r\n$4\r\nLPOP\r\n$1\r\nl\r\n").await;
    expect_reply(&mut client, "$-1\r\n").await;
    send(&mut client, b"*2\r\n$4\r\nTYPE\r\n$1\r\nl\r\n").await;
    expect_reply(&mut client, "+none\r\n").await;
}

#[tokio::test]
async fn a_list_command_on_a_string_key_is_a_wrongtype_error() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // Store a string, then try to push to it as if it were a list.
    send(&mut client, b"*3\r\n$3\r\nSET\r\n$1\r\ns\r\n$1\r\nv\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(&mut client, b"*3\r\n$5\r\nRPUSH\r\n$1\r\ns\r\n$1\r\nx\r\n").await;
    expect_reply(
        &mut client,
        "-WRONGTYPE Operation against a key holding the wrong kind of value\r\n",
    )
    .await;
    // The string survived the failed push unchanged.
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$1\r\ns\r\n").await;
    expect_reply(&mut client, "$1\r\nv\r\n").await;
}

#[tokio::test]
async fn hset_then_hget_round_trips_fields_and_reports_hash() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // HSET h f1 v1 f2 v2 -> two new fields, so :2.
    send(
        &mut client,
        b"*6\r\n$4\r\nHSET\r\n$1\r\nh\r\n$2\r\nf1\r\n$2\r\nv1\r\n$2\r\nf2\r\n$2\r\nv2\r\n",
    )
    .await;
    expect_reply(&mut client, ":2\r\n").await;
    // HGET reads a stored field back...
    send(&mut client, b"*3\r\n$4\r\nHGET\r\n$1\r\nh\r\n$2\r\nf1\r\n").await;
    expect_reply(&mut client, "$2\r\nv1\r\n").await;
    // ...and a missing field on an existing hash is null.
    send(
        &mut client,
        b"*3\r\n$4\r\nHGET\r\n$1\r\nh\r\n$4\r\nnope\r\n",
    )
    .await;
    expect_reply(&mut client, "$-1\r\n").await;
    // TYPE now reports 'hash'.
    send(&mut client, b"*2\r\n$4\r\nTYPE\r\n$1\r\nh\r\n").await;
    expect_reply(&mut client, "+hash\r\n").await;
}

#[tokio::test]
async fn hgetall_returns_the_stored_pair() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // A single field/value pair keeps HGETALL's reply order deterministic
    // (the HashMap order only matters once there is more than one pair).
    send(
        &mut client,
        b"*4\r\n$4\r\nHSET\r\n$1\r\nh\r\n$5\r\nfield\r\n$5\r\nvalue\r\n",
    )
    .await;
    expect_reply(&mut client, ":1\r\n").await;
    send(&mut client, b"*2\r\n$7\r\nHGETALL\r\n$1\r\nh\r\n").await;
    expect_reply(&mut client, "*2\r\n$5\r\nfield\r\n$5\r\nvalue\r\n").await;
}

#[tokio::test]
async fn hdel_removes_fields_and_deletes_the_emptied_hash() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // HSET h a 1 b 2 -> :2
    send(
        &mut client,
        b"*6\r\n$4\r\nHSET\r\n$1\r\nh\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\nb\r\n$1\r\n2\r\n",
    )
    .await;
    expect_reply(&mut client, ":2\r\n").await;
    // Remove one field...
    send(&mut client, b"*3\r\n$4\r\nHDEL\r\n$1\r\nh\r\n$1\r\na\r\n").await;
    expect_reply(&mut client, ":1\r\n").await;
    // ...then the last one, which empties and deletes the key.
    send(&mut client, b"*3\r\n$4\r\nHDEL\r\n$1\r\nh\r\n$1\r\nb\r\n").await;
    expect_reply(&mut client, ":1\r\n").await;
    send(&mut client, b"*2\r\n$4\r\nTYPE\r\n$1\r\nh\r\n").await;
    expect_reply(&mut client, "+none\r\n").await;
    // HGETALL on the now-missing key is an empty array.
    send(&mut client, b"*2\r\n$7\r\nHGETALL\r\n$1\r\nh\r\n").await;
    expect_reply(&mut client, "*0\r\n").await;
}

#[tokio::test]
async fn a_hash_command_on_a_string_key_is_a_wrongtype_error() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    // Store a string, then try to HSET into it as if it were a hash.
    send(&mut client, b"*3\r\n$3\r\nSET\r\n$1\r\ns\r\n$1\r\nv\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(
        &mut client,
        b"*4\r\n$4\r\nHSET\r\n$1\r\ns\r\n$1\r\nf\r\n$1\r\nv\r\n",
    )
    .await;
    expect_reply(
        &mut client,
        "-WRONGTYPE Operation against a key holding the wrong kind of value\r\n",
    )
    .await;
    // The string survived the failed HSET unchanged.
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$1\r\ns\r\n").await;
    expect_reply(&mut client, "$1\r\nv\r\n").await;
}

// --- RDB snapshot loading on startup -------------------------------------

/// Start a server that first loads an RDB snapshot from `dir/dbfilename`, the
/// way `run_with_config` does at boot. The listener is bound here (so the OS
/// picks the port) and the pre-loaded store is served on a background task.
async fn start_server_with_rdb(dir: PathBuf, dbfilename: &str) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = flashdb::Config {
        port: 0, // unused: the listener is already bound
        dir: dir.to_string_lossy().into_owned(),
        dbfilename: dbfilename.to_string(),
        replicaof: None,
    };
    tokio::spawn(async move {
        let _ = flashdb::run_with_config(listener, &config).await;
    });
    addr
}

/// A fresh, uniquely-named temp directory for one test's snapshot file.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("flashdb-it-{}-{}", std::process::id(), tag));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Hand-assemble a minimal valid RDB image: the `REDIS0011` header, a
/// `SELECTDB 0`, one string record per pair (prefixed by an `EXPIRETIME_MS`
/// opcode when a deadline is given), the `0xFF` end marker, and 8 zero bytes
/// standing in for the CRC64 the loader doesn't verify. Keys and values must be
/// shorter than 64 bytes so the length fits a single plain-length byte.
fn build_rdb(pairs: &[(&str, &str, Option<u64>)]) -> Vec<u8> {
    fn push_string(buf: &mut Vec<u8>, s: &str) {
        buf.push(s.len() as u8);
        buf.extend_from_slice(s.as_bytes());
    }
    let mut v = Vec::new();
    v.extend_from_slice(b"REDIS0011");
    v.push(0xFE);
    v.push(0x00); // SELECTDB 0
    for (key, value, expire_at_ms) in pairs {
        if let Some(ms) = expire_at_ms {
            v.push(0xFC);
            v.extend_from_slice(&ms.to_le_bytes());
        }
        v.push(0x00); // string value type
        push_string(&mut v, key);
        push_string(&mut v, value);
    }
    v.push(0xFF);
    v.extend_from_slice(&[0u8; 8]);
    v
}

#[tokio::test]
async fn loads_string_keys_from_an_rdb_on_startup() {
    let dir = temp_dir("load-strings");
    let rdb = build_rdb(&[
        ("greeting", "hello world", None),
        ("counter", "12345", None),
    ]);
    std::fs::write(dir.join("dump.rdb"), &rdb).unwrap();

    let addr = start_server_with_rdb(dir, "dump.rdb").await;
    let mut client = connect(addr).await;

    // Both persisted keys are readable straight after boot.
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$8\r\ngreeting\r\n").await;
    expect_reply(&mut client, "$11\r\nhello world\r\n").await;
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$7\r\ncounter\r\n").await;
    expect_reply(&mut client, "$5\r\n12345\r\n").await;
    // A key that was never in the snapshot is still absent.
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$4\r\nnope\r\n").await;
    expect_reply(&mut client, "$-1\r\n").await;
}

#[tokio::test]
async fn honours_expiry_metadata_when_loading() {
    let dir = temp_dir("load-expiry");
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    // One deadline comfortably in the future, one already in the past.
    let rdb = build_rdb(&[
        ("lives", "still here", Some(now_ms + 1_000_000)),
        ("dead", "gone", Some(now_ms - 1_000)),
    ]);
    std::fs::write(dir.join("dump.rdb"), &rdb).unwrap();

    let addr = start_server_with_rdb(dir, "dump.rdb").await;
    let mut client = connect(addr).await;

    // The future-dated key loaded and is readable.
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$5\r\nlives\r\n").await;
    expect_reply(&mut client, "$10\r\nstill here\r\n").await;
    // ...and it kept its expiry: PERSIST finds a TTL to strip (:1), proving the
    // absolute deadline was converted into a live one rather than dropped.
    send(&mut client, b"*2\r\n$7\r\nPERSIST\r\n$5\r\nlives\r\n").await;
    expect_reply(&mut client, ":1\r\n").await;
    // The key whose deadline had already passed was discarded on load.
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$4\r\ndead\r\n").await;
    expect_reply(&mut client, "$-1\r\n").await;
}

#[tokio::test]
async fn a_missing_snapshot_starts_an_empty_but_working_server() {
    let dir = temp_dir("no-snapshot");
    // No dump.rdb written at all — a clean first boot.
    let addr = start_server_with_rdb(dir, "dump.rdb").await;
    let mut client = connect(addr).await;

    // Nothing preloaded, but the server is fully functional.
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nany\r\n").await;
    expect_reply(&mut client, "$-1\r\n").await;
    send(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$3\r\nnew\r\n$3\r\nval\r\n",
    )
    .await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nnew\r\n").await;
    expect_reply(&mut client, "$3\r\nval\r\n").await;
}

// --- RDB snapshot saving (SAVE / BGSAVE) ---------------------------------

#[tokio::test]
async fn save_writes_a_snapshot_a_fresh_server_reloads() {
    let dir = temp_dir("save-reload");
    let addr = start_server_with_rdb(dir.clone(), "dump.rdb").await;
    let mut client = connect(addr).await;

    // Populate one of each value type: a string, a list, and a hash.
    send(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$8\r\ngreeting\r\n$11\r\nhello world\r\n",
    )
    .await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(
        &mut client,
        b"*4\r\n$5\r\nRPUSH\r\n$1\r\nl\r\n$1\r\na\r\n$1\r\nb\r\n",
    )
    .await;
    expect_reply(&mut client, ":2\r\n").await;
    send(
        &mut client,
        b"*4\r\n$4\r\nHSET\r\n$1\r\nh\r\n$1\r\nf\r\n$1\r\nv\r\n",
    )
    .await;
    expect_reply(&mut client, ":1\r\n").await;

    // SAVE persists synchronously and replies +OK.
    send(&mut client, b"*1\r\n$4\r\nSAVE\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;

    // A brand-new server booting against the same directory recovers all three
    // keys with their contents — the full save → load round trip over the wire.
    let addr2 = start_server_with_rdb(dir, "dump.rdb").await;
    let mut reborn = connect(addr2).await;
    send(&mut reborn, b"*2\r\n$3\r\nGET\r\n$8\r\ngreeting\r\n").await;
    expect_reply(&mut reborn, "$11\r\nhello world\r\n").await;
    send(
        &mut reborn,
        b"*4\r\n$6\r\nLRANGE\r\n$1\r\nl\r\n$1\r\n0\r\n$2\r\n-1\r\n",
    )
    .await;
    expect_reply(&mut reborn, "*2\r\n$1\r\na\r\n$1\r\nb\r\n").await;
    send(&mut reborn, b"*3\r\n$4\r\nHGET\r\n$1\r\nh\r\n$1\r\nf\r\n").await;
    expect_reply(&mut reborn, "$1\r\nv\r\n").await;
}

#[tokio::test]
async fn bgsave_persists_in_the_background_and_reloads() {
    let dir = temp_dir("bgsave");
    let addr = start_server_with_rdb(dir.clone(), "dump.rdb").await;
    let mut client = connect(addr).await;

    send(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$3\r\nval\r\n",
    )
    .await;
    expect_reply(&mut client, "+OK\r\n").await;
    // BGSAVE returns immediately, before the file is necessarily on disk.
    send(&mut client, b"*1\r\n$6\r\nBGSAVE\r\n").await;
    expect_reply(&mut client, "+Background saving started\r\n").await;

    // The background write is atomic (temp file then rename), so the snapshot
    // path appears only once it's complete. Wait for it, bounded, then reload.
    let path = dir.join("dump.rdb");
    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(path.exists(), "BGSAVE never wrote the snapshot");

    let addr2 = start_server_with_rdb(dir, "dump.rdb").await;
    let mut reborn = connect(addr2).await;
    send(&mut reborn, b"*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n").await;
    expect_reply(&mut reborn, "$3\r\nval\r\n").await;
}

// --- Replication (replica side) ------------------------------------------

/// Stand up a *mock master* that speaks just enough of the replication
/// protocol for a replica to sync: it accepts one connection, answers the
/// handshake (`+PONG`, `+OK`, `+OK`, `+FULLRESYNC ...`), then ships `rdb` as a
/// bulk payload (`$<len>\r\n<bytes>`, no trailing CRLF, exactly as Redis does).
///
/// The replica sends each handshake command and waits for its reply before
/// sending the next, so we read one command off the socket before each reply to
/// stay in lock-step. Returns the master's address.
async fn start_mock_master(rdb: Vec<u8>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut scratch = [0u8; 512];
        // PING -> +PONG
        let _ = sock.read(&mut scratch).await.unwrap();
        sock.write_all(b"+PONG\r\n").await.unwrap();
        // REPLCONF listening-port <port> -> +OK
        let _ = sock.read(&mut scratch).await.unwrap();
        sock.write_all(b"+OK\r\n").await.unwrap();
        // REPLCONF capa psync2 -> +OK
        let _ = sock.read(&mut scratch).await.unwrap();
        sock.write_all(b"+OK\r\n").await.unwrap();
        // PSYNC ? -1 -> +FULLRESYNC <replid> <offset>, then the bulk RDB.
        let _ = sock.read(&mut scratch).await.unwrap();
        sock.write_all(b"+FULLRESYNC 8371b4fb1155b71f4a04d3e1bc3e18c4a990aeeb 0\r\n")
            .await
            .unwrap();
        sock.write_all(format!("${}\r\n", rdb.len()).as_bytes())
            .await
            .unwrap();
        sock.write_all(&rdb).await.unwrap();
        // Hold the link open briefly so the replica finishes reading the blob.
        tokio::time::sleep(Duration::from_millis(300)).await;
    });
    addr
}

/// Start a replica pointed at `master_addr`, loading no local snapshot (empty
/// temp dir), and return the address it serves its own clients on.
async fn start_replica(master_addr: std::net::SocketAddr, tag: &str) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = flashdb::Config {
        port: addr.port(),
        dir: temp_dir(tag).to_string_lossy().into_owned(),
        dbfilename: "dump.rdb".to_string(),
        replicaof: Some((master_addr.ip().to_string(), master_addr.port())),
    };
    tokio::spawn(async move {
        let _ = flashdb::run_with_config(listener, &config).await;
    });
    addr
}

/// Poll `GET key` on the replica until it returns `want`, up to a bounded number
/// of tries. Replication runs on a background task, so the key appears a moment
/// after boot; this waits for it without a fixed sleep.
async fn wait_for_get(client: &mut TcpStream, get_cmd: &[u8], want: &str, tries: usize) -> bool {
    let mut chunk = [0u8; 256];
    for _ in 0..tries {
        send(client, get_cmd).await;
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut chunk))
            .await
            .expect("timed out waiting for GET reply")
            .expect("read failed");
        if String::from_utf8_lossy(&chunk[..n]) == want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn replica_loads_the_masters_snapshot_over_the_wire() {
    // The master's snapshot carries a plain key and a key with a live TTL.
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let rdb = build_rdb(&[
        ("greeting", "hello", None),
        ("temp", "soon", Some(now_ms + 1_000_000)),
    ]);

    let master_addr = start_mock_master(rdb).await;
    let replica_addr = start_replica(master_addr, "replica-sync").await;
    let mut client = connect(replica_addr).await;

    // The replicated string appears on the replica after the sync completes.
    let got = wait_for_get(
        &mut client,
        b"*2\r\n$3\r\nGET\r\n$8\r\ngreeting\r\n",
        "$5\r\nhello\r\n",
        40,
    )
    .await;
    assert!(got, "replica never received the master's key");

    // The TTL'd key transferred too, and kept its expiry: PERSIST finds a TTL to
    // strip (:1), proving the absolute deadline survived the wire and load.
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$4\r\ntemp\r\n").await;
    expect_reply(&mut client, "$4\r\nsoon\r\n").await;
    send(&mut client, b"*2\r\n$7\r\nPERSIST\r\n$4\r\ntemp\r\n").await;
    expect_reply(&mut client, ":1\r\n").await;
}

// --- Replication (master side) -------------------------------------------

#[tokio::test]
async fn a_real_flashdb_replica_syncs_and_follows_its_master() {
    // A real FlashDB master (not a mock) with one key already set.
    let master_addr = start_server().await;
    let mut master = connect(master_addr).await;
    send(
        &mut master,
        b"*3\r\n$3\r\nSET\r\n$4\r\nboot\r\n$2\r\nhi\r\n",
    )
    .await;
    expect_reply(&mut master, "+OK\r\n").await;

    // A real FlashDB replica points at it and syncs on boot.
    let replica_addr = start_replica(master_addr, "master-prop").await;
    let mut replica = connect(replica_addr).await;

    // The initial snapshot carried the pre-existing key across.
    let synced = wait_for_get(
        &mut replica,
        b"*2\r\n$3\r\nGET\r\n$4\r\nboot\r\n",
        "$2\r\nhi\r\n",
        40,
    )
    .await;
    assert!(synced, "replica never loaded the master's initial snapshot");

    // A brand-new write on the master must now stream to the replica live.
    send(
        &mut master,
        b"*3\r\n$3\r\nSET\r\n$5\r\nlive1\r\n$5\r\nvalue\r\n",
    )
    .await;
    expect_reply(&mut master, "+OK\r\n").await;
    let streamed = wait_for_get(
        &mut replica,
        b"*2\r\n$3\r\nGET\r\n$5\r\nlive1\r\n",
        "$5\r\nvalue\r\n",
        40,
    )
    .await;
    assert!(
        streamed,
        "a post-sync SET on the master never reached the replica"
    );

    // Deletes propagate too: removing the key on the master clears it downstream.
    send(&mut master, b"*2\r\n$3\r\nDEL\r\n$4\r\nboot\r\n").await;
    expect_reply(&mut master, ":1\r\n").await;
    let deleted = wait_for_get(
        &mut replica,
        b"*2\r\n$3\r\nGET\r\n$4\r\nboot\r\n",
        "$-1\r\n",
        40,
    )
    .await;
    assert!(
        deleted,
        "a DEL on the master never propagated to the replica"
    );
}

#[tokio::test]
async fn the_master_counts_its_replica_via_wait() {
    let master_addr = start_server().await;
    // No replicas yet: WAIT returns 0 at once.
    let mut master = connect(master_addr).await;
    send(&mut master, b"*3\r\n$4\r\nWAIT\r\n$1\r\n0\r\n$3\r\n100\r\n").await;
    expect_reply(&mut master, ":0\r\n").await;

    // Once a replica has connected and finished its handshake, WAIT sees it.
    let _replica_addr = start_replica(master_addr, "wait-count").await;
    let saw_replica = wait_for_get(
        &mut master,
        b"*3\r\n$4\r\nWAIT\r\n$1\r\n1\r\n$3\r\n100\r\n",
        ":1\r\n",
        40,
    )
    .await;
    assert!(saw_replica, "master never counted the connected replica");
}

// --- Transactions: MULTI / EXEC / DISCARD / WATCH over a real socket ---

#[tokio::test]
async fn multi_queues_then_exec_runs_the_batch() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    send(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;
    // Each queued command is answered +QUEUED, not run.
    send(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n",
    )
    .await;
    expect_reply(&mut client, "+QUEUED\r\n").await;
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n").await;
    expect_reply(&mut client, "+QUEUED\r\n").await;

    // EXEC returns one array with both replies in order.
    send(&mut client, b"*1\r\n$4\r\nEXEC\r\n").await;
    expect_reply(&mut client, "*2\r\n+OK\r\n$3\r\nbar\r\n").await;
}

#[tokio::test]
async fn discard_throws_the_queue_away() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    send(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n",
    )
    .await;
    expect_reply(&mut client, "+QUEUED\r\n").await;
    send(&mut client, b"*1\r\n$7\r\nDISCARD\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;

    // The SET never ran, so the key is still missing.
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n").await;
    expect_reply(&mut client, "$-1\r\n").await;
}

#[tokio::test]
async fn exec_without_multi_is_an_error() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    send(&mut client, b"*1\r\n$4\r\nEXEC\r\n").await;
    expect_reply(&mut client, "-ERR EXEC without MULTI\r\n").await;
}

#[tokio::test]
async fn a_bad_command_in_a_transaction_makes_exec_abort() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    send(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n",
    )
    .await;
    expect_reply(&mut client, "+QUEUED\r\n").await;
    // An unknown command is rejected at queue time and taints the transaction.
    send(&mut client, b"*1\r\n$4\r\nNOPE\r\n").await;
    expect_reply(&mut client, "-ERR unknown command 'nope'\r\n").await;

    send(&mut client, b"*1\r\n$4\r\nEXEC\r\n").await;
    expect_reply(
        &mut client,
        "-EXECABORT Transaction discarded because of previous errors.\r\n",
    )
    .await;
    // The queued SET must not have run.
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n").await;
    expect_reply(&mut client, "$-1\r\n").await;
}

#[tokio::test]
async fn watch_aborts_exec_when_another_client_writes_the_key() {
    let addr = start_server().await;
    let mut alice = connect(addr).await;
    let mut bob = connect(addr).await;

    // Alice watches k and opens a transaction that would set it.
    send(&mut alice, b"*2\r\n$5\r\nWATCH\r\n$1\r\nk\r\n").await;
    expect_reply(&mut alice, "+OK\r\n").await;
    send(&mut alice, b"*1\r\n$5\r\nMULTI\r\n").await;
    expect_reply(&mut alice, "+OK\r\n").await;
    send(&mut alice, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\nalice\r\n").await;
    expect_reply(&mut alice, "+QUEUED\r\n").await;

    // Bob writes k out from under her.
    send(&mut bob, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$3\r\nbob\r\n").await;
    expect_reply(&mut bob, "+OK\r\n").await;

    // Alice's EXEC aborts with the nil array and changes nothing.
    send(&mut alice, b"*1\r\n$4\r\nEXEC\r\n").await;
    expect_reply(&mut alice, "*-1\r\n").await;
    send(&mut alice, b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n").await;
    expect_reply(&mut alice, "$3\r\nbob\r\n").await;
}

#[tokio::test]
async fn watch_lets_exec_commit_when_the_key_is_untouched() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    send(&mut client, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\n1\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(&mut client, b"*2\r\n$5\r\nWATCH\r\n$1\r\nk\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(&mut client, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\n2\r\n").await;
    expect_reply(&mut client, "+QUEUED\r\n").await;

    // Nobody else touched k, so EXEC commits.
    send(&mut client, b"*1\r\n$4\r\nEXEC\r\n").await;
    expect_reply(&mut client, "*1\r\n+OK\r\n").await;
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n").await;
    expect_reply(&mut client, "$1\r\n2\r\n").await;
}

// ----- Pub/Sub ---------------------------------------------------------------

#[tokio::test]
async fn publish_delivers_a_message_to_a_subscriber() {
    let addr = start_server().await;

    // One connection subscribes and reads its subscribe confirmation. Awaiting
    // that reply guarantees the server has registered us before we publish.
    let mut sub = connect(addr).await;
    send(&mut sub, b"*2\r\n$9\r\nSUBSCRIBE\r\n$4\r\nnews\r\n").await;
    expect_reply(&mut sub, "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n").await;

    // A second connection publishes; it learns one client received the message.
    let mut pubr = connect(addr).await;
    send(
        &mut pubr,
        b"*3\r\n$7\r\nPUBLISH\r\n$4\r\nnews\r\n$5\r\nhello\r\n",
    )
    .await;
    expect_reply(&mut pubr, ":1\r\n").await;

    // The subscriber is pushed the message frame without having asked for it.
    expect_reply(
        &mut sub,
        "*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$5\r\nhello\r\n",
    )
    .await;
}

#[tokio::test]
async fn publish_to_a_channel_with_no_subscribers_returns_zero() {
    let addr = start_server().await;
    let mut client = connect(addr).await;
    send(
        &mut client,
        b"*3\r\n$7\r\nPUBLISH\r\n$5\r\nempty\r\n$2\r\nhi\r\n",
    )
    .await;
    expect_reply(&mut client, ":0\r\n").await;
}

#[tokio::test]
async fn a_subscribed_client_may_not_run_ordinary_commands() {
    let addr = start_server().await;
    let mut sub = connect(addr).await;
    send(&mut sub, b"*2\r\n$9\r\nSUBSCRIBE\r\n$4\r\nnews\r\n").await;
    expect_reply(&mut sub, "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n").await;

    // GET is refused while in subscribe mode.
    send(&mut sub, b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n").await;
    expect_reply(
        &mut sub,
        "-ERR Can't execute 'get': only (P)SUBSCRIBE / (P)UNSUBSCRIBE / PING / QUIT are allowed in this context\r\n",
    )
    .await;
}

#[tokio::test]
async fn unsubscribing_from_everything_returns_the_connection_to_normal_mode() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    send(&mut client, b"*2\r\n$9\r\nSUBSCRIBE\r\n$4\r\nnews\r\n").await;
    expect_reply(&mut client, "*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n").await;

    // Leaving the last channel drops the count to zero and exits subscribe mode.
    send(&mut client, b"*2\r\n$11\r\nUNSUBSCRIBE\r\n$4\r\nnews\r\n").await;
    expect_reply(
        &mut client,
        "*3\r\n$11\r\nunsubscribe\r\n$4\r\nnews\r\n:0\r\n",
    )
    .await;

    // Back in ordinary request/response mode: a plain command works again.
    send(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n",
    )
    .await;
    expect_reply(&mut client, "+OK\r\n").await;
    send(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n").await;
    expect_reply(&mut client, "$3\r\nbar\r\n").await;
}

#[tokio::test]
async fn a_message_reaches_every_subscriber_of_a_channel() {
    let addr = start_server().await;

    let mut sub1 = connect(addr).await;
    send(&mut sub1, b"*2\r\n$9\r\nSUBSCRIBE\r\n$4\r\nchat\r\n").await;
    expect_reply(&mut sub1, "*3\r\n$9\r\nsubscribe\r\n$4\r\nchat\r\n:1\r\n").await;

    let mut sub2 = connect(addr).await;
    send(&mut sub2, b"*2\r\n$9\r\nSUBSCRIBE\r\n$4\r\nchat\r\n").await;
    expect_reply(&mut sub2, "*3\r\n$9\r\nsubscribe\r\n$4\r\nchat\r\n:1\r\n").await;

    let mut pubr = connect(addr).await;
    send(
        &mut pubr,
        b"*3\r\n$7\r\nPUBLISH\r\n$4\r\nchat\r\n$2\r\nyo\r\n",
    )
    .await;
    expect_reply(&mut pubr, ":2\r\n").await;

    let frame = "*3\r\n$7\r\nmessage\r\n$4\r\nchat\r\n$2\r\nyo\r\n";
    expect_reply(&mut sub1, frame).await;
    expect_reply(&mut sub2, frame).await;
}

#[tokio::test]
async fn xadd_then_xlen_and_type_over_tcp() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    // XADD stream 1-1 field hello -> "1-1"
    send(
        &mut client,
        b"*5\r\n$4\r\nXADD\r\n$6\r\nstream\r\n$3\r\n1-1\r\n$5\r\nfield\r\n$5\r\nhello\r\n",
    )
    .await;
    expect_reply(&mut client, "$3\r\n1-1\r\n").await;

    // A second entry with an auto sequence in the same millisecond -> "1-2".
    send(
        &mut client,
        b"*5\r\n$4\r\nXADD\r\n$6\r\nstream\r\n$3\r\n1-*\r\n$5\r\nfield\r\n$5\r\nworld\r\n",
    )
    .await;
    expect_reply(&mut client, "$3\r\n1-2\r\n").await;

    // XLEN stream -> 2
    send(&mut client, b"*2\r\n$4\r\nXLEN\r\n$6\r\nstream\r\n").await;
    expect_reply(&mut client, ":2\r\n").await;

    // TYPE stream -> stream
    send(&mut client, b"*2\r\n$4\r\nTYPE\r\n$6\r\nstream\r\n").await;
    expect_reply(&mut client, "+stream\r\n").await;
}

#[tokio::test]
async fn xadd_rejects_a_non_increasing_id_over_tcp() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    send(
        &mut client,
        b"*5\r\n$4\r\nXADD\r\n$1\r\ns\r\n$3\r\n5-5\r\n$1\r\nf\r\n$1\r\nv\r\n",
    )
    .await;
    expect_reply(&mut client, "$3\r\n5-5\r\n").await;

    // The same ID again is refused with the Redis error string.
    send(
        &mut client,
        b"*5\r\n$4\r\nXADD\r\n$1\r\ns\r\n$3\r\n5-5\r\n$1\r\nf\r\n$1\r\nv\r\n",
    )
    .await;
    expect_reply(
        &mut client,
        "-ERR The ID specified in XADD is equal or smaller than the target stream top item\r\n",
    )
    .await;
}

#[tokio::test]
async fn xrange_returns_entries_in_order_over_tcp() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    for id in ["1-0", "2-0", "3-0"] {
        let frame = format!(
            "*5\r\n$4\r\nXADD\r\n$1\r\ns\r\n${}\r\n{}\r\n$1\r\nk\r\n$1\r\nv\r\n",
            id.len(),
            id
        );
        send(&mut client, frame.as_bytes()).await;
        let reply = format!("${}\r\n{}\r\n", id.len(), id);
        expect_reply(&mut client, &reply).await;
    }

    // XRANGE s 2 + -> the 2-0 and 3-0 entries as [id, [field, value]].
    send(
        &mut client,
        b"*4\r\n$6\r\nXRANGE\r\n$1\r\ns\r\n$1\r\n2\r\n$1\r\n+\r\n",
    )
    .await;
    let expected = "*2\r\n\
        *2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nk\r\n$1\r\nv\r\n\
        *2\r\n$3\r\n3-0\r\n*2\r\n$1\r\nk\r\n$1\r\nv\r\n";
    expect_reply(&mut client, expected).await;
}

#[tokio::test]
async fn xread_returns_entries_after_an_id_over_tcp() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    for id in ["1-0", "2-0"] {
        let frame = format!(
            "*5\r\n$4\r\nXADD\r\n$1\r\ns\r\n${}\r\n{}\r\n$1\r\nk\r\n$1\r\nv\r\n",
            id.len(),
            id
        );
        send(&mut client, frame.as_bytes()).await;
        let reply = format!("${}\r\n{}\r\n", id.len(), id);
        expect_reply(&mut client, &reply).await;
    }

    // XREAD STREAMS s 1-0 -> one stream, one entry (2-0).
    send(
        &mut client,
        b"*4\r\n$5\r\nXREAD\r\n$7\r\nSTREAMS\r\n$1\r\ns\r\n$3\r\n1-0\r\n",
    )
    .await;
    let expected = "*1\r\n\
        *2\r\n$1\r\ns\r\n\
        *1\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nk\r\n$1\r\nv\r\n";
    expect_reply(&mut client, expected).await;

    // Nothing newer than the end: nil array.
    send(
        &mut client,
        b"*4\r\n$5\r\nXREAD\r\n$7\r\nSTREAMS\r\n$1\r\ns\r\n$1\r\n$\r\n",
    )
    .await;
    expect_reply(&mut client, "*-1\r\n").await;
}

#[tokio::test]
async fn xread_block_times_out_to_nil_over_tcp() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    // BLOCK 50ms on an empty stream with no writer: the read parks briefly, then
    // gives up with a nil array rather than an empty one.
    send(
        &mut client,
        b"*6\r\n$5\r\nXREAD\r\n$5\r\nBLOCK\r\n$2\r\n50\r\n$7\r\nSTREAMS\r\n$1\r\ns\r\n$1\r\n$\r\n",
    )
    .await;
    expect_reply(&mut client, "*-1\r\n").await;
}

#[tokio::test]
async fn xread_block_wakes_on_another_clients_write_over_tcp() {
    let addr = start_server().await;
    let mut reader = connect(addr).await;
    let mut writer = connect(addr).await;

    // The reader blocks forever (BLOCK 0) on a not-yet-existent stream with `$`,
    // meaning "only entries added after now".
    send(
        &mut reader,
        b"*6\r\n$5\r\nXREAD\r\n$5\r\nBLOCK\r\n$1\r\n0\r\n$7\r\nSTREAMS\r\n$1\r\ns\r\n$1\r\n$\r\n",
    )
    .await;

    // Let the reader actually park before the write lands, so `$` is resolved
    // against the empty stream and the new entry counts as newer.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // A second client appends an entry, which should wake the blocked reader.
    send(
        &mut writer,
        b"*5\r\n$4\r\nXADD\r\n$1\r\ns\r\n$3\r\n1-1\r\n$1\r\nk\r\n$1\r\nv\r\n",
    )
    .await;
    expect_reply(&mut writer, "$3\r\n1-1\r\n").await;

    // The reader is delivered the freshly-added entry.
    let expected = "*1\r\n\
        *2\r\n$1\r\ns\r\n\
        *1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nk\r\n$1\r\nv\r\n";
    expect_reply(&mut reader, expected).await;
}

#[tokio::test]
async fn xrevrange_returns_entries_newest_first_over_tcp() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    for id in ["1-0", "2-0", "3-0"] {
        let frame = format!(
            "*5\r\n$4\r\nXADD\r\n$1\r\ns\r\n${}\r\n{}\r\n$1\r\nk\r\n$1\r\nv\r\n",
            id.len(),
            id
        );
        send(&mut client, frame.as_bytes()).await;
        let reply = format!("${}\r\n{}\r\n", id.len(), id);
        expect_reply(&mut client, &reply).await;
    }

    // XREVRANGE s + - -> all three entries, highest ID first.
    send(
        &mut client,
        b"*4\r\n$9\r\nXREVRANGE\r\n$1\r\ns\r\n$1\r\n+\r\n$1\r\n-\r\n",
    )
    .await;
    let expected = "*3\r\n\
        *2\r\n$3\r\n3-0\r\n*2\r\n$1\r\nk\r\n$1\r\nv\r\n\
        *2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nk\r\n$1\r\nv\r\n\
        *2\r\n$3\r\n1-0\r\n*2\r\n$1\r\nk\r\n$1\r\nv\r\n";
    expect_reply(&mut client, expected).await;

    // XRANGE with exclusive bounds `(1-0` `(3-0` keeps only the middle entry.
    send(
        &mut client,
        b"*4\r\n$6\r\nXRANGE\r\n$1\r\ns\r\n$4\r\n(1-0\r\n$4\r\n(3-0\r\n",
    )
    .await;
    let expected = "*1\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nk\r\n$1\r\nv\r\n";
    expect_reply(&mut client, expected).await;
}

#[tokio::test]
async fn xdel_removes_entries_and_reports_the_count_over_tcp() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    for id in ["1-0", "2-0", "3-0"] {
        let frame = format!(
            "*5\r\n$4\r\nXADD\r\n$1\r\ns\r\n${}\r\n{}\r\n$1\r\nk\r\n$1\r\nv\r\n",
            id.len(),
            id
        );
        send(&mut client, frame.as_bytes()).await;
        let reply = format!("${}\r\n{}\r\n", id.len(), id);
        expect_reply(&mut client, &reply).await;
    }

    // XDEL s 1-0 3-0 9-9 -> deletes the two present IDs, ignores the absent one.
    send(
        &mut client,
        b"*5\r\n$4\r\nXDEL\r\n$1\r\ns\r\n$3\r\n1-0\r\n$3\r\n3-0\r\n$3\r\n9-9\r\n",
    )
    .await;
    expect_reply(&mut client, ":2\r\n").await;

    // Only 2-0 survives.
    send(
        &mut client,
        b"*4\r\n$6\r\nXRANGE\r\n$1\r\ns\r\n$1\r\n-\r\n$1\r\n+\r\n",
    )
    .await;
    let expected = "*1\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nk\r\n$1\r\nv\r\n";
    expect_reply(&mut client, expected).await;

    // The high-water mark stayed at 3-0: re-adding it is refused.
    send(
        &mut client,
        b"*5\r\n$4\r\nXADD\r\n$1\r\ns\r\n$3\r\n3-0\r\n$1\r\nk\r\n$1\r\nv\r\n",
    )
    .await;
    let mut buf = [0u8; 256];
    let n = client.read(&mut buf).await.unwrap();
    assert!(
        buf[..n].starts_with(b"-ERR"),
        "re-adding a deleted top ID should be refused"
    );
}

#[tokio::test]
async fn xtrim_drops_the_oldest_entries_over_tcp() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    for id in ["1-0", "2-0", "3-0", "4-0"] {
        let frame = format!(
            "*5\r\n$4\r\nXADD\r\n$1\r\ns\r\n${}\r\n{}\r\n$1\r\nk\r\n$1\r\nv\r\n",
            id.len(),
            id
        );
        send(&mut client, frame.as_bytes()).await;
        let reply = format!("${}\r\n{}\r\n", id.len(), id);
        expect_reply(&mut client, &reply).await;
    }

    // XTRIM s MAXLEN 2 -> drops the two oldest entries, reports the count.
    send(
        &mut client,
        b"*4\r\n$5\r\nXTRIM\r\n$1\r\ns\r\n$6\r\nMAXLEN\r\n$1\r\n2\r\n",
    )
    .await;
    expect_reply(&mut client, ":2\r\n").await;

    // Only the two newest entries survive.
    send(
        &mut client,
        b"*4\r\n$6\r\nXRANGE\r\n$1\r\ns\r\n$1\r\n-\r\n$1\r\n+\r\n",
    )
    .await;
    let expected = "*2\r\n\
        *2\r\n$3\r\n3-0\r\n*2\r\n$1\r\nk\r\n$1\r\nv\r\n\
        *2\r\n$3\r\n4-0\r\n*2\r\n$1\r\nk\r\n$1\r\nv\r\n";
    expect_reply(&mut client, expected).await;

    // The high-water mark stayed at 4-0: re-adding it is refused even though
    // the entry itself was trimmed away.
    send(
        &mut client,
        b"*5\r\n$4\r\nXADD\r\n$1\r\ns\r\n$3\r\n4-0\r\n$1\r\nk\r\n$1\r\nv\r\n",
    )
    .await;
    let mut buf = [0u8; 256];
    let n = client.read(&mut buf).await.unwrap();
    assert!(
        buf[..n].starts_with(b"-ERR"),
        "re-adding a trimmed top ID should be refused"
    );

    // A threshold at or above the current length trims nothing.
    send(
        &mut client,
        b"*4\r\n$5\r\nXTRIM\r\n$1\r\ns\r\n$6\r\nMAXLEN\r\n$2\r\n10\r\n",
    )
    .await;
    expect_reply(&mut client, ":0\r\n").await;
}

#[tokio::test]
async fn xadd_with_maxlen_trims_after_each_add_over_tcp() {
    let addr = start_server().await;
    let mut client = connect(addr).await;

    // Each XADD carries its own `MAXLEN 2` clause, so the stream never grows
    // past two entries even though three are added.
    for id in ["1-0", "2-0", "3-0"] {
        let frame = format!(
            "*7\r\n$4\r\nXADD\r\n$1\r\ns\r\n$6\r\nMAXLEN\r\n$1\r\n2\r\n${}\r\n{}\r\n$1\r\nk\r\n$1\r\nv\r\n",
            id.len(),
            id
        );
        send(&mut client, frame.as_bytes()).await;
        let reply = format!("${}\r\n{}\r\n", id.len(), id);
        expect_reply(&mut client, &reply).await;
    }

    // The newest entry (3-0, just added) survives alongside 2-0 — proving
    // the trim ran *after* the append, not before it.
    send(&mut client, b"*2\r\n$4\r\nXLEN\r\n$1\r\ns\r\n").await;
    expect_reply(&mut client, ":2\r\n").await;

    send(
        &mut client,
        b"*4\r\n$6\r\nXRANGE\r\n$1\r\ns\r\n$1\r\n-\r\n$1\r\n+\r\n",
    )
    .await;
    let expected = "*2\r\n\
        *2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nk\r\n$1\r\nv\r\n\
        *2\r\n$3\r\n3-0\r\n*2\r\n$1\r\nk\r\n$1\r\nv\r\n";
    expect_reply(&mut client, expected).await;
}
