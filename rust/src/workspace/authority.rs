use super::capability::{self, read_secure_bytes};
use super::schema::{
    self, CapabilityRecordV1, ClosedJcs, CoordinatorCapabilityV1, JcsValue,
    PathClaimRequestV1, RepositoryIdentityV1, Sha256Digest,
};
use crate::sha256;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const AUTHORITY_COMPONENT: &str = "workspace-authority-v1";
const DATA_COMPONENT: &str = "workspaces-v1";
const REPOSITORY_RECORD: &str = "repository-authority-v1.json";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityRoots {
    pub authority: PathBuf,
    pub data: PathBuf,
    pub euid: u32,
}

pub trait AuthorityRootProvider {
    fn roots(&self) -> Result<AuthorityRoots, String>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemAuthorityRootProvider;
impl AuthorityRootProvider for SystemAuthorityRootProvider {
    fn roots(&self) -> Result<AuthorityRoots, String> {
        let euid = unsafe { libc::geteuid() };
        let home = passwd_home(euid)?;
        #[cfg(target_os = "macos")]
        let (authority, data) = (
            home.join("Library/Application Support/session-relay").join(AUTHORITY_COMPONENT),
            home.join("Library/Application Support/session-relay").join(DATA_COMPONENT),
        );
        #[cfg(not(target_os = "macos"))]
        let (authority, data) = (
            home.join(".local/state/session-relay").join(AUTHORITY_COMPONENT),
            home.join(".local/share/session-relay").join(DATA_COMPONENT),
        );
        Ok(AuthorityRoots { authority, data, euid })
    }
}

fn passwd_home(euid: u32) -> Result<PathBuf, String> {
    let initial = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut size = if initial > 0 { initial as usize } else { 16 * 1024 };
    loop {
        let mut buffer = vec![0_u8; size];
        let mut passwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let status = unsafe {
            libc::getpwuid_r(
                euid,
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && size < 1024 * 1024 {
            size *= 2;
            continue;
        }
        if status != 0 || result.is_null() {
            return Err(format!("getpwuid_r({euid}) failed: {}", std::io::Error::from_raw_os_error(status)));
        }
        let passwd = unsafe { passwd.assume_init() };
        if passwd.pw_dir.is_null() {
            return Err("getpwuid_r returned no home directory".to_string());
        }
        let bytes = unsafe { CStr::from_ptr(passwd.pw_dir) }.to_bytes();
        if bytes.is_empty() || bytes.contains(&0) {
            return Err("getpwuid_r returned an invalid home directory".to_string());
        }
        let home = PathBuf::from(OsStr::from_bytes(bytes));
        if !home.is_absolute() {
            return Err("getpwuid_r home directory is not absolute".to_string());
        }
        return Ok(home);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryAuthorityV1 {
    pub repository: RepositoryIdentityV1,
    pub current_generation: String,
    pub coordinator: CapabilityRecordV1,
    pub created_at: String,
    pub updated_at: String,
}

impl ClosedJcs for RepositoryAuthorityV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let mut object = value.object()?;
        let required = ["schema", "repository", "current_generation", "coordinator", "created_at", "updated_at"];
        if object.len() != required.len() || required.iter().any(|key| !object.contains_key(*key)) {
            return Err("RepositoryAuthorityV1 keys differ from the closed schema".to_string());
        }
        let string = |value: Option<&JcsValue>, name: &str| value.ok_or_else(|| format!("missing {name}"))?.as_str().map(str::to_string);
        if string(object.get("schema"), "schema")? != schema::SCHEMA_V1 { return Err("RepositoryAuthorityV1 schema mismatch".to_string()); }
        let repository = RepositoryIdentityV1::from_jcs(object.remove("repository").unwrap())?;
        let coordinator = CapabilityRecordV1::from_jcs(object.remove("coordinator").unwrap())?;
        let current_generation = string(object.get("current_generation"), "current_generation")?;
        schema::Decimal::parse(&current_generation)?;
        if coordinator.generation != current_generation { return Err("authority coordinator generation mismatch".to_string()); }
        let created_at = string(object.get("created_at"), "created_at")?;
        let updated_at = string(object.get("updated_at"), "updated_at")?;
        schema::Timestamp::parse(&created_at)?;
        schema::Timestamp::parse(&updated_at)?;
        Ok(Self { repository, current_generation, coordinator, created_at, updated_at })
    }

    fn to_jcs(&self) -> JcsValue {
        JcsValue::Object(BTreeMap::from([
            ("coordinator".into(), self.coordinator.to_jcs()),
            ("created_at".into(), JcsValue::String(self.created_at.clone())),
            ("current_generation".into(), JcsValue::String(self.current_generation.clone())),
            ("repository".into(), self.repository.to_jcs()),
            ("schema".into(), JcsValue::String(schema::SCHEMA_V1.into())),
            ("updated_at".into(), JcsValue::String(self.updated_at.clone())),
        ]))
    }
}

#[derive(Clone, Debug)]
pub struct BootstrapResult {
    pub capability: CoordinatorCapabilityV1,
    pub capability_file: PathBuf,
    pub bootstrap: BootstrapOutcome,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapOutcome { Created, Existing }

#[derive(Clone, Debug)]
pub struct WorkspaceAuthority {
    roots: AuthorityRoots,
}
impl WorkspaceAuthority {
    pub fn system() -> Result<Self, String> { Self::new(SystemAuthorityRootProvider.roots()?) }
    pub fn new(roots: AuthorityRoots) -> Result<Self, String> {
        if roots.euid != unsafe { libc::geteuid() } { return Err("authority root EUID differs from the process EUID".to_string()); }
        ensure_private_directory(&roots.authority, roots.euid)?;
        ensure_private_directory(&roots.data, roots.euid)?;
        ensure_private_directory(&roots.authority.join("repository-gates"), roots.euid)?;
        ensure_private_directory(&roots.authority.join("repositories"), roots.euid)?;
        Ok(Self { roots })
    }
    pub fn roots(&self) -> &AuthorityRoots { &self.roots }
    pub fn repository_dir(&self, repository_id: &str) -> Result<PathBuf, String> { Sha256Digest::parse(repository_id)?; Ok(self.roots.authority.join("repositories").join(repository_id)) }
    pub fn capability_path(&self, repository_id: &str, generation: u64) -> Result<PathBuf, String> { Ok(self.repository_dir(repository_id)?.join("coordinator-capabilities").join(format!("{generation:020}.json"))) }

    pub fn bootstrap_coordinator(&self, repository: &RepositoryIdentityV1, now: &str) -> Result<BootstrapResult, String> {
        schema::Timestamp::parse(now)?;
        let target = self.repository_dir(&repository.repository_id)?;
        let deterministic = self.capability_path(&repository.repository_id, 1)?;
        if target.exists() {
            return Err(format!("workspace authority already exists; retry with --coordinator-capability-file {}", deterministic.display()));
        }
        let repositories = self.roots.authority.join("repositories");
        let stage = repositories.join(format!(".bootstrap-{}-{}", repository.repository_id, crate::store::uuid_v4()));
        create_private_directory(&stage, self.roots.euid)?;
        let result = (|| {
            for relative in ["coordinator-capabilities", "sessions", "journal"] { create_private_directory(&stage.join(relative), self.roots.euid)?; }
            let (capability, record) = capability::mint_coordinator(&repository.repository_id, 1, now)?;
            let authority = RepositoryAuthorityV1 { repository: repository.clone(), current_generation: "1".into(), coordinator: record, created_at: now.into(), updated_at: now.into() };
            atomic_create_jcs(&stage.join(REPOSITORY_RECORD), &authority, 0o600)?;
            atomic_create_jcs(&stage.join("coordinator-capabilities/00000000000000000001.json"), &capability, 0o600)?;
            fsync_directory(&stage.join("coordinator-capabilities"))?;
            fsync_directory(&stage.join("sessions"))?;
            fsync_directory(&stage.join("journal"))?;
            fsync_directory(&stage)?;
            publish_bootstrap(&stage,&target,&repositories,&deterministic)?;
            Ok(BootstrapResult { capability, capability_file: deterministic, bootstrap: BootstrapOutcome::Created })
        })();
        if result.is_err() && stage.exists() { fs::remove_dir_all(&stage).ok(); }
        result
    }

    pub fn authenticate(&self, repository_id: &str, capability_path: &Path, action: &str) -> Result<(RepositoryAuthorityV1, CoordinatorCapabilityV1), String> {
        let authority = self.read_repository(repository_id)?;
        let generation = authority.current_generation.parse::<u64>().map_err(|_| "authority generation overflow".to_string())?;
        let expected = self.capability_path(repository_id, generation)?;
        let canonical = fs::canonicalize(capability_path).map_err(|error| format!("canonicalize coordinator capability: {error}"))?;
        if canonical != expected { return Err(format!("coordinator capability must be the exact current path {}", expected.display())); }
        let bytes = read_secure_bytes(&expected)?;
        let capability = CoordinatorCapabilityV1::from_jcs(schema::parse_jcs(&bytes, true)?)?;
        capability::authenticate_coordinator(&capability, &authority.coordinator, repository_id, generation, action)?;
        Ok((authority, capability))
    }

    pub fn rotate_coordinator(&self, repository_id: &str, current_path: &Path, now: &str) -> Result<BootstrapResult, String> {
        let (mut authority, _) = self.authenticate(repository_id, current_path, "recover")?;
        let current = authority.current_generation.parse::<u64>().map_err(|_| "authority generation overflow".to_string())?;
        let next = current.checked_add(1).ok_or_else(|| "coordinator generation exhausted".to_string())?;
        let (capability, record) = capability::mint_coordinator(repository_id, next, now)?;
        let path = self.capability_path(repository_id, next)?;
        atomic_create_jcs(&path, &capability, 0o600)?;
        fsync_directory(path.parent().unwrap())?;
        authority.current_generation = next.to_string();
        authority.coordinator = record;
        authority.updated_at = now.to_string();
        atomic_replace_jcs(&self.repository_dir(repository_id)?.join(REPOSITORY_RECORD), &authority, 0o600)?;
        Ok(BootstrapResult { capability, capability_file: path, bootstrap: BootstrapOutcome::Existing })
    }

    pub fn read_repository(&self, repository_id: &str) -> Result<RepositoryAuthorityV1, String> {
        let path = self.repository_dir(repository_id)?.join(REPOSITORY_RECORD);
        let bytes = read_secure_bytes(&path)?;
        let authority = RepositoryAuthorityV1::from_jcs(schema::parse_jcs(&bytes, true)?)?;
        if authority.repository.repository_id != repository_id { return Err("authority repository ID mismatch".to_string()); }
        Ok(authority)
    }
}

#[derive(Clone,Debug,Eq,PartialEq)]
pub struct LeaseOwnerV1 { pub session_id:String,pub acquired_at:String }
impl ClosedJcs for LeaseOwnerV1 {
    fn from_jcs(value:JcsValue)->Result<Self,String>{let object=value.object()?;let keys=["schema","session_id","acquired_at"];if object.len()!=keys.len()||keys.iter().any(|key|!object.contains_key(*key)){return Err("LeaseOwnerV1 keys differ".into())}let session_id=object["session_id"].as_str()?.to_string();schema::LowerUuidV4::parse(&session_id)?;let acquired_at=object["acquired_at"].as_str()?.to_string();schema::Timestamp::parse(&acquired_at)?;Ok(Self{session_id,acquired_at})}
    fn to_jcs(&self)->JcsValue{JcsValue::Object(BTreeMap::from([("acquired_at".into(),JcsValue::String(self.acquired_at.clone())),("schema".into(),JcsValue::String(schema::SCHEMA_V1.into())),("session_id".into(),JcsValue::String(self.session_id.clone()))]))}
}

pub struct WorkspaceLease { file: File, path: PathBuf }
impl WorkspaceLease {
    pub fn acquire(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() { ensure_private_directory(parent, unsafe { libc::geteuid() })?; }
        let file = OpenOptions::new().create(true).truncate(false).read(true).write(true).mode(0o600).custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW).open(path)
            .map_err(|error| format!("open workspace lease {}: {error}", path.display()))?;
        verify_regular(&file, path, 0o600, unsafe { libc::geteuid() })?;
        #[cfg(target_os = "linux")]
        {
            let mut lock = libc::flock { l_type: libc::F_WRLCK as i16, l_whence: libc::SEEK_SET as i16, l_start: 0, l_len: 0, l_pid: 0 };
            let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &mut lock) };
            if result < 0 { let error=std::io::Error::last_os_error(); if matches!(error.raw_os_error(),Some(libc::EAGAIN|libc::EACCES)){return Err("workspace lease is already held".to_string())} return Err(format!("lock workspace lease: {error}")); }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let result=unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)};
            if result<0{return Err(format!("lock workspace lease: {}",std::io::Error::last_os_error()))}
        }
        Ok(Self { file, path: path.to_path_buf() })
    }
    pub fn acquire_owned(path:&Path,owner_path:&Path,session_id:&str,acquired_at:&str)->Result<Self,String>{
        schema::LowerUuidV4::parse(session_id)?;
        schema::Timestamp::parse(acquired_at)?;
        match Self::acquire(path) {
            Ok(lease) => {
                atomic_replace_jcs(owner_path,&LeaseOwnerV1{session_id:session_id.into(),acquired_at:acquired_at.into()},0o600)?;
                Ok(lease)
            }
            Err(error) if error=="workspace lease is already held" => {
                let deadline=std::time::Instant::now()+std::time::Duration::from_secs(3);
                loop {
                    if let Ok(bytes)=read_secure_bytes(owner_path){
                        if let Ok(value)=schema::parse_jcs(&bytes,true){
                            if let Ok(owner)=LeaseOwnerV1::from_jcs(value){
                                return Err(format!("Workspace already owned by session {}. Open a separate worktree or continue in read-only mode.",owner.session_id));
                            }
                        }
                    }
                    if std::time::Instant::now()>=deadline{return Err("workspace lease is held but exact owner evidence is not durably readable; refusing recovery".into())}
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            Err(error)=>Err(error),
        }
    }
    pub fn duplicate(&self)->Result<File,String>{self.file.try_clone().map_err(|error|format!("duplicate workspace lease: {error}"))}
    pub fn as_raw_fd(&self) -> RawFd { self.file.as_raw_fd() }
    pub fn path(&self) -> &Path { &self.path }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalEventV1 { pub sequence:u64,pub previous_sha256:Option<String>,pub kind:String,pub payload:JcsValue,pub created_at:String }
impl ClosedJcs for JournalEventV1 {
 fn from_jcs(value:JcsValue)->Result<Self,String>{let o=value.object()?;let keys=["schema","sequence","previous_sha256","kind","payload","created_at"];if o.len()!=keys.len()||keys.iter().any(|k|!o.contains_key(*k)){return Err("JournalEventV1 keys differ".into())}if o["schema"].as_str()?!=schema::SCHEMA_V1{return Err("journal schema mismatch".into())}let sequence=o["sequence"].as_str()?.parse().map_err(|_|"invalid journal sequence".to_string())?;let previous_sha256=match &o["previous_sha256"]{JcsValue::Null=>None,JcsValue::String(v)=>{Sha256Digest::parse(v)?;Some(v.clone())},_=>return Err("invalid journal previous hash nullability".into())};let created_at=o["created_at"].as_str()?.to_string();schema::Timestamp::parse(&created_at)?;Ok(Self{sequence,previous_sha256,kind:o["kind"].as_str()?.to_string(),payload:o["payload"].clone(),created_at})}
 fn to_jcs(&self)->JcsValue{JcsValue::Object(BTreeMap::from([("created_at".into(),JcsValue::String(self.created_at.clone())),("kind".into(),JcsValue::String(self.kind.clone())),("payload".into(),self.payload.clone()),("previous_sha256".into(),self.previous_sha256.clone().map(JcsValue::String).unwrap_or(JcsValue::Null)),("schema".into(),JcsValue::String(schema::SCHEMA_V1.into())),("sequence".into(),JcsValue::String(self.sequence.to_string()))]))}
}

pub fn append_journal(repository_dir:&Path,event:&JournalEventV1,expected_sequence:u64,expected_head:Option<&str>)->Result<String,String>{
 if event.sequence!=expected_sequence||event.previous_sha256.as_deref()!=expected_head{return Err("journal CAS sequence/head mismatch".into())}
 let path=repository_dir.join("journal").join(format!("{:020}.json",event.sequence)); atomic_create_jcs(&path,event,0o600)?;let digest=schema::jcs_sha256(event);fsync_directory(path.parent().unwrap())?;Ok(digest)
}

pub fn atomic_create_jcs<T:ClosedJcs>(path:&Path,value:&T,mode:u32)->Result<(),String>{atomic_write(path,&schema::serialize_jcs_lf(value),mode,false)}
pub fn atomic_replace_jcs<T:ClosedJcs>(path:&Path,value:&T,mode:u32)->Result<(),String>{atomic_write(path,&schema::serialize_jcs_lf(value),mode,true)}
fn atomic_write(path:&Path,bytes:&[u8],mode:u32,replace:bool)->Result<(),String>{
 let parent=path.parent().ok_or_else(||"record path has no parent".to_string())?;let temp=parent.join(format!(".tmp-{}",crate::store::uuid_v4()));let mut file=OpenOptions::new().create_new(true).write(true).mode(mode).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&temp).map_err(|e|format!("create {}: {e}",temp.display()))?;file.write_all(bytes).and_then(|_|file.sync_all()).map_err(|e|format!("persist {}: {e}",temp.display()))?;drop(file);
 let result=if replace{fs::rename(&temp,path).map_err(|e|format!("replace {}: {e}",path.display()))}else{rename_noreplace(&temp,path).map_err(|e|format!("create-once publish {}: {e}",path.display()))};if result.is_err(){fs::remove_file(&temp).ok();return result}fsync_directory(parent)
}

pub fn ensure_private_directory(path:&Path,euid:u32)->Result<(),String>{
 if !path.is_absolute(){return Err(format!("private directory {} is not absolute",path.display()))}
 let mut current=PathBuf::from("/");
 for component in path.components().skip(1){
  current.push(component.as_os_str());
  match fs::symlink_metadata(&current){
   Ok(metadata)=>{if metadata.file_type().is_symlink()||!metadata.is_dir(){return Err(format!("private directory component {} is not a real directory",current.display()))}}
   Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{fs::create_dir(&current).map_err(|e|format!("create {}: {e}",current.display()))?;fs::set_permissions(&current,fs::Permissions::from_mode(0o700)).map_err(|e|format!("chmod {}: {e}",current.display()))?;}
   Err(error)=>return Err(format!("inspect private directory component {}: {error}",current.display())),
  }
 }
 let file=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY).open(path).map_err(|e|format!("securely open directory {}: {e}",path.display()))?;let metadata=file.metadata().map_err(|e|format!("fstat {}: {e}",path.display()))?;if !metadata.is_dir()||metadata.uid()!=euid||metadata.mode()&0o777!=0o700{return Err(format!("{} is not an EUID-owned mode-0700 directory",path.display()))}Ok(())
}
fn create_private_directory(path:&Path,euid:u32)->Result<(),String>{fs::create_dir(path).map_err(|e|format!("create private directory {}: {e}",path.display()))?;fs::set_permissions(path,fs::Permissions::from_mode(0o700)).map_err(|e|format!("chmod {}: {e}",path.display()))?;ensure_private_directory(path,euid)}
fn verify_regular(file:&File,path:&Path,mode:u32,euid:u32)->Result<(),String>{let m=file.metadata().map_err(|e|format!("fstat {}: {e}",path.display()))?;if !m.is_file()||m.uid()!=euid||m.nlink()!=1||m.mode()&0o777!=mode{return Err(format!("{} is not an EUID-owned single-link mode-{mode:o} file",path.display()))}Ok(())}
fn fsync_directory(path:&Path)->Result<(),String>{let file=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY).open(path).map_err(|e|format!("open directory {} for fsync: {e}",path.display()))?;file.sync_all().map_err(|e|format!("fsync directory {}: {e}",path.display()))}
fn publish_bootstrap(stage:&Path,target:&Path,repositories:&Path,deterministic:&Path)->Result<(),String>{
 match rename_noreplace(stage,target){
  Ok(())=>fsync_directory(repositories),
  Err(error) if error.kind()==std::io::ErrorKind::AlreadyExists=>{fs::remove_dir_all(stage).map_err(|cleanup|format!("remove losing bootstrap staging directory {}: {cleanup}",stage.display()))?;Err(format!("workspace authority already exists; retry with --coordinator-capability-file {}",deterministic.display()))},
  Err(error)=>Err(format!("create-once publish {}: {error}",target.display())),
 }
}
fn rename_noreplace(source:&Path,target:&Path)->Result<(),std::io::Error>{
 #[cfg(target_os="linux")]
 {let s=CString::new(source.as_os_str().as_bytes()).map_err(|_|std::io::Error::new(std::io::ErrorKind::InvalidInput,"source path contains NUL"))?;let t=CString::new(target.as_os_str().as_bytes()).map_err(|_|std::io::Error::new(std::io::ErrorKind::InvalidInput,"target path contains NUL"))?;let result=unsafe{libc::syscall(libc::SYS_renameat2,libc::AT_FDCWD,s.as_ptr(),libc::AT_FDCWD,t.as_ptr(),libc::RENAME_NOREPLACE)};if result==0{return Ok(())}Err(std::io::Error::last_os_error())}
 #[cfg(not(target_os="linux"))]
 {if target.exists(){return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists,"target exists"))}fs::rename(source,target)}
}

pub fn repository_id(euid:u32,dev:u64,ino:u64)->String{sha256::hex_digest(format!("session-relay/repository/v1\0{euid}\0{dev}\0{ino}").as_bytes())}

pub fn now_timestamp()->Result<String,String>{let mut time=libc::timespec{tv_sec:0,tv_nsec:0};if unsafe{libc::clock_gettime(libc::CLOCK_REALTIME,&mut time)}!=0{return Err(format!("clock_gettime: {}",std::io::Error::last_os_error()))}let mut tm=std::mem::MaybeUninit::<libc::tm>::uninit();if unsafe{libc::gmtime_r(&time.tv_sec,tm.as_mut_ptr())}.is_null(){return Err("gmtime_r failed".into())}let tm=unsafe{tm.assume_init()};Ok(format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",tm.tm_year+1900,tm.tm_mon+1,tm.tm_mday,tm.tm_hour,tm.tm_min,tm.tm_sec,time.tv_nsec/1_000_000))}

pub fn resolve_path_policy(
    root:&Path,
    owned_paths:&[PathClaimRequestV1],
    explicit_coordinator_paths:&[PathClaimRequestV1],
    overrides:&[JcsValue],
    allow_overrides:bool,
)->Result<Vec<PathClaimRequestV1>,String>{
 let root=fs::canonicalize(root).map_err(|error|format!("canonicalize claim root {}: {error}",root.display()))?;
 let mut coordinator=BTreeMap::<String,PathClaimRequestV1>::new();
 classify_coordinator_paths(&root,&root,&mut coordinator)?;
 for claim in explicit_coordinator_paths{
  claim.validate()?;
  validate_claim_target(&root,claim)?;
  let key=claim.path.to_ascii_lowercase();
  if coordinator.insert(key,claim.clone()).is_some(){return Err(format!("coordinator-owned path {} is duplicated or case-aliased",claim.path))}
 }
 let mut override_paths=BTreeSet::new();
 for value in overrides{
  let object=match value{JcsValue::Object(object)=>object,_=>return Err("CoordinatorOwnedOverrideV1 must be an object".into())};
  if object.len()!=2||!object.contains_key("path")||!object.contains_key("reason"){return Err("CoordinatorOwnedOverrideV1 keys differ from the closed schema".into())}
  let path=object["path"].as_str()?;schema::RelPath::parse(path)?;
  let reason=object["reason"].as_str()?;if reason.is_empty()||reason.as_bytes().contains(&0){return Err("coordinator-owned override reason is invalid".into())}
  if !allow_overrides{return Err("first workspace start cannot override coordinator-owned paths".into())}
  let key=path.to_ascii_lowercase();
  let Some(default)=coordinator.get(&key) else{return Err(format!("coordinator-owned override {path} does not name an exact resolved default"))};
  if default.path!=path{return Err(format!("coordinator-owned override {path} is a case alias"))}
  if !override_paths.insert(key){return Err(format!("coordinator-owned override {path} is duplicated"))}
 }
 for path in &override_paths{coordinator.remove(path);}
 for claim in owned_paths{
  claim.validate()?;
  validate_claim_target(&root,claim)?;
  let key=claim.path.to_ascii_lowercase();
  for default in coordinator.values(){
   let default_key=default.path.to_ascii_lowercase();
   if key==default_key||key.strip_prefix(&format!("{default_key}/")).is_some()||default_key.strip_prefix(&format!("{key}/")).is_some(){
    return Err(format!("owned path {} overlaps coordinator-owned default {}",claim.path,default.path))
   }
  }
 }
 let mut resolved=coordinator.into_values().collect::<Vec<_>>();
 resolved.sort_by(|left,right|left.path.as_bytes().cmp(right.path.as_bytes()));
 schema::validate_non_overlapping_claims(&resolved)?;
 Ok(resolved)
}

fn validate_claim_target(root:&Path,claim:&PathClaimRequestV1)->Result<(),String>{
 let path=root.join(&claim.path);
 let metadata=fs::symlink_metadata(&path).map_err(|error|if error.kind()==std::io::ErrorKind::NotFound{format!("claim {} does not exist; claim an existing directory when atomic replacement or new files are required",claim.path)}else{format!("inspect claim {}: {error}",claim.path)})?;
 if metadata.file_type().is_symlink(){return Err(format!("claim {} is a symlink",claim.path))}
 match claim.path_type.as_str(){
  "file" if !metadata.is_file()=>Err(format!("file claim {} is not a regular file",claim.path)),
  "directory" if !metadata.is_dir()=>Err(format!("directory claim {} is not a directory",claim.path)),
  _=>Ok(()),
 }
}

fn classify_coordinator_paths(root:&Path,current:&Path,output:&mut BTreeMap<String,PathClaimRequestV1>)->Result<(),String>{
 let mut entries=fs::read_dir(current).map_err(|error|format!("scan coordinator-owned defaults in {}: {error}",current.display()))?.collect::<Result<Vec<_>,_>>().map_err(|error|format!("scan coordinator-owned default entry: {error}"))?;
 entries.sort_by_key(|entry|entry.file_name());
 for entry in entries{
  let path=entry.path();let relative=path.strip_prefix(root).map_err(|_|"coordinator classifier path escaped root".to_string())?;
  let relative=relative.to_str().ok_or_else(||"coordinator-owned path is not UTF-8".to_string())?.to_string();
  schema::RelPath::parse(&relative)?;
  let metadata=fs::symlink_metadata(&path).map_err(|error|format!("inspect coordinator-owned candidate {relative}: {error}"))?;
  if metadata.file_type().is_symlink(){continue}
  let name=entry.file_name();let name=name.to_str().ok_or_else(||"coordinator-owned filename is not UTF-8".to_string())?;
  let classified=coordinator_default(name,&relative,metadata.is_dir());
  if classified{
   let path_type=if metadata.is_dir(){"directory"}else if metadata.is_file(){"file"}else{return Err(format!("coordinator-owned default {relative} has unsupported type"))};
   let key=relative.to_ascii_lowercase();
   if output.insert(key,PathClaimRequestV1{path:relative,path_type:path_type.into(),mode:"exclusive".into()}).is_some(){return Err("coordinator-owned defaults contain case aliases".into())}
  }else if metadata.is_dir()&&name!=".git"{
   classify_coordinator_paths(root,&path,output)?;
  }
 }
 Ok(())
}

fn coordinator_default(name:&str,relative:&str,is_directory:bool)->bool{
 let lower=name.to_ascii_lowercase();
 let components=relative.split('/').map(str::to_ascii_lowercase).collect::<Vec<_>>();
 if is_directory&&(matches!(lower.as_str(),".github"|".gitlab"|".circleci")||components.iter().any(|component|component=="migrations")){return true}
 if is_directory{return false}
 const MANIFESTS:&[&str]=&["package.json","cargo.toml","pyproject.toml","go.mod","go.sum","pom.xml","build.gradle","build.gradle.kts","settings.gradle","settings.gradle.kts","composer.json","gemfile","mix.exs","deno.json","deno.jsonc"];
 const LOCKFILES:&[&str]=&["package-lock.json","npm-shrinkwrap.json","pnpm-lock.yaml","yarn.lock","bun.lock","bun.lockb","cargo.lock","poetry.lock","uv.lock","composer.lock","gemfile.lock","mix.lock"];
 MANIFESTS.contains(&lower.as_str())||LOCKFILES.contains(&lower.as_str())||lower.ends_with(".lock")||
 matches!(lower.as_str(),"agents.md"|"claude.md")||lower.starts_with(".env")||lower.contains("config")||
 ((lower.contains("manifest")||lower.contains("catalog")||lower=="marketplace.json")&&matches!(Path::new(&lower).extension().and_then(OsStr::to_str),Some("json"|"toml"|"yaml"|"yml")))
}

#[cfg(test)]
mod tests{
 use super::*;
 #[test]fn repository_id_is_domain_separated(){assert_eq!(repository_id(1,2,3).len(),64);assert_ne!(repository_id(1,2,3),repository_id(1,2,4));}
 #[test]fn timestamp_shape(){assert!(schema::Timestamp::parse(&now_timestamp().unwrap()).is_ok());}
 #[test]fn bootstrap_publish_race_loser_gets_deterministic_guidance(){
  let root=std::env::temp_dir().join(format!("session-relay-bootstrap-{}",crate::store::uuid_v4()));fs::create_dir(&root).unwrap();
  let stage=root.join("stage");let target=root.join("target");fs::create_dir(&stage).unwrap();fs::create_dir(&target).unwrap();
  let capability=target.join("coordinator-capabilities/00000000000000000001.json");
  let error=publish_bootstrap(&stage,&target,&root,&capability).unwrap_err();
  assert_eq!(error,format!("workspace authority already exists; retry with --coordinator-capability-file {}",capability.display()));
  assert!(!stage.exists());
  fs::remove_dir_all(root).unwrap();
 }
}
