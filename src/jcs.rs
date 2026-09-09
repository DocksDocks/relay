use crate::sha256;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JcsValue {
    Null,
    Bool(bool),
    String(String),
    Integer(i64),
    Array(Vec<JcsValue>),
    Object(BTreeMap<String, JcsValue>),
}

impl JcsValue {
    pub fn object(self) -> Result<BTreeMap<String, JcsValue>, String> {
        match self {
            Self::Object(value) => Ok(value),
            _ => Err("JCS record must be a JSON object".to_string()),
        }
    }

    pub fn as_str(&self) -> Result<&str, String> {
        match self {
            Self::String(value) => Ok(value),
            _ => Err("JCS field must be a string".to_string()),
        }
    }
}

pub trait ClosedJcs: Sized {
    fn from_jcs(value: JcsValue) -> Result<Self, String>;
    fn to_jcs(&self) -> JcsValue;
}

pub fn parse_jcs(bytes: &[u8], trailing_lf: bool) -> Result<JcsValue, String> {
    let body = if trailing_lf {
        if !bytes.ends_with(b"\n") || bytes.ends_with(b"\n\n") {
            return Err("JCS JSON must end in exactly one LF".to_string());
        }
        &bytes[..bytes.len() - 1]
    } else {
        if bytes.ends_with(b"\n") {
            return Err("this canonical JSON transport must not contain a trailing LF".to_string());
        }
        bytes
    };
    let text = std::str::from_utf8(body).map_err(|_| "JCS JSON is not UTF-8".to_string())?;
    let mut parser = Parser {
        bytes: text.as_bytes(),
        offset: 0,
    };
    let value = parser.value()?;
    if parser.offset != parser.bytes.len() {
        return Err("JCS JSON has trailing bytes".to_string());
    }
    let canonical = serialize_jcs(&value);
    if canonical.as_bytes() != body {
        return Err("JCS JSON is not canonical RFC 8785 JCS".to_string());
    }
    Ok(value)
}

pub fn serialize_jcs(value: &JcsValue) -> String {
    let mut output = String::new();
    write_value(value, &mut output);
    output
}

pub fn serialize_jcs_lf<T: ClosedJcs>(value: &T) -> Vec<u8> {
    let mut bytes = serialize_jcs(&value.to_jcs()).into_bytes();
    bytes.push(b'\n');
    bytes
}

pub fn read_jcs_file<T: ClosedJcs>(
    path: &Path,
    expected_sha256: Option<&str>,
) -> Result<T, String> {
    T::from_jcs(read_jcs_value(path, expected_sha256)?)
}

/// Securely read and canonically parse a JCS file without binding it to a
/// record type, so a caller can inspect a discriminator before decoding.
pub fn read_jcs_value(path: &Path, expected_sha256: Option<&str>) -> Result<JcsValue, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("securely open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.is_file()
        // SAFETY: geteuid has no preconditions and cannot fail.
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(format!(
            "{} must be an EUID-owned, single-link, mode-0600 regular file",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if let Some(expected) = expected_sha256 {
        Sha256Digest::parse(expected)?;
        if !sha256::constant_time_eq(sha256::hex_digest(&bytes).as_bytes(), expected.as_bytes()) {
            return Err(format!("SHA-256 mismatch for {}", path.display()));
        }
    }
    parse_jcs(&bytes, true)
}

pub fn jcs_sha256<T: ClosedJcs>(value: &T) -> String {
    sha256::hex_digest(&serialize_jcs_lf(value))
}

fn write_value(value: &JcsValue, output: &mut String) {
    match value {
        JcsValue::Null => output.push_str("null"),
        JcsValue::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        JcsValue::String(value) => write_string(value, output),
        JcsValue::Integer(value) => output.push_str(&value.to_string()),
        JcsValue::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_value(value, output);
            }
            output.push(']');
        }
        JcsValue::Object(values) => {
            output.push('{');
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_by(|(left, _), (right, _)| utf16_cmp(left, right));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_string(key, output);
                output.push(':');
                write_value(value, output);
            }
            output.push('}');
        }
    }
}

fn utf16_cmp(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

const HEX_DIGITS: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

fn write_string(value: &str, output: &mut String) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{09}' => output.push_str("\\t"),
            '\n' => output.push_str("\\n"),
            '\u{0c}' => output.push_str("\\f"),
            '\r' => output.push_str("\\r"),
            c if c <= '\u{1f}' => {
                // `c` is at most 0x1f, so the escape is `\u00` plus two hex digits.
                let code = c as u32;
                output.push_str("\\u00");
                output.push(HEX_DIGITS[(code >> 4) as usize]);
                output.push(HEX_DIGITS[(code & 0x0f) as usize]);
            }
            c => output.push(c),
        }
    }
    output.push('"');
}

struct Parser<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl Parser<'_> {
    fn value(&mut self) -> Result<JcsValue, String> {
        match self.peek() {
            Some(b'n') => {
                self.literal(b"null")?;
                Ok(JcsValue::Null)
            }
            Some(b't') => {
                self.literal(b"true")?;
                Ok(JcsValue::Bool(true))
            }
            Some(b'f') => {
                self.literal(b"false")?;
                Ok(JcsValue::Bool(false))
            }
            Some(b'"') => self.string().map(JcsValue::String),
            Some(b'[') => self.array(),
            Some(b'{') => self.object(),
            Some(b'-' | b'0'..=b'9') => self.integer(),
            _ => Err("invalid JCS JSON token".to_string()),
        }
    }
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }
    fn take(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.offset += 1;
            Ok(())
        } else {
            Err("invalid JCS JSON punctuation".to_string())
        }
    }
    fn literal(&mut self, literal: &[u8]) -> Result<(), String> {
        if self.bytes.get(self.offset..self.offset + literal.len()) == Some(literal) {
            self.offset += literal.len();
            Ok(())
        } else {
            Err("invalid JCS JSON literal".to_string())
        }
    }
    fn string(&mut self) -> Result<String, String> {
        self.take(b'"')?;
        let mut result = String::new();
        while let Some(byte) = self.peek() {
            match byte {
                b'"' => {
                    self.offset += 1;
                    return Ok(result);
                }
                b'\\' => {
                    self.offset += 1;
                    let escaped = self
                        .peek()
                        .ok_or_else(|| "truncated JSON escape".to_string())?;
                    self.offset += 1;
                    match escaped {
                        b'"' => result.push('"'),
                        b'\\' => result.push('\\'),
                        b'/' => result.push('/'),
                        b'b' => result.push('\u{08}'),
                        b'f' => result.push('\u{0c}'),
                        b'n' => result.push('\n'),
                        b'r' => result.push('\r'),
                        b't' => result.push('\t'),
                        b'u' => {
                            let first = self.hex4()?;
                            let scalar = if (0xd800..=0xdbff).contains(&first) {
                                if self.bytes.get(self.offset..self.offset + 2) != Some(b"\\u") {
                                    return Err("unpaired JSON surrogate".to_string());
                                }
                                self.offset += 2;
                                let second = self.hex4()?;
                                if !(0xdc00..=0xdfff).contains(&second) {
                                    return Err("unpaired JSON surrogate".to_string());
                                }
                                0x10000
                                    + (((first - 0xd800) as u32) << 10)
                                    + (second - 0xdc00) as u32
                            } else if (0xdc00..=0xdfff).contains(&first) {
                                return Err("unpaired JSON surrogate".to_string());
                            } else {
                                first as u32
                            };
                            result.push(
                                char::from_u32(scalar)
                                    .ok_or_else(|| "invalid Unicode scalar".to_string())?,
                            );
                        }
                        _ => return Err("invalid JSON escape".to_string()),
                    }
                }
                0x00..=0x1f => return Err("unescaped JSON control character".to_string()),
                _ => {
                    let tail = std::str::from_utf8(&self.bytes[self.offset..])
                        .map_err(|_| "invalid UTF-8 in JSON string".to_string())?;
                    let character = tail
                        .chars()
                        .next()
                        .ok_or_else(|| "truncated JSON string".to_string())?;
                    result.push(character);
                    self.offset += character.len_utf8();
                }
            }
        }
        Err("unterminated JSON string".to_string())
    }
    fn hex4(&mut self) -> Result<u16, String> {
        let bytes = self
            .bytes
            .get(self.offset..self.offset + 4)
            .ok_or_else(|| "truncated Unicode escape".to_string())?;
        self.offset += 4;
        let mut value = 0u16;
        for byte in bytes {
            value <<= 4;
            value += match byte {
                b'0'..=b'9' => (byte - b'0') as u16,
                b'a'..=b'f' => (byte - b'a' + 10) as u16,
                b'A'..=b'F' => (byte - b'A' + 10) as u16,
                _ => return Err("invalid Unicode escape".to_string()),
            };
        }
        Ok(value)
    }
    fn array(&mut self) -> Result<JcsValue, String> {
        self.take(b'[')?;
        let mut values = Vec::new();
        if self.peek() == Some(b']') {
            self.offset += 1;
            return Ok(JcsValue::Array(values));
        }
        loop {
            values.push(self.value()?);
            match self.peek() {
                Some(b',') => self.offset += 1,
                Some(b']') => {
                    self.offset += 1;
                    break;
                }
                _ => return Err("invalid JSON array".to_string()),
            }
        }
        Ok(JcsValue::Array(values))
    }
    fn object(&mut self) -> Result<JcsValue, String> {
        self.take(b'{')?;
        let mut values = BTreeMap::new();
        if self.peek() == Some(b'}') {
            self.offset += 1;
            return Ok(JcsValue::Object(values));
        }
        loop {
            let key = self.string()?;
            self.take(b':')?;
            let value = self.value()?;
            if values.insert(key, value).is_some() {
                return Err("duplicate JSON object key".to_string());
            }
            match self.peek() {
                Some(b',') => self.offset += 1,
                Some(b'}') => {
                    self.offset += 1;
                    break;
                }
                _ => return Err("invalid JSON object".to_string()),
            }
        }
        Ok(JcsValue::Object(values))
    }
    fn integer(&mut self) -> Result<JcsValue, String> {
        let start = self.offset;
        if self.peek() == Some(b'-') {
            self.offset += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.offset += 1;
                if self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    return Err("noncanonical JSON number".to_string());
                }
            }
            Some(b'1'..=b'9') => {
                while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.offset += 1;
                }
            }
            _ => return Err("invalid JSON number".to_string()),
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err("JCS schemas do not admit non-integer JSON numbers".to_string());
        }
        let text = std::str::from_utf8(&self.bytes[start..self.offset])
            .map_err(|_| "JCS JSON integer is outside the exact range".to_string())?;
        let value = text
            .parse::<i64>()
            .map_err(|_| "JCS JSON integer is outside the exact range".to_string())?;
        if value.to_string() != text {
            return Err("noncanonical JSON integer".to_string());
        }
        Ok(JcsValue::Integer(value))
    }
}

macro_rules! string_primitive {
    ($name:ident, $validator:expr, $message:literal) => {
        #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);
        impl $name {
            pub fn parse(value: &str) -> Result<Self, String> {
                if ($validator)(value) {
                    Ok(Self(value.to_string()))
                } else {
                    Err($message.to_string())
                }
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
            pub fn into_string(self) -> String {
                self.0
            }
        }
    };
}

string_primitive!(
    LowerUuidV4,
    |v: &str| {
        let b = v.as_bytes();
        b.len() == 36
            && [8, 13, 18, 23].into_iter().all(|i| b[i] == b'-')
            && b[14] == b'4'
            && matches!(b[19], b'8' | b'9' | b'a' | b'b')
            && b.iter().enumerate().all(|(i, c)| {
                [8, 13, 18, 23].contains(&i) || c.is_ascii_digit() || matches!(c, b'a'..=b'f')
            })
    },
    "value is not a lowercase UUID v4"
);
string_primitive!(
    Sha256Digest,
    |v: &str| v.len() == 64
        && v.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f')),
    "value is not a lowercase SHA-256"
);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_roundtrip_and_duplicate_refusal() {
        let v = parse_jcs(b"{\"a\":[true,null,1],\"z\":\"x\"}\n", true).unwrap();
        assert_eq!(serialize_jcs(&v), "{\"a\":[true,null,1],\"z\":\"x\"}");
        assert!(parse_jcs(b"{\"a\":1,\"a\":2}\n", true).is_err());
    }
    #[test]
    fn control_characters_escape_to_four_hex_digits() {
        let text = String::from("\u{0}\u{f}\u{10}\u{1f} \u{7f}");
        let value = JcsValue::String(text);
        assert_eq!(
            serialize_jcs(&value),
            "\"\\u0000\\u000f\\u0010\\u001f \u{7f}\""
        );
        let parsed = parse_jcs(b"\"\\u0000\\u000f\\u0010\\u001f\"\n", true).unwrap();
        assert_eq!(serialize_jcs(&parsed), "\"\\u0000\\u000f\\u0010\\u001f\"");
    }
    #[test]
    fn primitives_are_closed() {
        assert!(LowerUuidV4::parse("11111111-1111-4111-8111-111111111111").is_ok());
        assert!(LowerUuidV4::parse("11111111-1111-4111-7111-111111111111").is_err());
    }
}
