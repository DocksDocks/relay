use crate::sha256;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

pub const SCHEMA_V1: &str = "1";
pub const COORDINATOR_ACTIONS: [&str; 7] = [
    "abort",
    "finish",
    "inspect",
    "integrate",
    "list",
    "recover",
    "start",
];
pub const WORKER_ACTIONS: [&str; 3] = ["git_commit", "git_index", "handback"];

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
            _ => Err("workspace record must be a JSON object".to_string()),
        }
    }

    pub fn as_str(&self) -> Result<&str, String> {
        match self {
            Self::String(value) => Ok(value),
            _ => Err("workspace field must be a string".to_string()),
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
            return Err("workspace JSON must end in exactly one LF".to_string());
        }
        &bytes[..bytes.len() - 1]
    } else {
        if bytes.ends_with(b"\n") {
            return Err("this canonical JSON transport must not contain a trailing LF".to_string());
        }
        bytes
    };
    let text = std::str::from_utf8(body).map_err(|_| "workspace JSON is not UTF-8".to_string())?;
    let mut parser = Parser {
        bytes: text.as_bytes(),
        offset: 0,
    };
    let value = parser.value()?;
    if parser.offset != parser.bytes.len() {
        return Err("workspace JSON has trailing bytes".to_string());
    }
    let canonical = serialize_jcs(&value);
    if canonical.as_bytes() != body {
        return Err("workspace JSON is not canonical RFC 8785 JCS".to_string());
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
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("securely open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.is_file()
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
    T::from_jcs(parse_jcs(&bytes, true)?)
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
                use std::fmt::Write as _;
                write!(output, "\\u{:04x}", c as u32).expect("String write");
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
            _ => Err("invalid workspace JSON token".to_string()),
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
            Err("invalid workspace JSON punctuation".to_string())
        }
    }
    fn literal(&mut self, literal: &[u8]) -> Result<(), String> {
        if self.bytes.get(self.offset..self.offset + literal.len()) == Some(literal) {
            self.offset += literal.len();
            Ok(())
        } else {
            Err("invalid workspace JSON literal".to_string())
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
            value = value.checked_mul(16).unwrap();
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
            return Err("workspace schemas do not admit non-integer JSON numbers".to_string());
        }
        let text = std::str::from_utf8(&self.bytes[start..self.offset]).unwrap();
        let value = text
            .parse::<i64>()
            .map_err(|_| "workspace JSON integer is outside the exact range".to_string())?;
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
string_primitive!(
    Decimal,
    |v: &str| v == "0"
        || (!v.is_empty() && !v.starts_with('0') && v.bytes().all(|b| b.is_ascii_digit())),
    "value is not canonical unsigned decimal"
);
string_primitive!(
    Timestamp,
    valid_timestamp,
    "value is not an exact millisecond UTC timestamp"
);
string_primitive!(
    TaskSlug,
    |v: &str| !v.is_empty()
        && v.len() <= 48
        && v.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !v.starts_with('-')
        && !v.ends_with('-')
        && !v.contains("--"),
    "task slug is invalid"
);

fn valid_timestamp(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() != 24
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'.'
        || b[23] != b'Z'
    {
        return false;
    }
    if b.iter()
        .enumerate()
        .any(|(i, c)| ![4, 7, 10, 13, 16, 19, 23].contains(&i) && !c.is_ascii_digit())
    {
        return false;
    }
    let number =
        |range: std::ops::Range<usize>| std::str::from_utf8(&b[range]).ok()?.parse::<u32>().ok();
    number(1..4).is_some()
        && matches!(number(5..7), Some(1..=12))
        && matches!(number(8..10), Some(1..=31))
        && matches!(number(11..13), Some(0..=23))
        && matches!(number(14..16), Some(0..=59))
        && matches!(number(17..19), Some(0..=60))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbsPath(PathBuf);
impl AbsPath {
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.contains('\0') {
            return Err("absolute path contains NUL".to_string());
        }
        let path = Path::new(value);
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
        {
            return Err("path is not canonical absolute form".to_string());
        }
        if path.to_str() != Some(value) {
            return Err("absolute path is not UTF-8".to_string());
        }
        Ok(Self(path.to_path_buf()))
    }
    pub fn securely_existing(value: &str) -> Result<Self, String> {
        let parsed = Self::parse(value)?;
        let canonical =
            fs::canonicalize(&parsed.0).map_err(|e| format!("canonicalize {value}: {e}"))?;
        if canonical != parsed.0 {
            return Err("absolute path is not canonical or traverses a symlink".to_string());
        }
        Ok(parsed)
    }
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RelPath(String);
impl RelPath {
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.is_empty()
            || value.starts_with('/')
            || value.ends_with('/')
            || value.contains('\0')
            || value.contains('\\')
            || value
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == "..")
        {
            return Err("relative path is not normalized slash form".to_string());
        }
        Ok(Self(value.to_string()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}
impl ObjectFormat {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "sha1" => Ok(Self::Sha1),
            "sha256" => Ok(Self::Sha256),
            _ => Err("unsupported Git object format".to_string()),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }
    pub fn oid_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitOid(String);
impl GitOid {
    pub fn parse(value: &str, format: ObjectFormat) -> Result<Self, String> {
        if value.len() == format.oid_len()
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
        {
            Ok(Self(value.to_string()))
        } else {
            Err(format!(
                "Git OID is not lowercase {} hexadecimal",
                format.oid_len()
            ))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceState {
    Reserved,
    Provisioning,
    LeaseHeld,
    Ready,
    Running,
    HandbackReady,
    IntegrationQueued,
    Integrated,
    IntegrationBlocked,
    Rejected,
    AbortedRetained,
    Releasing,
    Closed,
}
impl WorkspaceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "Reserved",
            Self::Provisioning => "Provisioning",
            Self::LeaseHeld => "LeaseHeld",
            Self::Ready => "Ready",
            Self::Running => "Running",
            Self::HandbackReady => "HandbackReady",
            Self::IntegrationQueued => "IntegrationQueued",
            Self::Integrated => "Integrated",
            Self::IntegrationBlocked => "IntegrationBlocked",
            Self::Rejected => "Rejected",
            Self::AbortedRetained => "AbortedRetained",
            Self::Releasing => "Releasing",
            Self::Closed => "Closed",
        }
    }
    pub fn parse(v: &str) -> Result<Self, String> {
        Ok(match v {
            "Reserved" => Self::Reserved,
            "Provisioning" => Self::Provisioning,
            "LeaseHeld" => Self::LeaseHeld,
            "Ready" => Self::Ready,
            "Running" => Self::Running,
            "HandbackReady" => Self::HandbackReady,
            "IntegrationQueued" => Self::IntegrationQueued,
            "Integrated" => Self::Integrated,
            "IntegrationBlocked" => Self::IntegrationBlocked,
            "Rejected" => Self::Rejected,
            "AbortedRetained" => Self::AbortedRetained,
            "Releasing" => Self::Releasing,
            "Closed" => Self::Closed,
            _ => return Err("unknown WorkspaceState".to_string()),
        })
    }
    pub fn may_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Reserved, Self::Provisioning)
                | (Self::Provisioning, Self::LeaseHeld)
                | (Self::LeaseHeld, Self::Ready)
                | (Self::Ready, Self::Running)
                | (Self::Running, Self::HandbackReady)
                | (Self::HandbackReady, Self::IntegrationQueued)
                | (Self::IntegrationQueued, Self::Integrated)
                | (Self::IntegrationQueued, Self::IntegrationBlocked)
                | (Self::IntegrationQueued, Self::Rejected)
                | (Self::Running, Self::Rejected)
                | (Self::Running, Self::AbortedRetained)
                | (Self::HandbackReady, Self::AbortedRetained)
                | (Self::IntegrationBlocked, Self::AbortedRetained)
                | (Self::Integrated, Self::Releasing)
                | (Self::Rejected, Self::Releasing)
                | (Self::AbortedRetained, Self::Releasing)
                | (Self::Releasing, Self::Closed)
        )
    }
}

fn require_keys(object: &BTreeMap<String, JcsValue>, keys: &[&str]) -> Result<(), String> {
    let actual: BTreeSet<_> = object.keys().map(String::as_str).collect();
    let expected: BTreeSet<_> = keys.iter().copied().collect();
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "closed workspace object keys differ: expected {:?}, got {:?}",
            expected, actual
        ))
    }
}
fn get_string(object: &BTreeMap<String, JcsValue>, key: &str) -> Result<String, String> {
    object
        .get(key)
        .ok_or_else(|| format!("missing {key}"))?
        .as_str()
        .map(str::to_string)
}
fn object(entries: impl IntoIterator<Item = (&'static str, JcsValue)>) -> JcsValue {
    JcsValue::Object(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryIdentityV1 {
    pub repository_id: String,
    pub common_dir_realpath: String,
    pub common_dir_dev: String,
    pub common_dir_ino: String,
    pub common_dir_owner_euid: String,
    pub euid: String,
    pub object_format: ObjectFormat,
}
impl ClosedJcs for RepositoryIdentityV1 {
    fn from_jcs(v: JcsValue) -> Result<Self, String> {
        let o = v.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "repository_id",
                "common_dir_realpath",
                "common_dir_dev",
                "common_dir_ino",
                "common_dir_owner_euid",
                "euid",
                "object_format",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("RepositoryIdentityV1 schema mismatch".to_string());
        }
        Sha256Digest::parse(&get_string(&o, "repository_id")?)?;
        for k in [
            "common_dir_dev",
            "common_dir_ino",
            "common_dir_owner_euid",
            "euid",
        ] {
            Decimal::parse(&get_string(&o, k)?)?;
        }
        AbsPath::parse(&get_string(&o, "common_dir_realpath")?)?;
        Ok(Self {
            repository_id: get_string(&o, "repository_id")?,
            common_dir_realpath: get_string(&o, "common_dir_realpath")?,
            common_dir_dev: get_string(&o, "common_dir_dev")?,
            common_dir_ino: get_string(&o, "common_dir_ino")?,
            common_dir_owner_euid: get_string(&o, "common_dir_owner_euid")?,
            euid: get_string(&o, "euid")?,
            object_format: ObjectFormat::parse(&get_string(&o, "object_format")?)?,
        })
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            (
                "common_dir_dev",
                JcsValue::String(self.common_dir_dev.clone()),
            ),
            (
                "common_dir_ino",
                JcsValue::String(self.common_dir_ino.clone()),
            ),
            (
                "common_dir_owner_euid",
                JcsValue::String(self.common_dir_owner_euid.clone()),
            ),
            (
                "common_dir_realpath",
                JcsValue::String(self.common_dir_realpath.clone()),
            ),
            ("euid", JcsValue::String(self.euid.clone())),
            (
                "object_format",
                JcsValue::String(self.object_format.as_str().into()),
            ),
            (
                "repository_id",
                JcsValue::String(self.repository_id.clone()),
            ),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityRecordV1 {
    pub capability_id: String,
    pub secret_sha256: String,
    pub generation: String,
    pub actions: Vec<String>,
    pub revoked_at: Option<String>,
}
impl ClosedJcs for CapabilityRecordV1 {
    fn from_jcs(v: JcsValue) -> Result<Self, String> {
        let o = v.object()?;
        require_keys(
            &o,
            &[
                "capability_id",
                "secret_sha256",
                "generation",
                "actions",
                "revoked_at",
            ],
        )?;
        LowerUuidV4::parse(&get_string(&o, "capability_id")?)?;
        Sha256Digest::parse(&get_string(&o, "secret_sha256")?)?;
        Decimal::parse(&get_string(&o, "generation")?)?;
        let actions = match o.get("actions") {
            Some(JcsValue::Array(v)) => v
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("actions must be an array".to_string()),
        };
        validate_action_set(&actions)?;
        let revoked_at = match o.get("revoked_at") {
            Some(JcsValue::Null) => None,
            Some(JcsValue::String(v)) => {
                Timestamp::parse(v)?;
                Some(v.clone())
            }
            _ => return Err("revoked_at has invalid nullability".to_string()),
        };
        Ok(Self {
            capability_id: get_string(&o, "capability_id")?,
            secret_sha256: get_string(&o, "secret_sha256")?,
            generation: get_string(&o, "generation")?,
            actions,
            revoked_at,
        })
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            (
                "actions",
                JcsValue::Array(self.actions.iter().cloned().map(JcsValue::String).collect()),
            ),
            (
                "capability_id",
                JcsValue::String(self.capability_id.clone()),
            ),
            ("generation", JcsValue::String(self.generation.clone())),
            (
                "revoked_at",
                self.revoked_at
                    .clone()
                    .map(JcsValue::String)
                    .unwrap_or(JcsValue::Null),
            ),
            (
                "secret_sha256",
                JcsValue::String(self.secret_sha256.clone()),
            ),
        ])
    }
}

pub fn validate_action_set(actions: &[String]) -> Result<(), String> {
    if actions.is_empty() || actions.windows(2).any(|w| w[0] >= w[1]) {
        return Err("capability actions must be sorted and unique".to_string());
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoordinatorCapabilityV1 {
    pub capability_id: String,
    pub repository_id: String,
    pub generation: String,
    pub actions: Vec<String>,
    pub secret_b64url: String,
    pub issued_at: String,
}
impl ClosedJcs for CoordinatorCapabilityV1 {
    fn from_jcs(v: JcsValue) -> Result<Self, String> {
        let o = v.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "capability_id",
                "repository_id",
                "generation",
                "actions",
                "secret_b64url",
                "issued_at",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("CoordinatorCapabilityV1 schema mismatch".to_string());
        }
        let actions = match o.get("actions") {
            Some(JcsValue::Array(v)) => v
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("actions must be array".into()),
        };
        if actions != COORDINATOR_ACTIONS.map(str::to_string) {
            return Err("coordinator actions differ from the closed set".into());
        }
        validate_cap_fields(&o)?;
        Ok(Self {
            capability_id: get_string(&o, "capability_id")?,
            repository_id: get_string(&o, "repository_id")?,
            generation: get_string(&o, "generation")?,
            actions,
            secret_b64url: get_string(&o, "secret_b64url")?,
            issued_at: get_string(&o, "issued_at")?,
        })
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            (
                "actions",
                JcsValue::Array(self.actions.iter().cloned().map(JcsValue::String).collect()),
            ),
            (
                "capability_id",
                JcsValue::String(self.capability_id.clone()),
            ),
            ("generation", JcsValue::String(self.generation.clone())),
            ("issued_at", JcsValue::String(self.issued_at.clone())),
            (
                "repository_id",
                JcsValue::String(self.repository_id.clone()),
            ),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
            (
                "secret_b64url",
                JcsValue::String(self.secret_b64url.clone()),
            ),
        ])
    }
}
fn validate_cap_fields(o: &BTreeMap<String, JcsValue>) -> Result<(), String> {
    LowerUuidV4::parse(&get_string(o, "capability_id")?)?;
    Sha256Digest::parse(&get_string(o, "repository_id")?)?;
    Decimal::parse(&get_string(o, "generation")?)?;
    Timestamp::parse(&get_string(o, "issued_at")?)?;
    let s = get_string(o, "secret_b64url")?;
    if s.len() != 43
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("capability secret is not 43-character unpadded base64url".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreserveRequestV1 {
    pub request_id: String,
    pub repository_path: String,
    pub base_commit: String,
    pub mode: String,
    pub label: String,
    pub created_at: String,
}
impl ClosedJcs for PreserveRequestV1 {
    fn from_jcs(v: JcsValue) -> Result<Self, String> {
        let o = v.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "request_id",
                "repository_path",
                "base_commit",
                "mode",
                "label",
                "created_at",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("PreserveRequestV1 schema mismatch".into());
        }
        LowerUuidV4::parse(&get_string(&o, "request_id")?)?;
        AbsPath::parse(&get_string(&o, "repository_path")?)?;
        if !matches!(get_string(&o, "mode")?.as_str(), "commit" | "artifact") {
            return Err("preserve mode must be commit|artifact".into());
        }
        Timestamp::parse(&get_string(&o, "created_at")?)?;
        let label = get_string(&o, "label")?;
        if label.is_empty() || label.len() > 128 || label.contains('\0') {
            return Err("preserve label is invalid".into());
        }
        Ok(Self {
            request_id: get_string(&o, "request_id")?,
            repository_path: get_string(&o, "repository_path")?,
            base_commit: get_string(&o, "base_commit")?,
            mode: get_string(&o, "mode")?,
            label,
            created_at: get_string(&o, "created_at")?,
        })
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            ("base_commit", JcsValue::String(self.base_commit.clone())),
            ("created_at", JcsValue::String(self.created_at.clone())),
            ("label", JcsValue::String(self.label.clone())),
            ("mode", JcsValue::String(self.mode.clone())),
            (
                "repository_path",
                JcsValue::String(self.repository_path.clone()),
            ),
            ("request_id", JcsValue::String(self.request_id.clone())),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathClaimRequestV1 {
    pub path: String,
    pub path_type: String,
    pub mode: String,
}
impl PathClaimRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        RelPath::parse(&self.path)?;
        if !matches!(self.path_type.as_str(), "file" | "directory") {
            return Err("claim path_type must be file|directory".into());
        }
        if self.mode != "exclusive" {
            return Err("claim mode must be exclusive".into());
        }
        Ok(())
    }
}

pub fn validate_non_overlapping_claims(claims: &[PathClaimRequestV1]) -> Result<(), String> {
    for claim in claims {
        claim.validate()?;
    }
    let mut sorted: Vec<(String, &str)> = claims
        .iter()
        .map(|claim| {
            (
                claim.path.chars().flat_map(char::to_lowercase).collect(),
                claim.path.as_str(),
            )
        })
        .collect();
    sorted.sort_by(|left, right| left.0.cmp(&right.0));
    for pair in sorted.windows(2) {
        if pair[0].0 == pair[1].0
            || pair[1]
                .0
                .strip_prefix(&pair[0].0)
                .is_some_and(|rest| rest.starts_with('/'))
        {
            return Err(format!(
                "overlapping path claims: {} and {}",
                pair[0].1, pair[1].1
            ));
        }
    }
    Ok(())
}

fn string_array(value: &JcsValue, name: &str) -> Result<Vec<String>, String> {
    match value {
        JcsValue::Array(values) => values
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Err(format!("{name} must be an array of strings")),
    }
}
fn optional_string(value: &JcsValue, name: &str) -> Result<Option<String>, String> {
    match value {
        JcsValue::Null => Ok(None),
        JcsValue::String(v) => Ok(Some(v.clone())),
        _ => Err(format!("{name} has invalid nullability")),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSnapshotV1 {
    pub head_oid: String,
    pub index_sha256: Option<String>,
    pub status_sha256: String,
    pub tracked_untracked_inventory_sha256: String,
}
impl SourceSnapshotV1 {
    fn from_value(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(
            &o,
            &[
                "head_oid",
                "index_sha256",
                "status_sha256",
                "tracked_untracked_inventory_sha256",
            ],
        )?;
        let index_sha256 = optional_string(&o["index_sha256"], "index_sha256")?;
        if let Some(v) = &index_sha256 {
            Sha256Digest::parse(v)?;
        }
        for key in ["status_sha256", "tracked_untracked_inventory_sha256"] {
            Sha256Digest::parse(&get_string(&o, key)?)?;
        }
        Ok(Self {
            head_oid: get_string(&o, "head_oid")?,
            index_sha256,
            status_sha256: get_string(&o, "status_sha256")?,
            tracked_untracked_inventory_sha256: get_string(
                &o,
                "tracked_untracked_inventory_sha256",
            )?,
        })
    }
    fn value(&self) -> JcsValue {
        object([
            ("head_oid", JcsValue::String(self.head_oid.clone())),
            (
                "index_sha256",
                self.index_sha256
                    .clone()
                    .map(JcsValue::String)
                    .unwrap_or(JcsValue::Null),
            ),
            (
                "status_sha256",
                JcsValue::String(self.status_sha256.clone()),
            ),
            (
                "tracked_untracked_inventory_sha256",
                JcsValue::String(self.tracked_untracked_inventory_sha256.clone()),
            ),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WipPayloadV1 {
    Commit {
        temporary_index_sha256: String,
        tree_oid: String,
        preserved_commit: String,
        preserve_ref: String,
    },
    Artifact {
        binary_diff: String,
        untracked_inventory: String,
        untracked_archive: String,
        archive_format: String,
        entries: Vec<String>,
    },
}
impl WipPayloadV1 {
    fn from_value(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        let kind = get_string(&o, "kind")?;
        match kind.as_str() {
            "commit" => {
                require_keys(
                    &o,
                    &[
                        "kind",
                        "temporary_index_sha256",
                        "tree_oid",
                        "preserved_commit",
                        "preserve_ref",
                    ],
                )?;
                Sha256Digest::parse(&get_string(&o, "temporary_index_sha256")?)?;
                let preserve_ref = get_string(&o, "preserve_ref")?;
                if !preserve_ref.starts_with("refs/docks/preserve/") {
                    return Err("preserve ref is outside refs/docks/preserve".into());
                }
                Ok(Self::Commit {
                    temporary_index_sha256: get_string(&o, "temporary_index_sha256")?,
                    tree_oid: get_string(&o, "tree_oid")?,
                    preserved_commit: get_string(&o, "preserved_commit")?,
                    preserve_ref,
                })
            }
            "artifact" => {
                require_keys(
                    &o,
                    &[
                        "kind",
                        "binary_diff",
                        "untracked_inventory",
                        "untracked_archive",
                        "archive_format",
                        "entries",
                    ],
                )?;
                if get_string(&o, "archive_format")? != "pax" {
                    return Err("artifact archive format must be pax".into());
                }
                for key in ["binary_diff", "untracked_inventory", "untracked_archive"] {
                    AbsPath::parse(&get_string(&o, key)?)?;
                }
                let entries = string_array(&o["entries"], "entries")?;
                let mut sorted = entries.clone();
                sorted.sort();
                sorted.dedup();
                if sorted != entries {
                    return Err("artifact entries must be sorted and unique".into());
                }
                for entry in &entries {
                    RelPath::parse(entry)?;
                }
                Ok(Self::Artifact {
                    binary_diff: get_string(&o, "binary_diff")?,
                    untracked_inventory: get_string(&o, "untracked_inventory")?,
                    untracked_archive: get_string(&o, "untracked_archive")?,
                    archive_format: "pax".into(),
                    entries,
                })
            }
            _ => Err("WIP payload kind must be commit|artifact".into()),
        }
    }
    fn value(&self) -> JcsValue {
        match self {
            Self::Commit {
                temporary_index_sha256,
                tree_oid,
                preserved_commit,
                preserve_ref,
            } => object([
                ("kind", JcsValue::String("commit".into())),
                ("preserve_ref", JcsValue::String(preserve_ref.clone())),
                (
                    "preserved_commit",
                    JcsValue::String(preserved_commit.clone()),
                ),
                (
                    "temporary_index_sha256",
                    JcsValue::String(temporary_index_sha256.clone()),
                ),
                ("tree_oid", JcsValue::String(tree_oid.clone())),
            ]),
            Self::Artifact {
                binary_diff,
                untracked_inventory,
                untracked_archive,
                archive_format,
                entries,
            } => object([
                ("archive_format", JcsValue::String(archive_format.clone())),
                ("binary_diff", JcsValue::String(binary_diff.clone())),
                (
                    "entries",
                    JcsValue::Array(entries.iter().cloned().map(JcsValue::String).collect()),
                ),
                ("kind", JcsValue::String("artifact".into())),
                (
                    "untracked_archive",
                    JcsValue::String(untracked_archive.clone()),
                ),
                (
                    "untracked_inventory",
                    JcsValue::String(untracked_inventory.clone()),
                ),
            ]),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WipReceiptV1 {
    pub receipt_id: String,
    pub request_sha256: String,
    pub repository: RepositoryIdentityV1,
    pub source_root: String,
    pub base_commit: String,
    pub mode: String,
    pub before: SourceSnapshotV1,
    pub after: SourceSnapshotV1,
    pub payload: WipPayloadV1,
    pub created_at: String,
}
impl ClosedJcs for WipReceiptV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let mut o = value.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "receipt_id",
                "request_sha256",
                "repository",
                "source_root",
                "base_commit",
                "mode",
                "before",
                "after",
                "payload",
                "created_at",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("WipReceiptV1 schema mismatch".into());
        }
        LowerUuidV4::parse(&get_string(&o, "receipt_id")?)?;
        Sha256Digest::parse(&get_string(&o, "request_sha256")?)?;
        AbsPath::parse(&get_string(&o, "source_root")?)?;
        Timestamp::parse(&get_string(&o, "created_at")?)?;
        let before = SourceSnapshotV1::from_value(o.remove("before").unwrap())?;
        let after = SourceSnapshotV1::from_value(o.remove("after").unwrap())?;
        if before != after {
            return Err("WIP source before/after snapshots differ".into());
        }
        let repository = RepositoryIdentityV1::from_jcs(o.remove("repository").unwrap())?;
        let payload = WipPayloadV1::from_value(o.remove("payload").unwrap())?;
        let base_commit = get_string(&o, "base_commit")?;
        GitOid::parse(&base_commit, repository.object_format)?;
        GitOid::parse(&before.head_oid, repository.object_format)?;
        if let WipPayloadV1::Commit {
            tree_oid,
            preserved_commit,
            ..
        } = &payload
        {
            GitOid::parse(tree_oid, repository.object_format)?;
            GitOid::parse(preserved_commit, repository.object_format)?;
        }
        let mode = get_string(&o, "mode")?;
        let mode_matches = matches!((&mode,&payload),(m,WipPayloadV1::Commit{..}) if m=="commit")
            || matches!((&mode,&payload),(m,WipPayloadV1::Artifact{..}) if m=="artifact");
        if !mode_matches {
            return Err("WIP mode and payload kind differ".into());
        }
        Ok(Self {
            receipt_id: get_string(&o, "receipt_id")?,
            request_sha256: get_string(&o, "request_sha256")?,
            repository,
            source_root: get_string(&o, "source_root")?,
            base_commit,
            mode,
            before,
            after,
            payload,
            created_at: get_string(&o, "created_at")?,
        })
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            ("after", self.after.value()),
            ("base_commit", JcsValue::String(self.base_commit.clone())),
            ("before", self.before.value()),
            ("created_at", JcsValue::String(self.created_at.clone())),
            ("mode", JcsValue::String(self.mode.clone())),
            ("payload", self.payload.value()),
            ("receipt_id", JcsValue::String(self.receipt_id.clone())),
            ("repository", self.repository.to_jcs()),
            (
                "request_sha256",
                JcsValue::String(self.request_sha256.clone()),
            ),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
            ("source_root", JcsValue::String(self.source_root.clone())),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolLaunchV1 {
    pub kind: String,
    pub executable_path: String,
    pub executable_sha256: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
}
impl ToolLaunchV1 {
    fn from_value(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(
            &o,
            &[
                "kind",
                "executable_path",
                "executable_sha256",
                "model",
                "effort",
                "service_tier",
            ],
        )?;
        let kind = get_string(&o, "kind")?;
        if !matches!(kind.as_str(), "claude" | "codex" | "omp") {
            return Err("tool kind must be claude|codex|omp".into());
        }
        AbsPath::parse(&get_string(&o, "executable_path")?)?;
        Sha256Digest::parse(&get_string(&o, "executable_sha256")?)?;
        let service_tier = optional_string(&o["service_tier"], "service_tier")?;
        if service_tier
            .as_deref()
            .is_some_and(|v| !matches!(v, "default" | "fast"))
        {
            return Err("service_tier must be null|default|fast".into());
        }
        Ok(Self {
            kind,
            executable_path: get_string(&o, "executable_path")?,
            executable_sha256: get_string(&o, "executable_sha256")?,
            model: optional_string(&o["model"], "model")?,
            effort: optional_string(&o["effort"], "effort")?,
            service_tier,
        })
    }
    fn value(&self) -> JcsValue {
        object([
            (
                "effort",
                self.effort
                    .clone()
                    .map(JcsValue::String)
                    .unwrap_or(JcsValue::Null),
            ),
            (
                "executable_path",
                JcsValue::String(self.executable_path.clone()),
            ),
            (
                "executable_sha256",
                JcsValue::String(self.executable_sha256.clone()),
            ),
            ("kind", JcsValue::String(self.kind.clone())),
            (
                "model",
                self.model
                    .clone()
                    .map(JcsValue::String)
                    .unwrap_or(JcsValue::Null),
            ),
            (
                "service_tier",
                self.service_tier
                    .clone()
                    .map(JcsValue::String)
                    .unwrap_or(JcsValue::Null),
            ),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceDecisionV1 {
    pub kind: String,
    pub name: String,
    pub state: String,
    pub provider_id: Option<String>,
    pub reason: Option<String>,
}
impl ResourceDecisionV1 {
    pub fn from_value(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        let state = get_string(&o, "state")?;
        let kind = get_string(&o, "kind")?;
        if !matches!(
            kind.as_str(),
            "port" | "temp_dir" | "build_dir" | "database_schema" | "log_dir" | "cache_dir"
        ) {
            return Err("unknown resource kind".into());
        }
        let name = get_string(&o, "name")?;
        if name.is_empty()
            || name.len() > 32
            || !name.bytes().enumerate().all(|(i, b)| {
                if i == 0 {
                    b.is_ascii_lowercase()
                } else {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
                }
            })
        {
            return Err("resource name is invalid".into());
        }
        match state.as_str() {
            "requested" => {
                require_keys(&o, &["kind", "name", "state", "provider_id"])?;
                Ok(Self {
                    kind,
                    name,
                    state,
                    provider_id: optional_string(&o["provider_id"], "provider_id")?,
                    reason: None,
                })
            }
            "unused" => {
                require_keys(&o, &["kind", "name", "state", "reason"])?;
                if get_string(&o, "reason")? != "task_does_not_use_resource" {
                    return Err("unused resource reason is not closed".into());
                }
                Ok(Self {
                    kind,
                    name,
                    state,
                    provider_id: None,
                    reason: Some("task_does_not_use_resource".into()),
                })
            }
            _ => Err("resource state must be requested|unused".into()),
        }
    }
    pub fn value(&self) -> JcsValue {
        if self.state == "requested" {
            object([
                ("kind", JcsValue::String(self.kind.clone())),
                ("name", JcsValue::String(self.name.clone())),
                (
                    "provider_id",
                    self.provider_id
                        .clone()
                        .map(JcsValue::String)
                        .unwrap_or(JcsValue::Null),
                ),
                ("state", JcsValue::String(self.state.clone())),
            ])
        } else {
            object([
                ("kind", JcsValue::String(self.kind.clone())),
                ("name", JcsValue::String(self.name.clone())),
                (
                    "reason",
                    JcsValue::String("task_does_not_use_resource".into()),
                ),
                ("state", JcsValue::String(self.state.clone())),
            ])
        }
    }
}

fn claim_from_value(value: JcsValue) -> Result<PathClaimRequestV1, String> {
    let o = value.object()?;
    require_keys(&o, &["path", "path_type", "mode"])?;
    let claim = PathClaimRequestV1 {
        path: get_string(&o, "path")?,
        path_type: get_string(&o, "path_type")?,
        mode: get_string(&o, "mode")?,
    };
    claim.validate()?;
    Ok(claim)
}
fn claim_value(claim: &PathClaimRequestV1) -> JcsValue {
    object([
        ("mode", JcsValue::String(claim.mode.clone())),
        ("path", JcsValue::String(claim.path.clone())),
        ("path_type", JcsValue::String(claim.path_type.clone())),
    ])
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceStartRequestV1 {
    pub request_id: String,
    pub repository_path: String,
    pub integration_root: String,
    pub base_commit: String,
    pub task_slug: String,
    pub task: String,
    pub tool: ToolLaunchV1,
    pub wip_receipt_path: String,
    pub wip_receipt_sha256: String,
    pub owned_paths: Vec<PathClaimRequestV1>,
    pub coordinator_owned_paths: Vec<PathClaimRequestV1>,
    pub coordinator_owned_overrides: Vec<JcsValue>,
    pub resources: Vec<ResourceDecisionV1>,
    pub created_at: String,
}
impl ClosedJcs for WorkspaceStartRequestV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "request_id",
                "repository_path",
                "integration_root",
                "base_commit",
                "task_slug",
                "task",
                "tool",
                "wip_receipt_path",
                "wip_receipt_sha256",
                "owned_paths",
                "coordinator_owned_paths",
                "coordinator_owned_overrides",
                "resources",
                "created_at",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("WorkspaceStartRequestV1 schema mismatch".into());
        }
        LowerUuidV4::parse(&get_string(&o, "request_id")?)?;
        for key in ["repository_path", "integration_root", "wip_receipt_path"] {
            AbsPath::parse(&get_string(&o, key)?)?;
        }
        TaskSlug::parse(&get_string(&o, "task_slug")?)?;
        Sha256Digest::parse(&get_string(&o, "wip_receipt_sha256")?)?;
        Timestamp::parse(&get_string(&o, "created_at")?)?;
        let owned_paths = match &o["owned_paths"] {
            JcsValue::Array(values) => values
                .iter()
                .cloned()
                .map(claim_from_value)
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("owned_paths must be array".into()),
        };
        let coordinator_owned_paths = match &o["coordinator_owned_paths"] {
            JcsValue::Array(values) => values
                .iter()
                .cloned()
                .map(claim_from_value)
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("coordinator_owned_paths must be array".into()),
        };
        validate_non_overlapping_claims(&owned_paths)?;
        validate_non_overlapping_claims(&coordinator_owned_paths)?;
        let coordinator_owned_overrides = match &o["coordinator_owned_overrides"] {
            JcsValue::Array(values) => values.clone(),
            _ => return Err("coordinator_owned_overrides must be array".into()),
        };
        for override_value in &coordinator_owned_overrides {
            let override_object = match override_value {
                JcsValue::Object(object) => object,
                _ => return Err("CoordinatorOwnedOverrideV1 must be an object".into()),
            };
            require_keys(override_object, &["path", "reason"])?;
            RelPath::parse(get_string(override_object, "path")?.as_str())?;
            let reason = get_string(override_object, "reason")?;
            if reason.is_empty() || reason.contains('\0') {
                return Err("coordinator-owned override reason is invalid".into());
            }
        }
        let resources = match &o["resources"] {
            JcsValue::Array(values) => values
                .iter()
                .cloned()
                .map(ResourceDecisionV1::from_value)
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("resources must be array".into()),
        };
        let kinds: BTreeSet<_> = resources
            .iter()
            .map(|resource| resource.kind.as_str())
            .collect();
        if resources.len() != 6
            || kinds
                != BTreeSet::from([
                    "port",
                    "temp_dir",
                    "build_dir",
                    "database_schema",
                    "log_dir",
                    "cache_dir",
                ])
        {
            return Err(
                "start request requires exactly one decision for all six resource kinds".into(),
            );
        }
        Ok(Self {
            request_id: get_string(&o, "request_id")?,
            repository_path: get_string(&o, "repository_path")?,
            integration_root: get_string(&o, "integration_root")?,
            base_commit: get_string(&o, "base_commit")?,
            task_slug: get_string(&o, "task_slug")?,
            task: get_string(&o, "task")?,
            tool: ToolLaunchV1::from_value(o["tool"].clone())?,
            wip_receipt_path: get_string(&o, "wip_receipt_path")?,
            wip_receipt_sha256: get_string(&o, "wip_receipt_sha256")?,
            owned_paths,
            coordinator_owned_paths,
            coordinator_owned_overrides,
            resources,
            created_at: get_string(&o, "created_at")?,
        })
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            ("base_commit", JcsValue::String(self.base_commit.clone())),
            (
                "coordinator_owned_overrides",
                JcsValue::Array(self.coordinator_owned_overrides.clone()),
            ),
            (
                "coordinator_owned_paths",
                JcsValue::Array(
                    self.coordinator_owned_paths
                        .iter()
                        .map(claim_value)
                        .collect(),
                ),
            ),
            ("created_at", JcsValue::String(self.created_at.clone())),
            (
                "integration_root",
                JcsValue::String(self.integration_root.clone()),
            ),
            (
                "owned_paths",
                JcsValue::Array(self.owned_paths.iter().map(claim_value).collect()),
            ),
            (
                "repository_path",
                JcsValue::String(self.repository_path.clone()),
            ),
            ("request_id", JcsValue::String(self.request_id.clone())),
            (
                "resources",
                JcsValue::Array(
                    self.resources
                        .iter()
                        .map(ResourceDecisionV1::value)
                        .collect(),
                ),
            ),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
            ("task", JcsValue::String(self.task.clone())),
            ("task_slug", JcsValue::String(self.task_slug.clone())),
            ("tool", self.tool.value()),
            (
                "wip_receipt_path",
                JcsValue::String(self.wip_receipt_path.clone()),
            ),
            (
                "wip_receipt_sha256",
                JcsValue::String(self.wip_receipt_sha256.clone()),
            ),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnvProjectionV1 {
    pub name: String,
    pub value: String,
}
impl EnvProjectionV1 {
    pub fn from_value(value: JcsValue) -> Result<Self, String> {
        let object = value.object()?;
        require_keys(&object, &["name", "value"])?;
        let name = get_string(&object, "name")?;
        let value = get_string(&object, "value")?;
        if name.is_empty()
            || name.contains('=')
            || name.as_bytes().contains(&0)
            || value.as_bytes().contains(&0)
        {
            return Err("resource environment projection is invalid".into());
        }
        Ok(Self { name, value })
    }
    pub fn value(&self) -> JcsValue {
        object([
            ("name", JcsValue::String(self.name.clone())),
            ("value", JcsValue::String(self.value.clone())),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceAllocationV1 {
    pub allocation_id: String,
    pub session_id: String,
    pub kind: String,
    pub name: String,
    pub provider_id: String,
    pub state: String,
    pub value: String,
    pub env: Vec<EnvProjectionV1>,
    pub create_receipt_sha256: String,
    pub inspect_receipt_sha256: String,
    pub delete_receipt_sha256: Option<String>,
    pub created_at: String,
    pub released_at: Option<String>,
}
impl ClosedJcs for ResourceAllocationV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let object = value.object()?;
        require_keys(
            &object,
            &[
                "schema",
                "allocation_id",
                "session_id",
                "kind",
                "name",
                "provider_id",
                "state",
                "value",
                "env",
                "create_receipt_sha256",
                "inspect_receipt_sha256",
                "delete_receipt_sha256",
                "created_at",
                "released_at",
            ],
        )?;
        if get_string(&object, "schema")? != "ResourceAllocationV1" {
            return Err("ResourceAllocationV1 schema mismatch".into());
        }
        let allocation_id = get_string(&object, "allocation_id")?;
        let session_id = get_string(&object, "session_id")?;
        LowerUuidV4::parse(&allocation_id)?;
        LowerUuidV4::parse(&session_id)?;
        let kind = get_string(&object, "kind")?;
        if !matches!(
            kind.as_str(),
            "port" | "temp_dir" | "build_dir" | "database_schema" | "log_dir" | "cache_dir"
        ) {
            return Err("unknown resource allocation kind".into());
        }
        let name = get_string(&object, "name")?;
        validate_resource_name(&name)?;
        let provider_id = get_string(&object, "provider_id")?;
        if provider_id.is_empty() || provider_id.as_bytes().contains(&0) {
            return Err("resource provider_id is invalid".into());
        }
        let state = get_string(&object, "state")?;
        if !matches!(state.as_str(), "allocated" | "released") {
            return Err("resource allocation state must be allocated|released".into());
        }
        let env = match &object["env"] {
            JcsValue::Array(values) => values
                .iter()
                .cloned()
                .map(EnvProjectionV1::from_value)
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("resource env must be an array".into()),
        };
        let mut names = BTreeSet::new();
        if env
            .iter()
            .any(|projection| !names.insert(projection.name.as_str()))
        {
            return Err("resource environment projection names must be unique".into());
        }
        let allocation_value = get_string(&object, "value")?;
        validate_resource_allocation_projection(
            &kind,
            &name,
            &provider_id,
            &allocation_value,
            &env,
        )?;
        let create_receipt_sha256 = get_string(&object, "create_receipt_sha256")?;
        let inspect_receipt_sha256 = get_string(&object, "inspect_receipt_sha256")?;
        Sha256Digest::parse(&create_receipt_sha256)?;
        Sha256Digest::parse(&inspect_receipt_sha256)?;
        let delete_receipt_sha256 =
            optional_string(&object["delete_receipt_sha256"], "delete_receipt_sha256")?;
        if let Some(value) = &delete_receipt_sha256 {
            Sha256Digest::parse(value)?;
        }
        let created_at = get_string(&object, "created_at")?;
        Timestamp::parse(&created_at)?;
        let released_at = optional_string(&object["released_at"], "released_at")?;
        if let Some(value) = &released_at {
            Timestamp::parse(value)?;
        }
        if (state == "allocated" && (delete_receipt_sha256.is_some() || released_at.is_some()))
            || (state == "released" && (delete_receipt_sha256.is_none() || released_at.is_none()))
        {
            return Err("resource release fields disagree with state".into());
        }
        Ok(Self {
            allocation_id,
            session_id,
            kind,
            name,
            provider_id,
            state,
            value: allocation_value,
            env,
            create_receipt_sha256,
            inspect_receipt_sha256,
            delete_receipt_sha256,
            created_at,
            released_at,
        })
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            (
                "allocation_id",
                JcsValue::String(self.allocation_id.clone()),
            ),
            (
                "create_receipt_sha256",
                JcsValue::String(self.create_receipt_sha256.clone()),
            ),
            ("created_at", JcsValue::String(self.created_at.clone())),
            (
                "delete_receipt_sha256",
                self.delete_receipt_sha256
                    .clone()
                    .map(JcsValue::String)
                    .unwrap_or(JcsValue::Null),
            ),
            (
                "env",
                JcsValue::Array(self.env.iter().map(EnvProjectionV1::value).collect()),
            ),
            (
                "inspect_receipt_sha256",
                JcsValue::String(self.inspect_receipt_sha256.clone()),
            ),
            ("kind", JcsValue::String(self.kind.clone())),
            ("name", JcsValue::String(self.name.clone())),
            ("provider_id", JcsValue::String(self.provider_id.clone())),
            (
                "released_at",
                self.released_at
                    .clone()
                    .map(JcsValue::String)
                    .unwrap_or(JcsValue::Null),
            ),
            ("schema", JcsValue::String("ResourceAllocationV1".into())),
            ("session_id", JcsValue::String(self.session_id.clone())),
            ("state", JcsValue::String(self.state.clone())),
            ("value", JcsValue::String(self.value.clone())),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceProviderRegistrationV1 {
    pub provider_id: String,
    pub executable_path: String,
    pub executable_sha256: String,
    pub config_path: String,
    pub config_sha256: String,
    pub supported_kinds: Vec<String>,
}
impl ResourceProviderRegistrationV1 {
    fn from_value(value: JcsValue) -> Result<Self, String> {
        let object = value.object()?;
        require_keys(
            &object,
            &[
                "provider_id",
                "executable_path",
                "executable_sha256",
                "config_path",
                "config_sha256",
                "supported_kinds",
            ],
        )?;
        let provider_id = get_string(&object, "provider_id")?;
        if provider_id.is_empty()
            || provider_id == "builtin"
            || !provider_id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err("resource provider_id is invalid".into());
        }
        let executable_path = get_string(&object, "executable_path")?;
        let config_path = get_string(&object, "config_path")?;
        AbsPath::parse(&executable_path)?;
        AbsPath::parse(&config_path)?;
        let executable_sha256 = get_string(&object, "executable_sha256")?;
        let config_sha256 = get_string(&object, "config_sha256")?;
        Sha256Digest::parse(&executable_sha256)?;
        Sha256Digest::parse(&config_sha256)?;
        let supported_kinds = match &object["supported_kinds"] {
            JcsValue::Array(values) => values
                .iter()
                .map(|value| value.as_str().map(str::to_string))
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("supported_kinds must be an array".into()),
        };
        if supported_kinds != ["database_schema"] {
            return Err("provider supported_kinds must be exactly database_schema".into());
        }
        Ok(Self {
            provider_id,
            executable_path,
            executable_sha256,
            config_path,
            config_sha256,
            supported_kinds,
        })
    }
    fn value(&self) -> JcsValue {
        object([
            ("config_path", JcsValue::String(self.config_path.clone())),
            (
                "config_sha256",
                JcsValue::String(self.config_sha256.clone()),
            ),
            (
                "executable_path",
                JcsValue::String(self.executable_path.clone()),
            ),
            (
                "executable_sha256",
                JcsValue::String(self.executable_sha256.clone()),
            ),
            ("provider_id", JcsValue::String(self.provider_id.clone())),
            (
                "supported_kinds",
                JcsValue::Array(
                    self.supported_kinds
                        .iter()
                        .cloned()
                        .map(JcsValue::String)
                        .collect(),
                ),
            ),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceProviderRegistryV1 {
    pub providers: Vec<ResourceProviderRegistrationV1>,
    pub updated_at: String,
}
impl ClosedJcs for ResourceProviderRegistryV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let object = value.object()?;
        require_keys(&object, &["schema", "providers", "updated_at"])?;
        if get_string(&object, "schema")? != "ResourceProviderRegistryV1" {
            return Err("ResourceProviderRegistryV1 schema mismatch".into());
        }
        let providers = match &object["providers"] {
            JcsValue::Array(values) => values
                .iter()
                .cloned()
                .map(ResourceProviderRegistrationV1::from_value)
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("providers must be an array".into()),
        };
        let mut ids = BTreeSet::new();
        if providers
            .iter()
            .any(|provider| !ids.insert(provider.provider_id.as_str()))
        {
            return Err("provider registry contains duplicate ids".into());
        }
        let updated_at = get_string(&object, "updated_at")?;
        Timestamp::parse(&updated_at)?;
        Ok(Self {
            providers,
            updated_at,
        })
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            (
                "providers",
                JcsValue::Array(
                    self.providers
                        .iter()
                        .map(ResourceProviderRegistrationV1::value)
                        .collect(),
                ),
            ),
            (
                "schema",
                JcsValue::String("ResourceProviderRegistryV1".into()),
            ),
            ("updated_at", JcsValue::String(self.updated_at.clone())),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderRequestV1 {
    pub operation: String,
    pub request_id: String,
    pub allocation_id: String,
    pub session_id: String,
    pub kind: String,
    pub name: String,
    pub prior_receipt_sha256: Option<String>,
}
impl ClosedJcs for ProviderRequestV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let object = value.object()?;
        require_keys(
            &object,
            &[
                "schema",
                "operation",
                "request_id",
                "allocation_id",
                "session_id",
                "kind",
                "name",
                "prior_receipt_sha256",
            ],
        )?;
        if get_string(&object, "schema")? != "ProviderRequestV1" {
            return Err("ProviderRequestV1 schema mismatch".into());
        }
        let operation = get_string(&object, "operation")?;
        if !matches!(operation.as_str(), "create" | "inspect" | "delete") {
            return Err("provider operation is invalid".into());
        }
        let request_id = get_string(&object, "request_id")?;
        let allocation_id = get_string(&object, "allocation_id")?;
        let session_id = get_string(&object, "session_id")?;
        LowerUuidV4::parse(&request_id)?;
        LowerUuidV4::parse(&allocation_id)?;
        LowerUuidV4::parse(&session_id)?;
        if get_string(&object, "kind")? != "database_schema" {
            return Err("provider kind must be database_schema".into());
        }
        let name = get_string(&object, "name")?;
        validate_resource_name(&name)?;
        let prior_receipt_sha256 =
            optional_string(&object["prior_receipt_sha256"], "prior_receipt_sha256")?;
        if let Some(value) = &prior_receipt_sha256 {
            Sha256Digest::parse(value)?;
        }
        if (operation == "create") != prior_receipt_sha256.is_none() {
            return Err("provider prior receipt nullability disagrees with operation".into());
        }
        Ok(Self {
            operation,
            request_id,
            allocation_id,
            session_id,
            kind: "database_schema".into(),
            name,
            prior_receipt_sha256,
        })
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            (
                "allocation_id",
                JcsValue::String(self.allocation_id.clone()),
            ),
            ("kind", JcsValue::String(self.kind.clone())),
            ("name", JcsValue::String(self.name.clone())),
            ("operation", JcsValue::String(self.operation.clone())),
            (
                "prior_receipt_sha256",
                self.prior_receipt_sha256
                    .clone()
                    .map(JcsValue::String)
                    .unwrap_or(JcsValue::Null),
            ),
            ("request_id", JcsValue::String(self.request_id.clone())),
            ("schema", JcsValue::String("ProviderRequestV1".into())),
            ("session_id", JcsValue::String(self.session_id.clone())),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderReceiptV1 {
    pub request_id: String,
    pub allocation_id: String,
    pub operation: String,
    pub outcome: String,
    pub value: String,
    pub provider_evidence_sha256: String,
    pub at: String,
}
impl ClosedJcs for ProviderReceiptV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let object = value.object()?;
        require_keys(
            &object,
            &[
                "schema",
                "request_id",
                "allocation_id",
                "operation",
                "outcome",
                "value",
                "provider_evidence_sha256",
                "at",
            ],
        )?;
        if get_string(&object, "schema")? != "ProviderReceiptV1" {
            return Err("ProviderReceiptV1 schema mismatch".into());
        }
        let request_id = get_string(&object, "request_id")?;
        let allocation_id = get_string(&object, "allocation_id")?;
        LowerUuidV4::parse(&request_id)?;
        LowerUuidV4::parse(&allocation_id)?;
        let operation = get_string(&object, "operation")?;
        if !matches!(operation.as_str(), "create" | "inspect" | "delete") {
            return Err("provider receipt operation is invalid".into());
        }
        let outcome = get_string(&object, "outcome")?;
        if !matches!(outcome.as_str(), "allocated" | "exists" | "released") {
            return Err("provider receipt outcome is invalid".into());
        }
        let provider_evidence_sha256 = get_string(&object, "provider_evidence_sha256")?;
        Sha256Digest::parse(&provider_evidence_sha256)?;
        let at = get_string(&object, "at")?;
        Timestamp::parse(&at)?;
        Ok(Self {
            request_id,
            allocation_id,
            operation,
            outcome,
            value: get_string(&object, "value")?,
            provider_evidence_sha256,
            at,
        })
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            (
                "allocation_id",
                JcsValue::String(self.allocation_id.clone()),
            ),
            ("at", JcsValue::String(self.at.clone())),
            ("operation", JcsValue::String(self.operation.clone())),
            ("outcome", JcsValue::String(self.outcome.clone())),
            (
                "provider_evidence_sha256",
                JcsValue::String(self.provider_evidence_sha256.clone()),
            ),
            ("request_id", JcsValue::String(self.request_id.clone())),
            ("schema", JcsValue::String("ProviderReceiptV1".into())),
            ("value", JcsValue::String(self.value.clone())),
        ])
    }
}

pub fn validate_resource_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 32
        || !name.bytes().enumerate().all(|(index, byte)| {
            if index == 0 {
                byte.is_ascii_lowercase()
            } else {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
            }
        })
    {
        return Err("resource name is invalid".into());
    }
    Ok(())
}

fn validate_resource_allocation_projection(
    kind: &str,
    name: &str,
    provider_id: &str,
    value: &str,
    env: &[EnvProjectionV1],
) -> Result<(), String> {
    let exact = |names: &[&str]| {
        env.len() == names.len()
            && env
                .iter()
                .zip(names)
                .all(|(projection, name)| projection.name == *name && projection.value == value)
    };
    match kind {
        "port" => {
            if provider_id != "builtin"
                || env.len() != 1
                || env[0].name != format!("DOCKS_RESOURCE_{}_FD", name.to_ascii_uppercase())
            {
                return Err("port allocation projection is invalid".into());
            }
            let fd = env[0]
                .value
                .parse::<i32>()
                .map_err(|_| "port allocation FD is not decimal".to_string())?;
            if fd < 3 || fd.to_string() != env[0].value {
                return Err("port allocation FD is not canonical".into());
            }
            let address = value
                .parse::<std::net::SocketAddr>()
                .map_err(|_| "port allocation value is not a socket address".to_string())?;
            if address.ip() != std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                || address.port() == 0
            {
                return Err("port allocation is not held IPv4 loopback".into());
            }
        }
        "temp_dir" => {
            if provider_id != "builtin" || !exact(&["TMPDIR", "TMP", "TEMP"]) {
                return Err("temp_dir allocation projection is invalid".into());
            }
            AbsPath::parse(value)?;
        }
        "build_dir" => {
            if provider_id != "builtin" || !exact(&["DOCKS_BUILD_DIR"]) {
                return Err("build_dir allocation projection is invalid".into());
            }
            AbsPath::parse(value)?;
        }
        "log_dir" => {
            if provider_id != "builtin" || !exact(&["DOCKS_LOG_DIR"]) {
                return Err("log_dir allocation projection is invalid".into());
            }
            AbsPath::parse(value)?;
        }
        "cache_dir" => {
            if provider_id != "builtin" || !exact(&["DOCKS_CACHE_DIR"]) {
                return Err("cache_dir allocation projection is invalid".into());
            }
            AbsPath::parse(value)?;
        }
        "database_schema" => {
            if provider_id == "builtin"
                || value.is_empty()
                || value.as_bytes().contains(&0)
                || !exact(&[&format!(
                    "DOCKS_RESOURCE_{}_DATABASE_SCHEMA",
                    name.to_ascii_uppercase()
                )])
            {
                return Err("database_schema allocation projection is invalid".into());
            }
        }
        _ => return Err("unknown resource allocation projection kind".into()),
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeIdentityV1 {
    pub identity_sha256: String,
    pub root_realpath: String,
    pub root_dev: String,
    pub root_ino: String,
    pub root_owner_euid: String,
    pub private_git_dir_realpath: String,
    pub private_git_dir_dev: String,
    pub private_git_dir_ino: String,
    pub branch_ref: String,
}
impl WorktreeIdentityV1 {
    pub fn value(&self) -> JcsValue {
        object([
            ("branch_ref", JcsValue::String(self.branch_ref.clone())),
            (
                "identity_sha256",
                JcsValue::String(self.identity_sha256.clone()),
            ),
            (
                "private_git_dir_dev",
                JcsValue::String(self.private_git_dir_dev.clone()),
            ),
            (
                "private_git_dir_ino",
                JcsValue::String(self.private_git_dir_ino.clone()),
            ),
            (
                "private_git_dir_realpath",
                JcsValue::String(self.private_git_dir_realpath.clone()),
            ),
            ("root_dev", JcsValue::String(self.root_dev.clone())),
            ("root_ino", JcsValue::String(self.root_ino.clone())),
            (
                "root_owner_euid",
                JcsValue::String(self.root_owner_euid.clone()),
            ),
            (
                "root_realpath",
                JcsValue::String(self.root_realpath.clone()),
            ),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
        ])
    }
    pub fn from_value(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "identity_sha256",
                "root_realpath",
                "root_dev",
                "root_ino",
                "root_owner_euid",
                "private_git_dir_realpath",
                "private_git_dir_dev",
                "private_git_dir_ino",
                "branch_ref",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("WorktreeIdentityV1 schema mismatch".into());
        }
        Sha256Digest::parse(&get_string(&o, "identity_sha256")?)?;
        for key in ["root_realpath", "private_git_dir_realpath"] {
            AbsPath::parse(&get_string(&o, key)?)?;
        }
        for key in [
            "root_dev",
            "root_ino",
            "root_owner_euid",
            "private_git_dir_dev",
            "private_git_dir_ino",
        ] {
            Decimal::parse(&get_string(&o, key)?)?;
        }
        Ok(Self {
            identity_sha256: get_string(&o, "identity_sha256")?,
            root_realpath: get_string(&o, "root_realpath")?,
            root_dev: get_string(&o, "root_dev")?,
            root_ino: get_string(&o, "root_ino")?,
            root_owner_euid: get_string(&o, "root_owner_euid")?,
            private_git_dir_realpath: get_string(&o, "private_git_dir_realpath")?,
            private_git_dir_dev: get_string(&o, "private_git_dir_dev")?,
            private_git_dir_ino: get_string(&o, "private_git_dir_ino")?,
            branch_ref: get_string(&o, "branch_ref")?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceStartResultV1 {
    pub session_id: String,
    pub repository_id: String,
    pub worktree_root: String,
    pub branch_ref: String,
    pub coordinator_capability_file: String,
    pub coordinator_generation: String,
    pub bootstrap: String,
}
impl ClosedJcs for WorkspaceStartResultV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "session_id",
                "repository_id",
                "worktree_root",
                "branch_ref",
                "coordinator_capability_file",
                "coordinator_generation",
                "bootstrap",
            ],
        )?;
        let session_id = get_string(&o, "session_id")?;
        LowerUuidV4::parse(&session_id)?;
        let bootstrap = get_string(&o, "bootstrap")?;
        if !matches!(bootstrap.as_str(), "created" | "existing") {
            return Err("bootstrap outcome invalid".into());
        }
        Ok(Self {
            session_id,
            repository_id: get_string(&o, "repository_id")?,
            worktree_root: get_string(&o, "worktree_root")?,
            branch_ref: get_string(&o, "branch_ref")?,
            coordinator_capability_file: get_string(&o, "coordinator_capability_file")?,
            coordinator_generation: get_string(&o, "coordinator_generation")?,
            bootstrap,
        })
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            ("bootstrap", JcsValue::String(self.bootstrap.clone())),
            ("branch_ref", JcsValue::String(self.branch_ref.clone())),
            (
                "coordinator_capability_file",
                JcsValue::String(self.coordinator_capability_file.clone()),
            ),
            (
                "coordinator_generation",
                JcsValue::String(self.coordinator_generation.clone()),
            ),
            (
                "repository_id",
                JcsValue::String(self.repository_id.clone()),
            ),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
            ("session_id", JcsValue::String(self.session_id.clone())),
            (
                "worktree_root",
                JcsValue::String(self.worktree_root.clone()),
            ),
        ])
    }
}

pub fn validate_git_oid(value: &str) -> Result<(), String> {
    if (value.len() == 40 || value.len() == 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err("Git OID is not lowercase 40- or 64-character hexadecimal".into())
    }
}

fn validate_oid_list(values: &[String], name: &str, nonempty: bool) -> Result<(), String> {
    if nonempty && values.is_empty() {
        return Err(format!("{name} must be nonempty"));
    }
    for value in values {
        validate_git_oid(value)?;
    }
    if let Some(first) = values.first() {
        if values.iter().any(|value| value.len() != first.len()) {
            return Err(format!("{name} mixes Git object formats"));
        }
    }
    Ok(())
}

fn validate_sorted_unique(values: &[String], name: &str, nonempty: bool) -> Result<(), String> {
    if nonempty && values.is_empty() {
        return Err(format!("{name} must be nonempty"));
    }
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(format!("{name} must be sorted and unique"));
    }
    Ok(())
}

fn get_bool(object: &BTreeMap<String, JcsValue>, key: &str) -> Result<bool, String> {
    match object.get(key) {
        Some(JcsValue::Bool(value)) => Ok(*value),
        _ => Err(format!("{key} must be a boolean")),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandbackRequestV1 {
    pub request_id: String,
    pub session_id: String,
    pub expected_head: String,
    pub created_at: String,
}
impl HandbackRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        LowerUuidV4::parse(&self.request_id)?;
        LowerUuidV4::parse(&self.session_id)?;
        validate_git_oid(&self.expected_head)?;
        Timestamp::parse(&self.created_at)?;
        Ok(())
    }
}
impl ClosedJcs for HandbackRequestV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "request_id",
                "session_id",
                "expected_head",
                "created_at",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("HandbackRequestV1 schema mismatch".into());
        }
        let request = Self {
            request_id: get_string(&o, "request_id")?,
            session_id: get_string(&o, "session_id")?,
            expected_head: get_string(&o, "expected_head")?,
            created_at: get_string(&o, "created_at")?,
        };
        request.validate()?;
        Ok(request)
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            ("created_at", JcsValue::String(self.created_at.clone())),
            (
                "expected_head",
                JcsValue::String(self.expected_head.clone()),
            ),
            ("request_id", JcsValue::String(self.request_id.clone())),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
            ("session_id", JcsValue::String(self.session_id.clone())),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandbackReceiptV1 {
    pub request_id: String,
    pub session_id: String,
    pub head_oid: String,
    pub outcome: String,
    pub produced_commits: Vec<String>,
    pub created_at: String,
}
impl HandbackReceiptV1 {
    pub fn validate(&self) -> Result<(), String> {
        LowerUuidV4::parse(&self.request_id)?;
        LowerUuidV4::parse(&self.session_id)?;
        validate_git_oid(&self.head_oid)?;
        Timestamp::parse(&self.created_at)?;
        if self.outcome != "validated" {
            return Err("handback outcome must be validated".into());
        }
        validate_oid_list(&self.produced_commits, "produced_commits", true)?;
        if self.produced_commits.last().map(String::as_str) != Some(self.head_oid.as_str()) {
            return Err("handback head_oid must equal the last produced commit".into());
        }
        Ok(())
    }
}
impl ClosedJcs for HandbackReceiptV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "request_id",
                "session_id",
                "head_oid",
                "outcome",
                "produced_commits",
                "created_at",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("HandbackReceiptV1 schema mismatch".into());
        }
        let receipt = Self {
            request_id: get_string(&o, "request_id")?,
            session_id: get_string(&o, "session_id")?,
            head_oid: get_string(&o, "head_oid")?,
            outcome: get_string(&o, "outcome")?,
            produced_commits: string_array(&o["produced_commits"], "produced_commits")?,
            created_at: get_string(&o, "created_at")?,
        };
        receipt.validate()?;
        Ok(receipt)
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            ("created_at", JcsValue::String(self.created_at.clone())),
            ("head_oid", JcsValue::String(self.head_oid.clone())),
            ("outcome", JcsValue::String(self.outcome.clone())),
            (
                "produced_commits",
                JcsValue::Array(
                    self.produced_commits
                        .iter()
                        .cloned()
                        .map(JcsValue::String)
                        .collect(),
                ),
            ),
            ("request_id", JcsValue::String(self.request_id.clone())),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
            ("session_id", JcsValue::String(self.session_id.clone())),
        ])
    }
}

fn validate_coordinator_cas(
    repository_path: &str,
    repository_id: &str,
    request_id: &str,
    session_id: &str,
    expected_journal_head_sha256: &str,
    expected_head: &str,
    created_at: &str,
) -> Result<(), String> {
    AbsPath::parse(repository_path)?;
    Sha256Digest::parse(repository_id)?;
    LowerUuidV4::parse(request_id)?;
    LowerUuidV4::parse(session_id)?;
    Sha256Digest::parse(expected_journal_head_sha256)?;
    validate_git_oid(expected_head)?;
    Timestamp::parse(created_at)?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrateRequestV1 {
    pub request_id: String,
    pub repository_path: String,
    pub repository_id: String,
    pub session_id: String,
    pub expected_state: WorkspaceState,
    pub expected_journal_head_sha256: String,
    pub expected_head: String,
    pub disposition: String,
    pub created_at: String,
}
impl IntegrateRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_coordinator_cas(
            &self.repository_path,
            &self.repository_id,
            &self.request_id,
            &self.session_id,
            &self.expected_journal_head_sha256,
            &self.expected_head,
            &self.created_at,
        )?;
        if self.expected_state != WorkspaceState::HandbackReady {
            return Err("integration expected_state must be HandbackReady".into());
        }
        if !matches!(self.disposition.as_str(), "integrate" | "reject") {
            return Err("integration disposition must be integrate|reject".into());
        }
        Ok(())
    }
}
impl ClosedJcs for IntegrateRequestV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "request_id",
                "repository_path",
                "repository_id",
                "session_id",
                "expected_state",
                "expected_journal_head_sha256",
                "expected_head",
                "disposition",
                "created_at",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("IntegrateRequestV1 schema mismatch".into());
        }
        let request = Self {
            request_id: get_string(&o, "request_id")?,
            repository_path: get_string(&o, "repository_path")?,
            repository_id: get_string(&o, "repository_id")?,
            session_id: get_string(&o, "session_id")?,
            expected_state: WorkspaceState::parse(&get_string(&o, "expected_state")?)?,
            expected_journal_head_sha256: get_string(&o, "expected_journal_head_sha256")?,
            expected_head: get_string(&o, "expected_head")?,
            disposition: get_string(&o, "disposition")?,
            created_at: get_string(&o, "created_at")?,
        };
        request.validate()?;
        Ok(request)
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            ("created_at", JcsValue::String(self.created_at.clone())),
            ("disposition", JcsValue::String(self.disposition.clone())),
            (
                "expected_head",
                JcsValue::String(self.expected_head.clone()),
            ),
            (
                "expected_journal_head_sha256",
                JcsValue::String(self.expected_journal_head_sha256.clone()),
            ),
            (
                "expected_state",
                JcsValue::String(self.expected_state.as_str().into()),
            ),
            (
                "repository_id",
                JcsValue::String(self.repository_id.clone()),
            ),
            (
                "repository_path",
                JcsValue::String(self.repository_path.clone()),
            ),
            ("request_id", JcsValue::String(self.request_id.clone())),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
            ("session_id", JcsValue::String(self.session_id.clone())),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrationReceiptV1 {
    pub request_id: String,
    pub session_id: String,
    pub pre_integration_head: String,
    pub worker_commits: Vec<String>,
    pub integration_commits: Vec<String>,
    pub post_integration_head: String,
    pub outcome: String,
    pub conflict_paths: Vec<String>,
    pub created_at: String,
}
impl IntegrationReceiptV1 {
    pub fn validate(&self) -> Result<(), String> {
        LowerUuidV4::parse(&self.request_id)?;
        LowerUuidV4::parse(&self.session_id)?;
        validate_git_oid(&self.pre_integration_head)?;
        validate_git_oid(&self.post_integration_head)?;
        validate_oid_list(&self.worker_commits, "worker_commits", true)?;
        validate_oid_list(&self.integration_commits, "integration_commits", false)?;
        Timestamp::parse(&self.created_at)?;
        for path in &self.conflict_paths {
            RelPath::parse(path)?;
        }
        match self.outcome.as_str() {
            "integrated" => {
                if self.integration_commits.len() != self.worker_commits.len() {
                    return Err(
                        "integrated receipt requires one integration output per worker input"
                            .into(),
                    );
                }
                if !self.conflict_paths.is_empty() {
                    return Err("integrated receipt cannot contain conflict paths".into());
                }
                if self.integration_commits.last().map(String::as_str)
                    != Some(self.post_integration_head.as_str())
                {
                    return Err(
                        "integrated post head must equal the last integration commit".into(),
                    );
                }
            }
            "rejected" => {
                if self.pre_integration_head != self.post_integration_head {
                    return Err("rejected receipt must preserve integration HEAD".into());
                }
                if !self.integration_commits.is_empty() || !self.conflict_paths.is_empty() {
                    return Err(
                        "rejected receipt cannot contain integration outputs or conflicts".into(),
                    );
                }
            }
            "needs_user_action" => {
                if self.pre_integration_head != self.post_integration_head {
                    return Err("conflict receipt must preserve integration HEAD".into());
                }
                if !self.integration_commits.is_empty() {
                    return Err(
                        "conflict receipt cannot contain partial integration outputs".into(),
                    );
                }
                validate_sorted_unique(&self.conflict_paths, "conflict_paths", true)?;
            }
            _ => {
                return Err(
                    "integration outcome must be integrated|rejected|needs_user_action".into(),
                );
            }
        }
        Ok(())
    }
}
impl ClosedJcs for IntegrationReceiptV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "request_id",
                "session_id",
                "pre_integration_head",
                "worker_commits",
                "integration_commits",
                "post_integration_head",
                "outcome",
                "conflict_paths",
                "created_at",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("IntegrationReceiptV1 schema mismatch".into());
        }
        let receipt = Self {
            request_id: get_string(&o, "request_id")?,
            session_id: get_string(&o, "session_id")?,
            pre_integration_head: get_string(&o, "pre_integration_head")?,
            worker_commits: string_array(&o["worker_commits"], "worker_commits")?,
            integration_commits: string_array(&o["integration_commits"], "integration_commits")?,
            post_integration_head: get_string(&o, "post_integration_head")?,
            outcome: get_string(&o, "outcome")?,
            conflict_paths: string_array(&o["conflict_paths"], "conflict_paths")?,
            created_at: get_string(&o, "created_at")?,
        };
        receipt.validate()?;
        Ok(receipt)
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            (
                "conflict_paths",
                JcsValue::Array(
                    self.conflict_paths
                        .iter()
                        .cloned()
                        .map(JcsValue::String)
                        .collect(),
                ),
            ),
            ("created_at", JcsValue::String(self.created_at.clone())),
            (
                "integration_commits",
                JcsValue::Array(
                    self.integration_commits
                        .iter()
                        .cloned()
                        .map(JcsValue::String)
                        .collect(),
                ),
            ),
            ("outcome", JcsValue::String(self.outcome.clone())),
            (
                "post_integration_head",
                JcsValue::String(self.post_integration_head.clone()),
            ),
            (
                "pre_integration_head",
                JcsValue::String(self.pre_integration_head.clone()),
            ),
            ("request_id", JcsValue::String(self.request_id.clone())),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
            ("session_id", JcsValue::String(self.session_id.clone())),
            (
                "worker_commits",
                JcsValue::Array(
                    self.worker_commits
                        .iter()
                        .cloned()
                        .map(JcsValue::String)
                        .collect(),
                ),
            ),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoverRequestV1 {
    pub request_id: String,
    pub repository_path: String,
    pub repository_id: String,
    pub session_id: String,
    pub expected_state: WorkspaceState,
    pub expected_journal_head_sha256: String,
    pub expected_head: String,
    pub action: String,
    pub created_at: String,
}
impl RecoverRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_coordinator_cas(
            &self.repository_path,
            &self.repository_id,
            &self.request_id,
            &self.session_id,
            &self.expected_journal_head_sha256,
            &self.expected_head,
            &self.created_at,
        )?;
        if !matches!(
            self.action.as_str(),
            "inspect" | "resume_prelaunch" | "retain_abort" | "rotate_coordinator"
        ) {
            return Err(
                "recover action must be inspect|resume_prelaunch|retain_abort|rotate_coordinator"
                    .into(),
            );
        }
        if self.expected_state == WorkspaceState::IntegrationBlocked
            && !matches!(self.action.as_str(), "inspect" | "retain_abort")
        {
            return Err("IntegrationBlocked permits inspect or explicit retain_abort only; integration is never retried".into());
        }
        if self.action == "resume_prelaunch"
            && !matches!(
                self.expected_state,
                WorkspaceState::Reserved
                    | WorkspaceState::Provisioning
                    | WorkspaceState::LeaseHeld
                    | WorkspaceState::Ready
            )
        {
            return Err("resume_prelaunch requires a prelaunch state".into());
        }
        if self.action == "retain_abort"
            && !matches!(
                self.expected_state,
                WorkspaceState::Running
                    | WorkspaceState::HandbackReady
                    | WorkspaceState::IntegrationBlocked
            )
        {
            return Err(
                "retain_abort requires Running, HandbackReady, or IntegrationBlocked".into(),
            );
        }
        if self.action == "rotate_coordinator" && self.expected_state == WorkspaceState::Closed {
            return Err("Closed coordinator authority cannot be rotated".into());
        }
        Ok(())
    }
}
impl ClosedJcs for RecoverRequestV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "request_id",
                "repository_path",
                "repository_id",
                "session_id",
                "expected_state",
                "expected_journal_head_sha256",
                "expected_head",
                "action",
                "created_at",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("RecoverRequestV1 schema mismatch".into());
        }
        let request = Self {
            request_id: get_string(&o, "request_id")?,
            repository_path: get_string(&o, "repository_path")?,
            repository_id: get_string(&o, "repository_id")?,
            session_id: get_string(&o, "session_id")?,
            expected_state: WorkspaceState::parse(&get_string(&o, "expected_state")?)?,
            expected_journal_head_sha256: get_string(&o, "expected_journal_head_sha256")?,
            expected_head: get_string(&o, "expected_head")?,
            action: get_string(&o, "action")?,
            created_at: get_string(&o, "created_at")?,
        };
        request.validate()?;
        Ok(request)
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            ("action", JcsValue::String(self.action.clone())),
            ("created_at", JcsValue::String(self.created_at.clone())),
            (
                "expected_head",
                JcsValue::String(self.expected_head.clone()),
            ),
            (
                "expected_journal_head_sha256",
                JcsValue::String(self.expected_journal_head_sha256.clone()),
            ),
            (
                "expected_state",
                JcsValue::String(self.expected_state.as_str().into()),
            ),
            (
                "repository_id",
                JcsValue::String(self.repository_id.clone()),
            ),
            (
                "repository_path",
                JcsValue::String(self.repository_path.clone()),
            ),
            ("request_id", JcsValue::String(self.request_id.clone())),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
            ("session_id", JcsValue::String(self.session_id.clone())),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbortRequestV1 {
    pub request_id: String,
    pub repository_path: String,
    pub repository_id: String,
    pub session_id: String,
    pub expected_state: WorkspaceState,
    pub expected_journal_head_sha256: String,
    pub expected_head: String,
    pub reason: String,
    pub created_at: String,
}
impl AbortRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_coordinator_cas(
            &self.repository_path,
            &self.repository_id,
            &self.request_id,
            &self.session_id,
            &self.expected_journal_head_sha256,
            &self.expected_head,
            &self.created_at,
        )?;
        if self.reason.is_empty() || self.reason.contains('\0') {
            return Err("abort reason must be nonempty and contain no NUL".into());
        }
        if !matches!(
            self.expected_state,
            WorkspaceState::Running
                | WorkspaceState::HandbackReady
                | WorkspaceState::IntegrationBlocked
        ) {
            return Err(
                "abort expected_state must be Running, HandbackReady, or IntegrationBlocked".into(),
            );
        }
        Ok(())
    }
}
impl ClosedJcs for AbortRequestV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "request_id",
                "repository_path",
                "repository_id",
                "session_id",
                "expected_state",
                "expected_journal_head_sha256",
                "expected_head",
                "reason",
                "created_at",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("AbortRequestV1 schema mismatch".into());
        }
        let request = Self {
            request_id: get_string(&o, "request_id")?,
            repository_path: get_string(&o, "repository_path")?,
            repository_id: get_string(&o, "repository_id")?,
            session_id: get_string(&o, "session_id")?,
            expected_state: WorkspaceState::parse(&get_string(&o, "expected_state")?)?,
            expected_journal_head_sha256: get_string(&o, "expected_journal_head_sha256")?,
            expected_head: get_string(&o, "expected_head")?,
            reason: get_string(&o, "reason")?,
            created_at: get_string(&o, "created_at")?,
        };
        request.validate()?;
        Ok(request)
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            ("created_at", JcsValue::String(self.created_at.clone())),
            (
                "expected_head",
                JcsValue::String(self.expected_head.clone()),
            ),
            (
                "expected_journal_head_sha256",
                JcsValue::String(self.expected_journal_head_sha256.clone()),
            ),
            (
                "expected_state",
                JcsValue::String(self.expected_state.as_str().into()),
            ),
            ("reason", JcsValue::String(self.reason.clone())),
            (
                "repository_id",
                JcsValue::String(self.repository_id.clone()),
            ),
            (
                "repository_path",
                JcsValue::String(self.repository_path.clone()),
            ),
            ("request_id", JcsValue::String(self.request_id.clone())),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
            ("session_id", JcsValue::String(self.session_id.clone())),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinishRequestV1 {
    pub request_id: String,
    pub repository_path: String,
    pub repository_id: String,
    pub session_id: String,
    pub expected_state: WorkspaceState,
    pub expected_journal_head_sha256: String,
    pub expected_head: String,
    pub acknowledge_needs_user_action: bool,
    pub created_at: String,
}
impl FinishRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_coordinator_cas(
            &self.repository_path,
            &self.repository_id,
            &self.request_id,
            &self.session_id,
            &self.expected_journal_head_sha256,
            &self.expected_head,
            &self.created_at,
        )?;
        if self.expected_state == WorkspaceState::IntegrationBlocked {
            return if self.acknowledge_needs_user_action {
                Ok(())
            } else {
                Err("IntegrationBlocked finish requires acknowledge_needs_user_action=true".into())
            };
        }
        if !matches!(
            self.expected_state,
            WorkspaceState::Integrated | WorkspaceState::Rejected | WorkspaceState::AbortedRetained
        ) {
            return Err("finish expected_state must be Integrated, Rejected, AbortedRetained, or acknowledged IntegrationBlocked".into());
        }
        if self.acknowledge_needs_user_action {
            return Err(
                "acknowledge_needs_user_action is invalid outside IntegrationBlocked".into(),
            );
        }
        Ok(())
    }
}
impl ClosedJcs for FinishRequestV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "request_id",
                "repository_path",
                "repository_id",
                "session_id",
                "expected_state",
                "expected_journal_head_sha256",
                "expected_head",
                "acknowledge_needs_user_action",
                "created_at",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("FinishRequestV1 schema mismatch".into());
        }
        let request = Self {
            request_id: get_string(&o, "request_id")?,
            repository_path: get_string(&o, "repository_path")?,
            repository_id: get_string(&o, "repository_id")?,
            session_id: get_string(&o, "session_id")?,
            expected_state: WorkspaceState::parse(&get_string(&o, "expected_state")?)?,
            expected_journal_head_sha256: get_string(&o, "expected_journal_head_sha256")?,
            expected_head: get_string(&o, "expected_head")?,
            acknowledge_needs_user_action: get_bool(&o, "acknowledge_needs_user_action")?,
            created_at: get_string(&o, "created_at")?,
        };
        request.validate()?;
        Ok(request)
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            (
                "acknowledge_needs_user_action",
                JcsValue::Bool(self.acknowledge_needs_user_action),
            ),
            ("created_at", JcsValue::String(self.created_at.clone())),
            (
                "expected_head",
                JcsValue::String(self.expected_head.clone()),
            ),
            (
                "expected_journal_head_sha256",
                JcsValue::String(self.expected_journal_head_sha256.clone()),
            ),
            (
                "expected_state",
                JcsValue::String(self.expected_state.as_str().into()),
            ),
            (
                "repository_id",
                JcsValue::String(self.repository_id.clone()),
            ),
            (
                "repository_path",
                JcsValue::String(self.repository_path.clone()),
            ),
            ("request_id", JcsValue::String(self.request_id.clone())),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
            ("session_id", JcsValue::String(self.session_id.clone())),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HashedFileV1 {
    pub path: String,
    pub sha256: String,
    pub size: String,
}
impl HashedFileV1 {
    pub fn validate(&self) -> Result<(), String> {
        AbsPath::parse(&self.path)?;
        Sha256Digest::parse(&self.sha256)?;
        Decimal::parse(&self.size)?;
        Ok(())
    }
    pub fn from_value(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(&o, &["path", "sha256", "size"])?;
        let file = Self {
            path: get_string(&o, "path")?,
            sha256: get_string(&o, "sha256")?,
            size: get_string(&o, "size")?,
        };
        file.validate()?;
        Ok(file)
    }
    pub fn value(&self) -> JcsValue {
        object([
            ("path", JcsValue::String(self.path.clone())),
            ("sha256", JcsValue::String(self.sha256.clone())),
            ("size", JcsValue::String(self.size.clone())),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionProofV1 {
    pub session_id: String,
    pub branch_ref: String,
    pub head_oid: String,
    pub bundle: HashedFileV1,
    pub dirty_artifact: Option<HashedFileV1>,
    pub reachable_oids: Vec<String>,
    pub reason: String,
    pub proven_at: String,
}
impl RetentionProofV1 {
    pub fn validate(&self) -> Result<(), String> {
        LowerUuidV4::parse(&self.session_id)?;
        validate_branch_ref(&self.branch_ref, &self.session_id)?;
        validate_git_oid(&self.head_oid)?;
        self.bundle.validate()?;
        if let Some(artifact) = &self.dirty_artifact {
            artifact.validate()?;
        }
        validate_oid_list(&self.reachable_oids, "reachable_oids", true)?;
        Timestamp::parse(&self.proven_at)?;
        if !matches!(
            self.reason.as_str(),
            "rejected" | "integration_blocked" | "abort"
        ) {
            return Err("retention reason must be rejected|integration_blocked|abort".into());
        }
        let unique: BTreeSet<_> = self.reachable_oids.iter().collect();
        if unique.len() != self.reachable_oids.len() {
            return Err("reachable_oids must be unique".into());
        }
        if !self.reachable_oids.iter().any(|oid| oid == &self.head_oid) {
            return Err("retention head_oid must be present in reachable_oids".into());
        }
        if self.reason != "abort" && self.dirty_artifact.is_some() {
            return Err("dirty_artifact is admissible only for abort retention".into());
        }
        Ok(())
    }
}
impl ClosedJcs for RetentionProofV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let mut o = value.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "session_id",
                "branch_ref",
                "head_oid",
                "bundle",
                "dirty_artifact",
                "reachable_oids",
                "reason",
                "proven_at",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("RetentionProofV1 schema mismatch".into());
        }
        let dirty_artifact = match o.remove("dirty_artifact").unwrap() {
            JcsValue::Null => None,
            value => Some(HashedFileV1::from_value(value)?),
        };
        let proof = Self {
            session_id: get_string(&o, "session_id")?,
            branch_ref: get_string(&o, "branch_ref")?,
            head_oid: get_string(&o, "head_oid")?,
            bundle: HashedFileV1::from_value(o.remove("bundle").unwrap())?,
            dirty_artifact,
            reachable_oids: string_array(&o["reachable_oids"], "reachable_oids")?,
            reason: get_string(&o, "reason")?,
            proven_at: get_string(&o, "proven_at")?,
        };
        proof.validate()?;
        Ok(proof)
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            ("branch_ref", JcsValue::String(self.branch_ref.clone())),
            ("bundle", self.bundle.value()),
            (
                "dirty_artifact",
                self.dirty_artifact
                    .as_ref()
                    .map(HashedFileV1::value)
                    .unwrap_or(JcsValue::Null),
            ),
            ("head_oid", JcsValue::String(self.head_oid.clone())),
            ("proven_at", JcsValue::String(self.proven_at.clone())),
            (
                "reachable_oids",
                JcsValue::Array(
                    self.reachable_oids
                        .iter()
                        .cloned()
                        .map(JcsValue::String)
                        .collect(),
                ),
            ),
            ("reason", JcsValue::String(self.reason.clone())),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
            ("session_id", JcsValue::String(self.session_id.clone())),
        ])
    }
}

pub fn validate_branch_ref(branch_ref: &str, session_id: &str) -> Result<(), String> {
    let prefix = format!("refs/heads/docks/{session_id}/");
    if !branch_ref.starts_with(&prefix) {
        return Err("retention branch_ref is outside the session namespace".into());
    }
    TaskSlug::parse(&branch_ref[prefix.len()..])?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CleanupReceiptV1 {
    pub request_id: String,
    pub session_id: String,
    pub retention_sha256: Option<String>,
    pub resource_receipts: Vec<String>,
    pub worktree_removed: bool,
    pub branch_removed: bool,
    pub capabilities_revoked: bool,
    pub custody_empty_sha256: String,
    pub lease_released: bool,
    pub outcome: String,
    pub created_at: String,
}
impl CleanupReceiptV1 {
    pub fn validate(&self) -> Result<(), String> {
        LowerUuidV4::parse(&self.request_id)?;
        LowerUuidV4::parse(&self.session_id)?;
        if let Some(digest) = &self.retention_sha256 {
            Sha256Digest::parse(digest)?;
        }
        for digest in &self.resource_receipts {
            Sha256Digest::parse(digest)?;
        }
        validate_sorted_unique(&self.resource_receipts, "resource_receipts", false)?;
        Sha256Digest::parse(&self.custody_empty_sha256)?;
        Timestamp::parse(&self.created_at)?;
        if !self.capabilities_revoked {
            return Err("cleanup receipt requires proven capability revocation".into());
        }
        if !self.lease_released {
            return Err("cleanup receipt requires proven lease release".into());
        }
        if self.outcome != "closed" {
            return Err("cleanup outcome must be closed".into());
        }
        if self.retention_sha256.is_none() && (!self.worktree_removed || !self.branch_removed) {
            return Err(
                "cleanup without retention proof must remove the worktree and branch".into(),
            );
        }
        Ok(())
    }
}
impl ClosedJcs for CleanupReceiptV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let o = value.object()?;
        require_keys(
            &o,
            &[
                "schema",
                "request_id",
                "session_id",
                "retention_sha256",
                "resource_receipts",
                "worktree_removed",
                "branch_removed",
                "capabilities_revoked",
                "custody_empty_sha256",
                "lease_released",
                "outcome",
                "created_at",
            ],
        )?;
        if get_string(&o, "schema")? != SCHEMA_V1 {
            return Err("CleanupReceiptV1 schema mismatch".into());
        }
        let receipt = Self {
            request_id: get_string(&o, "request_id")?,
            session_id: get_string(&o, "session_id")?,
            retention_sha256: optional_string(&o["retention_sha256"], "retention_sha256")?,
            resource_receipts: string_array(&o["resource_receipts"], "resource_receipts")?,
            worktree_removed: get_bool(&o, "worktree_removed")?,
            branch_removed: get_bool(&o, "branch_removed")?,
            capabilities_revoked: get_bool(&o, "capabilities_revoked")?,
            custody_empty_sha256: get_string(&o, "custody_empty_sha256")?,
            lease_released: get_bool(&o, "lease_released")?,
            outcome: get_string(&o, "outcome")?,
            created_at: get_string(&o, "created_at")?,
        };
        receipt.validate()?;
        Ok(receipt)
    }
    fn to_jcs(&self) -> JcsValue {
        object([
            ("branch_removed", JcsValue::Bool(self.branch_removed)),
            (
                "capabilities_revoked",
                JcsValue::Bool(self.capabilities_revoked),
            ),
            ("created_at", JcsValue::String(self.created_at.clone())),
            (
                "custody_empty_sha256",
                JcsValue::String(self.custody_empty_sha256.clone()),
            ),
            ("lease_released", JcsValue::Bool(self.lease_released)),
            ("outcome", JcsValue::String(self.outcome.clone())),
            ("request_id", JcsValue::String(self.request_id.clone())),
            (
                "resource_receipts",
                JcsValue::Array(
                    self.resource_receipts
                        .iter()
                        .cloned()
                        .map(JcsValue::String)
                        .collect(),
                ),
            ),
            (
                "retention_sha256",
                self.retention_sha256
                    .clone()
                    .map(JcsValue::String)
                    .unwrap_or(JcsValue::Null),
            ),
            ("schema", JcsValue::String(SCHEMA_V1.into())),
            ("session_id", JcsValue::String(self.session_id.clone())),
            ("worktree_removed", JcsValue::Bool(self.worktree_removed)),
        ])
    }
}
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
    fn primitives_are_closed() {
        assert!(LowerUuidV4::parse("11111111-1111-4111-8111-111111111111").is_ok());
        assert!(LowerUuidV4::parse("11111111-1111-4111-7111-111111111111").is_err());
        assert!(RelPath::parse("a/b").is_ok());
        assert!(RelPath::parse("a/../b").is_err());
    }
    #[test]
    fn decimal_requires_at_least_one_digit() {
        assert!(Decimal::parse("").is_err());
        assert!(Decimal::parse("0").is_ok());
        assert!(Decimal::parse("1").is_ok());
        assert!(Decimal::parse("01").is_err());
    }
    #[test]
    fn case_alias_claims_remain_mutually_exclusive() {
        let claims = [
            PathClaimRequestV1 {
                path: "Foo".into(),
                path_type: "directory".into(),
                mode: "exclusive".into(),
            },
            PathClaimRequestV1 {
                path: "foo/bar".into(),
                path_type: "file".into(),
                mode: "exclusive".into(),
            },
        ];
        assert!(validate_non_overlapping_claims(&claims).is_err());
    }
    #[test]
    fn lifecycle_schemas_canonical_roundtrip() {
        fn roundtrip<T: ClosedJcs + Eq + std::fmt::Debug>(value: T) {
            let bytes = serialize_jcs_lf(&value);
            let parsed = T::from_jcs(parse_jcs(&bytes, true).unwrap()).unwrap();
            assert_eq!(parsed, value);
        }
        let request_id = "11111111-1111-4111-8111-111111111111".to_string();
        let session_id = "22222222-2222-4222-8222-222222222222".to_string();
        let oid = "a".repeat(40);
        let output_oid = "b".repeat(40);
        let digest = "c".repeat(64);
        let at = "2026-07-22T12:34:56.789Z".to_string();
        roundtrip(HandbackRequestV1 {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            expected_head: oid.clone(),
            created_at: at.clone(),
        });
        roundtrip(HandbackReceiptV1 {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            head_oid: oid.clone(),
            outcome: "validated".into(),
            produced_commits: vec![oid.clone()],
            created_at: at.clone(),
        });
        roundtrip(IntegrateRequestV1 {
            request_id: request_id.clone(),
            repository_path: "/repo".into(),
            repository_id: digest.clone(),
            session_id: session_id.clone(),
            expected_state: WorkspaceState::HandbackReady,
            expected_journal_head_sha256: digest.clone(),
            expected_head: oid.clone(),
            disposition: "integrate".into(),
            created_at: at.clone(),
        });
        roundtrip(IntegrationReceiptV1 {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            pre_integration_head: oid.clone(),
            worker_commits: vec![oid.clone()],
            integration_commits: vec![output_oid.clone()],
            post_integration_head: output_oid,
            outcome: "integrated".into(),
            conflict_paths: Vec::new(),
            created_at: at.clone(),
        });
        roundtrip(RecoverRequestV1 {
            request_id: request_id.clone(),
            repository_path: "/repo".into(),
            repository_id: digest.clone(),
            session_id: session_id.clone(),
            expected_state: WorkspaceState::Running,
            expected_journal_head_sha256: digest.clone(),
            expected_head: oid.clone(),
            action: "inspect".into(),
            created_at: at.clone(),
        });
        roundtrip(AbortRequestV1 {
            request_id: request_id.clone(),
            repository_path: "/repo".into(),
            repository_id: digest.clone(),
            session_id: session_id.clone(),
            expected_state: WorkspaceState::Running,
            expected_journal_head_sha256: digest.clone(),
            expected_head: oid.clone(),
            reason: "operator requested abort".into(),
            created_at: at.clone(),
        });
        roundtrip(FinishRequestV1 {
            request_id: request_id.clone(),
            repository_path: "/repo".into(),
            repository_id: digest.clone(),
            session_id: session_id.clone(),
            expected_state: WorkspaceState::Integrated,
            expected_journal_head_sha256: digest.clone(),
            expected_head: oid.clone(),
            acknowledge_needs_user_action: false,
            created_at: at.clone(),
        });
        roundtrip(RetentionProofV1 {
            session_id: session_id.clone(),
            branch_ref: format!("refs/heads/docks/{session_id}/task"),
            head_oid: oid.clone(),
            bundle: HashedFileV1 {
                path: "/proof/bundle".into(),
                sha256: digest.clone(),
                size: "1".into(),
            },
            dirty_artifact: None,
            reachable_oids: vec![oid],
            reason: "rejected".into(),
            proven_at: at.clone(),
        });
        roundtrip(CleanupReceiptV1 {
            request_id,
            session_id,
            retention_sha256: None,
            resource_receipts: vec![digest.clone()],
            worktree_removed: true,
            branch_removed: true,
            capabilities_revoked: true,
            custody_empty_sha256: digest,
            lease_released: true,
            outcome: "closed".into(),
            created_at: at,
        });
    }
    #[test]
    fn lifecycle_schemas_reject_wrong_keys_outcomes_and_nullability() {
        let request_id = "11111111-1111-4111-8111-111111111111".to_string();
        let session_id = "22222222-2222-4222-8222-222222222222".to_string();
        let oid = "a".repeat(40);
        let digest = "c".repeat(64);
        let at = "2026-07-22T12:34:56.789Z".to_string();
        let mut handback = HandbackRequestV1 {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            expected_head: oid.clone(),
            created_at: at.clone(),
        }
        .to_jcs()
        .object()
        .unwrap();
        handback.insert("extra".into(), JcsValue::Null);
        assert!(HandbackRequestV1::from_jcs(JcsValue::Object(handback)).is_err());
        let bad_handback = HandbackReceiptV1 {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            head_oid: oid.clone(),
            outcome: "accepted".into(),
            produced_commits: vec![oid.clone()],
            created_at: at.clone(),
        };
        assert!(HandbackReceiptV1::from_jcs(bad_handback.to_jcs()).is_err());
        let conflict = IntegrationReceiptV1 {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            pre_integration_head: oid.clone(),
            worker_commits: vec![oid.clone()],
            integration_commits: Vec::new(),
            post_integration_head: oid.clone(),
            outcome: "needs_user_action".into(),
            conflict_paths: Vec::new(),
            created_at: at.clone(),
        };
        assert!(IntegrationReceiptV1::from_jcs(conflict.to_jcs()).is_err());
        let proof = RetentionProofV1 {
            session_id: session_id.clone(),
            branch_ref: format!("refs/heads/docks/{session_id}/task"),
            head_oid: oid.clone(),
            bundle: HashedFileV1 {
                path: "/proof/bundle".into(),
                sha256: digest.clone(),
                size: "1".into(),
            },
            dirty_artifact: None,
            reachable_oids: vec![oid],
            reason: "rejected".into(),
            proven_at: at.clone(),
        };
        let mut proof_object = proof.to_jcs().object().unwrap();
        proof_object.insert("dirty_artifact".into(), JcsValue::String("invented".into()));
        assert!(RetentionProofV1::from_jcs(JcsValue::Object(proof_object)).is_err());
        let cleanup = CleanupReceiptV1 {
            request_id,
            session_id,
            retention_sha256: None,
            resource_receipts: Vec::new(),
            worktree_removed: true,
            branch_removed: true,
            capabilities_revoked: true,
            custody_empty_sha256: digest,
            lease_released: true,
            outcome: "closed".into(),
            created_at: at,
        };
        let mut cleanup_object = cleanup.to_jcs().object().unwrap();
        cleanup_object.insert("retention_sha256".into(), JcsValue::Bool(false));
        assert!(CleanupReceiptV1::from_jcs(JcsValue::Object(cleanup_object)).is_err());
    }
    #[test]
    fn integration_blocked_requires_explicit_retained_abort_acknowledgement() {
        let request_id = "11111111-1111-4111-8111-111111111111".to_string();
        let session_id = "22222222-2222-4222-8222-222222222222".to_string();
        let oid = "a".repeat(40);
        let digest = "c".repeat(64);
        let at = "2026-07-22T12:34:56.789Z".to_string();
        assert!(
            WorkspaceState::IntegrationBlocked.may_transition_to(WorkspaceState::AbortedRetained)
        );
        assert!(
            !WorkspaceState::IntegrationBlocked
                .may_transition_to(WorkspaceState::IntegrationQueued)
        );
        assert!(!WorkspaceState::IntegrationBlocked.may_transition_to(WorkspaceState::Running));
        let finish = |acknowledge_needs_user_action| FinishRequestV1 {
            request_id: request_id.clone(),
            repository_path: "/repo".into(),
            repository_id: digest.clone(),
            session_id: session_id.clone(),
            expected_state: WorkspaceState::IntegrationBlocked,
            expected_journal_head_sha256: digest.clone(),
            expected_head: oid.clone(),
            acknowledge_needs_user_action,
            created_at: at.clone(),
        };
        assert!(finish(false).validate().is_err());
        assert!(finish(true).validate().is_ok());
        assert!(
            AbortRequestV1 {
                request_id: request_id.clone(),
                repository_path: "/repo".into(),
                repository_id: digest.clone(),
                session_id: session_id.clone(),
                expected_state: WorkspaceState::IntegrationBlocked,
                expected_journal_head_sha256: digest.clone(),
                expected_head: oid.clone(),
                reason: "explicit retained abort after conflict".into(),
                created_at: at.clone()
            }
            .validate()
            .is_ok()
        );
        let recover = |action: &str| RecoverRequestV1 {
            request_id: request_id.clone(),
            repository_path: "/repo".into(),
            repository_id: digest.clone(),
            session_id: session_id.clone(),
            expected_state: WorkspaceState::IntegrationBlocked,
            expected_journal_head_sha256: digest.clone(),
            expected_head: oid.clone(),
            action: action.into(),
            created_at: at.clone(),
        };
        assert!(recover("inspect").validate().is_ok());
        assert!(recover("retain_abort").validate().is_ok());
        assert!(recover("resume_prelaunch").validate().is_err());
    }
    #[test]
    fn ready_recovery_preserves_frozen_transition_graph() {
        let request_id = "11111111-1111-4111-8111-111111111111".to_string();
        let session_id = "22222222-2222-4222-8222-222222222222".to_string();
        let oid = "a".repeat(40);
        let digest = "c".repeat(64);
        let at = "2026-07-22T12:34:56.789Z".to_string();
        assert!(WorkspaceState::Ready.may_transition_to(WorkspaceState::Running));
        assert!(!WorkspaceState::Ready.may_transition_to(WorkspaceState::AbortedRetained));
        let recover = |action: &str| RecoverRequestV1 {
            request_id: request_id.clone(),
            repository_path: "/repo".into(),
            repository_id: digest.clone(),
            session_id: session_id.clone(),
            expected_state: WorkspaceState::Ready,
            expected_journal_head_sha256: digest.clone(),
            expected_head: oid.clone(),
            action: action.into(),
            created_at: at.clone(),
        };
        assert!(recover("resume_prelaunch").validate().is_ok());
        assert!(recover("retain_abort").validate().is_err());
        assert!(
            AbortRequestV1 {
                request_id,
                repository_path: "/repo".into(),
                repository_id: digest.clone(),
                session_id,
                expected_state: WorkspaceState::Ready,
                expected_journal_head_sha256: digest,
                expected_head: oid,
                reason: "must first prove a pinned retry to Running".into(),
                created_at: at
            }
            .validate()
            .is_err()
        );
    }
}
