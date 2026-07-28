//! End-to-end tests that start a real FlashDB server on an ephemeral port and
//! drive it over a genuine TCP socket, sending raw RESP bytes and asserting on
//! the raw RESP bytes that come back. Where the parser unit tests prove single
//! frames parse correctly in isolation, these prove the whole pipeline —
//! accept, read, dispatch, reply — behaves against a live client.

use std::time::Duration;
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
