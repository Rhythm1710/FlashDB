//! Development WebSocket-to-RESP bridge for the static playground.
//!
//! This binary is optional. The `flashdb` server never links it and never
//! speaks HTTP. The bridge serves the files in `playground/` and, for each
//! text frame a browser sends on `/ws`, writes one RESP command to a FlashDB
//! (or Redis) socket and returns the reply as text.
//!
//! ```sh
//! cargo run                       # FlashDB on 127.0.0.1:6379
//! cargo run --example ws_bridge   # page + bridge on 127.0.0.1:8080
//! ```

use anyhow::{anyhow, Context, Result};
use flashdb::resp::{self, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const HELP: &str = "\
Usage: ws_bridge [--listen ADDR] [--flashdb ADDR] [--dir PATH] [--allow-origin ORIGIN]

  --listen ADDR         HTTP and WebSocket bind address (default 127.0.0.1:8080)
  --flashdb ADDR        RESP database to dial (default 127.0.0.1:6379)
  --dir PATH            static playground directory (default <crate>/playground)
  --allow-origin ORIGIN extra browser Origin allowed to open /ws (repeatable)
";

struct BridgeConfig {
    listen: std::net::SocketAddr,
    flashdb: String,
    root: PathBuf,
    allow_origins: Vec<String>,
}

struct Request {
    path: String,
    websocket_key: Option<String>,
    host: Option<String>,
    origin: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = match parse_args() {
        Ok(config) => config,
        Err(msg) => {
            eprintln!("ws_bridge: {msg}");
            std::process::exit(1);
        }
    };
    if !config.root.join("index.html").is_file() {
        eprintln!(
            "ws_bridge: playground files not found in {}",
            config.root.display()
        );
        std::process::exit(1);
    }
    let listener = TcpListener::bind(config.listen).await?;
    println!(
        "FlashDB playground at http://{} (database {})",
        config.listen, config.flashdb
    );
    let root = Arc::new(config.root);
    let allow_origins = Arc::new(config.allow_origins);
    loop {
        let (stream, _) = listener.accept().await?;
        let flashdb = config.flashdb.clone();
        let root = Arc::clone(&root);
        let allow_origins = Arc::clone(&allow_origins);
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, &flashdb, &root, &allow_origins).await {
                eprintln!("playground client error: {e:#}");
            }
        });
    }
}

fn parse_args() -> std::result::Result<BridgeConfig, String> {
    let mut listen = "127.0.0.1:8080".to_string();
    let mut flashdb = "127.0.0.1:6379".to_string();
    let mut dir = None;
    let mut allow_origins = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--listen" => listen = value()?,
            "--flashdb" => flashdb = value()?,
            "--dir" => dir = Some(value()?),
            "--allow-origin" => allow_origins.push(value()?),
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    let listen = listen
        .parse()
        .map_err(|_| format!("invalid listen address '{listen}'"))?;
    let root = match dir {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("playground"),
    };
    Ok(BridgeConfig {
        listen,
        flashdb,
        root,
        allow_origins,
    })
}

async fn handle_client(
    mut stream: TcpStream,
    flashdb: &str,
    root: &Path,
    allow_origins: &[String],
) -> Result<()> {
    let head = read_headers(&mut stream).await?;
    let request = parse_request(&head)?;
    if request.path == "/ws" {
        if !origin_allowed(
            request.origin.as_deref(),
            request.host.as_deref(),
            allow_origins,
        ) {
            return write_http(
                &mut stream,
                403,
                "Forbidden",
                "text/plain; charset=utf-8",
                b"origin is not allowed",
            )
            .await;
        }
        let key = request
            .websocket_key
            .context("WebSocket upgrade is missing Sec-WebSocket-Key")?;
        return websocket_session(&mut stream, &key, flashdb).await;
    }
    serve_static(&mut stream, root, &request.path).await
}

/// Browsers send `Origin` on a WebSocket handshake. The default is the page's
/// own origin: `Origin` must be `http(s)://` plus the request `Host`. A
/// missing `Origin` is allowed so non-browser clients can still connect.
/// `--allow-origin` adds further exact origins.
fn origin_allowed(origin: Option<&str>, host: Option<&str>, allow_origins: &[String]) -> bool {
    let Some(origin) = origin.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    if allow_origins.iter().any(|allowed| {
        allowed
            .trim()
            .trim_end_matches('/')
            .eq_ignore_ascii_case(origin.trim().trim_end_matches('/'))
    }) {
        return true;
    }
    let Some(host) = host.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    let Some((_, rest)) = origin.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority.eq_ignore_ascii_case(host)
}

async fn read_headers(stream: &mut TcpStream) -> Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 16 * 1024 {
            anyhow::bail!("HTTP headers are too large");
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("client closed before sending headers");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8(buf).context("HTTP headers are not UTF-8")?;
    let end = text
        .find("\r\n\r\n")
        .context("HTTP headers are truncated")?;
    Ok(text[..end].to_string())
}

fn parse_request(head: &str) -> Result<Request> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("/");
    if method != "GET" {
        anyhow::bail!("only GET is supported");
    }
    let path = raw_path.split('?').next().unwrap_or("/").to_string();
    let mut websocket_key = None;
    let mut host = None;
    let mut origin = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        if name.eq_ignore_ascii_case("Sec-WebSocket-Key") {
            websocket_key = Some(value);
        } else if name.eq_ignore_ascii_case("Host") {
            host = Some(value);
        } else if name.eq_ignore_ascii_case("Origin") {
            origin = Some(value);
        }
    }
    Ok(Request {
        path,
        websocket_key,
        host,
        origin,
    })
}

async fn serve_static(stream: &mut TcpStream, root: &Path, path: &str) -> Result<()> {
    let Some((name, content_type)) = static_file(path) else {
        return write_http(
            stream,
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            b"not found",
        )
        .await;
    };
    match std::fs::read(root.join(name)) {
        Ok(body) => write_http(stream, 200, "OK", content_type, &body).await,
        Err(_) => {
            write_http(
                stream,
                404,
                "Not Found",
                "text/plain; charset=utf-8",
                b"not found",
            )
            .await
        }
    }
}

fn static_file(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/" | "/index.html" => Some(("index.html", "text/html; charset=utf-8")),
        "/app.js" => Some(("app.js", "text/javascript; charset=utf-8")),
        "/style.css" => Some(("style.css", "text/css; charset=utf-8")),
        "/manifest.webmanifest" => Some(("manifest.webmanifest", "application/manifest+json")),
        "/sw.js" => Some(("sw.js", "text/javascript; charset=utf-8")),
        "/icon-192.png" => Some(("icon-192.png", "image/png")),
        "/icon-512.png" => Some(("icon-512.png", "image/png")),
        _ => None,
    }
}

async fn write_http(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

async fn websocket_session(stream: &mut TcpStream, key: &str, flashdb: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        ws_accept(key)
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;

    let mut db = None;
    let mut buf = Vec::new();
    loop {
        let Some(line) = read_ws_text(stream).await? else {
            break;
        };
        let reply = match run_command(&mut db, &mut buf, flashdb, &line).await {
            Ok(text) => text,
            Err(e) => format!("(error) {e:#}"),
        };
        if !reply.is_empty() {
            write_ws_text(stream, &reply).await?;
        }
    }
    Ok(())
}

async fn run_command(
    db: &mut Option<TcpStream>,
    buf: &mut Vec<u8>,
    addr: &str,
    line: &str,
) -> Result<String> {
    let parts = split_command(line).map_err(|e| anyhow!(e))?;
    if parts.is_empty() {
        return Ok(String::new());
    }
    if db.is_none() {
        *db = Some(
            TcpStream::connect(addr)
                .await
                .with_context(|| format!("connecting to {addr}"))?,
        );
        buf.clear();
    }
    let payload = encode_resp(&parts);
    let result = round_trip(
        db.as_mut().expect("database socket just opened"),
        buf,
        &payload,
    )
    .await;
    match result {
        Ok(value) => Ok(format_reply(&value)),
        Err(e) => {
            *db = None;
            buf.clear();
            Err(e)
        }
    }
}

async fn round_trip(conn: &mut TcpStream, buf: &mut Vec<u8>, payload: &str) -> Result<Value> {
    conn.write_all(payload.as_bytes()).await?;
    conn.flush().await?;
    read_resp(conn, buf).await
}

async fn read_resp(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Value> {
    loop {
        if let Some((value, n)) = resp::parse_message(buf)? {
            buf.drain(..n);
            return Ok(value);
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("database closed the connection");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn encode_resp(parts: &[String]) -> String {
    let mut out = format!("*{}\r\n", parts.len());
    for part in parts {
        out.push_str(&format!("${}\r\n{part}\r\n", part.len()));
    }
    out
}

/// Split a playground command line on whitespace, keeping double-quoted
/// stretches together. A backslash inside quotes escapes the next character.
fn split_command(line: &str) -> std::result::Result<Vec<String>, String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escape = false;
    let mut any = false;
    for ch in line.chars() {
        if escape {
            current.push(ch);
            escape = false;
            any = true;
            continue;
        }
        match ch {
            '\\' if in_quotes => escape = true,
            '"' => in_quotes = !in_quotes,
            ch if ch.is_whitespace() && !in_quotes => {
                if any {
                    parts.push(std::mem::take(&mut current));
                    any = false;
                }
            }
            ch => {
                current.push(ch);
                any = true;
            }
        }
    }
    if in_quotes || escape {
        return Err("unterminated quote".to_string());
    }
    if any {
        parts.push(current);
    }
    Ok(parts)
}

fn format_reply(value: &Value) -> String {
    fn scalar(value: &Value) -> String {
        match value {
            Value::SimpleString(text) | Value::BulkString(text) => text.clone(),
            Value::Error(text) => format!("(error) {text}"),
            Value::Integer(n) => format!("(integer) {n}"),
            Value::Null | Value::NullArray => "(nil)".to_string(),
            Value::Array(_) => String::new(),
        }
    }
    fn rec(value: &Value, indent: usize) -> String {
        let pad = "  ".repeat(indent);
        match value {
            Value::Array(items) if items.is_empty() => format!("{pad}(empty array)"),
            Value::Array(items) => items
                .iter()
                .enumerate()
                .map(|(i, item)| match item {
                    Value::Array(_) => format!("{pad}{})\n{}", i + 1, rec(item, indent + 1)),
                    other => format!("{pad}{}) {}", i + 1, scalar(other)),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            other => format!("{pad}{}", scalar(other)),
        }
    }
    rec(value, 0)
}

async fn read_ws_text(stream: &mut TcpStream) -> Result<Option<String>> {
    let mut message = Vec::new();
    let mut started = false;
    loop {
        let (fin, opcode, payload) = read_frame(stream).await?;
        match opcode {
            0x1 => {
                message = payload;
                started = true;
            }
            0x0 if started => message.extend_from_slice(&payload),
            0x8 => {
                let _ = write_frame(stream, 0x8, &[]).await;
                return Ok(None);
            }
            0x9 => {
                write_frame(stream, 0xA, &payload).await?;
                continue;
            }
            0xA => continue,
            _ => anyhow::bail!("unsupported websocket opcode {opcode}"),
        }
        if fin && started {
            return Ok(Some(
                String::from_utf8(message).context("text frame is not UTF-8")?,
            ));
        }
    }
}

async fn read_frame(stream: &mut TcpStream) -> Result<(bool, u8, Vec<u8>)> {
    let header = read_exact(stream, 2).await?;
    let fin = header[0] & 0x80 != 0;
    let opcode = header[0] & 0x0F;
    let masked = header[1] & 0x80 != 0;
    let mut len = (header[1] & 0x7F) as u64;
    if len == 126 {
        let bytes = read_exact(stream, 2).await?;
        len = u16::from_be_bytes([bytes[0], bytes[1]]) as u64;
    } else if len == 127 {
        let bytes = read_exact(stream, 8).await?;
        len = u64::from_be_bytes(bytes.as_slice().try_into().expect("8 bytes"));
        if len & 0x8000_0000_0000_0000 != 0 {
            anyhow::bail!("websocket frame length has the reserved high bit set");
        }
    }
    if len > 1_000_000 {
        anyhow::bail!("websocket frame is too large");
    }
    if !masked {
        anyhow::bail!("client websocket frames must be masked");
    }
    let mask = read_exact(stream, 4).await?;
    let mut payload = read_exact(stream, len as usize).await?;
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
    Ok((fin, opcode, payload))
}

async fn read_exact(stream: &mut TcpStream, n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn write_ws_text(stream: &mut TcpStream, text: &str) -> Result<()> {
    write_frame(stream, 0x1, text.as_bytes()).await
}

async fn write_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> Result<()> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | opcode);
    if payload.len() < 126 {
        out.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    stream.write_all(&out).await?;
    stream.flush().await?;
    Ok(())
}

fn ws_accept(key: &str) -> String {
    let mut material = String::with_capacity(key.len() + WS_GUID.len());
    material.push_str(key);
    material.push_str(WS_GUID);
    base64(&sha1(material.as_bytes()))
}

fn sha1(message: &[u8]) -> [u8; 20] {
    let bit_len = (message.len() as u64).wrapping_mul(8);
    let mut data = message.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());

    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    for chunk in data.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            let start = i * 4;
            *word = u32::from_be_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | data[i + 2] as u32;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
        i += 3;
    }
    match data.len() - i {
        1 => {
            let n = (data[i] as u32) << 16;
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_the_empty_string_and_abc() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn websocket_accept_matches_the_rfc_example() {
        assert_eq!(
            ws_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn split_command_keeps_quoted_spaces_and_rejects_a_dangling_quote() {
        assert_eq!(
            split_command(r#"SET greeting "hello world""#).unwrap(),
            vec!["SET", "greeting", "hello world"]
        );
        assert_eq!(
            split_command(r#"SET q "say \"hi\"""#).unwrap(),
            vec!["SET", "q", "say \"hi\""]
        );
        assert!(split_command("SET greeting \"oops").is_err());
    }

    #[test]
    fn websocket_origin_defaults_to_the_request_host() {
        let allow = Vec::new();
        assert!(origin_allowed(None, Some("127.0.0.1:8080"), &allow));
        assert!(origin_allowed(
            Some("http://127.0.0.1:8080"),
            Some("127.0.0.1:8080"),
            &allow
        ));
        assert!(!origin_allowed(
            Some("http://evil.example"),
            Some("127.0.0.1:8080"),
            &allow
        ));
        assert!(!origin_allowed(Some("http://127.0.0.1:8080"), None, &allow));
        let allow = vec!["https://phone.example".to_string()];
        assert!(origin_allowed(
            Some("https://phone.example"),
            Some("192.168.1.20:8080"),
            &allow
        ));
        assert!(origin_allowed(
            Some("https://phone.example/"),
            Some("192.168.1.20:8080"),
            &allow
        ));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
