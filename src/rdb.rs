//! Reading Redis's on-disk snapshot format (RDB).
//!
//! An RDB file is a compact binary dump of the whole keyspace: a short header,
//! then a stream of length-prefixed keys and values, then an end marker and a
//! checksum. This module turns those bytes back into keys and values so a
//! freshly-started FlashDB can pick up where a previous run left off.
//!
//! Two layers live here. The lower one is the pair of decoders every value in
//! an RDB file is built from — the *length encoding* and the *string encoding*
//! — plus the byte cursor they read through. The upper one, [`parse_rdb`],
//! walks the file's header and opcode stream and materialises each key into an
//! [`RdbEntry`]. Strings, lists, and hashes are understood in their classic
//! (uncompressed) encodings; the loader turns the entries into live store
//! entries. The inverse — writing an in-memory keyspace back out — lives in the
//! [`write`] submodule, so a snapshot FlashDB saves loads straight back in.

use crate::StoredValue;
use std::collections::{HashMap, VecDeque};

/// Everything that can go wrong while decoding an RDB file.
///
/// Each variant names a *specific* corruption or unsupported feature so a
/// failed load produces a readable message instead of a panic. Deriving
/// `thiserror::Error` gives us the `Display` text after `#[error(...)]` for
/// free, exactly as the protocol layer does for `ProtocolError`.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RdbError {
    #[error("not an RDB file: bad magic string")]
    BadMagic,
    #[error("unexpected end of RDB data")]
    UnexpectedEof,
    #[error("invalid UTF-8 in an RDB string")]
    InvalidUtf8,
    #[error("unsupported RDB length encoding")]
    UnsupportedLengthEncoding,
    #[error("LZF-compressed strings are not supported yet")]
    LzfUnsupported,
    #[error("unsupported RDB value type {0:#04x}")]
    UnsupportedValueType(u8),
}

/// A read-only walk through a byte slice, tracking how far we've got.
///
/// Every reader below borrows the underlying bytes (`&'a [u8]`) rather than
/// copying them, and each `read_*` advances `pos`. Reading past the end is a
/// clean `Err(UnexpectedEof)` — never an out-of-bounds panic — which is what
/// lets a truncated or corrupt file fail gracefully.
pub(crate) struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }

    /// Read exactly `n` bytes, returning a borrowed slice into the buffer.
    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], RdbError> {
        let end = self.pos.checked_add(n).ok_or(RdbError::UnexpectedEof)?;
        let slice = self
            .data
            .get(self.pos..end)
            .ok_or(RdbError::UnexpectedEof)?;
        self.pos = end;
        Ok(slice)
    }

    /// Read a single byte.
    fn read_u8(&mut self) -> Result<u8, RdbError> {
        Ok(self.read_bytes(1)?[0])
    }

    /// Read a big-endian `u32` — the width RDB uses for 32-bit *lengths*.
    fn read_u32_be(&mut self) -> Result<u32, RdbError> {
        let b = self.read_bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a big-endian `u64` — the width RDB uses for 64-bit *lengths*.
    fn read_u64_be(&mut self) -> Result<u64, RdbError> {
        let b = self.read_bytes(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Read a little-endian `u16` — the width of an int16-*encoded* string.
    fn read_u16_le(&mut self) -> Result<u16, RdbError> {
        let b = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Read a little-endian `u32` — the width of an int32-*encoded* string,
    /// and of a seconds-precision expiry timestamp.
    fn read_u32_le(&mut self) -> Result<u32, RdbError> {
        let b = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a little-endian `u64` — the width of a millisecond-precision expiry
    /// timestamp (the `0xFC` opcode's payload).
    fn read_u64_le(&mut self) -> Result<u64, RdbError> {
        let b = self.read_bytes(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
}

/// The result of decoding a length prefix.
///
/// A length byte usually announces "the next N bytes are the payload", but the
/// two high bits can instead flag a *special encoding* — the payload is a small
/// integer written in place of a string, or an LZF-compressed blob. Modelling
/// that fork as an enum forces the string decoder to decide what each case
/// means rather than conflating "a length of 5" with "encoding kind 5".
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Length {
    /// A plain byte count: this many bytes of payload follow.
    Plain(u64),
    /// A "special format" flagged by the `11` prefix; the inner byte is the
    /// remaining 6 bits (0 = int8, 1 = int16, 2 = int32, 3 = LZF).
    Special(u8),
}

/// Decode RDB's variable-length integer used for every length and count.
///
/// The top two bits of the first byte select the format:
/// - `00` → the low 6 bits *are* the length (0–63), one byte total.
/// - `01` → 14-bit length spread over this byte's low 6 bits and the next.
/// - `10` → the low 6 bits pick a wide form: `0` → a 32-bit big-endian length
///   in the next 4 bytes, `1` → a 64-bit big-endian length in the next 8.
/// - `11` → not a length at all but a *special encoding*; the low 6 bits say
///   which (returned as [`Length::Special`]).
pub(crate) fn read_length(cur: &mut Cursor) -> Result<Length, RdbError> {
    let first = cur.read_u8()?;
    // The two most-significant bits are the format selector.
    match first >> 6 {
        0b00 => Ok(Length::Plain((first & 0x3F) as u64)),
        0b01 => {
            let second = cur.read_u8()?;
            let len = (((first & 0x3F) as u64) << 8) | second as u64;
            Ok(Length::Plain(len))
        }
        0b10 => match first & 0x3F {
            0 => Ok(Length::Plain(cur.read_u32_be()? as u64)),
            1 => Ok(Length::Plain(cur.read_u64_be()?)),
            _ => Err(RdbError::UnsupportedLengthEncoding),
        },
        0b11 => Ok(Length::Special(first & 0x3F)),
        _ => unreachable!("a two-bit value is always 0..=3"),
    }
}

/// Decode one RDB string.
///
/// Redis stores strings in two shapes. The common one is length-prefixed raw
/// bytes. The clever one is an *integer-encoded* string: rather than write
/// "12345" as five bytes, RDB writes the number in 1/2/4 little-endian bytes
/// and reconstructs the decimal text on load — which is why `SET counter 12345`
/// round-trips through this function back into the string `"12345"`.
pub(crate) fn read_string(cur: &mut Cursor) -> Result<String, RdbError> {
    match read_length(cur)? {
        Length::Plain(n) => {
            let bytes = cur.read_bytes(n as usize)?;
            String::from_utf8(bytes.to_vec()).map_err(|_| RdbError::InvalidUtf8)
        }
        // Integers are stored signed, smallest width first, little-endian, and
        // rendered back to their decimal text form.
        Length::Special(0) => Ok((cur.read_u8()? as i8 as i64).to_string()),
        Length::Special(1) => Ok((cur.read_u16_le()? as i16 as i64).to_string()),
        Length::Special(2) => Ok((cur.read_u32_le()? as i32 as i64).to_string()),
        Length::Special(3) => Err(RdbError::LzfUnsupported),
        Length::Special(_) => Err(RdbError::UnsupportedLengthEncoding),
    }
}

/// One key/value pair recovered from an RDB file, before it becomes a live
/// store entry.
///
/// `expire_at_ms` is an *absolute* Unix timestamp in milliseconds — an
/// instruction like "this key dies at 3:47:22 PM", not "in 100 seconds". The
/// loader is what turns that wall-clock deadline into the monotonic `Instant`
/// the store actually uses, dropping keys whose deadline has already passed.
#[derive(Debug, PartialEq)]
pub struct RdbEntry {
    pub key: String,
    pub value: StoredValue,
    pub expire_at_ms: Option<u64>,
}

// The one-byte opcodes that can appear where a value-type byte would otherwise
// start. Anything below `0xFA` is a real value type (0 = string); these mark
// the metadata and structural records instead.
const OP_AUX: u8 = 0xFA; // an auxiliary "redis-ver = 7.0.15"-style metadata field
const OP_RESIZEDB: u8 = 0xFB; // hint: hash-table + expiry-table sizes for the db
const OP_EXPIRETIME_MS: u8 = 0xFC; // the next key expires at this ms timestamp
const OP_EXPIRETIME_S: u8 = 0xFD; // the next key expires at this second timestamp
const OP_SELECTDB: u8 = 0xFE; // switch to database N for the records that follow
const OP_EOF: u8 = 0xFF; // end of the keyspace; an 8-byte CRC64 follows

const TYPE_STRING: u8 = 0; // a plain string value
const TYPE_LIST: u8 = 1; // the classic list encoding: a count then that many strings
const TYPE_HASH: u8 = 4; // the classic hash encoding: a count then that many field/value pairs

/// Parse a whole RDB image into the entries it holds.
///
/// The walk is: check the `REDIS` magic and skip the 4-digit version, then loop
/// over records until the `0xFF` end marker. Metadata opcodes (`AUX`,
/// `SELECTDB`, `RESIZEDB`) are read to advance past them but otherwise ignored;
/// an expiry opcode stashes a deadline that attaches to the very next key; and
/// any other leading byte is a value type introducing a `key, value` pair. The
/// trailing CRC64 after `0xFF` is not verified.
pub fn parse_rdb(data: &[u8]) -> Result<Vec<RdbEntry>, RdbError> {
    let mut cur = Cursor::new(data);

    // Header: the ASCII magic "REDIS" then a 4-byte version we accept as-is.
    if cur.read_bytes(5)? != b"REDIS" {
        return Err(RdbError::BadMagic);
    }
    cur.read_bytes(4)?;

    let mut entries = Vec::new();
    // A pending expiry set by 0xFC/0xFD binds to the next key only. `take()`
    // hands it to that key and clears it, so a following key without its own
    // expiry opcode is correctly left immortal.
    let mut pending_expire: Option<u64> = None;

    loop {
        match cur.read_u8()? {
            OP_EOF => break,
            OP_SELECTDB => {
                read_length(&mut cur)?; // db number — single database, so ignored
            }
            OP_RESIZEDB => {
                read_length(&mut cur)?; // keyspace size hint
                read_length(&mut cur)?; // expires-table size hint
            }
            OP_AUX => {
                read_string(&mut cur)?; // metadata key
                read_string(&mut cur)?; // metadata value
            }
            OP_EXPIRETIME_MS => pending_expire = Some(cur.read_u64_le()?),
            OP_EXPIRETIME_S => pending_expire = Some(cur.read_u32_le()? as u64 * 1000),
            value_type => {
                let key = read_string(&mut cur)?;
                let value = read_value(&mut cur, value_type)?;
                entries.push(RdbEntry {
                    key,
                    value,
                    expire_at_ms: pending_expire.take(),
                });
            }
        }
    }
    Ok(entries)
}

/// Decode a single value of the given RDB type. FlashDB understands the three
/// value types its store can hold — string, list, and hash — in their classic
/// uncompressed encodings. Any other type number (a modern listpack/quicklist,
/// a set, a stream, …) is reported rather than misread, so an unsupported dump
/// fails loudly instead of loading garbage.
fn read_value(cur: &mut Cursor, value_type: u8) -> Result<StoredValue, RdbError> {
    match value_type {
        TYPE_STRING => Ok(StoredValue::Str(read_string(cur)?)),
        // A list is a length followed by that many element strings.
        TYPE_LIST => {
            let count = read_count(cur)?;
            let mut list = VecDeque::new();
            for _ in 0..count {
                list.push_back(read_string(cur)?);
            }
            Ok(StoredValue::List(list))
        }
        // A hash is a length followed by that many (field, value) string pairs.
        TYPE_HASH => {
            let count = read_count(cur)?;
            let mut map = HashMap::new();
            for _ in 0..count {
                let field = read_string(cur)?;
                let value = read_string(cur)?;
                map.insert(field, value);
            }
            Ok(StoredValue::Hash(map))
        }
        other => Err(RdbError::UnsupportedValueType(other)),
    }
}

/// Read a length that must be a plain byte count, not one of the `11`-flagged
/// "special" encodings — the element/field count in front of a list or hash.
/// A special encoding here means a corrupt file, so it's an error rather than a
/// silent misread. We don't pre-size collections from this count: a corrupt
/// huge value would otherwise trigger a giant allocation before the reads that
/// would fail anyway run out of bytes.
fn read_count(cur: &mut Cursor) -> Result<u64, RdbError> {
    match read_length(cur)? {
        Length::Plain(n) => Ok(n),
        Length::Special(_) => Err(RdbError::UnsupportedLengthEncoding),
    }
}

/// Writing the RDB format — the inverse of the decoders above.
///
/// [`serialize`] turns a slice of [`RdbEntry`]s into the exact byte image a
/// `dump.rdb` file holds: the `REDIS` header, a single database, one typed
/// record per key (each optionally preceded by its expiry), the `0xFF` end
/// marker, and the CRC64 trailer Redis checks on load. The length and string
/// encoders mirror [`read_length`] / [`read_string`], so a snapshot FlashDB
/// writes loads straight back through [`parse_rdb`] — and, because the checksum
/// is Redis's own CRC64 variant, into a real `redis-server` too.
pub mod write {
    use super::{
        RdbEntry, OP_EOF, OP_EXPIRETIME_MS, OP_SELECTDB, TYPE_HASH, TYPE_LIST, TYPE_STRING,
    };
    use crate::StoredValue;

    /// The version stamped after the `REDIS` magic. We emit v6 — deliberately
    /// conservative. A reader refuses a file whose version exceeds the newest it
    /// knows, so a low version keeps snapshots loadable by the widest range of
    /// Redis releases (and by FlashDB, whose loader accepts any 4-digit
    /// version). We only use ancient opcodes and the classic string/list/hash
    /// encodings, all valid since the earliest RDB versions, so nothing here
    /// needs a newer version to be understood.
    const RDB_VERSION: &[u8] = b"0006";

    /// Append `n` in RDB's variable-length integer encoding — the inverse of
    /// [`super::read_length`]. The width follows the magnitude: 6 bits for
    /// values below 64 (one byte), 14 bits below 16384 (two bytes), otherwise a
    /// 32- or 64-bit big-endian length behind a `0x80` / `0x81` flag byte. We
    /// never emit the `11`-flagged "special" forms, so every length we write is
    /// a plain byte count a reader takes at face value.
    pub fn write_length(buf: &mut Vec<u8>, n: u64) {
        if n < 1 << 6 {
            // 00xxxxxx — the value is the low six bits of a single byte.
            buf.push(n as u8);
        } else if n < 1 << 14 {
            // 01xxxxxx xxxxxxxx — 14 bits, high six in this byte, low eight next.
            buf.push(0b0100_0000 | (n >> 8) as u8);
            buf.push(n as u8);
        } else if n <= u32::MAX as u64 {
            // 0x80 then a 32-bit big-endian length.
            buf.push(0x80);
            buf.extend_from_slice(&(n as u32).to_be_bytes());
        } else {
            // 0x81 then a 64-bit big-endian length.
            buf.push(0x81);
            buf.extend_from_slice(&n.to_be_bytes());
        }
    }

    /// Append a string as a length prefix then its raw UTF-8 bytes — the inverse
    /// of [`super::read_string`]'s plain form. We always write the plain form
    /// (never the compact integer encoding); that's valid RDB every reader
    /// accepts, and the integer packing is a size optimisation, not a
    /// correctness requirement.
    pub fn write_string(buf: &mut Vec<u8>, s: &str) {
        write_length(buf, s.len() as u64);
        buf.extend_from_slice(s.as_bytes());
    }

    /// The one-byte type tag that introduces a value's record.
    fn value_type_tag(value: &StoredValue) -> u8 {
        match value {
            StoredValue::Str(_) => TYPE_STRING,
            StoredValue::List(_) => TYPE_LIST,
            StoredValue::Hash(_) => TYPE_HASH,
        }
    }

    /// Append a value's payload (everything after its type tag), in the same
    /// classic encoding [`super::read_value`] reads back: a string is one
    /// string; a list is a count then its elements; a hash is a count then its
    /// field/value string pairs.
    fn write_payload(buf: &mut Vec<u8>, value: &StoredValue) {
        match value {
            StoredValue::Str(s) => write_string(buf, s),
            StoredValue::List(list) => {
                write_length(buf, list.len() as u64);
                for item in list {
                    write_string(buf, item);
                }
            }
            StoredValue::Hash(map) => {
                write_length(buf, map.len() as u64);
                for (field, value) in map {
                    write_string(buf, field);
                    write_string(buf, value);
                }
            }
        }
    }

    /// Serialize a whole keyspace into the RDB byte image.
    ///
    /// Layout: `REDIS` + version, a single `SELECTDB 0`, then for each entry an
    /// optional `EXPIRETIME_MS` opcode carrying its absolute millisecond
    /// deadline followed by the `<type><key><payload>` record, then the `0xFF`
    /// end marker, and finally the 8-byte little-endian CRC64 of everything
    /// written before it. Feeding the result back to [`parse_rdb`] returns the
    /// same entries.
    pub fn serialize(entries: &[RdbEntry]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"REDIS");
        buf.extend_from_slice(RDB_VERSION);

        // A single database, number 0.
        buf.push(OP_SELECTDB);
        write_length(&mut buf, 0);

        for entry in entries {
            if let Some(deadline_ms) = entry.expire_at_ms {
                buf.push(OP_EXPIRETIME_MS);
                buf.extend_from_slice(&deadline_ms.to_le_bytes());
            }
            // Record order matches the reader: type tag, then key, then value.
            buf.push(value_type_tag(&entry.value));
            write_string(&mut buf, &entry.key);
            write_payload(&mut buf, &entry.value);
        }

        buf.push(OP_EOF);
        // The checksum covers the whole image up to and including the EOF byte.
        let checksum = crc64(0, &buf);
        buf.extend_from_slice(&checksum.to_le_bytes());
        buf
    }

    /// CRC64 as Redis computes it — the Jones polynomial with reflected input
    /// and output (`poly 0xad93d23594c935a9`, init 0, no final xor). Matching
    /// Redis bit-for-bit is what lets a real `redis-server` accept a snapshot
    /// FlashDB wrote; a wrong checksum would be rejected as a corrupt file.
    pub(crate) fn crc64(mut crc: u64, data: &[u8]) -> u64 {
        const POLY: u64 = 0xad93d23594c935a9;
        for &byte in data {
            // Reflected input: fold in the byte's bits low-to-high.
            for i in 0..8 {
                let mut bit = (crc & 0x8000_0000_0000_0000) != 0;
                if byte & (1 << i) != 0 {
                    bit = !bit;
                }
                crc <<= 1;
                if bit {
                    crc ^= POLY;
                }
            }
        }
        // Reflected output: mirror the 64-bit register end for end.
        crc_reflect(crc)
    }

    /// Reverse the order of all 64 bits — the output reflection CRC-64/Jones
    /// applies to the final register.
    fn crc_reflect(mut data: u64) -> u64 {
        let mut ret = data & 1;
        for _ in 1..64 {
            data >>= 1;
            ret = (ret << 1) | (data & 1);
        }
        ret
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_six_bit_length() {
        // 0x0A = 0b00_001010 → plain length 10.
        let mut cur = Cursor::new(&[0x0A]);
        assert_eq!(read_length(&mut cur).unwrap(), Length::Plain(10));
    }

    #[test]
    fn fourteen_bit_length() {
        // 0x41 0x02 = 0b01_000001, 0x02 → (1 << 8) | 2 = 258.
        let mut cur = Cursor::new(&[0x41, 0x02]);
        assert_eq!(read_length(&mut cur).unwrap(), Length::Plain(258));
    }

    #[test]
    fn thirty_two_bit_length_is_big_endian() {
        // 0x80 flags a 32-bit BE length; 0x00000100 = 256.
        let mut cur = Cursor::new(&[0x80, 0x00, 0x00, 0x01, 0x00]);
        assert_eq!(read_length(&mut cur).unwrap(), Length::Plain(256));
    }

    #[test]
    fn sixty_four_bit_length_is_big_endian() {
        // 0x81 flags a 64-bit BE length; here that decodes to 1.
        let mut cur = Cursor::new(&[0x81, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(read_length(&mut cur).unwrap(), Length::Plain(1));
    }

    #[test]
    fn special_encoding_is_flagged_not_treated_as_a_length() {
        // 0xC0 = 0b11_000000 → special format 0 (an int8-encoded string).
        let mut cur = Cursor::new(&[0xC0]);
        assert_eq!(read_length(&mut cur).unwrap(), Length::Special(0));
    }

    #[test]
    fn plain_string_round_trips() {
        // length 3 then "abc".
        let mut cur = Cursor::new(&[0x03, b'a', b'b', b'c']);
        assert_eq!(read_string(&mut cur).unwrap(), "abc");
    }

    #[test]
    fn empty_string_is_a_zero_length() {
        let mut cur = Cursor::new(&[0x00]);
        assert_eq!(read_string(&mut cur).unwrap(), "");
    }

    #[test]
    fn int8_encoded_string() {
        // 0xC0 then 0x40 = 64.
        let mut cur = Cursor::new(&[0xC0, 0x40]);
        assert_eq!(read_string(&mut cur).unwrap(), "64");
    }

    #[test]
    fn int16_encoded_string_is_little_endian_and_signed() {
        // 0xC1 then 0x39 0x30 = 0x3039 = 12345.
        let mut cur = Cursor::new(&[0xC1, 0x39, 0x30]);
        assert_eq!(read_string(&mut cur).unwrap(), "12345");
        // 0xFF 0xFF as i16 is -1.
        let mut neg = Cursor::new(&[0xC1, 0xFF, 0xFF]);
        assert_eq!(read_string(&mut neg).unwrap(), "-1");
    }

    #[test]
    fn int32_encoded_string_is_little_endian() {
        // 0xC2 then 0x40 0x42 0x0F 0x00 = 0x000F4240 = 1_000_000.
        let mut cur = Cursor::new(&[0xC2, 0x40, 0x42, 0x0F, 0x00]);
        assert_eq!(read_string(&mut cur).unwrap(), "1000000");
    }

    #[test]
    fn lzf_strings_report_unsupported_rather_than_guessing() {
        let mut cur = Cursor::new(&[0xC3, 0x00]);
        assert_eq!(read_string(&mut cur), Err(RdbError::LzfUnsupported));
    }

    #[test]
    fn reading_past_the_end_is_a_clean_error() {
        // A plain length of 5 with no body must not panic — it's EOF.
        let mut cur = Cursor::new(&[0x05, b'h', b'i']);
        assert_eq!(read_string(&mut cur), Err(RdbError::UnexpectedEof));
    }

    // Build a minimal-but-valid RDB by hand: header, SELECTDB 0, one plain
    // string key, EOF, and 8 bytes standing in for the (unverified) CRC64.
    fn minimal_rdb(key: &str, val: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"REDIS0011");
        v.push(OP_SELECTDB);
        v.push(0x00); // db 0, plain length
        v.push(TYPE_STRING);
        v.push(key.len() as u8);
        v.extend_from_slice(key.as_bytes());
        v.push(val.len() as u8);
        v.extend_from_slice(val.as_bytes());
        v.push(OP_EOF);
        v.extend_from_slice(&[0; 8]);
        v
    }

    #[test]
    fn rejects_a_file_without_the_redis_magic() {
        assert_eq!(parse_rdb(b"NOPE0011\xff"), Err(RdbError::BadMagic));
    }

    #[test]
    fn parses_a_single_string_key() {
        let bytes = minimal_rdb("foo", "bar");
        let entries = parse_rdb(&bytes).unwrap();
        assert_eq!(
            entries,
            vec![RdbEntry {
                key: "foo".to_string(),
                value: StoredValue::Str("bar".to_string()),
                expire_at_ms: None,
            }]
        );
    }

    #[test]
    fn an_expiry_opcode_binds_to_the_next_key_only() {
        let mut v = Vec::new();
        v.extend_from_slice(b"REDIS0011");
        // Key "a" with a millisecond expiry, then key "b" with none.
        v.push(OP_EXPIRETIME_MS);
        v.extend_from_slice(&1_000u64.to_le_bytes());
        v.push(TYPE_STRING);
        v.extend_from_slice(&[0x01, b'a', 0x01, b'x']);
        v.push(TYPE_STRING);
        v.extend_from_slice(&[0x01, b'b', 0x01, b'y']);
        v.push(OP_EOF);
        v.extend_from_slice(&[0; 8]);

        let entries = parse_rdb(&v).unwrap();
        assert_eq!(entries[0].key, "a");
        assert_eq!(entries[0].expire_at_ms, Some(1_000));
        assert_eq!(entries[1].key, "b");
        // Crucially the second key did NOT inherit the first key's expiry.
        assert_eq!(entries[1].expire_at_ms, None);
    }

    #[test]
    fn a_seconds_expiry_is_scaled_to_milliseconds() {
        let mut v = Vec::new();
        v.extend_from_slice(b"REDIS0011");
        v.push(OP_EXPIRETIME_S);
        v.extend_from_slice(&5u32.to_le_bytes()); // 5 seconds
        v.push(TYPE_STRING);
        v.extend_from_slice(&[0x01, b'k', 0x01, b'v']);
        v.push(OP_EOF);
        v.extend_from_slice(&[0; 8]);

        let entries = parse_rdb(&v).unwrap();
        assert_eq!(entries[0].expire_at_ms, Some(5_000));
    }

    #[test]
    fn an_unsupported_value_type_is_reported_not_misread() {
        let mut v = Vec::new();
        v.extend_from_slice(b"REDIS0011");
        // Type 0x02 is a set — a value type FlashDB's store can't hold yet, so
        // it must be reported rather than misread as something it understands.
        v.push(0x02);
        v.extend_from_slice(&[0x01, b'k']);
        assert_eq!(parse_rdb(&v), Err(RdbError::UnsupportedValueType(0x02)));
    }

    // A real snapshot produced by `redis-server` 7.0.15 via `SAVE`, captured
    // byte-for-byte. It carries the AUX metadata block, a RESIZEDB hint, three
    // string keys (two of them integer-encoded), one key with a millisecond
    // expiry, and an empty-string value — the exact shapes the decoders above
    // must survive. Proving we read Redis's own output is the real test.
    const REDIS_7_GOLDEN: &[u8] = &[
        0x52, 0x45, 0x44, 0x49, 0x53, 0x30, 0x30, 0x31, 0x30, 0xfa, 0x09, 0x72, 0x65, 0x64, 0x69,
        0x73, 0x2d, 0x76, 0x65, 0x72, 0x06, 0x37, 0x2e, 0x30, 0x2e, 0x31, 0x35, 0xfa, 0x0a, 0x72,
        0x65, 0x64, 0x69, 0x73, 0x2d, 0x62, 0x69, 0x74, 0x73, 0xc0, 0x40, 0xfa, 0x05, 0x63, 0x74,
        0x69, 0x6d, 0x65, 0xc2, 0xdc, 0x0c, 0x6b, 0x6a, 0xfa, 0x08, 0x75, 0x73, 0x65, 0x64, 0x2d,
        0x6d, 0x65, 0x6d, 0xc2, 0xa0, 0x15, 0x0a, 0x00, 0xfa, 0x08, 0x61, 0x6f, 0x66, 0x2d, 0x62,
        0x61, 0x73, 0x65, 0xc0, 0x00, 0xfe, 0x00, 0xfb, 0x05, 0x01, 0x00, 0x06, 0x62, 0x69, 0x67,
        0x6e, 0x75, 0x6d, 0xc2, 0x40, 0x42, 0x0f, 0x00, 0x00, 0x07, 0x63, 0x6f, 0x75, 0x6e, 0x74,
        0x65, 0x72, 0xc1, 0x39, 0x30, 0xfc, 0x8d, 0x1c, 0x20, 0xb8, 0x9f, 0x01, 0x00, 0x00, 0x00,
        0x04, 0x74, 0x65, 0x6d, 0x70, 0x0c, 0x65, 0x78, 0x70, 0x69, 0x72, 0x65, 0x73, 0x20, 0x73,
        0x6f, 0x6f, 0x6e, 0x00, 0x08, 0x67, 0x72, 0x65, 0x65, 0x74, 0x69, 0x6e, 0x67, 0x0b, 0x68,
        0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x77, 0x6f, 0x72, 0x6c, 0x64, 0x00, 0x08, 0x65, 0x6d, 0x70,
        0x74, 0x79, 0x69, 0x73, 0x68, 0x00, 0xff, 0xe2, 0xfc, 0xc5, 0xc1, 0x3b, 0x95, 0x25, 0xb5,
    ];

    #[test]
    fn parses_a_real_redis_snapshot() {
        let entries = parse_rdb(REDIS_7_GOLDEN).unwrap();
        // Look keys up by name — HGETALL-style ordering isn't guaranteed, but
        // for the load order Redis wrote them in it happens to be stable; we
        // assert on values rather than positions to be safe.
        let by_key = |name: &str| entries.iter().find(|e| e.key == name).unwrap();

        // Integer-encoded values come back as their decimal text.
        assert_eq!(by_key("bignum").value, StoredValue::Str("1000000".into()));
        assert_eq!(by_key("counter").value, StoredValue::Str("12345".into()));
        // Plain string values.
        assert_eq!(
            by_key("greeting").value,
            StoredValue::Str("hello world".into())
        );
        // An empty string round-trips as an empty string, not a missing key.
        assert_eq!(by_key("emptyish").value, StoredValue::Str("".into()));
        // The one key SET with an expiry carries an absolute ms deadline; the
        // rest have none.
        assert!(by_key("temp").expire_at_ms.is_some());
        assert_eq!(by_key("greeting").expire_at_ms, None);
        assert_eq!(entries.len(), 5);
    }

    // --- Writing (the `write` submodule) ---------------------------------

    use super::write;

    // Every length we write must read back as the same plain byte count, across
    // the 1-, 2-, 5-, and 9-byte width boundaries the encoder switches on.
    #[test]
    fn written_lengths_round_trip_through_the_reader() {
        for &n in &[
            0u64,
            1,
            63,
            64,
            255,
            16_383,
            16_384,
            70_000,
            u32::MAX as u64,
            1 << 40,
        ] {
            let mut buf = Vec::new();
            write::write_length(&mut buf, n);
            let mut cur = Cursor::new(&buf);
            assert_eq!(read_length(&mut cur).unwrap(), Length::Plain(n), "n = {n}");
        }
    }

    // The width boundaries produce the byte counts we expect: 6-bit → 1 byte,
    // 14-bit → 2, 32-bit → 5, 64-bit → 9.
    #[test]
    fn length_encoding_uses_the_smallest_width() {
        let sizes = |n| {
            let mut b = Vec::new();
            write::write_length(&mut b, n);
            b.len()
        };
        assert_eq!(sizes(63), 1);
        assert_eq!(sizes(64), 2);
        assert_eq!(sizes(16_383), 2);
        assert_eq!(sizes(16_384), 5);
        assert_eq!(sizes(u32::MAX as u64), 5);
        assert_eq!(sizes(1 << 40), 9);
    }

    #[test]
    fn written_strings_round_trip_through_the_reader() {
        for s in ["", "a", "hello world", &"x".repeat(300)] {
            let mut buf = Vec::new();
            write::write_string(&mut buf, s);
            let mut cur = Cursor::new(&buf);
            assert_eq!(read_string(&mut cur).unwrap(), s);
        }
    }

    // The canonical CRC-64/Jones check value, plus Redis's documented one.
    #[test]
    fn crc64_matches_the_canonical_check_value() {
        assert_eq!(write::crc64(0, b"123456789"), 0xe9c6d914c4b8d9ca);
    }

    // The strongest proof our checksum is Redis-correct: recompute the CRC over
    // the real 7.0.15 snapshot's body and match the 8 bytes Redis itself wrote.
    #[test]
    fn crc64_reproduces_a_real_redis_snapshots_trailer() {
        let n = REDIS_7_GOLDEN.len();
        let stored = u64::from_le_bytes(REDIS_7_GOLDEN[n - 8..].try_into().unwrap());
        assert_eq!(write::crc64(0, &REDIS_7_GOLDEN[..n - 8]), stored);
    }

    // A serialize → parse_rdb round-trip for every value type, an expiry, and an
    // empty string — the whole shape a snapshot has to survive.
    #[test]
    fn serialize_then_parse_round_trips_every_value_type() {
        let mut list = VecDeque::new();
        list.push_back("a".to_string());
        list.push_back("b".to_string());
        let mut hash = HashMap::new();
        hash.insert("field".to_string(), "value".to_string());

        let input = vec![
            RdbEntry {
                key: "s".to_string(),
                value: StoredValue::Str("hello".to_string()),
                expire_at_ms: None,
            },
            RdbEntry {
                key: "empty".to_string(),
                value: StoredValue::Str("".to_string()),
                expire_at_ms: None,
            },
            RdbEntry {
                key: "temp".to_string(),
                value: StoredValue::Str("v".to_string()),
                expire_at_ms: Some(1_700_000_000_000),
            },
            RdbEntry {
                key: "l".to_string(),
                value: StoredValue::List(list),
                expire_at_ms: None,
            },
            RdbEntry {
                key: "h".to_string(),
                value: StoredValue::Hash(hash),
                expire_at_ms: None,
            },
        ];

        let bytes = write::serialize(&input);
        let mut got = parse_rdb(&bytes).unwrap();
        // Order isn't guaranteed to match; sort both sides by key to compare.
        got.sort_by(|a, b| a.key.cmp(&b.key));
        let mut want = input;
        want.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(got, want);
    }

    // An empty keyspace still produces a valid, checksummed, loadable image.
    #[test]
    fn serialize_of_an_empty_keyspace_is_valid_and_empty() {
        let bytes = write::serialize(&[]);
        assert!(bytes.starts_with(b"REDIS"));
        assert_eq!(parse_rdb(&bytes).unwrap(), vec![]);
    }
}
