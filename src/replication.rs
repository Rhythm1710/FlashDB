//! Replica-side replication: syncing this server's keyspace from a master.
//!
//! When FlashDB is started with `--replicaof <host> <port>` it comes up as a
//! *replica*. Before (and while) it serves its own clients, it dials the master
//! and performs the Redis replication handshake:
//!
//! ```text
//!   replica → master   PING
//!   master  → replica  +PONG
//!   replica → master   REPLCONF listening-port <port>
//!   master  → replica  +OK
//!   replica → master   REPLCONF capa psync2
//!   master  → replica  +OK
//!   replica → master   PSYNC ? -1
//!   master  → replica  +FULLRESYNC <replid> <offset>
//!   master  → replica  $<len>\r\n<len bytes of RDB>      (no trailing CRLF!)
//! ```
//!
//! The bulk RDB that follows the `+FULLRESYNC` line is a full point-in-time
//! snapshot of the master's keyspace, in the exact `dump.rdb` byte format the
//! reader in [`crate::rdb`] already understands. So the *only* new work here is
//! the async handshake and framing that RDB blob off the wire; once we have the
//! bytes we hand them straight to the existing `parse_rdb` + `entries_to_store`
//! path that startup loading already uses.
//!
//! After the snapshot, the replica keeps the link open and applies the master's
//! *ongoing* writes: the master streams every mutating command down the same
//! connection as an ordinary RESP array, and the replica parses each one and
//! runs it against its own store, so the two keyspaces stay in step. That live
//! stream is the second half of this module (see [`ReplConn::stream_writes`]);
//! the master side that produces it lives in [`crate::serve_replica`].

use crate::{entries_to_store, now_unix_ms, process_command, rdb, resp, Server, Store};
use anyhow::{bail, Context, Result};
use bytes::{Buf, BytesMut};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// Connect to the master at `master_addr`, run the replication handshake
/// announcing our own client `listening_port`, load the master's RDB snapshot
/// into `server`'s store, then stay connected and apply the master's ongoing
/// writes as they stream in.
///
/// This returns only when the link ends — the master closing the connection
/// cleanly is `Ok(())`; anything else (unreachable master, malformed reply,
/// unparseable snapshot, a mid-frame drop) is an error the caller logs. Either
/// way the replica keeps serving whatever it already had rather than crashing.
pub async fn sync_from_master(
    master_addr: &str,
    listening_port: u16,
    server: Server,
) -> Result<()> {
    let stream = TcpStream::connect(master_addr)
        .await
        .with_context(|| format!("connecting to master at {master_addr}"))?;
    let mut conn = ReplConn::new(stream);
    let rdb_bytes = conn.handshake(listening_port).await?;
    load_rdb_into_store(&rdb_bytes, &server.store)?;
    // Caught up to the snapshot; now follow the master's live write stream.
    conn.stream_writes(&server).await
}

/// Parse a received RDB image and merge its entries into the live store.
///
/// The heavy lifting is all reused: `parse_rdb` decodes the bytes and
/// `entries_to_store` resolves each key's wall-clock expiry against our
/// monotonic clock (dropping any key already expired), exactly as startup
/// loading does. We take the lock only to copy the resolved keys in.
fn load_rdb_into_store(rdb_bytes: &[u8], store: &Store) -> Result<()> {
    let entries = rdb::parse_rdb(rdb_bytes).context("parsing the master's RDB snapshot")?;
    let loaded = entries_to_store(entries, now_unix_ms(), Instant::now());
    let mut map = store.lock().unwrap();
    for (key, entry) in loaded {
        map.insert(key, entry);
    }
    Ok(())
}

/// A buffered wrapper over the connection to the master.
///
/// It is generic over the stream type (`S: AsyncRead + AsyncWrite`) rather than
/// hard-wired to a [`TcpStream`], so the handshake logic can be driven in unit
/// tests over an in-memory pipe (`tokio::io::duplex`) with no real socket. The
/// `buf` accumulates bytes as they arrive: a reply line, the `+FULLRESYNC`
/// header, and the RDB blob can all land in one read or dribble in over several,
/// and the readers below simply pull from `buf`, topping it up as needed.
struct ReplConn<S> {
    stream: S,
    buf: BytesMut,
}

impl<S> ReplConn<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn new(stream: S) -> Self {
        ReplConn {
            stream,
            buf: BytesMut::with_capacity(4096),
        }
    }

    /// Run the full handshake and return the raw RDB snapshot bytes.
    async fn handshake(&mut self, listening_port: u16) -> Result<Vec<u8>> {
        self.send_command(&["PING"]).await?;
        self.expect_simple("PONG").await?;

        let port = listening_port.to_string();
        self.send_command(&["REPLCONF", "listening-port", &port])
            .await?;
        self.expect_simple("OK").await?;

        self.send_command(&["REPLCONF", "capa", "psync2"]).await?;
        self.expect_simple("OK").await?;

        self.send_command(&["PSYNC", "?", "-1"]).await?;
        let line = self.read_line().await?;
        if !line.starts_with(b"+FULLRESYNC") {
            bail!(
                "expected +FULLRESYNC from master, got {:?}",
                String::from_utf8_lossy(&line)
            );
        }

        self.read_rdb_bulk().await
    }

    /// Serialize `parts` as a RESP array of bulk strings and write it to the
    /// master. This is the same wire shape a normal client uses to send a
    /// command, e.g. `["PING"]` → `*1\r\n$4\r\nPING\r\n`.
    async fn send_command(&mut self, parts: &[&str]) -> Result<()> {
        let mut out = format!("*{}\r\n", parts.len());
        for part in parts {
            out.push_str(&format!("${}\r\n{}\r\n", part.len(), part));
        }
        self.stream.write_all(out.as_bytes()).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Read one more chunk from the socket into `buf`. A zero-length read means
    /// the master hung up mid-handshake, which we treat as an error.
    async fn fill(&mut self) -> Result<()> {
        let n = self.stream.read_buf(&mut self.buf).await?;
        if n == 0 {
            bail!("master closed the connection during the handshake");
        }
        Ok(())
    }

    /// Read one `\r\n`-terminated line, returning its bytes *without* the CRLF.
    /// Keeps reading from the socket until a full line is buffered.
    async fn read_line(&mut self) -> Result<Vec<u8>> {
        loop {
            if let Some(pos) = find_crlf(&self.buf) {
                let line = self.buf.split_to(pos).to_vec();
                self.buf.advance(2); // drop the trailing \r\n
                return Ok(line);
            }
            self.fill().await?;
        }
    }

    /// Expect a RESP simple-string reply (`+<word>\r\n`) whose body equals
    /// `word`. A `-...` error reply from the master, or any other frame, is an
    /// error.
    async fn expect_simple(&mut self, word: &str) -> Result<()> {
        let line = self.read_line().await?;
        match line.split_first() {
            Some((b'+', rest)) if rest == word.as_bytes() => Ok(()),
            Some((b'+', rest)) => bail!(
                "expected +{word} from master, got +{}",
                String::from_utf8_lossy(rest)
            ),
            Some((b'-', rest)) => {
                bail!(
                    "master returned an error: {}",
                    String::from_utf8_lossy(rest)
                )
            }
            _ => bail!(
                "unexpected reply from master: {:?}",
                String::from_utf8_lossy(&line)
            ),
        }
    }

    /// Read the bulk RDB payload that follows `+FULLRESYNC`.
    ///
    /// It is framed like a RESP bulk string — a `$<len>\r\n` header then `len`
    /// bytes — but, unlike a normal bulk string, there is **no** trailing CRLF
    /// after the body (the RDB is binary and may itself end in any bytes). So we
    /// read exactly `len` bytes and stop.
    async fn read_rdb_bulk(&mut self) -> Result<Vec<u8>> {
        let header = self.read_line().await?;
        let Some((b'$', rest)) = header.split_first() else {
            bail!(
                "expected an RDB bulk header ($<len>) from master, got {:?}",
                String::from_utf8_lossy(&header)
            );
        };
        let len = parse_len(rest).context("parsing the RDB bulk length")?;
        while self.buf.len() < len {
            self.fill().await?;
        }
        Ok(self.buf.split_to(len).to_vec())
    }

    /// Follow the master's live write stream until the link ends.
    ///
    /// After the snapshot, the master forwards each write it applies as an
    /// ordinary RESP command array on this same connection. We read them one
    /// frame at a time and replay each against our own store via
    /// [`process_command`], discarding the reply — a replica doesn't answer the
    /// master for propagated writes. A clean close by the master ends the loop
    /// with `Ok(())`.
    async fn stream_writes(&mut self, server: &Server) -> Result<()> {
        while let Some(command) = self.read_command().await? {
            // The reply is intentionally dropped: this is one-way replication,
            // and `process_command` already applied the change to our store.
            let _ = process_command(command, server);
        }
        Ok(())
    }

    /// Read the next whole RESP command frame the master sent, or `None` when
    /// the master closes the connection cleanly between frames.
    ///
    /// Reuses the same incremental parser the server uses for client input, so
    /// a command split across TCP reads is reassembled and several commands
    /// arriving in one read are drained one call at a time.
    async fn read_command(&mut self) -> Result<Option<resp::Value>> {
        loop {
            match resp::parse_message(&self.buf) {
                Ok(Some((value, consumed))) => {
                    self.buf.advance(consumed);
                    return Ok(Some(value));
                }
                // Not a full frame yet — pull more bytes, or stop if the master
                // hung up. A clean close mid-buffer (partial frame) is an error.
                Ok(None) => {
                    let n = self.stream.read_buf(&mut self.buf).await?;
                    if n == 0 {
                        if self.buf.is_empty() {
                            return Ok(None);
                        }
                        bail!("master closed the connection mid-frame");
                    }
                }
                Err(e) => bail!("protocol error in the master's command stream: {e}"),
            }
        }
    }
}

/// Find the byte offset of the first `\r\n` in `buf`, or `None` if there isn't a
/// complete line yet. Returns the index of the `\r`.
fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|pair| pair == b"\r\n")
}

/// Parse the digits after a `$` length header into a byte count.
fn parse_len(digits: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(digits).context("bulk length is not valid UTF-8")?;
    text.trim()
        .parse::<usize>()
        .with_context(|| format!("bulk length {text:?} is not a non-negative integer"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdb::RdbEntry;
    use crate::StoredValue;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;

    #[test]
    fn find_crlf_locates_the_line_break() {
        assert_eq!(find_crlf(b"+OK\r\n"), Some(3));
        assert_eq!(find_crlf(b"\r\nrest"), Some(0));
        assert_eq!(find_crlf(b"no break yet"), None);
        // A lone \r without the \n is not a line break.
        assert_eq!(find_crlf(b"abc\rdef"), None);
    }

    #[test]
    fn parse_len_reads_a_byte_count() {
        assert_eq!(parse_len(b"88").unwrap(), 88);
        assert_eq!(parse_len(b"0").unwrap(), 0);
        assert!(parse_len(b"-1").is_err());
        assert!(parse_len(b"notanumber").is_err());
    }

    /// Assemble the exact bytes a master sends across the whole handshake:
    /// three simple-string replies, the `+FULLRESYNC` line, then the RDB bulk
    /// header and body.
    fn scripted_master_replies(rdb: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"+PONG\r\n");
        out.extend_from_slice(b"+OK\r\n");
        out.extend_from_slice(b"+OK\r\n");
        out.extend_from_slice(b"+FULLRESYNC 8371b4fb1155b71f4a04d3e1bc3e18c4a990aeeb 0\r\n");
        out.extend_from_slice(format!("${}\r\n", rdb.len()).as_bytes());
        out.extend_from_slice(rdb);
        out
    }

    #[tokio::test]
    async fn handshake_returns_the_rdb_body_after_fullresync() {
        // The RDB body here is opaque to the handshake — it just needs to be
        // returned verbatim, so arbitrary bytes (including an embedded CRLF and
        // a trailing 0xFF) prove the length framing, not the RDB parser.
        let rdb = vec![0x52, 0x45, 0x0d, 0x0a, 0x00, 0xff];
        let (client, mut server) = tokio::io::duplex(4096);

        let replies = scripted_master_replies(&rdb);
        let master = tokio::spawn(async move {
            server.write_all(&replies).await.unwrap();
            // Drain the replica's handshake commands so the pipe stays open
            // until the replica finishes reading and drops its end.
            let mut sink = Vec::new();
            let _ = server.read_to_end(&mut sink).await;
        });

        let mut conn = ReplConn::new(client);
        let got = conn.handshake(6380).await.unwrap();
        assert_eq!(got, rdb);
        // Drop our end so the master's `read_to_end` sees EOF and its task ends.
        drop(conn);
        master.await.unwrap();
    }

    #[tokio::test]
    async fn a_real_snapshot_round_trips_into_the_store() {
        // A genuine RDB written by our own serializer, shipped over the wire and
        // loaded end-to-end — proving the handshake bytes hand off cleanly to
        // the existing RDB reader.
        let entries = vec![
            RdbEntry {
                key: "greeting".to_string(),
                value: StoredValue::Str("hello".to_string()),
                expire_at_ms: None,
            },
            RdbEntry {
                key: "count".to_string(),
                value: StoredValue::Str("42".to_string()),
                expire_at_ms: None,
            },
        ];
        let rdb = rdb::write::serialize(&entries);
        let (client, mut server) = tokio::io::duplex(8192);

        let replies = scripted_master_replies(&rdb);
        let master = tokio::spawn(async move {
            server.write_all(&replies).await.unwrap();
            let mut sink = Vec::new();
            let _ = server.read_to_end(&mut sink).await;
        });

        let mut conn = ReplConn::new(client);
        let rdb_bytes = conn.handshake(6380).await.unwrap();
        drop(conn); // let the master task observe EOF and finish
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        load_rdb_into_store(&rdb_bytes, &store).unwrap();

        // Scope the guard in its own block so it can't be held across the await
        // below (which clippy's await_holding_lock rightly forbids).
        {
            let map = store.lock().unwrap();
            assert!(
                matches!(map.get("greeting").map(|e| &e.value), Some(StoredValue::Str(s)) if s == "hello")
            );
            assert!(
                matches!(map.get("count").map(|e| &e.value), Some(StoredValue::Str(s)) if s == "42")
            );
        }
        master.await.unwrap();
    }

    #[tokio::test]
    async fn an_error_reply_from_the_master_aborts_the_handshake() {
        let (client, mut server) = tokio::io::duplex(1024);
        let master = tokio::spawn(async move {
            // Master refuses at the very first step.
            server.write_all(b"-ERR go away\r\n").await.unwrap();
            let mut sink = Vec::new();
            let _ = server.read_to_end(&mut sink).await;
        });

        let mut conn = ReplConn::new(client);
        let err = conn.handshake(6380).await.unwrap_err();
        assert!(err.to_string().contains("go away"));
        drop(conn);
        master.await.unwrap();
    }
}
