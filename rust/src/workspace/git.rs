use super::authority;
use super::schema::{
    self, GitOid, ObjectFormat, PathClaimRequestV1, PreserveRequestV1, RepositoryIdentityV1,
    SourceSnapshotV1, WipPayloadV1, WipReceiptV1, WorktreeIdentityV1,
};
use crate::sha256;
use std::fs::{self,File,OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt,OpenOptionsExt,PermissionsExt};
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

#[derive(Clone,Debug,Eq,PartialEq)]
pub struct PreserveResult { pub receipt:WipReceiptV1,pub receipt_file:PathBuf,pub receipt_sha256:String }

pub fn source_snapshot(repository:&OpenedRepository)->Result<SourceSnapshotV1,String>{
 let head=repository.head()?.as_str().to_string();
 let index_path=run_git_text(&repository.root,&["rev-parse","--path-format=absolute","--git-path","index"])?;
 let index_sha256=match fs::read(&index_path){Ok(bytes)=>Some(sha256::hex_digest(&bytes)),Err(error) if error.kind()==std::io::ErrorKind::NotFound=>None,Err(error)=>return Err(format!("read Git index {index_path}: {error}"))};
 let status=run_git_bytes(&repository.root,&["status","--porcelain=v2","-z","--untracked-files=all"])?;
 let inventory=run_git_bytes(&repository.root,&["ls-files","-z","--cached","--others","--exclude-standard"])?;
 Ok(SourceSnapshotV1{head_oid:head,index_sha256,status_sha256:sha256::hex_digest(&status),tracked_untracked_inventory_sha256:sha256::hex_digest(&inventory)})
}

pub fn preserve(repository:&OpenedRepository,request:&PreserveRequestV1,request_sha256:&str,preserve_root:&Path)->Result<PreserveResult,String>{
 repository.validate_unchanged()?;
 let base=repository.validate_oid(&request.base_commit)?;
 if base.as_str()!=request.base_commit{return Err("preserve base OID differs from canonical request".into())}
 if repository.root.to_str()!=Some(request.repository_path.as_str()){return Err("preserve request repository path differs from opened root".into())}
 let ancestor=run_git(&repository.root,&["merge-base","--is-ancestor",&request.base_commit,"HEAD"])?;if !ancestor.status.success(){return Err("preserve base is not an ancestor of source HEAD".into())}
 authority::ensure_private_directory(preserve_root,unsafe{libc::geteuid()})?;
 let receipt_id=crate::store::uuid_v4();
 let output=preserve_root.join(&request.request_id);
 if output.exists(){return Err("preserve request was already materialized; refusing replacement".into())}
 fs::create_dir(&output).map_err(|e|format!("create preserve output {}: {e}",output.display()))?;fs::set_permissions(&output,fs::Permissions::from_mode(0o700)).map_err(|e|format!("chmod preserve output: {e}"))?;
 let before=source_snapshot(repository)?;
 let payload=match request.mode.as_str(){
  "commit"=>preserve_commit(repository,request,&output)?,
  "artifact"=>preserve_artifact(repository,request,&output)?,
  _=>return Err("preserve mode must be commit|artifact".into()),
 };
 let after=source_snapshot(repository)?;
 if before!=after{return Err("preservation changed source HEAD/index/status/inventory".into())}
 let receipt=WipReceiptV1{receipt_id,request_sha256:request_sha256.into(),repository:repository.identity.clone(),source_root:repository.root.to_string_lossy().into_owned(),base_commit:request.base_commit.clone(),mode:request.mode.clone(),before,after,payload,created_at:request.created_at.clone()};
 let receipt_file=output.join("wip-receipt-v1.json");authority::atomic_create_jcs(&receipt_file,&receipt,0o600)?;let receipt_sha256=schema::jcs_sha256(&receipt);
 Ok(PreserveResult{receipt,receipt_file,receipt_sha256})
}

fn preserve_commit(repository:&OpenedRepository,request:&PreserveRequestV1,output:&Path)->Result<WipPayloadV1,String>{
 let index=output.join("temporary-index");let index_text=index.to_str().ok_or_else(||"temporary index path is not UTF-8".to_string())?;
 git_env(&repository.root,&["read-tree",&request.base_commit],&[("GIT_INDEX_FILE",index_text)])?;
 git_env(&repository.root,&["add","-A","--", "."],&[("GIT_INDEX_FILE",index_text)])?;
 let tree=git_env(&repository.root,&["write-tree"],&[("GIT_INDEX_FILE",index_text)])?;
 repository.validate_oid(&tree)?;
 let commit=fixed_commit_tree(repository,&tree,&request.base_commit,&request.created_at,"session-relay preserved WIP")?;
 let preserve_ref=format!("refs/docks/preserve/{}",request.request_id);
 git_env(&repository.root,&["update-ref","--no-deref",&preserve_ref,&commit,""],&[])?;
 let published=run_git_text(&repository.root,&["rev-parse","--verify",&preserve_ref])?;if published!=commit{return Err("preserve ref read-back differs from created commit".into())}
 let published_tree=run_git_text(&repository.root,&["rev-parse","--verify",&format!("{commit}^{{tree}}")])?;if published_tree!=tree{return Err("preserved commit tree read-back differs".into())}
 let published_parent=run_git_text(&repository.root,&["rev-parse","--verify",&format!("{commit}^")])?;if published_parent!=request.base_commit{return Err("preserved commit parent read-back differs".into())}
 fs::set_permissions(&index,fs::Permissions::from_mode(0o600)).map_err(|e|format!("chmod temporary index: {e}"))?;let index_bytes=fs::read(&index).map_err(|e|format!("read temporary index: {e}"))?;let temporary_index_sha256=sha256::hex_digest(&index_bytes);if fs::read(&index).map_err(|e|format!("read back temporary index: {e}"))?!=index_bytes{return Err("temporary index read-back differs".into())}
 Ok(WipPayloadV1::Commit{temporary_index_sha256,tree_oid:tree,preserved_commit:commit,preserve_ref})
}

fn preserve_artifact(repository:&OpenedRepository,request:&PreserveRequestV1,output:&Path)->Result<WipPayloadV1,String>{
 let binary_diff_staging=output.join("tracked-full-index.binary");let diff=run_git_bytes(&repository.root,&["diff","--binary","--full-index",&request.base_commit,"--"])?;write_private(&binary_diff_staging,&diff)?;
 if fs::read(&binary_diff_staging).map_err(|e|format!("read back tracked artifact: {e}"))?!=diff{return Err("tracked artifact read-back differs".into())}
 let inventory_bytes=run_git_bytes(&repository.root,&["ls-files","-z","--others","--exclude-standard"])?;
 let mut entries=Vec::new();
 for bytes in inventory_bytes.split(|byte|*byte==0).filter(|entry|!entry.is_empty()){
  let value=std::str::from_utf8(bytes).map_err(|_|"artifact path is not UTF-8".to_string())?;schema::RelPath::parse(value)?;
  let path=repository.root.join(value);let metadata=fs::symlink_metadata(&path).map_err(|e|format!("inspect artifact member {value}: {e}"))?;
  if metadata.file_type().is_file(){if metadata.nlink()!=1{return Err(format!("artifact member {value} is a hard-linked regular file"))}}
  else if metadata.file_type().is_symlink(){let target=fs::read_link(&path).map_err(|e|format!("read artifact symlink {value}: {e}"))?;if target.is_absolute()||target.components().any(|c|matches!(c,std::path::Component::ParentDir)){return Err(format!("artifact symlink {value} escapes the source root"))}}
  else{return Err(format!("artifact member {value} has an unsupported type"))}
  entries.push(value.to_string());
 }
 entries.sort();if entries.windows(2).any(|pair|pair[0]==pair[1]){return Err("artifact inventory contains duplicate paths".into())}
 let mut inventory=Vec::new();for entry in &entries{inventory.extend_from_slice(entry.as_bytes());inventory.push(0)}
 if inventory!=inventory_bytes{return Err("artifact inventory is not raw-byte ordered".into())}
 let untracked_inventory_staging=output.join("untracked.inventory");write_private(&untracked_inventory_staging,&inventory)?;
 if fs::read(&untracked_inventory_staging).map_err(|e|format!("read back artifact inventory: {e}"))?!=inventory{return Err("artifact inventory read-back differs".into())}
 let untracked_archive_staging=output.join("untracked.pax");create_pax_archive(&repository.root,&untracked_archive_staging,&inventory)?;
 let archive_file=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&untracked_archive_staging).map_err(|e|format!("reopen PAX archive: {e}"))?;archive_file.sync_all().map_err(|e|format!("sync PAX archive: {e}"))?;let metadata=archive_file.metadata().map_err(|e|format!("inspect PAX archive: {e}"))?;if !metadata.is_file()||metadata.uid()!=unsafe{libc::geteuid()}||metadata.nlink()!=1||metadata.mode()&0o777!=0o600{return Err("PAX archive is not an EUID-owned single-link mode-0600 regular file".into())}
 let archive=fs::read(&untracked_archive_staging).map_err(|e|format!("read PAX archive: {e}"))?;
 let binary_diff=publish_content_addressed(&binary_diff_staging,&diff,"tracked-full-index.binary")?;
 let untracked_inventory=publish_content_addressed(&untracked_inventory_staging,&inventory,"untracked.inventory")?;
 let untracked_archive=publish_content_addressed(&untracked_archive_staging,&archive,"untracked.pax")?;
 Ok(WipPayloadV1::Artifact{binary_diff:binary_diff.to_string_lossy().into_owned(),untracked_inventory:untracked_inventory.to_string_lossy().into_owned(),untracked_archive:untracked_archive.to_string_lossy().into_owned(),archive_format:"pax".into(),entries})
}

fn create_pax_archive(root:&Path,archive:&Path,inventory:&[u8])->Result<(),String>{
 let mut child=Command::new("tar").args(["--format=pax","--null","--no-recursion","-cf"]).arg(archive).arg("--files-from=-").current_dir(root).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e|format!("spawn pax archive writer: {e}"))?;
 child.stdin.take().ok_or_else(||"pax archive writer has no stdin".to_string())?.write_all(inventory).map_err(|e|format!("write pax inventory: {e}"))?;
 let output=child.wait_with_output().map_err(|e|format!("wait pax archive writer: {e}"))?;if !output.status.success(){return Err(format!("pax archive creation failed: {}",String::from_utf8_lossy(&output.stderr).trim()))}
 fs::set_permissions(archive,fs::Permissions::from_mode(0o600)).map_err(|e|format!("chmod pax archive: {e}"))
}

fn write_private(path:&Path,bytes:&[u8])->Result<(),String>{let mut file=OpenOptions::new().create_new(true).write(true).mode(0o600).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(path).map_err(|e|format!("create {}: {e}",path.display()))?;file.write_all(bytes).and_then(|_|file.sync_all()).map_err(|e|format!("persist {}: {e}",path.display()))}

fn publish_content_addressed(staging:&Path,bytes:&[u8],label:&str)->Result<PathBuf,String>{
 let digest=sha256::hex_digest(bytes);let target=staging.parent().ok_or_else(||"artifact staging path has no parent".to_string())?.join(format!("{digest}.{label}"));
 fs::rename(staging,&target).map_err(|e|format!("publish content-addressed artifact {}: {e}",target.display()))?;
 let directory=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY).open(target.parent().unwrap()).map_err(|e|format!("open artifact directory for fsync: {e}"))?;directory.sync_all().map_err(|e|format!("fsync artifact directory: {e}"))?;
 Ok(target)
}

fn read_content_addressed(path:&Path,label:&str)->Result<Vec<u8>,String>{
 let file_name=path.file_name().and_then(|name|name.to_str()).ok_or_else(||format!("content-addressed {label} path is not UTF-8"))?;
 let digest=file_name.strip_suffix(&format!(".{label}")).ok_or_else(||format!("{label} path is not content-addressed"))?;schema::Sha256Digest::parse(digest)?;
 let mut file=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(path).map_err(|e|format!("securely open {label}: {e}"))?;
 let metadata=file.metadata().map_err(|e|format!("fstat {label}: {e}"))?;if !metadata.is_file()||metadata.uid()!=unsafe{libc::geteuid()}||metadata.nlink()!=1||metadata.mode()&0o777!=0o600{return Err(format!("{label} is not an EUID-owned single-link mode-0600 regular file"))}
 let mut bytes=Vec::new();use std::io::Read;file.read_to_end(&mut bytes).map_err(|e|format!("read {label}: {e}"))?;
 if sha256::hex_digest(&bytes)!=digest{return Err(format!("{label} content digest differs from its receipt-bound name"))}
 Ok(bytes)
}

fn git_env(cwd:&Path,args:&[&str],environment:&[(&str,&str)])->Result<String,String>{let output=Command::new("git").args(args).envs(environment.iter().copied()).current_dir(cwd).stdin(Stdio::null()).output().map_err(|e|format!("run git {}: {e}",args.join(" ")))?;if !output.status.success(){return Err(format!("git {} failed: {}",args.join(" "),String::from_utf8_lossy(&output.stderr).trim()))}String::from_utf8(output.stdout).map(|v|v.trim().to_string()).map_err(|_|"Git output was not UTF-8".into())}
fn fixed_commit_tree(repository:&OpenedRepository,tree:&str,parent:&str,timestamp:&str,message:&str)->Result<String,String>{let environment=[("GIT_AUTHOR_NAME","Session Relay"),("GIT_AUTHOR_EMAIL","session-relay@localhost"),("GIT_AUTHOR_DATE",timestamp),("GIT_COMMITTER_NAME","Session Relay"),("GIT_COMMITTER_EMAIL","session-relay@localhost"),("GIT_COMMITTER_DATE",timestamp)];git_env(&repository.root,&["commit-tree",tree,"-p",parent,"-m",message],&environment)}

pub fn provision_worktree(repository:&OpenedRepository,root:&Path,branch_ref:&str,session_id:&str,task_slug:&str,base_commit:&str)->Result<WorktreeIdentityV1,String>{
 if root.exists(){return Err(format!("deterministic workspace root {} already exists; suffixing is forbidden",root.display()))}
 let short_branch=branch_ref.strip_prefix("refs/heads/").ok_or_else(||"workspace branch is not under refs/heads".to_string())?;
 let reason=format!("session-relay:{session_id}");let root_text=root.to_str().ok_or_else(||"workspace root is not UTF-8".to_string())?;
 run_git_text(&repository.root,&["worktree","add","--lock","--reason",&reason,"-b",short_branch,root_text,base_commit])?;
 let identity=worktree_identity(root,branch_ref)?;
 if run_git_text(root,&["symbolic-ref","-q","HEAD"])?!=branch_ref{return Err("created workspace symbolic branch differs from deterministic ref".into())}
 if !branch_ref.ends_with(&format!("/{task_slug}")){return Err("created workspace branch task slug differs".into())}
 Ok(identity)
}

pub fn worktree_identity(root:&Path,branch_ref:&str)->Result<WorktreeIdentityV1,String>{
 let canonical=fs::canonicalize(root).map_err(|e|format!("canonicalize workspace root: {e}"))?;if canonical!=root{return Err("workspace root is not canonical or traverses a symlink".into())}
 let root_meta=fs::symlink_metadata(&canonical).map_err(|e|format!("stat workspace root: {e}"))?;if !root_meta.is_dir()||root_meta.uid()!=unsafe{libc::geteuid()}{return Err("workspace root is not an EUID-owned real directory".into())}
 let private=actual_private_git_dir(root)?;let private_meta=fs::symlink_metadata(&private).map_err(|e|format!("stat private Git dir: {e}"))?;
 let identity_sha256=sha256::hex_digest(format!("session-relay/worktree/v1\\0{}\\0{}\\0{}\\0{}\\0{}\\0{}",canonical.display(),root_meta.dev(),root_meta.ino(),private.display(),private_meta.dev(),private_meta.ino()).as_bytes());
 Ok(WorktreeIdentityV1{identity_sha256,root_realpath:canonical.to_string_lossy().into_owned(),root_dev:root_meta.dev().to_string(),root_ino:root_meta.ino().to_string(),root_owner_euid:root_meta.uid().to_string(),private_git_dir_realpath:private.to_string_lossy().into_owned(),private_git_dir_dev:private_meta.dev().to_string(),private_git_dir_ino:private_meta.ino().to_string(),branch_ref:branch_ref.into()})
}

pub fn apply_wip(repository:&OpenedRepository,worktree:&Path,branch_ref:&str,base_commit:&str,receipt:&WipReceiptV1,timestamp:&str)->Result<String,String>{
 if run_git_text(worktree,&["symbolic-ref","-q","HEAD"])?!=branch_ref{return Err("workspace branch changed before WIP application".into())}
 if run_git_text(worktree,&["rev-parse","--verify","HEAD"])?!=base_commit{return Err("workspace HEAD changed before WIP application".into())}
 if receipt.repository!=repository.identity||receipt.source_root!=repository.root.to_string_lossy(){return Err("WIP receipt repository provenance differs at application".into())}
 match &receipt.payload{
  WipPayloadV1::Commit{tree_oid,preserved_commit,preserve_ref,..}=>{
   repository.validate_oid(tree_oid)?;repository.validate_oid(preserved_commit)?;
   let published=run_git_text(&repository.root,&["rev-parse","--verify",preserve_ref])?;if &published!=preserved_commit{return Err("preserve ref changed before WIP application".into())}
   let preserved_tree=run_git_text(&repository.root,&["rev-parse","--verify",&format!("{preserved_commit}^{{tree}}")])?;if &preserved_tree!=tree_oid{return Err("preserved commit tree differs before WIP application".into())}
   let parent=run_git_text(&repository.root,&["rev-parse","--verify",&format!("{preserved_commit}^")])?;if parent!=base_commit{return Err("preserved commit parent differs before WIP application".into())}
   run_git_text(worktree,&["read-tree","--reset","-u",tree_oid])?;
  }
  WipPayloadV1::Artifact{binary_diff,untracked_inventory,untracked_archive,entries,..}=>{
   let diff=read_content_addressed(Path::new(binary_diff),"tracked-full-index.binary")?;
   let inventory=read_content_addressed(Path::new(untracked_inventory),"untracked.inventory")?;
   let _archive=read_content_addressed(Path::new(untracked_archive),"untracked.pax")?;
   let parsed=inventory.split(|byte|*byte==0).filter(|value|!value.is_empty()).map(|value|std::str::from_utf8(value).map(str::to_string).map_err(|_|"artifact inventory is not UTF-8".to_string())).collect::<Result<Vec<_>,_>>()?;if &parsed!=entries{return Err("artifact inventory differs from receipt entries".into())}
   let listed=Command::new("tar").args(["-tf",untracked_archive]).stdin(Stdio::null()).output().map_err(|e|format!("list artifact archive: {e}"))?;if !listed.status.success(){return Err("artifact archive cannot be listed".into())}let archive_entries=String::from_utf8(listed.stdout).map_err(|_|"artifact archive member is not UTF-8".to_string())?.lines().map(|line|line.trim_end_matches('/').to_string()).filter(|line|!line.is_empty()).collect::<Vec<_>>();if archive_entries!=*entries{return Err("artifact archive membership differs from receipt".into())}
   if !diff.is_empty(){let output=Command::new("git").args(["apply","--index","--binary"]).arg(binary_diff).current_dir(worktree).stdin(Stdio::null()).output().map_err(|e|format!("apply tracked artifact: {e}"))?;if !output.status.success(){return Err(format!("apply tracked artifact failed: {}",String::from_utf8_lossy(&output.stderr).trim()))}}
   let output=Command::new("tar").args(["--extract","--no-same-owner","--no-same-permissions","--keep-old-files","-f",untracked_archive]).current_dir(worktree).stdin(Stdio::null()).output().map_err(|e|format!("extract artifact: {e}"))?;if !output.status.success(){return Err(format!("artifact extraction failed: {}",String::from_utf8_lossy(&output.stderr).trim()))}
   if !entries.is_empty(){let mut args=vec!["add","--"];args.extend(entries.iter().map(String::as_str));run_git_text(worktree,&args)?;}
  }
 }
 let tree=run_git_text(worktree,&["write-tree"])?;let commit=fixed_commit_tree(repository,&tree,base_commit,timestamp,"session-relay applied WIP")?;repository.validate_oid(&commit)?;
 run_git_text(&repository.root,&["update-ref","--no-deref",branch_ref,&commit,base_commit])?;run_git_text(worktree,&["reset","--hard",&commit])?;
 Ok(commit)
}

#[derive(Clone,Debug,Eq,PartialEq)]
pub struct NameStatusChange{pub status:String,pub source:Option<String>,pub destination:String}
pub fn parse_name_status_z(bytes:&[u8])->Result<Vec<NameStatusChange>,String>{
 if !bytes.is_empty()&&!bytes.ends_with(&[0]){return Err("name-status output is truncated".into())}
 let fields=bytes.split(|byte|*byte==0).filter(|field|!field.is_empty()).collect::<Vec<_>>();let mut index=0;let mut changes=Vec::new();
 while index<fields.len(){let status=std::str::from_utf8(fields[index]).map_err(|_|"name-status token is not UTF-8".to_string())?.to_string();index+=1;let rename=status.starts_with('R')||status.starts_with('C');if !matches!(status.as_bytes().first(),Some(b'A'|b'M'|b'D'|b'T'|b'R'|b'C')){return Err(format!("unsupported name-status code {status}"))}let first=fields.get(index).ok_or_else(||"name-status path is missing".to_string())?;index+=1;let first=std::str::from_utf8(first).map_err(|_|"name-status path is not UTF-8".to_string())?.to_string();schema::RelPath::parse(&first)?;if rename{let second=fields.get(index).ok_or_else(||"rename/copy destination is missing".to_string())?;index+=1;let second=std::str::from_utf8(second).map_err(|_|"name-status destination is not UTF-8".to_string())?.to_string();schema::RelPath::parse(&second)?;changes.push(NameStatusChange{status,source:Some(first),destination:second})}else{changes.push(NameStatusChange{status,source:None,destination:first})}}
 Ok(changes)
}

pub fn validate_changed_paths(changes:&[NameStatusChange],claims:&[PathClaimRequestV1])->Result<(),String>{
 let owns=|path:&str|claims.iter().any(|claim|path==claim.path||(claim.path_type=="directory"&&path.strip_prefix(&claim.path).is_some_and(|suffix|suffix.starts_with('/'))));
 for change in changes{if !owns(&change.destination)||change.source.as_deref().is_some_and(|source|!owns(source)){return Err(format!("changed path is outside admitted claims: {:?}",change))}}
 Ok(())
}

pub fn create_worker_commit(repository:&OpenedRepository,worktree:&Path,branch_ref:&str,message:&str,timestamp:&str)->Result<String,String>{
 if message.is_empty()||message.contains('\0'){return Err("worker commit message is invalid".into())}
 if run_git_text(worktree,&["symbolic-ref","-q","HEAD"])?!=branch_ref{return Err("workspace branch changed before worker commit".into())}
 let head=run_git_text(worktree,&["rev-parse","--verify","HEAD"])?;repository.validate_oid(&head)?;
 let private=actual_private_git_dir(worktree)?;for marker in ["MERGE_HEAD","REBASE_HEAD","CHERRY_PICK_HEAD","BISECT_START"]{if private.join(marker).exists(){return Err(format!("worker commit refused during forbidden Git operation {marker}"))}}
 let tree=run_git_text(worktree,&["write-tree"])?;let commit=fixed_commit_tree(repository,&tree,&head,timestamp,message)?;repository.validate_oid(&commit)?;
 run_git_text(&repository.root,&["update-ref","--no-deref",branch_ref,&commit,&head])?;
 Ok(commit)
}

#[cfg(test)]
mod tests{
 use super::*;
 fn command(cwd:&Path,args:&[&str])->String{let output=Command::new("git").args(args).current_dir(cwd).output().unwrap();assert!(output.status.success(),"git {:?}: {}",args,String::from_utf8_lossy(&output.stderr));String::from_utf8(output.stdout).unwrap().trim().into()}
 #[test]
 fn preservation_modes_and_applied_wip_are_real(){
  let root=std::env::temp_dir().join(format!("session-relay-s3-{}",crate::store::uuid_v4()));fs::create_dir(&root).unwrap();let repository_root=root.join("repository");fs::create_dir(&repository_root).unwrap();
  command(&repository_root,&["init","-q"]);command(&repository_root,&["config","user.name","Test"]);command(&repository_root,&["config","user.email","test@example.invalid"]);fs::write(repository_root.join("tracked.txt"),"base\n").unwrap();command(&repository_root,&["add","tracked.txt"]);command(&repository_root,&["commit","-qm","base"]);let base=command(&repository_root,&["rev-parse","HEAD"]);
  fs::write(repository_root.join("tracked.txt"),"changed\n").unwrap();fs::write(repository_root.join("untracked.bin"),[0,1,2,3]).unwrap();let repository=OpenedRepository::open(&repository_root).unwrap();let original=source_snapshot(&repository).unwrap();let preserve_root=root.join("preserved");let data_root=root.join("data");authority::ensure_private_directory(&data_root,unsafe{libc::geteuid()}).unwrap();
  for (index,mode) in ["commit","artifact"].into_iter().enumerate(){
   let request_id=crate::store::uuid_v4();let request=PreserveRequestV1{request_id:request_id.clone(),repository_path:repository.root.to_string_lossy().into_owned(),base_commit:base.clone(),mode:mode.into(),label:"smoke".into(),created_at:"2026-07-22T00:00:00.000Z".into()};
   let result=preserve(&repository,&request,&"a".repeat(64),&preserve_root).unwrap();assert_eq!(source_snapshot(&repository).unwrap(),original);
   let worktree=data_root.join(format!("worker-{index}"));let branch_ref=format!("refs/heads/docks/{request_id}/smoke");provision_worktree(&repository,&worktree,&branch_ref,&request_id,"smoke",&base).unwrap();
   let applied=apply_wip(&repository,&worktree,&branch_ref,&base,&result.receipt,"2026-07-22T00:00:00.000Z").unwrap();assert_eq!(command(&worktree,&["rev-parse","HEAD"]),applied);assert_eq!(fs::read_to_string(worktree.join("tracked.txt")).unwrap(),"changed\n");assert_eq!(fs::read(worktree.join("untracked.bin")).unwrap(),[0,1,2,3]);assert_eq!(command(&worktree,&["rev-parse","HEAD^"]),base);
   command(&repository.root,&["worktree","unlock",worktree.to_str().unwrap()]);command(&repository.root,&["worktree","remove",worktree.to_str().unwrap()]);
  }
  fs::remove_dir_all(root).unwrap();
 }
 #[test]
 fn changed_path_authorization_is_exact_case(){
  let file_claim=PathClaimRequestV1{path:"src/Foo.rs".into(),path_type:"file".into(),mode:"exclusive".into()};
  let directory_claim=PathClaimRequestV1{path:"Assets".into(),path_type:"directory".into(),mode:"exclusive".into()};
  assert!(validate_changed_paths(&[NameStatusChange{status:"M".into(),source:None,destination:"src/Foo.rs".into()}],&[file_claim.clone()]).is_ok());
  assert!(validate_changed_paths(&[NameStatusChange{status:"M".into(),source:None,destination:"src/foo.rs".into()}],&[file_claim]).is_err());
  assert!(validate_changed_paths(&[NameStatusChange{status:"M".into(),source:None,destination:"Assets/logo.svg".into()}],&[directory_claim.clone()]).is_ok());
  assert!(validate_changed_paths(&[NameStatusChange{status:"M".into(),source:None,destination:"assets/logo.svg".into()}],&[directory_claim]).is_err());
 }
 #[test]
 fn artifact_bytes_are_receipt_bound_and_empty_diff_is_supported(){
  let root=std::env::temp_dir().join(format!("session-relay-artifact-{}",crate::store::uuid_v4()));fs::create_dir(&root).unwrap();let repository_root=root.join("repository");fs::create_dir(&repository_root).unwrap();
  command(&repository_root,&["init","-q"]);command(&repository_root,&["config","user.name","Test"]);command(&repository_root,&["config","user.email","test@example.invalid"]);fs::write(repository_root.join("tracked.txt"),"base\n").unwrap();command(&repository_root,&["add","tracked.txt"]);command(&repository_root,&["commit","-qm","base"]);let base=command(&repository_root,&["rev-parse","HEAD"]);
  fs::write(repository_root.join("only-untracked.txt"),"payload\n").unwrap();let repository=OpenedRepository::open(&repository_root).unwrap();let preserve_root=root.join("preserved");let data_root=root.join("data");authority::ensure_private_directory(&data_root,unsafe{libc::geteuid()}).unwrap();
  let make_request=||{let request_id=crate::store::uuid_v4();PreserveRequestV1{request_id,repository_path:repository.root.to_string_lossy().into_owned(),base_commit:base.clone(),mode:"artifact".into(),label:"smoke".into(),created_at:"2026-07-22T00:00:00.000Z".into()}};
  let tampered_request=make_request();let tampered=preserve(&repository,&tampered_request,&"a".repeat(64),&preserve_root).unwrap();
  let WipPayloadV1::Artifact{binary_diff,untracked_inventory,untracked_archive,..}=&tampered.receipt.payload else{panic!("artifact payload")};
  for (index,path) in [binary_diff,untracked_inventory,untracked_archive].into_iter().enumerate(){
   let original=fs::read(path).unwrap();fs::write(path,b"different valid-looking bytes\n").unwrap();let session_id=crate::store::uuid_v4();let tampered_worktree=data_root.join(format!("tampered-{index}"));let tampered_branch=format!("refs/heads/docks/{session_id}/smoke");provision_worktree(&repository,&tampered_worktree,&tampered_branch,&session_id,"smoke",&base).unwrap();
   assert!(apply_wip(&repository,&tampered_worktree,&tampered_branch,&base,&tampered.receipt,"2026-07-22T00:00:00.000Z").is_err());assert_eq!(command(&tampered_worktree,&["rev-parse","HEAD"]),base);assert!(!tampered_worktree.join("only-untracked.txt").exists());
   command(&repository.root,&["worktree","unlock",tampered_worktree.to_str().unwrap()]);command(&repository.root,&["worktree","remove",tampered_worktree.to_str().unwrap()]);fs::write(path,original).unwrap();
  }
  let clean_request=make_request();let clean=preserve(&repository,&clean_request,&"b".repeat(64),&preserve_root).unwrap();let clean_worktree=data_root.join("clean");let clean_branch=format!("refs/heads/docks/{}/smoke",clean_request.request_id);provision_worktree(&repository,&clean_worktree,&clean_branch,&clean_request.request_id,"smoke",&base).unwrap();
  let applied=apply_wip(&repository,&clean_worktree,&clean_branch,&base,&clean.receipt,"2026-07-22T00:00:00.000Z").unwrap();assert_ne!(applied,base);assert_eq!(fs::read_to_string(clean_worktree.join("only-untracked.txt")).unwrap(),"payload\n");
  command(&repository.root,&["worktree","unlock",clean_worktree.to_str().unwrap()]);command(&repository.root,&["worktree","remove",clean_worktree.to_str().unwrap()]);fs::remove_dir_all(root).unwrap();
 }
}
