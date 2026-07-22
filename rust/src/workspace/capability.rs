use super::authority::AuthorityExclusionLock;
use super::schema::{
    self, COORDINATOR_ACTIONS, CapabilityRecordV1, ClosedJcs, CoordinatorCapabilityV1, Decimal,
    JcsValue, LowerUuidV4, Sha256Digest, Timestamp, WORKER_ACTIONS,
};
use crate::sha256;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerCapabilityV1 {
    pub capability_id: String,
    pub repository_id: String,
    pub session_id: String,
    pub generation: String,
    pub actions: Vec<String>,
    pub secret_b64url: String,
    pub broker_socket: String,
    pub issued_at: String,
    pub expires_at: String,
}

impl ClosedJcs for WorkerCapabilityV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let object = value.object()?;
        let keys = [
            "schema", "capability_id", "repository_id", "session_id", "generation", "actions",
            "secret_b64url", "broker_socket", "issued_at", "expires_at",
        ];
        if object.len() != keys.len() || keys.iter().any(|key| !object.contains_key(*key)) {
            return Err("WorkerCapabilityV1 keys differ from the closed schema".to_string());
        }
        let string = |key: &str| object[key].as_str().map(str::to_string);
        if string("schema")? != schema::SCHEMA_V1 { return Err("WorkerCapabilityV1 schema mismatch".to_string()); }
        LowerUuidV4::parse(&string("capability_id")?)?;
        LowerUuidV4::parse(&string("session_id")?)?;
        Sha256Digest::parse(&string("repository_id")?)?;
        Decimal::parse(&string("generation")?)?;
        Timestamp::parse(&string("issued_at")?)?;
        Timestamp::parse(&string("expires_at")?)?;
        let actions = match &object["actions"] {
            JcsValue::Array(values) => values.iter().map(|value| value.as_str().map(str::to_string)).collect::<Result<Vec<_>, _>>()?,
            _ => return Err("worker actions must be an array".to_string()),
        };
        if actions != WORKER_ACTIONS.map(str::to_string) { return Err("worker actions differ from the closed set".to_string()); }
        let secret_b64url = string("secret_b64url")?;
        decode_base64url(&secret_b64url)?;
        let broker_socket = string("broker_socket")?;
        if !Path::new(&broker_socket).is_absolute() { return Err("worker broker socket is not absolute".to_string()); }
        Ok(Self { capability_id:string("capability_id")?, repository_id:string("repository_id")?, session_id:string("session_id")?, generation:string("generation")?, actions, secret_b64url, broker_socket, issued_at:string("issued_at")?, expires_at:string("expires_at")? })
    }

    fn to_jcs(&self) -> JcsValue {
        JcsValue::Object(BTreeMap::from([
            ("actions".into(), JcsValue::Array(self.actions.iter().cloned().map(JcsValue::String).collect())),
            ("broker_socket".into(), JcsValue::String(self.broker_socket.clone())),
            ("capability_id".into(), JcsValue::String(self.capability_id.clone())),
            ("expires_at".into(), JcsValue::String(self.expires_at.clone())),
            ("generation".into(), JcsValue::String(self.generation.clone())),
            ("issued_at".into(), JcsValue::String(self.issued_at.clone())),
            ("repository_id".into(), JcsValue::String(self.repository_id.clone())),
            ("schema".into(), JcsValue::String(schema::SCHEMA_V1.into())),
            ("secret_b64url".into(), JcsValue::String(self.secret_b64url.clone())),
            ("session_id".into(), JcsValue::String(self.session_id.clone())),
        ]))
    }
}

pub fn mint_coordinator(
    repository_id: &str,
    generation: u64,
    issued_at: &str,
) -> Result<(CoordinatorCapabilityV1, CapabilityRecordV1), String> {
    Sha256Digest::parse(repository_id)?;
    Timestamp::parse(issued_at)?;
    let secret = random_secret()?;
    let capability = CoordinatorCapabilityV1 {
        capability_id: crate::store::uuid_v4(),
        repository_id: repository_id.to_string(),
        generation: generation.to_string(),
        actions: COORDINATOR_ACTIONS.map(str::to_string).to_vec(),
        secret_b64url: encode_base64url(&secret),
        issued_at: issued_at.to_string(),
    };
    let record = record_for(
        &capability.capability_id,
        &capability.secret_b64url,
        &capability.generation,
        &capability.actions,
    )?;
    Ok((capability, record))
}

pub fn mint_worker(
    repository_id: &str,
    session_id: &str,
    generation: u64,
    broker_socket: &Path,
    issued_at: &str,
    expires_at: &str,
) -> Result<(WorkerCapabilityV1, CapabilityRecordV1), String> {
    Sha256Digest::parse(repository_id)?;
    LowerUuidV4::parse(session_id)?;
    Timestamp::parse(issued_at)?;
    Timestamp::parse(expires_at)?;
    if expires_at <= issued_at {
        return Err("worker capability expiry must follow issuance".to_string());
    }
    let socket = broker_socket
        .to_str()
        .ok_or_else(|| "broker socket path is not UTF-8".to_string())?;
    if !broker_socket.is_absolute() || socket.contains('\0') {
        return Err("broker socket path is not absolute".to_string());
    }
    let secret = random_secret()?;
    let capability = WorkerCapabilityV1 {
        capability_id: crate::store::uuid_v4(),
        repository_id: repository_id.to_string(),
        session_id: session_id.to_string(),
        generation: generation.to_string(),
        actions: WORKER_ACTIONS.map(str::to_string).to_vec(),
        secret_b64url: encode_base64url(&secret),
        broker_socket: socket.to_string(),
        issued_at: issued_at.to_string(),
        expires_at: expires_at.to_string(),
    };
    let record = record_for(
        &capability.capability_id,
        &capability.secret_b64url,
        &capability.generation,
        &capability.actions,
    )?;
    Ok((capability, record))
}

pub fn authenticate_coordinator(
    capability: &CoordinatorCapabilityV1,
    record: &CapabilityRecordV1,
    repository_id: &str,
    generation: u64,
    action: &str,
) -> Result<(), String> {
    if capability.repository_id != repository_id
        || capability.generation != generation.to_string()
        || record.generation != capability.generation
        || record.capability_id != capability.capability_id
        || record.revoked_at.is_some()
        || !capability.actions.iter().any(|candidate| candidate == action)
        || !record.actions.iter().any(|candidate| candidate == action)
    {
        return Err("coordinator capability scope, generation, or revocation mismatch".to_string());
    }
    authenticate_secret(&capability.secret_b64url, &record.secret_sha256)
}

pub fn authenticate_worker(
    capability: &WorkerCapabilityV1,
    record: &CapabilityRecordV1,
    repository_id: &str,
    session_id: &str,
    generation: u64,
    action: &str,
    now: &str,
) -> Result<(), String> {
    Timestamp::parse(now)?;
    if capability.repository_id != repository_id
        || capability.session_id != session_id
        || capability.generation != generation.to_string()
        || capability.expires_at.as_str() <= now
        || record.generation != capability.generation
        || record.capability_id != capability.capability_id
        || record.revoked_at.is_some()
        || !capability.actions.iter().any(|candidate| candidate == action)
        || !record.actions.iter().any(|candidate| candidate == action)
    {
        return Err("worker capability scope, generation, expiry, or revocation mismatch".to_string());
    }
    authenticate_secret(&capability.secret_b64url, &record.secret_sha256)
}

pub fn revoke(record: &mut CapabilityRecordV1, revoked_at: &str) -> Result<(), String> {
    Timestamp::parse(revoked_at)?;
    match &record.revoked_at {
        Some(existing) if existing != revoked_at => {
            Err("capability was already revoked at a different timestamp".to_string())
        }
        Some(_) => Ok(()),
        None => {
            record.revoked_at = Some(revoked_at.to_string());
            Ok(())
        }
    }
}

pub fn revoke_worker_durable(exclusion:&AuthorityExclusionLock,record_path:&Path,capability_id:&str,generation:u64,revoked_at:&str)->Result<CapabilityRecordV1,String>{
    LowerUuidV4::parse(capability_id)?;
    Timestamp::parse(revoked_at)?;
    let parent=record_path.parent().ok_or_else(||"worker capability record path has no parent".to_string())?;
    let repository=fs::canonicalize(exclusion.repository_dir()).map_err(|error|format!("canonicalize locked repository authority: {error}"))?;
    let parent=fs::canonicalize(parent).map_err(|error|format!("canonicalize worker capability record parent: {error}"))?;
    if !parent.starts_with(&repository){return Err("worker capability record is outside the locked repository authority".into())}
    exclusion.revalidate()?;
    let bytes=read_secure_bytes(record_path)?;
    let mut record=CapabilityRecordV1::from_jcs(schema::parse_jcs(&bytes,true)?)?;
    if record.capability_id!=capability_id||record.generation!=generation.to_string(){
        return Err("worker capability revocation expectation differs from the durable record".into())
    }
    if record.revoked_at.is_some(){return Ok(record)}
    revoke(&mut record,revoked_at)?;
    exclusion.revalidate()?;
    super::authority::atomic_replace_jcs(record_path,&record,0o600)?;
    let durable=CapabilityRecordV1::from_jcs(schema::parse_jcs(&read_secure_bytes(record_path)?,true)?)?;
    if durable!=record{return Err("worker capability revocation is not durably re-readable".into())}
    Ok(durable)
}

fn record_for(
    capability_id: &str,
    secret_b64url: &str,
    generation: &str,
    actions: &[String],
) -> Result<CapabilityRecordV1, String> {
    LowerUuidV4::parse(capability_id)?;
    Decimal::parse(generation)?;
    let secret = decode_base64url(secret_b64url)?;
    Ok(CapabilityRecordV1 {
        capability_id: capability_id.to_string(),
        secret_sha256: sha256::hex_digest(&secret),
        generation: generation.to_string(),
        actions: actions.to_vec(),
        revoked_at: None,
    })
}

fn authenticate_secret(encoded: &str, expected_sha256: &str) -> Result<(), String> {
    let secret = decode_base64url(encoded)?;
    let digest = sha256::digest(&secret);
    let expected = decode_hex_32(expected_sha256)?;
    if sha256::constant_time_eq(&digest, &expected) {
        Ok(())
    } else {
        Err("capability secret authentication failed".to_string())
    }
}

pub fn random_secret() -> Result<[u8; 32], String> {
    let mut secret = [0_u8; 32];
    #[cfg(target_os = "linux")]
    {
        let result = unsafe { libc::getrandom(secret.as_mut_ptr().cast(), secret.len(), 0) };
        if result == secret.len() as isize {
            return Ok(secret);
        }
        if result >= 0 {
            return Err("getrandom returned a short capability secret".to_string());
        }
    }
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open("/dev/urandom")
        .map_err(|error| format!("open /dev/urandom: {error}"))?;
    source
        .read_exact(&mut secret)
        .map_err(|error| format!("read capability secret: {error}"))?;
    Ok(secret)
}

pub fn encode_base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let value = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        output.push(TABLE[((value >> 18) & 63) as usize] as char);
        output.push(TABLE[((value >> 12) & 63) as usize] as char);
        if chunk.len() > 1 { output.push(TABLE[((value >> 6) & 63) as usize] as char); }
        if chunk.len() > 2 { output.push(TABLE[(value & 63) as usize] as char); }
    }
    output
}

pub fn decode_base64url(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 43 || value.contains('=') {
        return Err("capability secret must be exactly 43 unpadded base64url characters".to_string());
    }
    let mut output = Vec::with_capacity(32);
    let mut accumulator = 0_u32;
    let mut bits = 0_u8;
    for byte in value.bytes() {
        accumulator = (accumulator << 6) | u32::from(decode_base64_byte(byte)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1_u32 << bits).wrapping_sub(1);
        }
    }
    if accumulator != 0 {
        return Err("capability secret has noncanonical base64url tail bits".to_string());
    }
    if output.len() != 32 || encode_base64url(&output) != value {
        return Err("capability secret has noncanonical base64url tail bits".to_string());
    }
    let mut secret = [0_u8; 32];
    secret.copy_from_slice(&output);
    Ok(secret)
}

fn decode_base64_byte(byte: u8) -> Result<u8, String> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'-' => Ok(62),
        b'_' => Ok(63),
        _ => Err("invalid base64url capability character".to_string()),
    }
}

fn decode_hex_32(value: &str) -> Result<[u8; 32], String> {
    Sha256Digest::parse(value)?;
    let mut output = [0_u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (hex_nibble(chunk[0])? << 4) | hex_nibble(chunk[1])?;
    }
    Ok(output)
}
fn hex_nibble(value: u8) -> Result<u8, String> {
    match value { b'0'..=b'9' => Ok(value-b'0'), b'a'..=b'f' => Ok(value-b'a'+10), _ => Err("invalid lowercase hexadecimal digest".to_string()) }
}

pub fn secure_regular_file(path: &Path, expected_mode: u32) -> Result<File, String> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW).open(path)
        .map_err(|error| format!("securely open {}: {error}", path.display()))?;
    let metadata = file.metadata().map_err(|error| format!("fstat {}: {error}", path.display()))?;
    use std::os::unix::fs::MetadataExt;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 || metadata.mode() & 0o777 != expected_mode {
        return Err(format!("{} has unsafe owner/type/link/mode", path.display()));
    }
    Ok(file)
}

pub fn read_secure_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let mut file = secure_regular_file(path, 0o600)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(bytes)
}

pub fn set_private_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| format!("chmod {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn base64_secret_round_trips() { let bytes=[0xa5;32]; let encoded=encode_base64url(&bytes); assert_eq!(encoded.len(),43); assert_eq!(decode_base64url(&encoded).unwrap(),bytes); }
    #[test] fn wrong_secret_fails() { let (cap,record)=mint_coordinator(&"a".repeat(64),1,"2026-01-01T00:00:00.000Z").unwrap(); let mut changed=cap; changed.secret_b64url=encode_base64url(&[7;32]); assert!(authenticate_coordinator(&changed,&record,&"a".repeat(64),1,"start").is_err()); }
}
