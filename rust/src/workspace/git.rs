use super::authority;
use super::schema::{GitOid,ObjectFormat,RepositoryIdentityV1};
use std::fs::{self,File,OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt,OpenOptionsExt};
use std::path::{Path,PathBuf};
use std::process::{Command,Output,Stdio};

#[derive(Debug)]
pub struct OpenedRepository{pub root:PathBuf,pub identity:RepositoryIdentityV1,common_dir:File}
impl OpenedRepository{
 pub fn open(path:&Path)->Result<Self,String>{
  let root_text=run_git_text(path,&["rev-parse","--path-format=absolute","--show-toplevel"])?;let root=fs::canonicalize(&root_text).map_err(|e|format!("canonicalize repository root {root_text}: {e}"))?;
  if root.to_str()!=Some(root_text.as_str()){return Err("repository root is not canonical UTF-8 absolute form".into())}
  let common_text=run_git_text(&root,&["rev-parse","--path-format=absolute","--git-common-dir"])?;let common=fs::canonicalize(&common_text).map_err(|e|format!("canonicalize Git common dir {common_text}: {e}"))?;
  if common.to_str()!=Some(common_text.as_str()){return Err("Git common dir is not canonical UTF-8 absolute form".into())}
  let common_dir=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY).open(&common).map_err(|e|format!("securely open Git common dir {}: {e}",common.display()))?;
  let metadata=common_dir.metadata().map_err(|e|format!("fstat Git common dir: {e}"))?;let euid=unsafe{libc::geteuid()};if !metadata.is_dir()||metadata.uid()!=euid{return Err("Git common dir is not an EUID-owned real directory".into())}
  let object_format=ObjectFormat::parse(&run_git_text(&root,&["rev-parse","--show-object-format"])?)?;
  let repository_id=authority::repository_id(euid,metadata.dev(),metadata.ino());
  let identity=RepositoryIdentityV1{repository_id,common_dir_realpath:common.to_string_lossy().into_owned(),common_dir_dev:metadata.dev().to_string(),common_dir_ino:metadata.ino().to_string(),common_dir_owner_euid:metadata.uid().to_string(),euid:euid.to_string(),object_format};
  Ok(Self{root,identity,common_dir})
 }
 pub fn validate_unchanged(&self)->Result<(),String>{let reopened=Self::open(&self.root)?;if reopened.identity!=self.identity{return Err("repository identity or object format changed".into())}Ok(())}
 pub fn common_dir_fd(&self)->i32{self.common_dir.as_raw_fd()}
 pub fn head(&self)->Result<GitOid,String>{GitOid::parse(&run_git_text(&self.root,&["rev-parse","--verify","HEAD"])?,self.identity.object_format)}
 pub fn validate_oid(&self,value:&str)->Result<GitOid,String>{GitOid::parse(value,self.identity.object_format)}
}

pub fn run_git(cwd:&Path,args:&[&str])->Result<Output,String>{Command::new("git").args(args).current_dir(cwd).stdin(Stdio::null()).output().map_err(|e|format!("run git {} in {}: {e}",args.join(" "),cwd.display()))}
pub fn run_git_text(cwd:&Path,args:&[&str])->Result<String,String>{let output=run_git(cwd,args)?;if !output.status.success(){return Err(format!("git {} failed in {}: {}",args.join(" "),cwd.display(),String::from_utf8_lossy(&output.stderr).trim()))}let text=String::from_utf8(output.stdout).map_err(|_|"Git output was not UTF-8".to_string())?;Ok(text.trim_end_matches(['\r','\n']).to_string())}
pub fn run_git_bytes(cwd:&Path,args:&[&str])->Result<Vec<u8>,String>{let output=run_git(cwd,args)?;if !output.status.success(){return Err(format!("git {} failed in {}: {}",args.join(" "),cwd.display(),String::from_utf8_lossy(&output.stderr).trim()))}Ok(output.stdout)}

pub fn actual_private_git_dir(worktree:&Path)->Result<PathBuf,String>{
 let value=run_git_text(worktree,&["rev-parse","--path-format=absolute","--git-dir"])?;let path=fs::canonicalize(&value).map_err(|e|format!("canonicalize private Git dir {value}: {e}"))?;if path.to_str()!=Some(value.as_str()){return Err("private Git dir is not canonical UTF-8 absolute form".into())}let file=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY).open(&path).map_err(|e|format!("securely open private Git dir: {e}"))?;let metadata=file.metadata().map_err(|e|format!("fstat private Git dir: {e}"))?;if !metadata.is_dir()||metadata.uid()!=unsafe{libc::geteuid()}{return Err("private Git dir is not an EUID-owned real directory".into())}Ok(path)
}
