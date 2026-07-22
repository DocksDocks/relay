use crate::sha256;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self,OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt,OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

pub const SCHEMA_V1: &str = "1";
pub const COORDINATOR_ACTIONS: [&str; 7] = [
    "abort", "finish", "inspect", "integrate", "list", "recover", "start",
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
    let mut parser = Parser { bytes: text.as_bytes(), offset: 0 };
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

pub fn read_jcs_file<T: ClosedJcs>(path: &Path, expected_sha256: Option<&str>) -> Result<T, String> {
    let mut file=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(path).map_err(|error|format!("securely open {}: {error}",path.display()))?;
    let metadata=file.metadata().map_err(|error|format!("inspect {}: {error}",path.display()))?;
    if !metadata.is_file()||metadata.uid()!=unsafe{libc::geteuid()}||metadata.nlink()!=1||metadata.mode()&0o7777!=0o600{return Err(format!("{} must be an EUID-owned, single-link, mode-0600 regular file",path.display()))}
    let mut bytes=Vec::new();file.read_to_end(&mut bytes).map_err(|error|format!("read {}: {error}",path.display()))?;
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
                if index > 0 { output.push(','); }
                write_value(value, output);
            }
            output.push(']');
        }
        JcsValue::Object(values) => {
            output.push('{');
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_by(|(left, _), (right, _)| utf16_cmp(left, right));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 { output.push(','); }
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

struct Parser<'a> { bytes: &'a [u8], offset: usize }
impl Parser<'_> {
    fn value(&mut self) -> Result<JcsValue, String> {
        match self.peek() {
            Some(b'n') => { self.literal(b"null")?; Ok(JcsValue::Null) }
            Some(b't') => { self.literal(b"true")?; Ok(JcsValue::Bool(true)) }
            Some(b'f') => { self.literal(b"false")?; Ok(JcsValue::Bool(false)) }
            Some(b'"') => self.string().map(JcsValue::String),
            Some(b'[') => self.array(),
            Some(b'{') => self.object(),
            Some(b'-' | b'0'..=b'9') => self.integer(),
            _ => Err("invalid workspace JSON token".to_string()),
        }
    }
    fn peek(&self) -> Option<u8> { self.bytes.get(self.offset).copied() }
    fn take(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) { self.offset += 1; Ok(()) } else { Err("invalid workspace JSON punctuation".to_string()) }
    }
    fn literal(&mut self, literal: &[u8]) -> Result<(), String> {
        if self.bytes.get(self.offset..self.offset + literal.len()) == Some(literal) { self.offset += literal.len(); Ok(()) } else { Err("invalid workspace JSON literal".to_string()) }
    }
    fn string(&mut self) -> Result<String, String> {
        self.take(b'"')?;
        let mut result = String::new();
        while let Some(byte) = self.peek() {
            match byte {
                b'"' => { self.offset += 1; return Ok(result); }
                b'\\' => {
                    self.offset += 1;
                    let escaped = self.peek().ok_or_else(|| "truncated JSON escape".to_string())?;
                    self.offset += 1;
                    match escaped {
                        b'"' => result.push('"'), b'\\' => result.push('\\'), b'/' => result.push('/'),
                        b'b' => result.push('\u{08}'), b'f' => result.push('\u{0c}'), b'n' => result.push('\n'),
                        b'r' => result.push('\r'), b't' => result.push('\t'),
                        b'u' => {
                            let first = self.hex4()?;
                            let scalar = if (0xd800..=0xdbff).contains(&first) {
                                if self.bytes.get(self.offset..self.offset + 2) != Some(b"\\u") { return Err("unpaired JSON surrogate".to_string()); }
                                self.offset += 2;
                                let second = self.hex4()?;
                                if !(0xdc00..=0xdfff).contains(&second) { return Err("unpaired JSON surrogate".to_string()); }
                                0x10000 + (((first - 0xd800) as u32) << 10) + (second - 0xdc00) as u32
                            } else if (0xdc00..=0xdfff).contains(&first) { return Err("unpaired JSON surrogate".to_string()); }
                            else { first as u32 };
                            result.push(char::from_u32(scalar).ok_or_else(|| "invalid Unicode scalar".to_string())?);
                        }
                        _ => return Err("invalid JSON escape".to_string()),
                    }
                }
                0x00..=0x1f => return Err("unescaped JSON control character".to_string()),
                _ => {
                    let tail = std::str::from_utf8(&self.bytes[self.offset..]).map_err(|_| "invalid UTF-8 in JSON string".to_string())?;
                    let character = tail.chars().next().ok_or_else(|| "truncated JSON string".to_string())?;
                    result.push(character);
                    self.offset += character.len_utf8();
                }
            }
        }
        Err("unterminated JSON string".to_string())
    }
    fn hex4(&mut self) -> Result<u16, String> {
        let bytes = self.bytes.get(self.offset..self.offset + 4).ok_or_else(|| "truncated Unicode escape".to_string())?;
        self.offset += 4;
        let mut value = 0u16;
        for byte in bytes {
            value = value.checked_mul(16).unwrap();
            value += match byte { b'0'..=b'9' => (byte-b'0') as u16, b'a'..=b'f' => (byte-b'a'+10) as u16, b'A'..=b'F' => (byte-b'A'+10) as u16, _ => return Err("invalid Unicode escape".to_string()) };
        }
        Ok(value)
    }
    fn array(&mut self) -> Result<JcsValue, String> {
        self.take(b'[')?;
        let mut values = Vec::new();
        if self.peek() == Some(b']') { self.offset += 1; return Ok(JcsValue::Array(values)); }
        loop {
            values.push(self.value()?);
            match self.peek() { Some(b',') => self.offset += 1, Some(b']') => { self.offset += 1; break; }, _ => return Err("invalid JSON array".to_string()) }
        }
        Ok(JcsValue::Array(values))
    }
    fn object(&mut self) -> Result<JcsValue, String> {
        self.take(b'{')?;
        let mut values = BTreeMap::new();
        if self.peek() == Some(b'}') { self.offset += 1; return Ok(JcsValue::Object(values)); }
        loop {
            let key = self.string()?;
            self.take(b':')?;
            let value = self.value()?;
            if values.insert(key, value).is_some() { return Err("duplicate JSON object key".to_string()); }
            match self.peek() { Some(b',') => self.offset += 1, Some(b'}') => { self.offset += 1; break; }, _ => return Err("invalid JSON object".to_string()) }
        }
        Ok(JcsValue::Object(values))
    }
    fn integer(&mut self) -> Result<JcsValue, String> {
        let start = self.offset;
        if self.peek() == Some(b'-') { self.offset += 1; }
        match self.peek() {
            Some(b'0') => { self.offset += 1; if self.peek().is_some_and(|b| b.is_ascii_digit()) { return Err("noncanonical JSON number".to_string()); } }
            Some(b'1'..=b'9') => while self.peek().is_some_and(|b| b.is_ascii_digit()) { self.offset += 1; },
            _ => return Err("invalid JSON number".to_string()),
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) { return Err("workspace schemas do not admit non-integer JSON numbers".to_string()); }
        let text = std::str::from_utf8(&self.bytes[start..self.offset]).unwrap();
        let value = text.parse::<i64>().map_err(|_| "workspace JSON integer is outside the exact range".to_string())?;
        if value.to_string() != text { return Err("noncanonical JSON integer".to_string()); }
        Ok(JcsValue::Integer(value))
    }
}

macro_rules! string_primitive {
    ($name:ident, $validator:expr, $message:literal) => {
        #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);
        impl $name {
            pub fn parse(value: &str) -> Result<Self, String> {
                if ($validator)(value) { Ok(Self(value.to_string())) } else { Err($message.to_string()) }
            }
            pub fn as_str(&self) -> &str { &self.0 }
            pub fn into_string(self) -> String { self.0 }
        }
    };
}

string_primitive!(LowerUuidV4, |v: &str| {
    let b=v.as_bytes(); b.len()==36 && [8,13,18,23].into_iter().all(|i| b[i]==b'-') && b[14]==b'4' && matches!(b[19],b'8'|b'9'|b'a'|b'b') && b.iter().enumerate().all(|(i,c)| [8,13,18,23].contains(&i) || c.is_ascii_digit() || matches!(c,b'a'..=b'f'))
}, "value is not a lowercase UUID v4");
string_primitive!(Sha256Digest, |v: &str| v.len()==64 && v.bytes().all(|b| b.is_ascii_digit() || matches!(b,b'a'..=b'f')), "value is not a lowercase SHA-256");
string_primitive!(Decimal, |v: &str| v=="0" || (!v.starts_with('0') && v.bytes().all(|b| b.is_ascii_digit())), "value is not canonical unsigned decimal");
string_primitive!(Timestamp, valid_timestamp, "value is not an exact millisecond UTC timestamp");
string_primitive!(TaskSlug, |v: &str| !v.is_empty() && v.len()<=48 && v.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b==b'-') && !v.starts_with('-') && !v.ends_with('-') && !v.contains("--"), "task slug is invalid");

fn valid_timestamp(value: &str) -> bool {
    let b=value.as_bytes();
    if b.len()!=24 || b[4]!=b'-' || b[7]!=b'-' || b[10]!=b'T' || b[13]!=b':' || b[16]!=b':' || b[19]!=b'.' || b[23]!=b'Z' { return false; }
    if b.iter().enumerate().any(|(i,c)| ![4,7,10,13,16,19,23].contains(&i) && !c.is_ascii_digit()) { return false; }
    let number=|range:std::ops::Range<usize>| std::str::from_utf8(&b[range]).ok()?.parse::<u32>().ok();
    matches!(number(1..4),Some(_)) && matches!(number(5..7),Some(1..=12)) && matches!(number(8..10),Some(1..=31)) && matches!(number(11..13),Some(0..=23)) && matches!(number(14..16),Some(0..=59)) && matches!(number(17..19),Some(0..=60))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbsPath(PathBuf);
impl AbsPath {
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.contains('\0') { return Err("absolute path contains NUL".to_string()); }
        let path=Path::new(value);
        if !path.is_absolute() || path.components().any(|c| matches!(c,Component::CurDir|Component::ParentDir)) { return Err("path is not canonical absolute form".to_string()); }
        if path.to_str()!=Some(value) { return Err("absolute path is not UTF-8".to_string()); }
        Ok(Self(path.to_path_buf()))
    }
    pub fn securely_existing(value: &str) -> Result<Self,String> {
        let parsed=Self::parse(value)?;
        let canonical=fs::canonicalize(&parsed.0).map_err(|e| format!("canonicalize {value}: {e}"))?;
        if canonical!=parsed.0 { return Err("absolute path is not canonical or traverses a symlink".to_string()); }
        Ok(parsed)
    }
    pub fn as_path(&self)->&Path { &self.0 }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RelPath(String);
impl RelPath {
    pub fn parse(value:&str)->Result<Self,String>{
        if value.is_empty() || value.starts_with('/') || value.ends_with('/') || value.contains('\0') || value.contains('\\') || value.split('/').any(|c| c.is_empty() || c=="." || c=="..") { return Err("relative path is not normalized slash form".to_string()); }
        Ok(Self(value.to_string()))
    }
    pub fn as_str(&self)->&str{&self.0}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectFormat { Sha1, Sha256 }
impl ObjectFormat {
    pub fn parse(value:&str)->Result<Self,String>{match value{"sha1"=>Ok(Self::Sha1),"sha256"=>Ok(Self::Sha256),_=>Err("unsupported Git object format".to_string())}}
    pub fn as_str(self)->&'static str{match self{Self::Sha1=>"sha1",Self::Sha256=>"sha256"}}
    pub fn oid_len(self)->usize{match self{Self::Sha1=>40,Self::Sha256=>64}}
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitOid(String);
impl GitOid { pub fn parse(value:&str, format:ObjectFormat)->Result<Self,String>{if value.len()==format.oid_len() && value.bytes().all(|b|b.is_ascii_digit()||matches!(b,b'a'..=b'f')){Ok(Self(value.to_string()))}else{Err(format!("Git OID is not lowercase {} hexadecimal",format.oid_len()))}} pub fn as_str(&self)->&str{&self.0} }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceState { Reserved, Provisioning, LeaseHeld, Ready, Running, HandbackReady, IntegrationQueued, Integrated, IntegrationBlocked, Rejected, AbortedRetained, Releasing, Closed }
impl WorkspaceState {
    pub fn as_str(self)->&'static str{match self{Self::Reserved=>"Reserved",Self::Provisioning=>"Provisioning",Self::LeaseHeld=>"LeaseHeld",Self::Ready=>"Ready",Self::Running=>"Running",Self::HandbackReady=>"HandbackReady",Self::IntegrationQueued=>"IntegrationQueued",Self::Integrated=>"Integrated",Self::IntegrationBlocked=>"IntegrationBlocked",Self::Rejected=>"Rejected",Self::AbortedRetained=>"AbortedRetained",Self::Releasing=>"Releasing",Self::Closed=>"Closed"}}
    pub fn parse(v:&str)->Result<Self,String>{Ok(match v{"Reserved"=>Self::Reserved,"Provisioning"=>Self::Provisioning,"LeaseHeld"=>Self::LeaseHeld,"Ready"=>Self::Ready,"Running"=>Self::Running,"HandbackReady"=>Self::HandbackReady,"IntegrationQueued"=>Self::IntegrationQueued,"Integrated"=>Self::Integrated,"IntegrationBlocked"=>Self::IntegrationBlocked,"Rejected"=>Self::Rejected,"AbortedRetained"=>Self::AbortedRetained,"Releasing"=>Self::Releasing,"Closed"=>Self::Closed,_=>return Err("unknown WorkspaceState".to_string())})}
    pub fn may_transition_to(self,next:Self)->bool{matches!((self,next),(Self::Reserved,Self::Provisioning)|(Self::Provisioning,Self::LeaseHeld)|(Self::LeaseHeld,Self::Ready)|(Self::Ready,Self::Running)|(Self::Running,Self::HandbackReady)|(Self::HandbackReady,Self::IntegrationQueued)|(Self::IntegrationQueued,Self::Integrated)|(Self::IntegrationQueued,Self::IntegrationBlocked)|(Self::Running,Self::Rejected)|(Self::Running,Self::AbortedRetained)|(Self::HandbackReady,Self::AbortedRetained)|(Self::Integrated,Self::Releasing)|(Self::Rejected,Self::Releasing)|(Self::AbortedRetained,Self::Releasing)|(Self::Releasing,Self::Closed))}
}

fn require_keys(object:&BTreeMap<String,JcsValue>, keys:&[&str])->Result<(),String>{
    let actual:BTreeSet<_>=object.keys().map(String::as_str).collect(); let expected:BTreeSet<_>=keys.iter().copied().collect();
    if actual==expected{Ok(())}else{Err(format!("closed workspace object keys differ: expected {:?}, got {:?}",expected,actual))}
}
fn get_string(object:&BTreeMap<String,JcsValue>,key:&str)->Result<String,String>{object.get(key).ok_or_else(||format!("missing {key}"))?.as_str().map(str::to_string)}
fn object(entries:impl IntoIterator<Item=(&'static str,JcsValue)>)->JcsValue{JcsValue::Object(entries.into_iter().map(|(k,v)|(k.to_string(),v)).collect())}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryIdentityV1 { pub repository_id:String,pub common_dir_realpath:String,pub common_dir_dev:String,pub common_dir_ino:String,pub common_dir_owner_euid:String,pub euid:String,pub object_format:ObjectFormat }
impl ClosedJcs for RepositoryIdentityV1 {
 fn from_jcs(v:JcsValue)->Result<Self,String>{let o=v.object()?;require_keys(&o,&["schema","repository_id","common_dir_realpath","common_dir_dev","common_dir_ino","common_dir_owner_euid","euid","object_format"])?;if get_string(&o,"schema")?!=SCHEMA_V1{return Err("RepositoryIdentityV1 schema mismatch".to_string())}Sha256Digest::parse(&get_string(&o,"repository_id")?)?;for k in ["common_dir_dev","common_dir_ino","common_dir_owner_euid","euid"]{Decimal::parse(&get_string(&o,k)?)?;}AbsPath::parse(&get_string(&o,"common_dir_realpath")?)?;Ok(Self{repository_id:get_string(&o,"repository_id")?,common_dir_realpath:get_string(&o,"common_dir_realpath")?,common_dir_dev:get_string(&o,"common_dir_dev")?,common_dir_ino:get_string(&o,"common_dir_ino")?,common_dir_owner_euid:get_string(&o,"common_dir_owner_euid")?,euid:get_string(&o,"euid")?,object_format:ObjectFormat::parse(&get_string(&o,"object_format")?)?})}
 fn to_jcs(&self)->JcsValue{object([("common_dir_dev",JcsValue::String(self.common_dir_dev.clone())),("common_dir_ino",JcsValue::String(self.common_dir_ino.clone())),("common_dir_owner_euid",JcsValue::String(self.common_dir_owner_euid.clone())),("common_dir_realpath",JcsValue::String(self.common_dir_realpath.clone())),("euid",JcsValue::String(self.euid.clone())),("object_format",JcsValue::String(self.object_format.as_str().into())),("repository_id",JcsValue::String(self.repository_id.clone())),("schema",JcsValue::String(SCHEMA_V1.into()))])}
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityRecordV1 { pub capability_id:String,pub secret_sha256:String,pub generation:String,pub actions:Vec<String>,pub revoked_at:Option<String> }
impl ClosedJcs for CapabilityRecordV1 {
 fn from_jcs(v:JcsValue)->Result<Self,String>{let o=v.object()?;require_keys(&o,&["capability_id","secret_sha256","generation","actions","revoked_at"])?;LowerUuidV4::parse(&get_string(&o,"capability_id")?)?;Sha256Digest::parse(&get_string(&o,"secret_sha256")?)?;Decimal::parse(&get_string(&o,"generation")?)?;let actions=match o.get("actions"){Some(JcsValue::Array(v))=>v.iter().map(|v|v.as_str().map(str::to_string)).collect::<Result<Vec<_>,_>>()?,_=>return Err("actions must be an array".to_string())};validate_action_set(&actions)?;let revoked_at=match o.get("revoked_at"){Some(JcsValue::Null)=>None,Some(JcsValue::String(v))=>{Timestamp::parse(v)?;Some(v.clone())},_=>return Err("revoked_at has invalid nullability".to_string())};Ok(Self{capability_id:get_string(&o,"capability_id")?,secret_sha256:get_string(&o,"secret_sha256")?,generation:get_string(&o,"generation")?,actions,revoked_at})}
 fn to_jcs(&self)->JcsValue{object([("actions",JcsValue::Array(self.actions.iter().cloned().map(JcsValue::String).collect())),("capability_id",JcsValue::String(self.capability_id.clone())),("generation",JcsValue::String(self.generation.clone())),("revoked_at",self.revoked_at.clone().map(JcsValue::String).unwrap_or(JcsValue::Null)),("secret_sha256",JcsValue::String(self.secret_sha256.clone()))])}
}

pub fn validate_action_set(actions:&[String])->Result<(),String>{if actions.is_empty()||actions.windows(2).any(|w|w[0]>=w[1]){return Err("capability actions must be sorted and unique".to_string())} Ok(())}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoordinatorCapabilityV1 { pub capability_id:String,pub repository_id:String,pub generation:String,pub actions:Vec<String>,pub secret_b64url:String,pub issued_at:String }
impl ClosedJcs for CoordinatorCapabilityV1 {
 fn from_jcs(v:JcsValue)->Result<Self,String>{let o=v.object()?;require_keys(&o,&["schema","capability_id","repository_id","generation","actions","secret_b64url","issued_at"])?;if get_string(&o,"schema")?!=SCHEMA_V1{return Err("CoordinatorCapabilityV1 schema mismatch".to_string())}let actions=match o.get("actions"){Some(JcsValue::Array(v))=>v.iter().map(|v|v.as_str().map(str::to_string)).collect::<Result<Vec<_>,_>>()?,_=>return Err("actions must be array".into())};if actions!=COORDINATOR_ACTIONS.map(str::to_string){return Err("coordinator actions differ from the closed set".into())}validate_cap_fields(&o)?;Ok(Self{capability_id:get_string(&o,"capability_id")?,repository_id:get_string(&o,"repository_id")?,generation:get_string(&o,"generation")?,actions,secret_b64url:get_string(&o,"secret_b64url")?,issued_at:get_string(&o,"issued_at")?})}
 fn to_jcs(&self)->JcsValue{object([("actions",JcsValue::Array(self.actions.iter().cloned().map(JcsValue::String).collect())),("capability_id",JcsValue::String(self.capability_id.clone())),("generation",JcsValue::String(self.generation.clone())),("issued_at",JcsValue::String(self.issued_at.clone())),("repository_id",JcsValue::String(self.repository_id.clone())),("schema",JcsValue::String(SCHEMA_V1.into())),("secret_b64url",JcsValue::String(self.secret_b64url.clone()))])}
}
fn validate_cap_fields(o:&BTreeMap<String,JcsValue>)->Result<(),String>{LowerUuidV4::parse(&get_string(o,"capability_id")?)?;Sha256Digest::parse(&get_string(o,"repository_id")?)?;Decimal::parse(&get_string(o,"generation")?)?;Timestamp::parse(&get_string(o,"issued_at")?)?;let s=get_string(o,"secret_b64url")?;if s.len()!=43||!s.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'-'||b==b'_'){return Err("capability secret is not 43-character unpadded base64url".into())}Ok(())}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreserveRequestV1 { pub request_id:String,pub repository_path:String,pub base_commit:String,pub mode:String,pub label:String,pub created_at:String }
impl ClosedJcs for PreserveRequestV1 {
 fn from_jcs(v:JcsValue)->Result<Self,String>{let o=v.object()?;require_keys(&o,&["schema","request_id","repository_path","base_commit","mode","label","created_at"])?;if get_string(&o,"schema")?!=SCHEMA_V1{return Err("PreserveRequestV1 schema mismatch".into())}LowerUuidV4::parse(&get_string(&o,"request_id")?)?;AbsPath::parse(&get_string(&o,"repository_path")?)?;if !matches!(get_string(&o,"mode")?.as_str(),"commit"|"artifact"){return Err("preserve mode must be commit|artifact".into())}Timestamp::parse(&get_string(&o,"created_at")?)?;let label=get_string(&o,"label")?;if label.is_empty()||label.len()>128||label.contains('\0'){return Err("preserve label is invalid".into())}Ok(Self{request_id:get_string(&o,"request_id")?,repository_path:get_string(&o,"repository_path")?,base_commit:get_string(&o,"base_commit")?,mode:get_string(&o,"mode")?,label,created_at:get_string(&o,"created_at")?})}
 fn to_jcs(&self)->JcsValue{object([("base_commit",JcsValue::String(self.base_commit.clone())),("created_at",JcsValue::String(self.created_at.clone())),("label",JcsValue::String(self.label.clone())),("mode",JcsValue::String(self.mode.clone())),("repository_path",JcsValue::String(self.repository_path.clone())),("request_id",JcsValue::String(self.request_id.clone())),("schema",JcsValue::String(SCHEMA_V1.into()))])}
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathClaimRequestV1 { pub path:String,pub path_type:String,pub mode:String }
impl PathClaimRequestV1 { pub fn validate(&self)->Result<(),String>{RelPath::parse(&self.path)?;if !matches!(self.path_type.as_str(),"file"|"directory"){return Err("claim path_type must be file|directory".into())}if self.mode!="exclusive"{return Err("claim mode must be exclusive".into())}Ok(())} }

pub fn validate_non_overlapping_claims(claims:&[PathClaimRequestV1])->Result<(),String>{
 for claim in claims{claim.validate()?;}
 let mut sorted:Vec<(String,&str)>=claims.iter().map(|claim|(claim.path.chars().flat_map(char::to_lowercase).collect(),claim.path.as_str())).collect();
 sorted.sort_by(|left,right|left.0.cmp(&right.0));
 for pair in sorted.windows(2){if pair[0].0==pair[1].0||pair[1].0.strip_prefix(&pair[0].0).is_some_and(|rest|rest.starts_with('/')){return Err(format!("overlapping path claims: {} and {}",pair[0].1,pair[1].1))}}
 Ok(())
}


fn string_array(value:&JcsValue,name:&str)->Result<Vec<String>,String>{match value{JcsValue::Array(values)=>values.iter().map(|v|v.as_str().map(str::to_string)).collect(),_=>Err(format!("{name} must be an array of strings"))}}
fn optional_string(value:&JcsValue,name:&str)->Result<Option<String>,String>{match value{JcsValue::Null=>Ok(None),JcsValue::String(v)=>Ok(Some(v.clone())),_=>Err(format!("{name} has invalid nullability"))}}

#[derive(Clone,Debug,Eq,PartialEq)]
pub struct SourceSnapshotV1{pub head_oid:String,pub index_sha256:Option<String>,pub status_sha256:String,pub tracked_untracked_inventory_sha256:String}
impl SourceSnapshotV1{
 fn from_value(value:JcsValue)->Result<Self,String>{let o=value.object()?;require_keys(&o,&["head_oid","index_sha256","status_sha256","tracked_untracked_inventory_sha256"])?;let index_sha256=optional_string(&o["index_sha256"],"index_sha256")?;if let Some(v)=&index_sha256{Sha256Digest::parse(v)?;}for key in ["status_sha256","tracked_untracked_inventory_sha256"]{Sha256Digest::parse(&get_string(&o,key)?)?;}Ok(Self{head_oid:get_string(&o,"head_oid")?,index_sha256,status_sha256:get_string(&o,"status_sha256")?,tracked_untracked_inventory_sha256:get_string(&o,"tracked_untracked_inventory_sha256")?})}
 fn value(&self)->JcsValue{object([("head_oid",JcsValue::String(self.head_oid.clone())),("index_sha256",self.index_sha256.clone().map(JcsValue::String).unwrap_or(JcsValue::Null)),("status_sha256",JcsValue::String(self.status_sha256.clone())),("tracked_untracked_inventory_sha256",JcsValue::String(self.tracked_untracked_inventory_sha256.clone()))])}
}

#[derive(Clone,Debug,Eq,PartialEq)]
pub enum WipPayloadV1{
 Commit{temporary_index_sha256:String,tree_oid:String,preserved_commit:String,preserve_ref:String},
 Artifact{binary_diff:String,untracked_inventory:String,untracked_archive:String,archive_format:String,entries:Vec<String>},
}
impl WipPayloadV1{
 fn from_value(value:JcsValue)->Result<Self,String>{let o=value.object()?;let kind=get_string(&o,"kind")?;match kind.as_str(){
  "commit"=>{require_keys(&o,&["kind","temporary_index_sha256","tree_oid","preserved_commit","preserve_ref"])?;Sha256Digest::parse(&get_string(&o,"temporary_index_sha256")?)?;let preserve_ref=get_string(&o,"preserve_ref")?;if !preserve_ref.starts_with("refs/docks/preserve/"){return Err("preserve ref is outside refs/docks/preserve".into())}Ok(Self::Commit{temporary_index_sha256:get_string(&o,"temporary_index_sha256")?,tree_oid:get_string(&o,"tree_oid")?,preserved_commit:get_string(&o,"preserved_commit")?,preserve_ref})}
  "artifact"=>{require_keys(&o,&["kind","binary_diff","untracked_inventory","untracked_archive","archive_format","entries"])?;if get_string(&o,"archive_format")?!="pax"{return Err("artifact archive format must be pax".into())}for key in ["binary_diff","untracked_inventory","untracked_archive"]{AbsPath::parse(&get_string(&o,key)?)?;}let entries=string_array(&o["entries"],"entries")?;let mut sorted=entries.clone();sorted.sort();sorted.dedup();if sorted!=entries{return Err("artifact entries must be sorted and unique".into())}for entry in &entries{RelPath::parse(entry)?;}Ok(Self::Artifact{binary_diff:get_string(&o,"binary_diff")?,untracked_inventory:get_string(&o,"untracked_inventory")?,untracked_archive:get_string(&o,"untracked_archive")?,archive_format:"pax".into(),entries})}
  _=>Err("WIP payload kind must be commit|artifact".into())
 }}
 fn value(&self)->JcsValue{match self{
  Self::Commit{temporary_index_sha256,tree_oid,preserved_commit,preserve_ref}=>object([("kind",JcsValue::String("commit".into())),("preserve_ref",JcsValue::String(preserve_ref.clone())),("preserved_commit",JcsValue::String(preserved_commit.clone())),("temporary_index_sha256",JcsValue::String(temporary_index_sha256.clone())),("tree_oid",JcsValue::String(tree_oid.clone()))]),
  Self::Artifact{binary_diff,untracked_inventory,untracked_archive,archive_format,entries}=>object([("archive_format",JcsValue::String(archive_format.clone())),("binary_diff",JcsValue::String(binary_diff.clone())),("entries",JcsValue::Array(entries.iter().cloned().map(JcsValue::String).collect())),("kind",JcsValue::String("artifact".into())),("untracked_archive",JcsValue::String(untracked_archive.clone())),("untracked_inventory",JcsValue::String(untracked_inventory.clone()))])
 }}
}

#[derive(Clone,Debug,Eq,PartialEq)]
pub struct WipReceiptV1{pub receipt_id:String,pub request_sha256:String,pub repository:RepositoryIdentityV1,pub source_root:String,pub base_commit:String,pub mode:String,pub before:SourceSnapshotV1,pub after:SourceSnapshotV1,pub payload:WipPayloadV1,pub created_at:String}
impl ClosedJcs for WipReceiptV1{
 fn from_jcs(value:JcsValue)->Result<Self,String>{
  let mut o=value.object()?;require_keys(&o,&["schema","receipt_id","request_sha256","repository","source_root","base_commit","mode","before","after","payload","created_at"])?;
  if get_string(&o,"schema")?!=SCHEMA_V1{return Err("WipReceiptV1 schema mismatch".into())}
  LowerUuidV4::parse(&get_string(&o,"receipt_id")?)?;Sha256Digest::parse(&get_string(&o,"request_sha256")?)?;AbsPath::parse(&get_string(&o,"source_root")?)?;Timestamp::parse(&get_string(&o,"created_at")?)?;
  let before=SourceSnapshotV1::from_value(o.remove("before").unwrap())?;let after=SourceSnapshotV1::from_value(o.remove("after").unwrap())?;if before!=after{return Err("WIP source before/after snapshots differ".into())}
  let repository=RepositoryIdentityV1::from_jcs(o.remove("repository").unwrap())?;let payload=WipPayloadV1::from_value(o.remove("payload").unwrap())?;let base_commit=get_string(&o,"base_commit")?;
  GitOid::parse(&base_commit,repository.object_format)?;GitOid::parse(&before.head_oid,repository.object_format)?;
  if let WipPayloadV1::Commit{tree_oid,preserved_commit,..}=&payload{GitOid::parse(tree_oid,repository.object_format)?;GitOid::parse(preserved_commit,repository.object_format)?;}
  let mode=get_string(&o,"mode")?;let mode_matches=matches!((&mode,&payload),(m,WipPayloadV1::Commit{..}) if m=="commit")||matches!((&mode,&payload),(m,WipPayloadV1::Artifact{..}) if m=="artifact");if !mode_matches{return Err("WIP mode and payload kind differ".into())}
  Ok(Self{receipt_id:get_string(&o,"receipt_id")?,request_sha256:get_string(&o,"request_sha256")?,repository,source_root:get_string(&o,"source_root")?,base_commit,mode,before,after,payload,created_at:get_string(&o,"created_at")?})
 }
 fn to_jcs(&self)->JcsValue{object([("after",self.after.value()),("base_commit",JcsValue::String(self.base_commit.clone())),("before",self.before.value()),("created_at",JcsValue::String(self.created_at.clone())),("mode",JcsValue::String(self.mode.clone())),("payload",self.payload.value()),("receipt_id",JcsValue::String(self.receipt_id.clone())),("repository",self.repository.to_jcs()),("request_sha256",JcsValue::String(self.request_sha256.clone())),("schema",JcsValue::String(SCHEMA_V1.into())),("source_root",JcsValue::String(self.source_root.clone()))])}
}

#[derive(Clone,Debug,Eq,PartialEq)]
pub struct ToolLaunchV1{pub kind:String,pub executable_path:String,pub executable_sha256:String,pub model:Option<String>,pub effort:Option<String>,pub service_tier:Option<String>}
impl ToolLaunchV1{
 fn from_value(value:JcsValue)->Result<Self,String>{let o=value.object()?;require_keys(&o,&["kind","executable_path","executable_sha256","model","effort","service_tier"])?;let kind=get_string(&o,"kind")?;if !matches!(kind.as_str(),"claude"|"codex"|"omp"){return Err("tool kind must be claude|codex|omp".into())}AbsPath::parse(&get_string(&o,"executable_path")?)?;Sha256Digest::parse(&get_string(&o,"executable_sha256")?)?;let service_tier=optional_string(&o["service_tier"],"service_tier")?;if service_tier.as_deref().is_some_and(|v|!matches!(v,"default"|"fast")){return Err("service_tier must be null|default|fast".into())}Ok(Self{kind,executable_path:get_string(&o,"executable_path")?,executable_sha256:get_string(&o,"executable_sha256")?,model:optional_string(&o["model"],"model")?,effort:optional_string(&o["effort"],"effort")?,service_tier})}
 fn value(&self)->JcsValue{object([("effort",self.effort.clone().map(JcsValue::String).unwrap_or(JcsValue::Null)),("executable_path",JcsValue::String(self.executable_path.clone())),("executable_sha256",JcsValue::String(self.executable_sha256.clone())),("kind",JcsValue::String(self.kind.clone())),("model",self.model.clone().map(JcsValue::String).unwrap_or(JcsValue::Null)),("service_tier",self.service_tier.clone().map(JcsValue::String).unwrap_or(JcsValue::Null))])}
}

#[derive(Clone,Debug,Eq,PartialEq)]
pub struct ResourceDecisionV1{pub kind:String,pub name:String,pub state:String,pub provider_id:Option<String>,pub reason:Option<String>}
impl ResourceDecisionV1{
 fn from_value(value:JcsValue)->Result<Self,String>{let o=value.object()?;let state=get_string(&o,"state")?;let kind=get_string(&o,"kind")?;if !matches!(kind.as_str(),"port"|"temp_dir"|"build_dir"|"database_schema"|"log_dir"|"cache_dir"){return Err("unknown resource kind".into())}let name=get_string(&o,"name")?;if name.is_empty()||name.len()>32||!name.bytes().enumerate().all(|(i,b)|if i==0{b.is_ascii_lowercase()}else{b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'}){return Err("resource name is invalid".into())}match state.as_str(){"requested"=>{require_keys(&o,&["kind","name","state","provider_id"])?;Ok(Self{kind,name,state,provider_id:optional_string(&o["provider_id"],"provider_id")?,reason:None})},"unused"=>{require_keys(&o,&["kind","name","state","reason"])?;if get_string(&o,"reason")?!="task_does_not_use_resource"{return Err("unused resource reason is not closed".into())}Ok(Self{kind,name,state,provider_id:None,reason:Some("task_does_not_use_resource".into())})},_=>Err("resource state must be requested|unused".into())}}
 fn value(&self)->JcsValue{if self.state=="requested"{object([("kind",JcsValue::String(self.kind.clone())),("name",JcsValue::String(self.name.clone())),("provider_id",self.provider_id.clone().map(JcsValue::String).unwrap_or(JcsValue::Null)),("state",JcsValue::String(self.state.clone()))])}else{object([("kind",JcsValue::String(self.kind.clone())),("name",JcsValue::String(self.name.clone())),("reason",JcsValue::String("task_does_not_use_resource".into())),("state",JcsValue::String(self.state.clone()))])}}
}

fn claim_from_value(value:JcsValue)->Result<PathClaimRequestV1,String>{let o=value.object()?;require_keys(&o,&["path","path_type","mode"])?;let claim=PathClaimRequestV1{path:get_string(&o,"path")?,path_type:get_string(&o,"path_type")?,mode:get_string(&o,"mode")?};claim.validate()?;Ok(claim)}
fn claim_value(claim:&PathClaimRequestV1)->JcsValue{object([("mode",JcsValue::String(claim.mode.clone())),("path",JcsValue::String(claim.path.clone())),("path_type",JcsValue::String(claim.path_type.clone()))])}

#[derive(Clone,Debug,Eq,PartialEq)]
pub struct WorkspaceStartRequestV1{pub request_id:String,pub repository_path:String,pub integration_root:String,pub base_commit:String,pub task_slug:String,pub task:String,pub tool:ToolLaunchV1,pub wip_receipt_path:String,pub wip_receipt_sha256:String,pub owned_paths:Vec<PathClaimRequestV1>,pub coordinator_owned_paths:Vec<PathClaimRequestV1>,pub coordinator_owned_overrides:Vec<JcsValue>,pub resources:Vec<ResourceDecisionV1>,pub created_at:String}
impl ClosedJcs for WorkspaceStartRequestV1{
 fn from_jcs(value:JcsValue)->Result<Self,String>{
  let o=value.object()?;
  require_keys(&o,&["schema","request_id","repository_path","integration_root","base_commit","task_slug","task","tool","wip_receipt_path","wip_receipt_sha256","owned_paths","coordinator_owned_paths","coordinator_owned_overrides","resources","created_at"])?;
  if get_string(&o,"schema")?!=SCHEMA_V1{return Err("WorkspaceStartRequestV1 schema mismatch".into())}
  LowerUuidV4::parse(&get_string(&o,"request_id")?)?;
  for key in ["repository_path","integration_root","wip_receipt_path"]{AbsPath::parse(&get_string(&o,key)?)?;}
  TaskSlug::parse(&get_string(&o,"task_slug")?)?;Sha256Digest::parse(&get_string(&o,"wip_receipt_sha256")?)?;Timestamp::parse(&get_string(&o,"created_at")?)?;
  let owned_paths=match &o["owned_paths"]{JcsValue::Array(values)=>values.iter().cloned().map(claim_from_value).collect::<Result<Vec<_>,_>>()?,_=>return Err("owned_paths must be array".into())};
  let coordinator_owned_paths=match &o["coordinator_owned_paths"]{JcsValue::Array(values)=>values.iter().cloned().map(claim_from_value).collect::<Result<Vec<_>,_>>()?,_=>return Err("coordinator_owned_paths must be array".into())};
  validate_non_overlapping_claims(&owned_paths)?;validate_non_overlapping_claims(&coordinator_owned_paths)?;
  let coordinator_owned_overrides=match &o["coordinator_owned_overrides"]{JcsValue::Array(values)=>values.clone(),_=>return Err("coordinator_owned_overrides must be array".into())};
  for override_value in &coordinator_owned_overrides{let override_object=match override_value{JcsValue::Object(object)=>object,_=>return Err("CoordinatorOwnedOverrideV1 must be an object".into())};require_keys(override_object,&["path","reason"])?;RelPath::parse(get_string(override_object,"path")?.as_str())?;let reason=get_string(override_object,"reason")?;if reason.is_empty()||reason.contains('\0'){return Err("coordinator-owned override reason is invalid".into())}}
  let resources=match &o["resources"]{JcsValue::Array(values)=>values.iter().cloned().map(ResourceDecisionV1::from_value).collect::<Result<Vec<_>,_>>()?,_=>return Err("resources must be array".into())};
  let kinds:BTreeSet<_>=resources.iter().map(|resource|resource.kind.as_str()).collect();if resources.len()!=6||kinds!=BTreeSet::from(["port","temp_dir","build_dir","database_schema","log_dir","cache_dir"]){return Err("start request requires exactly one decision for all six resource kinds".into())}
  Ok(Self{request_id:get_string(&o,"request_id")?,repository_path:get_string(&o,"repository_path")?,integration_root:get_string(&o,"integration_root")?,base_commit:get_string(&o,"base_commit")?,task_slug:get_string(&o,"task_slug")?,task:get_string(&o,"task")?,tool:ToolLaunchV1::from_value(o["tool"].clone())?,wip_receipt_path:get_string(&o,"wip_receipt_path")?,wip_receipt_sha256:get_string(&o,"wip_receipt_sha256")?,owned_paths,coordinator_owned_paths,coordinator_owned_overrides,resources,created_at:get_string(&o,"created_at")?})
 }
 fn to_jcs(&self)->JcsValue{object([("base_commit",JcsValue::String(self.base_commit.clone())),("coordinator_owned_overrides",JcsValue::Array(self.coordinator_owned_overrides.clone())),("coordinator_owned_paths",JcsValue::Array(self.coordinator_owned_paths.iter().map(claim_value).collect())),("created_at",JcsValue::String(self.created_at.clone())),("integration_root",JcsValue::String(self.integration_root.clone())),("owned_paths",JcsValue::Array(self.owned_paths.iter().map(claim_value).collect())),("repository_path",JcsValue::String(self.repository_path.clone())),("request_id",JcsValue::String(self.request_id.clone())),("resources",JcsValue::Array(self.resources.iter().map(ResourceDecisionV1::value).collect())),("schema",JcsValue::String(SCHEMA_V1.into())),("task",JcsValue::String(self.task.clone())),("task_slug",JcsValue::String(self.task_slug.clone())),("tool",self.tool.value()),("wip_receipt_path",JcsValue::String(self.wip_receipt_path.clone())),("wip_receipt_sha256",JcsValue::String(self.wip_receipt_sha256.clone()))])}
}

#[derive(Clone,Debug,Eq,PartialEq)]
pub struct WorktreeIdentityV1{pub identity_sha256:String,pub root_realpath:String,pub root_dev:String,pub root_ino:String,pub root_owner_euid:String,pub private_git_dir_realpath:String,pub private_git_dir_dev:String,pub private_git_dir_ino:String,pub branch_ref:String}
impl WorktreeIdentityV1{
 pub fn value(&self)->JcsValue{object([("branch_ref",JcsValue::String(self.branch_ref.clone())),("identity_sha256",JcsValue::String(self.identity_sha256.clone())),("private_git_dir_dev",JcsValue::String(self.private_git_dir_dev.clone())),("private_git_dir_ino",JcsValue::String(self.private_git_dir_ino.clone())),("private_git_dir_realpath",JcsValue::String(self.private_git_dir_realpath.clone())),("root_dev",JcsValue::String(self.root_dev.clone())),("root_ino",JcsValue::String(self.root_ino.clone())),("root_owner_euid",JcsValue::String(self.root_owner_euid.clone())),("root_realpath",JcsValue::String(self.root_realpath.clone())),("schema",JcsValue::String(SCHEMA_V1.into()))])}
 pub fn from_value(value:JcsValue)->Result<Self,String>{let o=value.object()?;require_keys(&o,&["schema","identity_sha256","root_realpath","root_dev","root_ino","root_owner_euid","private_git_dir_realpath","private_git_dir_dev","private_git_dir_ino","branch_ref"])?;if get_string(&o,"schema")?!=SCHEMA_V1{return Err("WorktreeIdentityV1 schema mismatch".into())}Sha256Digest::parse(&get_string(&o,"identity_sha256")?)?;for key in ["root_realpath","private_git_dir_realpath"]{AbsPath::parse(&get_string(&o,key)?)?;}for key in ["root_dev","root_ino","root_owner_euid","private_git_dir_dev","private_git_dir_ino"]{Decimal::parse(&get_string(&o,key)?)?;}Ok(Self{identity_sha256:get_string(&o,"identity_sha256")?,root_realpath:get_string(&o,"root_realpath")?,root_dev:get_string(&o,"root_dev")?,root_ino:get_string(&o,"root_ino")?,root_owner_euid:get_string(&o,"root_owner_euid")?,private_git_dir_realpath:get_string(&o,"private_git_dir_realpath")?,private_git_dir_dev:get_string(&o,"private_git_dir_dev")?,private_git_dir_ino:get_string(&o,"private_git_dir_ino")?,branch_ref:get_string(&o,"branch_ref")?})}
}

#[derive(Clone,Debug,Eq,PartialEq)]
pub struct WorkspaceStartResultV1{pub session_id:String,pub repository_id:String,pub worktree_root:String,pub branch_ref:String,pub coordinator_capability_file:String,pub coordinator_generation:String,pub bootstrap:String}
impl ClosedJcs for WorkspaceStartResultV1{
 fn from_jcs(value:JcsValue)->Result<Self,String>{let o=value.object()?;require_keys(&o,&["schema","session_id","repository_id","worktree_root","branch_ref","coordinator_capability_file","coordinator_generation","bootstrap"])?;let session_id=get_string(&o,"session_id")?;LowerUuidV4::parse(&session_id)?;let bootstrap=get_string(&o,"bootstrap")?;if !matches!(bootstrap.as_str(),"created"|"existing"){return Err("bootstrap outcome invalid".into())}Ok(Self{session_id,repository_id:get_string(&o,"repository_id")?,worktree_root:get_string(&o,"worktree_root")?,branch_ref:get_string(&o,"branch_ref")?,coordinator_capability_file:get_string(&o,"coordinator_capability_file")?,coordinator_generation:get_string(&o,"coordinator_generation")?,bootstrap})}
 fn to_jcs(&self)->JcsValue{object([("bootstrap",JcsValue::String(self.bootstrap.clone())),("branch_ref",JcsValue::String(self.branch_ref.clone())),("coordinator_capability_file",JcsValue::String(self.coordinator_capability_file.clone())),("coordinator_generation",JcsValue::String(self.coordinator_generation.clone())),("repository_id",JcsValue::String(self.repository_id.clone())),("schema",JcsValue::String(SCHEMA_V1.into())),("session_id",JcsValue::String(self.session_id.clone())),("worktree_root",JcsValue::String(self.worktree_root.clone()))])}
}
#[cfg(test)]
mod tests {
 use super::*;
 #[test] fn canonical_roundtrip_and_duplicate_refusal(){let v=parse_jcs(b"{\"a\":[true,null,1],\"z\":\"x\"}\n",true).unwrap();assert_eq!(serialize_jcs(&v),"{\"a\":[true,null,1],\"z\":\"x\"}");assert!(parse_jcs(b"{\"a\":1,\"a\":2}\n",true).is_err());}
 #[test] fn primitives_are_closed(){assert!(LowerUuidV4::parse("11111111-1111-4111-8111-111111111111").is_ok());assert!(LowerUuidV4::parse("11111111-1111-4111-7111-111111111111").is_err());assert!(RelPath::parse("a/b").is_ok());assert!(RelPath::parse("a/../b").is_err());}
}
