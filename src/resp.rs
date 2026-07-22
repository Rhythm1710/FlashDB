use anyhow::Result;
use bytes::BytesMut;
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

    pub async fn read_value(&mut self) -> Result<Option<Value>> {
        let bytes_read = self.stream.read_buf(&mut self.buffer).await?;
        if bytes_read == 0 {
            return Ok(None);
        }
        let (v, _) = parse_message(self.buffer.split())?;
        Ok(Some(v))
    }

    pub async fn write_value(&mut self, value: Value) -> Result<()> {
        // write_all guarantees the whole response is flushed; write() could
        // report a short write and silently truncate the reply.
        self.stream.write_all(value.serialize().as_bytes()).await?;
        Ok(())
    }
}

fn parse_message(buffer: BytesMut) -> Result<(Value, usize)> {
    match buffer[0] as char {
        '+' => parse_simple_string(buffer),
        '*' => parse_array(buffer),
        '$' => parse_bulk_string(buffer),
        _ => Err(anyhow::anyhow!("Not a known value type {:?}", buffer)),
    }
}

fn parse_simple_string(buffer: BytesMut) -> Result<(Value, usize)> {
    if let Some((line, len)) = read_until_crlf(&buffer[1..]) {
        let string = String::from_utf8(line.to_vec())?;
        Ok((Value::SimpleString(string), len + 1))
    } else {
        Err(anyhow::anyhow!("Invalid string {:?}", buffer))
    }
}

fn parse_array(buffer: BytesMut) -> Result<(Value, usize)> {
    let (array_length, mut bytes_consumed) =
        if let Some((line, len)) = read_until_crlf(&buffer[1..]) {
            (parse_int(line)?, len + 1)
        } else {
            return Err(anyhow::anyhow!("Invalid array format {:?}", buffer));
        };
    let mut items = vec![];
    for _ in 0..array_length {
        let (array_item, len) = parse_message(BytesMut::from(&buffer[bytes_consumed..]))?;
        items.push(array_item);
        bytes_consumed += len;
    }
    Ok((Value::Array(items), bytes_consumed))
}

fn parse_bulk_string(buffer: BytesMut) -> Result<(Value, usize)> {
    let (bulk_str_len, bytes_consumed) = if let Some((line, len)) = read_until_crlf(&buffer[1..]) {
        (parse_int(line)?, len + 1)
    } else {
        return Err(anyhow::anyhow!("Invalid bulk string format {:?}", buffer));
    };
    let end_of_bulk_str = bytes_consumed + bulk_str_len as usize;
    let total_parsed = end_of_bulk_str + 2;
    let s = String::from_utf8(buffer[bytes_consumed..end_of_bulk_str].to_vec())?;
    Ok((Value::BulkString(s), total_parsed))
}

fn read_until_crlf(buffer: &[u8]) -> Option<(&[u8], usize)> {
    for i in 1..buffer.len() {
        if buffer[i - 1] == b'\r' && buffer[i] == b'\n' {
            return Some((&buffer[0..(i - 1)], i + 1));
        }
    }
    None
}

fn parse_int(buffer: &[u8]) -> Result<i64> {
    Ok(String::from_utf8(buffer.to_vec())?.parse::<i64>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let buf = BytesMut::from("*2\r\n$4\r\nECHO\r\n$3\r\nhey\r\n");
        let (value, _) = parse_message(buf).unwrap();
        assert_eq!(
            value,
            Value::Array(vec![
                Value::BulkString("ECHO".into()),
                Value::BulkString("hey".into()),
            ])
        );
    }
}
