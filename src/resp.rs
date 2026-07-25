use anyhow::Result;
use bytes::{Buf, BytesMut};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    SimpleString(String),
    Error(String),
    Integer(i64),
    BulkString(String),
    Array(Vec<Value>),
    Null,
}

impl Value {
    /// Serialize a value into its RESP wire representation.
    ///
    /// This is total: every variant has a representation, so serializing can
    /// never panic (a cache miss returns `Null` -> `$-1\r\n`).
    pub fn serialize(&self) -> String {
        match self {
            Value::SimpleString(s) => format!("+{}\r\n", s),
            Value::Error(s) => format!("-{}\r\n", s),
            Value::Integer(i) => format!(":{}\r\n", i),
            // RESP bulk length is measured in bytes, not chars.
            Value::BulkString(s) => format!("${}\r\n{}\r\n", s.len(), s),
            Value::Null => "$-1\r\n".to_string(),
            Value::Array(items) => {
                let mut out = format!("*{}\r\n", items.len());
                for item in items {
                    out.push_str(&item.serialize());
                }
                out
            }
        }
    }
}

/// A client sent bytes that can never become a valid RESP frame.
///
/// This is distinct from *incomplete* input (more bytes may still arrive on
/// the socket); parse functions signal that case with `Ok(None)` instead.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("Protocol error: {0}")]
pub struct ProtocolError(pub String);

/// Outcome of trying to parse one frame from a buffer: either a complete
/// value plus the number of bytes it occupied, or `None` for "need more
/// bytes" — the caller should keep the buffer and read again.
type Parsed = Result<Option<(Value, usize)>, ProtocolError>;

fn protocol_err(msg: impl Into<String>) -> ProtocolError {
    ProtocolError(msg.into())
}

/// Try to parse a single RESP frame from the start of `buffer`.
///
/// Never panics and never reads past the end of the buffer: truncated
/// frames yield `Ok(None)` so the caller can accumulate more bytes.
pub fn parse_message(buffer: &[u8]) -> Parsed {
    let Some(first) = buffer.first() else {
        return Ok(None);
    };
    match first {
        b'+' => parse_simple_string(buffer),
        b'-' => parse_error(buffer),
        b':' => parse_integer(buffer),
        b'*' => parse_array(buffer),
        b'$' => parse_bulk_string(buffer),
        other => Err(protocol_err(format!(
            "invalid frame type byte {:?}",
            *other as char
        ))),
    }
}

fn parse_simple_string(buffer: &[u8]) -> Parsed {
    let Some((line, len)) = read_until_crlf(&buffer[1..]) else {
        return Ok(None);
    };
    let string = as_utf8(line)?;
    Ok(Some((Value::SimpleString(string), len + 1)))
}

fn parse_error(buffer: &[u8]) -> Parsed {
    let Some((line, len)) = read_until_crlf(&buffer[1..]) else {
        return Ok(None);
    };
    let string = as_utf8(line)?;
    Ok(Some((Value::Error(string), len + 1)))
}

fn parse_integer(buffer: &[u8]) -> Parsed {
    let Some((line, len)) = read_until_crlf(&buffer[1..]) else {
        return Ok(None);
    };
    Ok(Some((Value::Integer(parse_int(line)?), len + 1)))
}

fn parse_array(buffer: &[u8]) -> Parsed {
    let Some((line, header_len)) = read_until_crlf(&buffer[1..]) else {
        return Ok(None);
    };
    let array_length = parse_int(line)?;
    let mut bytes_consumed = header_len + 1;

    // `*-1\r\n` is RESP's null array.
    if array_length == -1 {
        return Ok(Some((Value::Null, bytes_consumed)));
    }
    if array_length < 0 {
        return Err(protocol_err(format!("invalid array length {array_length}")));
    }

    let mut items = Vec::with_capacity(array_length as usize);
    for _ in 0..array_length {
        // A truncated element makes the whole array incomplete.
        let Some((item, len)) = parse_message(&buffer[bytes_consumed..])? else {
            return Ok(None);
        };
        items.push(item);
        bytes_consumed += len;
    }
    Ok(Some((Value::Array(items), bytes_consumed)))
}

fn parse_bulk_string(buffer: &[u8]) -> Parsed {
    let Some((line, header_len)) = read_until_crlf(&buffer[1..]) else {
        return Ok(None);
    };
    let declared_len = parse_int(line)?;
    let header = header_len + 1;

    // `$-1\r\n` is RESP's null bulk string.
    if declared_len == -1 {
        return Ok(Some((Value::Null, header)));
    }
    if declared_len < 0 {
        return Err(protocol_err(format!(
            "invalid bulk string length {declared_len}"
        )));
    }

    let body_len = declared_len as usize;
    let total = header + body_len + 2;
    // Bounds check before slicing: a truncated body is "not yet", not "bad".
    if buffer.len() < total {
        return Ok(None);
    }
    let body = &buffer[header..header + body_len];
    if &buffer[header + body_len..total] != b"\r\n" {
        return Err(protocol_err("bulk string missing trailing CRLF"));
    }
    Ok(Some((Value::BulkString(as_utf8(body)?), total)))
}

fn read_until_crlf(buffer: &[u8]) -> Option<(&[u8], usize)> {
    for i in 1..buffer.len() {
        if buffer[i - 1] == b'\r' && buffer[i] == b'\n' {
            return Some((&buffer[0..(i - 1)], i + 1));
        }
    }
    None
}

fn as_utf8(bytes: &[u8]) -> Result<String, ProtocolError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| protocol_err("invalid UTF-8 in frame"))
}

fn parse_int(buffer: &[u8]) -> Result<i64, ProtocolError> {
    as_utf8(buffer)?
        .parse::<i64>()
        .map_err(|_| protocol_err("invalid integer in frame"))
}

pub struct RespHandler {
    stream: TcpStream,
    buffer: BytesMut,
}

impl RespHandler {
    pub fn new(stream: TcpStream) -> Self {
        RespHandler {
            stream,
            buffer: BytesMut::with_capacity(512),
        }
    }

    /// Read one complete RESP value from the connection.
    ///
    /// Bytes accumulate in `self.buffer` across reads, so a frame split over
    /// several TCP segments is reassembled, and when a client pipelines
    /// several commands into one segment we consume exactly one frame per
    /// call — the rest stay buffered for the next call.
    pub async fn read_value(&mut self) -> Result<Option<Value>> {
        loop {
            if let Some((value, consumed)) = parse_message(&self.buffer)? {
                self.buffer.advance(consumed);
                return Ok(Some(value));
            }
            // Not enough buffered bytes for a full frame — read more.
            let bytes_read = self.stream.read_buf(&mut self.buffer).await?;
            if bytes_read == 0 {
                if self.buffer.is_empty() {
                    return Ok(None); // clean disconnect between frames
                }
                anyhow::bail!("connection closed mid-frame");
            }
        }
    }

    pub async fn write_value(&mut self, value: Value) -> Result<()> {
        // write_all guarantees the whole response is flushed; write() could
        // report a short write and silently truncate the reply.
        self.stream.write_all(value.serialize().as_bytes()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_complete(input: &str) -> (Value, usize) {
        parse_message(input.as_bytes())
            .expect("valid frame")
            .expect("complete frame")
    }

    #[test]
    fn serialize_simple_string() {
        assert_eq!(Value::SimpleString("OK".into()).serialize(), "+OK\r\n");
    }

    #[test]
    fn serialize_error() {
        assert_eq!(Value::Error("ERR bad".into()).serialize(), "-ERR bad\r\n");
    }

    #[test]
    fn serialize_integer() {
        assert_eq!(Value::Integer(3).serialize(), ":3\r\n");
    }

    #[test]
    fn serialize_bulk_string_uses_byte_length() {
        assert_eq!(Value::BulkString("hey".into()).serialize(), "$3\r\nhey\r\n");
    }

    #[test]
    fn serialize_null_does_not_panic() {
        // Regression: a cache miss returns Null and must serialize, not panic.
        assert_eq!(Value::Null.serialize(), "$-1\r\n");
    }

    #[test]
    fn serialize_array() {
        let v = Value::Array(vec![Value::BulkString("a".into()), Value::Integer(1)]);
        assert_eq!(v.serialize(), "*2\r\n$1\r\na\r\n:1\r\n");
    }

    #[test]
    fn parse_array_of_bulk_strings() {
        let (value, consumed) = parse_complete("*2\r\n$4\r\nECHO\r\n$3\r\nhey\r\n");
        assert_eq!(consumed, 23);
        assert_eq!(
            value,
            Value::Array(vec![
                Value::BulkString("ECHO".into()),
                Value::BulkString("hey".into()),
            ])
        );
    }

    #[test]
    fn parse_integer_and_error_frames() {
        assert_eq!(parse_complete(":42\r\n"), (Value::Integer(42), 5));
        assert_eq!(
            parse_complete("-ERR oops\r\n"),
            (Value::Error("ERR oops".into()), 11)
        );
    }

    #[test]
    fn parse_null_bulk_and_null_array() {
        assert_eq!(parse_complete("$-1\r\n"), (Value::Null, 5));
        assert_eq!(parse_complete("*-1\r\n"), (Value::Null, 5));
    }

    #[test]
    fn round_trip_command() {
        let original = Value::Array(vec![
            Value::BulkString("SET".into()),
            Value::BulkString("key".into()),
            Value::BulkString("value".into()),
        ]);
        let wire = original.serialize();
        let (parsed, consumed) = parse_complete(&wire);
        assert_eq!(parsed, original);
        assert_eq!(consumed, wire.len());
    }

    #[test]
    fn incomplete_frames_ask_for_more_bytes() {
        // Every truncation of a valid frame must be Ok(None), never a panic
        // or an error.
        let full = "*2\r\n$4\r\nECHO\r\n$3\r\nhey\r\n";
        for end in 0..full.len() {
            let result = parse_message(&full.as_bytes()[..end]);
            assert_eq!(result, Ok(None), "truncated at byte {end}");
        }
    }

    #[test]
    fn incomplete_bulk_string_body_is_not_an_error() {
        // Header says 5 bytes but only 3 arrived so far.
        assert_eq!(parse_message(b"$5\r\nhel"), Ok(None));
    }

    #[test]
    fn pipelined_frames_parse_one_at_a_time() {
        let wire = b"*1\r\n$4\r\nPING\r\n*2\r\n$4\r\nECHO\r\n$2\r\nhi\r\n";
        let (first, consumed) = parse_message(wire).unwrap().unwrap();
        assert_eq!(first, Value::Array(vec![Value::BulkString("PING".into())]));
        let (second, rest) = parse_message(&wire[consumed..]).unwrap().unwrap();
        assert_eq!(
            second,
            Value::Array(vec![
                Value::BulkString("ECHO".into()),
                Value::BulkString("hi".into()),
            ])
        );
        assert_eq!(consumed + rest, wire.len());
    }

    #[test]
    fn unknown_type_byte_is_a_protocol_error() {
        assert!(parse_message(b"!bad\r\n").is_err());
    }

    #[test]
    fn bad_lengths_are_protocol_errors() {
        assert!(parse_message(b"$abc\r\n").is_err());
        assert!(parse_message(b"$-5\r\n").is_err());
        assert!(parse_message(b"*-5\r\n").is_err());
        assert!(parse_message(b":notanum\r\n").is_err());
    }

    #[test]
    fn bulk_string_body_must_end_with_crlf() {
        // Declared length 3, but the frame continues with junk instead of
        // CRLF — previously this sliced blindly; now it is a clean error.
        assert!(parse_message(b"$3\r\nheyXX").is_err());
    }
}
