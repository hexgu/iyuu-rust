//! Minimal bencode decoder + info-hash computation.
//!
//! Cross-seeding correctness depends entirely on computing the exact same
//! `info_hash` qBittorrent / Transmission use, which means we must hash the
//! **byte slice of the `info` dictionary exactly as it appears in the .torrent
//! file** — not a re-serialized version of it. Any re-encoding (key reorder,
//! dropping unknown keys, normalizing ints) produces a different hash and the
//! torrent will never match. So the decoder records byte spans, and
//! `info_hash()` hashes the original file bytes for that span.

use sha1::{Digest, Sha1};
use std::fmt;

#[derive(Debug)]
pub enum BencodeError {
    UnexpectedEof,
    InvalidType(u8),
    InvalidLength,
    TrailingBytes,
}

impl fmt::Display for BencodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BencodeError::UnexpectedEof => write!(f, "bencode: unexpected end of input"),
            BencodeError::InvalidType(b) => write!(f, "bencode: invalid type byte 0x{b:02x}"),
            BencodeError::InvalidLength => write!(f, "bencode: invalid length prefix"),
            BencodeError::TrailingBytes => write!(f, "bencode: trailing bytes after root value"),
        }
    }
}

impl std::error::Error for BencodeError {}

pub type Result<T> = std::result::Result<T, BencodeError>;

/// A parsed bencode value. `Str` keeps the raw bytes (bencode strings are
/// byte strings, not necessarily UTF-8 — piece hashes and names are binary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Str(Vec<u8>),
    List(Vec<Value>),
    Dict(Vec<(Vec<u8>, Value)>),
}

impl Value {
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Decode a byte string as UTF-8, lossily. Torrent metadata in the wild is
    /// frequently not valid UTF-8 (legacy GBK/Shift-JIS names), so we never fail
    /// the whole parse over it.
    #[must_use]
    pub fn as_str_lossy(&self) -> Option<String> {
        self.as_bytes()
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }

    #[must_use]
    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_dict(&self) -> Option<&[(Vec<u8>, Value)]> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Look up a key in a dict. Keys are raw bytes.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_dict()?
            .iter()
            .find(|(k, _)| k.as_slice() == key.as_bytes())
            .map(|(_, v)| v)
    }
}

/// Decode a full bencode document.
///
/// # Errors
/// Returns [`BencodeError`] when the input is truncated, malformed, or carries
/// bytes after the top-level value.
pub fn decode(input: &[u8]) -> Result<Value> {
    let mut cursor = Cursor {
        data: input,
        pos: 0,
    };
    let value = cursor.parse_value()?;
    if cursor.pos != input.len() {
        return Err(BencodeError::TrailingBytes);
    }
    Ok(value)
}

/// Byte span of the `info` dictionary inside the original document.
pub struct InfoSpan {
    pub start: usize,
    pub end: usize,
}

/// Locate the `info` dict's byte span in a `.torrent` file, without needing to
/// materialize the whole structure. This is the torrust/transmission approach:
/// walk the top-level dict, and when the key is `info`, remember where the
/// value starts and where it ends.
///
/// # Errors
/// Returns [`BencodeError`] when the document is not a bencode dict or has no
/// `info` key.
pub fn find_info_span(input: &[u8]) -> Result<InfoSpan> {
    let mut cursor = Cursor {
        data: input,
        pos: 0,
    };
    if cursor.peek()? != b'd' {
        return Err(BencodeError::InvalidType(cursor.peek()?));
    }
    cursor.pos = 1; // consume 'd'

    let mut span: Option<(usize, usize)> = None;
    loop {
        if cursor.peek()? == b'e' {
            break;
        }
        let key = cursor.parse_bytes()?;
        let value_start = cursor.pos;
        cursor.skip_value()?;
        let value_end = cursor.pos;
        if key.as_slice() == b"info" {
            span = Some((value_start, value_end));
        }
    }

    let (start, end) = span.ok_or(BencodeError::UnexpectedEof)?;
    Ok(InfoSpan { start, end })
}

/// Compute the v1 info-hash (SHA-1 over the raw `info` dict bytes).
///
/// # Errors
/// Propagates [`find_info_span`]'s failure when the `info` dict cannot be
/// located.
pub fn info_hash_from_torrent(input: &[u8]) -> Result<[u8; 20]> {
    let span = find_info_span(input)?;
    Ok(sha1_bytes(&input[span.start..span.end]))
}

/// Whether the torrent carries `BitTorrent` v2 metadata (only `piece layers` /
/// `meta version 2` means the v1 hash may be absent or the torrent is hybrid).
///
/// # Errors
/// Propagates [`decode`]'s failure when the document is malformed.
pub fn is_v2(input: &[u8]) -> Result<bool> {
    let v = decode(input)?;
    let version = v.get("info").and_then(|i| i.get("meta version"));
    Ok(matches!(version.and_then(Value::as_int), Some(2)))
}

#[must_use]
pub fn sha1_bytes(data: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(data);
    let out = h.finalize();
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&out);
    arr
}

/// Lowercase hex, which is what qBittorrent and Transmission both use on the
/// wire and in their APIs.
#[must_use]
pub fn hash_to_hex(hash: &[u8; 20]) -> String {
    hex::encode(hash)
}

/// Parse a 40-char hex info-hash into bytes.
#[must_use]
pub fn hex_to_hash(s: &str) -> Option<[u8; 20]> {
    let s = s.trim();
    if s.len() != 40 {
        return None;
    }
    let bytes = hex::decode(s).ok()?;
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&bytes);
    Some(arr)
}

/// Normalize any info-hash spelling to lowercase hex.
///
/// qBittorrent is inconsistent: most of its API returns lowercase hex, but
/// torrents added with magnet links occasionally surface uppercase, and
/// Transmission returns uppercase hex in some fields. Dedup/matching bugs from
/// this are notorious, so every hash entering the system goes through here.
#[must_use]
pub fn normalize_hash(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Result<u8> {
        self.data
            .get(self.pos)
            .copied()
            .ok_or(BencodeError::UnexpectedEof)
    }

    fn parse_value(&mut self) -> Result<Value> {
        match self.peek()? {
            b'i' => self.parse_int().map(Value::Int),
            b'l' => self.parse_list(),
            b'd' => self.parse_dict(),
            b'0'..=b'9' => self.parse_bytes().map(Value::Str),
            other => Err(BencodeError::InvalidType(other)),
        }
    }

    fn parse_int(&mut self) -> Result<i64> {
        debug_assert_eq!(self.data[self.pos], b'i');
        self.pos += 1;
        let start = self.pos;
        while self.peek()? != b'e' {
            self.pos += 1;
        }
        let raw = &self.data[start..self.pos];
        self.pos += 1; // consume 'e'
        let s = std::str::from_utf8(raw).map_err(|_| BencodeError::InvalidLength)?;
        s.parse::<i64>().map_err(|_| BencodeError::InvalidLength)
    }

    fn parse_bytes(&mut self) -> Result<Vec<u8>> {
        let start = self.pos;
        while self.peek()?.is_ascii_digit() {
            self.pos += 1;
        }
        if self.pos == start {
            return Err(BencodeError::InvalidLength);
        }
        let len: usize = std::str::from_utf8(&self.data[start..self.pos])
            .map_err(|_| BencodeError::InvalidLength)?
            .parse()
            .map_err(|_| BencodeError::InvalidLength)?;
        if self.peek()? != b':' {
            return Err(BencodeError::InvalidLength);
        }
        self.pos += 1;
        let end = self
            .pos
            .checked_add(len)
            .ok_or(BencodeError::InvalidLength)?;
        if end > self.data.len() {
            return Err(BencodeError::UnexpectedEof);
        }
        let out = self.data[self.pos..end].to_vec();
        self.pos = end;
        Ok(out)
    }

    fn parse_list(&mut self) -> Result<Value> {
        self.pos += 1; // 'l'
        let mut items = Vec::new();
        loop {
            if self.peek()? == b'e' {
                self.pos += 1;
                break;
            }
            items.push(self.parse_value()?);
        }
        Ok(Value::List(items))
    }

    fn parse_dict(&mut self) -> Result<Value> {
        self.pos += 1; // 'd'
        let mut items: Vec<(Vec<u8>, Value)> = Vec::new();
        loop {
            if self.peek()? == b'e' {
                self.pos += 1;
                break;
            }
            let key = self.parse_bytes()?;
            let value = self.parse_value()?;
            items.push((key, value));
        }
        Ok(Value::Dict(items))
    }

    /// Advance past one value without building it. Used by `find_info_span`,
    /// which only needs offsets.
    fn skip_value(&mut self) -> Result<()> {
        match self.peek()? {
            b'i' => {
                while self.peek()? != b'e' {
                    self.pos += 1;
                }
                self.pos += 1;
                Ok(())
            }
            b'l' | b'd' => {
                let is_dict = self.peek()? == b'd';
                self.pos += 1;
                loop {
                    if self.peek()? == b'e' {
                        self.pos += 1;
                        break;
                    }
                    if is_dict {
                        self.parse_bytes()?; // key
                    }
                    self.skip_value()?;
                }
                Ok(())
            }
            b'0'..=b'9' => {
                self.parse_bytes()?;
                Ok(())
            }
            other => Err(BencodeError::InvalidType(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_ints_strings_lists_dicts() {
        let v = decode(b"d3:agei30e4:name4:anny5:itemsl1:a1:bee").unwrap();
        assert_eq!(v.get("age").unwrap().as_int(), Some(30));
        assert_eq!(
            v.get("name").unwrap().as_str_lossy().as_deref(),
            Some("anny")
        );
        assert_eq!(v.get("items").unwrap().as_list().unwrap().len(), 2);
    }

    #[test]
    fn rejects_trailing_bytes() {
        assert!(matches!(decode(b"i1ee"), Err(BencodeError::TrailingBytes)));
    }

    #[test]
    fn info_hash_uses_raw_bytes_not_reserialized() {
        // Two torrents with the same info keys in different order MUST hash
        // differently — this is why we hash the original span.
        let a =
            b"d4:infod4:name1:a6:lengthi1e12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
        let b =
            b"d4:infod6:lengthi1e4:name1:a12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
        let ha = info_hash_from_torrent(a).unwrap();
        let hb = info_hash_from_torrent(b).unwrap();
        assert_ne!(ha, hb);

        // And identical bytes give identical hashes.
        assert_eq!(ha, info_hash_from_torrent(a).unwrap());
    }

    #[test]
    fn info_hash_matches_manual_sha1_of_span() {
        let doc = b"d8:announce3:url4:infod4:name1:xee";
        let span = find_info_span(doc).unwrap();
        let expected = sha1_bytes(&doc[span.start..span.end]);
        assert_eq!(info_hash_from_torrent(doc).unwrap(), expected);
    }

    #[test]
    fn normalize_hash_lowercases() {
        assert_eq!(
            normalize_hash("AABBCCDDEEFF00112233445566778899AABBCCDD"),
            "aabbccddeeff00112233445566778899aabbccdd"
        );
    }

    #[test]
    fn detects_v2() {
        let v1 = b"d4:infod4:name1:xee";
        assert!(!is_v2(v1).unwrap());
        let v2 = b"d4:infod4:name1:x12:meta versioni2eee";
        assert!(is_v2(v2).unwrap());
    }
}
