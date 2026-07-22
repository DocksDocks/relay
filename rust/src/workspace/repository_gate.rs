use super::authority::{self, AuthorityRoots};
use super::schema::{self, ClosedJcs, JcsValue, RepositoryIdentityV1};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read,Write};
use std::os::fd::{AsRawFd,FromRawFd};
use std::os::unix::fs::{MetadataExt,OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const GATE_PROTOCOL: &str = "RepositoryGateV1";
pub const GATE_TIMEOUT: Duration = Duration::from_secs(3);
const MARKER_RELATIVE: &str = "docks/workspace-admission-v1.json";

fn open_private_marker_directory(repository:&RepositoryIdentityV1,euid:u32)->Result<(File,PathBuf),String>{
    let common_path=Path::new(&repository.common_dir_realpath);
    let common=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY).open(common_path).map_err(|e|format!("securely open Git common directory for marker: {e}"))?;
    let common_metadata=common.metadata().map_err(|e|format!("fstat Git common directory for marker: {e}"))?;
    let expected_dev=repository.common_dir_dev.parse::<u64>().map_err(|_|"repository common-dir device is not decimal".to_string())?;let expected_ino=repository.common_dir_ino.parse::<u64>().map_err(|_|"repository common-dir inode is not decimal".to_string())?;
    if !common_metadata.is_dir()||common_metadata.uid()!=euid||common_metadata.dev()!=expected_dev||common_metadata.ino()!=expected_ino{return Err("Git common directory identity changed before marker publication".into())}
    let name=std::ffi::CString::new("docks").unwrap();let created=unsafe{libc::mkdirat(common.as_raw_fd(),name.as_ptr(),0o700)};
    if created!=0{let error=std::io::Error::last_os_error();if error.kind()!=std::io::ErrorKind::AlreadyExists{return Err(format!("create managed marker directory: {error}"))}}
    let fd=unsafe{libc::openat(common.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY,0)};
    if fd<0{return Err(format!("securely open managed marker directory: {}",std::io::Error::last_os_error()))}
    let directory=unsafe{File::from_raw_fd(fd)};let metadata=directory.metadata().map_err(|e|format!("fstat managed marker directory: {e}"))?;
    if !metadata.is_dir()||metadata.uid()!=euid||metadata.mode()&0o777!=0o700{return Err("managed marker directory is not an EUID-owned mode-0700 real directory".into())}
    if created==0{common.sync_all().map_err(|e|format!("fsync Git common directory after marker directory creation: {e}"))?}
    Ok((directory,common_path.join("docks")))
}

fn open_marker_at(directory:&File,flags:libc::c_int,mode:libc::mode_t)->Result<File,std::io::Error>{
    let name=std::ffi::CString::new("workspace-admission-v1.json").unwrap();
    let fd=unsafe{libc::openat(directory.as_raw_fd(),name.as_ptr(),flags|libc::O_CLOEXEC|libc::O_NOFOLLOW,mode)};
    if fd<0{return Err(std::io::Error::last_os_error())}
    Ok(unsafe{File::from_raw_fd(fd)})
}

fn read_marker_at(directory:&File,euid:u32)->Result<Option<Vec<u8>>,String>{
    let mut file=match open_marker_at(directory,libc::O_RDONLY,0){
        Ok(file)=>file,
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return Ok(None),
        Err(error)=>return Err(format!("securely open managed marker: {error}")),
    };
    let metadata=file.metadata().map_err(|e|format!("fstat managed marker: {e}"))?;
    if !metadata.is_file()||metadata.uid()!=euid||metadata.nlink()!=1||metadata.mode()&0o777!=0o600{return Err("managed marker is not an EUID-owned single-link mode-0600 regular file".into())}
    let mut bytes=Vec::new();file.read_to_end(&mut bytes).map_err(|e|format!("read managed marker: {e}"))?;Ok(Some(bytes))
}

pub struct RepositoryGate { file: File, path: PathBuf, repository_id: String }
impl RepositoryGate {
    pub fn acquire(roots:&AuthorityRoots,repository:&RepositoryIdentityV1)->Result<Self,String>{
        if repository.euid!=roots.euid.to_string(){return Err("repository identity EUID differs from authority root".into())}
        let gates=roots.authority.join("repository-gates");authority::ensure_private_directory(&gates,roots.euid)?;
        let path=gates.join(format!("{}.lock",repository.repository_id));
        let file=OpenOptions::new().create(true).truncate(false).read(true).write(true).mode(0o600).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&path).map_err(|e|format!("open RepositoryGate {}: {e}",path.display()))?;
        let metadata=file.metadata().map_err(|e|format!("fstat RepositoryGate: {e}"))?;
        if !metadata.is_file()||metadata.uid()!=roots.euid||metadata.nlink()!=1||metadata.mode()&0o777!=0o600{return Err("RepositoryGate has unsafe owner/type/link/mode".into())}
        let deadline=Instant::now()+GATE_TIMEOUT;
        loop {let result=unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)};if result==0{break}let error=std::io::Error::last_os_error();if error.raw_os_error()!=Some(libc::EINTR)&&error.raw_os_error()!=Some(libc::EAGAIN){return Err(format!("lock RepositoryGate: {error}"))}if Instant::now()>=deadline{return Err("RepositoryGate contention exceeded three seconds; no mutation performed".into())}std::thread::sleep(Duration::from_millis(10));}
        Ok(Self{file,path,repository_id:repository.repository_id.clone()})
    }
    pub fn path(&self)->&Path{&self.path}
    pub fn repository_id(&self)->&str{&self.repository_id}
    pub fn as_raw_fd(&self)->i32{self.file.as_raw_fd()}

    pub fn admit_workspace_storage(&self,roots:&AuthorityRoots,repository:&RepositoryIdentityV1)->Result<(),String>{
        if self.repository_id!=repository.repository_id{return Err("RepositoryGate identity differs from workspace admission".into())}
        admit_ext4_path(&roots.data)?;
        admit_ext4_path(Path::new(&repository.common_dir_realpath))?;
        Ok(())
    }

    pub fn refuse_legacy_if_managed(&self,roots:&AuthorityRoots,common_dir:&Path)->Result<(),String>{
        let authority=roots.authority.join("repositories").join(&self.repository_id);
        let marker=common_dir.join(MARKER_RELATIVE);
        if fs::symlink_metadata(&authority).is_ok()||fs::symlink_metadata(&marker).is_ok(){return Err("repository is in managed workspace mode; legacy fanout mutation is refused".into())}
        Ok(())
    }

    pub fn publish_workspace_marker(&self,roots:&AuthorityRoots,repository:&RepositoryIdentityV1,minimum_relay_version:&str,created_at:&str)->Result<PathBuf,String>{
        if self.repository_id!=repository.repository_id{return Err("RepositoryGate identity differs from marker repository".into())}
        schema::Timestamp::parse(created_at)?;
        let authority_repository=roots.authority.join("repositories").join(&repository.repository_id);
        if !authority_repository.is_dir(){return Err("workspace authority must be durable before marker publication".into())}
        let (directory,docks)=open_private_marker_directory(repository,roots.euid)?;
        let marker=WorkspaceAdmissionV1{repository_id:repository.repository_id.clone(),mode:"workspace".into(),gate_protocol:GATE_PROTOCOL.into(),minimum_relay_version:minimum_relay_version.into(),authority_repository_path:authority_repository.to_string_lossy().into_owned(),created_at:created_at.into()};
        let path=docks.join("workspace-admission-v1.json");
        if let Some(bytes)=read_marker_at(&directory,roots.euid)?{let parsed=WorkspaceAdmissionV1::from_jcs(schema::parse_jcs(&bytes,true)?)?;if parsed!=marker{return Err("managed workspace marker exists with different identity or contract".into())}return Ok(path)}
        let bytes=schema::serialize_jcs_lf(&marker);
        let mut file=match open_marker_at(&directory,libc::O_WRONLY|libc::O_CREAT|libc::O_EXCL,0o600){
            Ok(file)=>file,
            Err(error) if error.kind()==std::io::ErrorKind::AlreadyExists=>{let existing=read_marker_at(&directory,roots.euid)?.ok_or_else(||"managed marker disappeared during publication".to_string())?;let parsed=WorkspaceAdmissionV1::from_jcs(schema::parse_jcs(&existing,true)?)?;if parsed!=marker{return Err("managed workspace marker exists with different identity or contract".into())}return Ok(path)},
            Err(error)=>return Err(format!("create managed marker: {error}")),
        };
        file.write_all(&bytes).and_then(|_|file.sync_all()).map_err(|e|format!("persist managed marker: {e}"))?;
        directory.sync_all().map_err(|e|format!("fsync managed marker directory: {e}"))?;
        Ok(path)
    }
}
#[derive(Clone,Debug,Eq,PartialEq)]
pub struct WorkspaceAdmissionV1{pub repository_id:String,pub mode:String,pub gate_protocol:String,pub minimum_relay_version:String,pub authority_repository_path:String,pub created_at:String}
impl ClosedJcs for WorkspaceAdmissionV1{
 fn from_jcs(value:JcsValue)->Result<Self,String>{let object=value.object()?;let keys=["schema","repository_id","mode","gate_protocol","minimum_relay_version","authority_repository_path","created_at"];if object.len()!=keys.len()||keys.iter().any(|k|!object.contains_key(*k)){return Err("WorkspaceAdmissionV1 keys differ".into())}let s=|k:&str|object[k].as_str().map(str::to_string);if s("schema")?!=schema::SCHEMA_V1{return Err("workspace marker schema mismatch".into())}schema::Sha256Digest::parse(&s("repository_id")?)?;if s("mode")?!="workspace"||s("gate_protocol")?!=GATE_PROTOCOL{return Err("workspace marker mode/protocol mismatch".into())}schema::AbsPath::parse(&s("authority_repository_path")?)?;schema::Timestamp::parse(&s("created_at")?)?;Ok(Self{repository_id:s("repository_id")?,mode:s("mode")?,gate_protocol:s("gate_protocol")?,minimum_relay_version:s("minimum_relay_version")?,authority_repository_path:s("authority_repository_path")?,created_at:s("created_at")?})}
 fn to_jcs(&self)->JcsValue{JcsValue::Object(BTreeMap::from([("authority_repository_path".into(),JcsValue::String(self.authority_repository_path.clone())),("created_at".into(),JcsValue::String(self.created_at.clone())),("gate_protocol".into(),JcsValue::String(self.gate_protocol.clone())),("minimum_relay_version".into(),JcsValue::String(self.minimum_relay_version.clone())),("mode".into(),JcsValue::String(self.mode.clone())),("repository_id".into(),JcsValue::String(self.repository_id.clone())),("schema".into(),JcsValue::String(schema::SCHEMA_V1.into()))]))}
}

#[derive(Clone,Debug,Eq,PartialEq)]
pub struct MountAdmission{pub mount_id:u64,pub filesystem_type:String}
pub fn admit_ext4_path(path:&Path)->Result<MountAdmission,String>{
 let file=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY).open(path).map_err(|e|format!("open authoritative directory {}: {e}",path.display()))?;admit_ext4_fd(&file)
}
pub fn admit_ext4_fd(file:&File)->Result<MountAdmission,String>{
 #[cfg(target_os="linux")]
 {let mut stat=std::mem::MaybeUninit::<libc::statx>::zeroed();let result=unsafe{libc::statx(file.as_raw_fd(),b"\0".as_ptr().cast(),libc::AT_EMPTY_PATH|libc::AT_SYMLINK_NOFOLLOW,libc::STATX_MNT_ID,stat.as_mut_ptr())};if result!=0{return Err(format!("statx STATX_MNT_ID failed: {}",std::io::Error::last_os_error()))}let stat=unsafe{stat.assume_init()};if stat.stx_mask&libc::STATX_MNT_ID==0{return Err("statx did not report STATX_MNT_ID".into())}let mount_id=stat.stx_mnt_id;let mut text=String::new();File::open("/proc/self/mountinfo").and_then(|mut f|f.read_to_string(&mut text)).map_err(|e|format!("read /proc/self/mountinfo: {e}"))?;let mut found=None;for line in text.lines(){let mut split=line.split(" - ");let left=split.next().unwrap_or("");let right=split.next();if split.next().is_some()||right.is_none(){continue}let Some(id)=left.split_whitespace().next().and_then(|v|v.parse::<u64>().ok())else{continue};if id==mount_id{let filesystem=right.unwrap().split_whitespace().next().unwrap_or("");found=Some(filesystem.to_string());break}}let filesystem_type=found.ok_or_else(||format!("mount ID {mount_id} is absent from /proc/self/mountinfo"))?;if filesystem_type!="ext4"{return Err(format!("managed workspace requires exact ext4; mount ID {mount_id} is {filesystem_type}"))}Ok(MountAdmission{mount_id,filesystem_type})}
 #[cfg(not(target_os="linux"))]
 {let _=file;Err("managed workspace filesystem admission is unavailable on this platform".into())}
}

#[cfg(test)]
mod tests {
 use super::*;
 use super::super::schema::ObjectFormat;
 use std::os::unix::fs::symlink;

 fn fixture() -> (PathBuf,AuthorityRoots,RepositoryIdentityV1) {
  let base=PathBuf::from("/dev/shm").join(format!("session-relay-gate-{}",crate::store::uuid_v4()));
  let roots=AuthorityRoots{authority:base.join("authority"),data:base.join("data"),euid:unsafe{libc::geteuid()}};
  authority::ensure_private_directory(&roots.authority,roots.euid).unwrap();
  authority::ensure_private_directory(&roots.data,roots.euid).unwrap();
  authority::ensure_private_directory(&roots.authority.join("repository-gates"),roots.euid).unwrap();
  authority::ensure_private_directory(&roots.authority.join("repositories"),roots.euid).unwrap();
  let repository=RepositoryIdentityV1{repository_id:"a".repeat(64),common_dir_realpath:base.join("common").to_string_lossy().into_owned(),common_dir_dev:"1".into(),common_dir_ino:"1".into(),common_dir_owner_euid:roots.euid.to_string(),euid:roots.euid.to_string(),object_format:ObjectFormat::Sha1};
  (base,roots,repository)
 }

 #[test]
 fn portable_gate_does_not_require_managed_ext4_admission() {
  let (base,roots,repository)=fixture();
  let gate=RepositoryGate::acquire(&roots,&repository).unwrap();
  assert_eq!(gate.repository_id(),repository.repository_id);
  assert!(gate.admit_workspace_storage(&roots,&repository).is_err());
  drop(gate);
  fs::remove_dir_all(base).unwrap();
 }

 #[test]
 fn marker_publication_refuses_symlinked_private_directory() {
  let (base,roots,mut repository)=fixture();
  fs::create_dir(Path::new(&repository.common_dir_realpath)).unwrap();let common_metadata=fs::metadata(&repository.common_dir_realpath).unwrap();repository.common_dir_dev=common_metadata.dev().to_string();repository.common_dir_ino=common_metadata.ino().to_string();
  let outside=base.join("outside");
  fs::create_dir(&outside).unwrap();
  symlink(&outside,Path::new(&repository.common_dir_realpath).join("docks")).unwrap();
  authority::ensure_private_directory(&roots.authority.join("repositories").join(&repository.repository_id),roots.euid).unwrap();
  let gate=RepositoryGate::acquire(&roots,&repository).unwrap();
  assert!(gate.publish_workspace_marker(&roots,&repository,"1.0.0","2026-07-22T00:00:00.000Z").is_err());
  assert!(!outside.join("workspace-admission-v1.json").exists());
  drop(gate);
  fs::remove_dir_all(base).unwrap();
 }

 #[test]
 fn marker_publication_is_private_durable_and_idempotent() {
  let (base,roots,mut repository)=fixture();fs::create_dir(Path::new(&repository.common_dir_realpath)).unwrap();let metadata=fs::metadata(&repository.common_dir_realpath).unwrap();repository.common_dir_dev=metadata.dev().to_string();repository.common_dir_ino=metadata.ino().to_string();
  authority::ensure_private_directory(&roots.authority.join("repositories").join(&repository.repository_id),roots.euid).unwrap();let gate=RepositoryGate::acquire(&roots,&repository).unwrap();
  let first=gate.publish_workspace_marker(&roots,&repository,"1.0.0","2026-07-22T00:00:00.000Z").unwrap();let second=gate.publish_workspace_marker(&roots,&repository,"1.0.0","2026-07-22T00:00:00.000Z").unwrap();assert_eq!(first,second);
  let docks=fs::metadata(first.parent().unwrap()).unwrap();let marker=fs::metadata(&first).unwrap();assert_eq!(docks.mode()&0o777,0o700);assert_eq!(marker.mode()&0o777,0o600);assert_eq!(marker.nlink(),1);
  drop(gate);fs::remove_dir_all(base).unwrap();
 }
}
