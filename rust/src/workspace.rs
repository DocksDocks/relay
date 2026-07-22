pub mod authority;
pub mod capability;
pub mod custody;
pub mod git;
pub mod repository_gate;
pub mod platform;
pub mod resources;
pub mod schema;

use authority::{AuthorityRootProvider, AuthorityRoots, BootstrapOutcome, JournalEventV1, SystemAuthorityRootProvider, WorkspaceAuthority, WorkspaceLease};
use capability::{WorkerCapabilityV1, mint_worker};
use crate::sha256;
use schema::{AbsPath, ClosedJcs, JcsValue, LowerUuidV4, Sha256Digest, WorkspaceStartRequestV1, WorkspaceStartResultV1, WorkspaceState, WipReceiptV1};
use std::collections::BTreeMap;
use std::fs::{self,File,OpenOptions};
use std::io::{Read,Write};
use std::os::fd::{FromRawFd,RawFd};
use std::os::unix::fs::{OpenOptionsExt,PermissionsExt};
use std::os::unix::net::{UnixListener,UnixStream};
use std::path::{Path,PathBuf};
use std::process::{Command,Stdio};
use std::time::{Duration,Instant};

pub const WORKSPACE_HELP: &str = "usage:\n  session-relay workspace preserve --request-file <absolute-file> --request-sha256 <sha256>\n  session-relay workspace start --request-file <absolute-file> --request-sha256 <sha256> [--coordinator-capability-file <absolute-file>]\n  session-relay workspace list --repository <canonical-root> --coordinator-capability-file <absolute-file>\n  session-relay workspace inspect <session-id> --repository <canonical-root> --coordinator-capability-file <absolute-file>\n  session-relay workspace handback --request-file <absolute-file> --request-sha256 <sha256> --worker-capability-file <absolute-file>\n  session-relay workspace integrate|recover|finish|abort --request-file <absolute-file> --request-sha256 <sha256> --coordinator-capability-file <absolute-file>";
pub const WORKSPACE_WORKER_POLICY:&str="Work only in the assigned Session Relay workspace. Use the generated Git shim for supported Git mutation, stay within admitted path claims, and use only the projected session resources. Do not reenter wake, attach, watch, shared app-server, integration-checkout, or unmanaged writer paths.";
pub const MANAGED_MUTATION_REFUSAL:&str="mutation is refused for a managed workspace or integration checkout; use session-relay workspace start for a contained writer or continue in read-only mode";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoordinatorMutation { Integrate, Recover, Finish, Abort }
impl CoordinatorMutation {
    pub fn action(self) -> &'static str { match self { Self::Integrate=>"integrate",Self::Recover=>"recover",Self::Finish=>"finish",Self::Abort=>"abort" } }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceCommand {
    Preserve { request_file: PathBuf, request_sha256: String },
    Start { request_file: PathBuf, request_sha256: String, coordinator_capability_file: Option<PathBuf> },
    List { repository: PathBuf, coordinator_capability_file: PathBuf },
    Inspect { session_id: String, repository: PathBuf, coordinator_capability_file: PathBuf },
    Handback { request_file: PathBuf, request_sha256: String, worker_capability_file: PathBuf },
    Coordinator { operation: CoordinatorMutation, request_file: PathBuf, request_sha256: String, coordinator_capability_file: PathBuf },
}

pub fn parse_command(args: &[String]) -> Result<WorkspaceCommand, String> {
    let Some(command) = args.first().map(String::as_str) else { return Err(WORKSPACE_HELP.to_string()); };
    match command {
        "preserve" => {
            let flags=Flags::parse(&args[1..], &["--request-file","--request-sha256"])?;
            Ok(WorkspaceCommand::Preserve{request_file:flags.absolute("--request-file")?,request_sha256:flags.sha("--request-sha256")?})
        }
        "start" => {
            let flags=Flags::parse(&args[1..], &["--request-file","--request-sha256","--coordinator-capability-file"])?;
            Ok(WorkspaceCommand::Start{request_file:flags.absolute("--request-file")?,request_sha256:flags.sha("--request-sha256")?,coordinator_capability_file:flags.optional_absolute("--coordinator-capability-file")?})
        }
        "list" => {
            let flags=Flags::parse(&args[1..], &["--repository","--coordinator-capability-file"])?;
            Ok(WorkspaceCommand::List{repository:flags.absolute("--repository")?,coordinator_capability_file:flags.absolute("--coordinator-capability-file")?})
        }
        "inspect" => {
            let session=args.get(1).ok_or_else(||"workspace inspect requires one session UUID".to_string())?;
            LowerUuidV4::parse(session)?;
            let flags=Flags::parse(&args[2..], &["--repository","--coordinator-capability-file"])?;
            Ok(WorkspaceCommand::Inspect{session_id:session.clone(),repository:flags.absolute("--repository")?,coordinator_capability_file:flags.absolute("--coordinator-capability-file")?})
        }
        "handback" => {
            let flags=Flags::parse(&args[1..], &["--request-file","--request-sha256","--worker-capability-file"])?;
            Ok(WorkspaceCommand::Handback{request_file:flags.absolute("--request-file")?,request_sha256:flags.sha("--request-sha256")?,worker_capability_file:flags.absolute("--worker-capability-file")?})
        }
        "integrate"|"recover"|"finish"|"abort" => {
            let operation=match command{"integrate"=>CoordinatorMutation::Integrate,"recover"=>CoordinatorMutation::Recover,"finish"=>CoordinatorMutation::Finish,_=>CoordinatorMutation::Abort};
            let flags=Flags::parse(&args[1..], &["--request-file","--request-sha256","--coordinator-capability-file"])?;
            Ok(WorkspaceCommand::Coordinator{operation,request_file:flags.absolute("--request-file")?,request_sha256:flags.sha("--request-sha256")?,coordinator_capability_file:flags.absolute("--coordinator-capability-file")?})
        }
        _ => Err(format!("unknown workspace command {command}\n{WORKSPACE_HELP}")),
    }
}

struct Flags(std::collections::BTreeMap<String,String>);
impl Flags {
    fn parse(args:&[String],admitted:&[&str])->Result<Self,String>{
        if args.len()%2!=0{return Err("workspace flags require one value each".to_string())}
        let mut flags=std::collections::BTreeMap::new();
        for pair in args.chunks_exact(2){if !admitted.contains(&pair[0].as_str()){return Err(format!("unknown workspace flag {}",pair[0]))}if pair[1].is_empty()||pair[1].contains('\0'){return Err(format!("workspace flag {} has an invalid value",pair[0]))}if flags.insert(pair[0].clone(),pair[1].clone()).is_some(){return Err(format!("duplicate workspace flag {}",pair[0]))}}
        Ok(Self(flags))
    }
    fn value(&self,key:&str)->Result<&str,String>{self.0.get(key).map(String::as_str).ok_or_else(||format!("missing required workspace flag {key}"))}
    fn absolute(&self,key:&str)->Result<PathBuf,String>{let value=self.value(key)?;AbsPath::parse(value)?;Ok(PathBuf::from(value))}
    fn optional_absolute(&self,key:&str)->Result<Option<PathBuf>,String>{self.0.get(key).map(|v|{AbsPath::parse(v)?;Ok(PathBuf::from(v))}).transpose()}
    fn sha(&self,key:&str)->Result<String,String>{let value=self.value(key)?;Sha256Digest::parse(value)?;Ok(value.to_string())}
}

#[derive(Clone,Debug,Eq,PartialEq)]
struct WorkspaceManifestRecord(JcsValue);
impl ClosedJcs for WorkspaceManifestRecord {
 fn from_jcs(value:JcsValue)->Result<Self,String>{
  let object=match &value{JcsValue::Object(object)=>object,_=>return Err("WorkspaceManifestV1 must be an object".into())};
  let keys=["schema","session_id","repository","integration_root","worktree_identity","worktree_root","branch_ref","base_commit","wip_receipt_path","wip_receipt_sha256","applied_wip_commit","worker_base_commit","task_slug","task","tool","owned_paths","coordinator_owned_paths","resources","state","produced_commits","integration_commits","lease_evidence","custody_evidence","worker_capability_file","retention_evidence","last_error","journal_sequence","journal_head_sha256","created_at","updated_at"];
  if object.len()!=keys.len()||keys.iter().any(|key|!object.contains_key(*key)){return Err("WorkspaceManifestV1 keys differ from the closed schema".into())}
  if object["schema"].as_str()!=Ok(schema::SCHEMA_V1){return Err("WorkspaceManifestV1 schema mismatch".into())}
  let state=WorkspaceState::parse(object["state"].as_str()?)?;
  let early=matches!(state,WorkspaceState::Reserved|WorkspaceState::Provisioning);
  if early != matches!(object["worktree_identity"],JcsValue::Null){return Err("workspace identity nullability differs from state".into())}
  let applied_null=matches!(object["applied_wip_commit"],JcsValue::Null)||matches!(object["worker_base_commit"],JcsValue::Null);
  if early!=applied_null{return Err("applied WIP nullability differs from state".into())}
  Ok(Self(value))
 }
 fn to_jcs(&self)->JcsValue{self.0.clone()}
}

struct CanonicalRecord(JcsValue);
impl ClosedJcs for CanonicalRecord {
 fn from_jcs(value:JcsValue)->Result<Self,String>{Ok(Self(value))}
 fn to_jcs(&self)->JcsValue{self.0.clone()}
}


fn write_private_bytes(path:&Path,bytes:&[u8])->Result<(),String>{
 let mut file=OpenOptions::new().create_new(true).write(true).mode(0o600).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(path).map_err(|e|format!("create {}: {e}",path.display()))?;file.write_all(bytes).and_then(|_|file.sync_all()).map_err(|e|format!("persist {}: {e}",path.display()))?;let parent=path.parent().ok_or_else(||"private record has no parent".to_string())?;let directory=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY).open(parent).map_err(|e|format!("open {} for fsync: {e}",parent.display()))?;directory.sync_all().map_err(|e|format!("fsync {}: {e}",parent.display()))
}
pub struct StartedWorkspace { pub result:WorkspaceStartResultV1,pub lease:WorkspaceLease,pub manifest_file:PathBuf,pub worker_capability_file:PathBuf,pub resources:resources::ResourceSet,pub tool_launch_file:PathBuf,pub tool_launch_sha256:String }

pub fn preserve_workspace(request_file:&Path,request_sha256:&str)->Result<git::PreserveResult,String>{let roots=SystemAuthorityRootProvider.roots()?;preserve_workspace_with_roots(&roots,request_file,request_sha256)}
pub fn preserve_workspace_with_roots(roots:&AuthorityRoots,request_file:&Path,request_sha256:&str)->Result<git::PreserveResult,String>{
 let request:schema::PreserveRequestV1=schema::read_jcs_file(request_file,Some(request_sha256))?;
 let repository=git::OpenedRepository::open(Path::new(&request.repository_path))?;
 repository.validate_oid(&request.base_commit)?;
 let authority=WorkspaceAuthority::new(roots.clone())?;
 let gate=repository_gate::RepositoryGate::acquire(roots,&repository.identity)?;
 gate.admit_workspace_storage(roots,&repository.identity)?;
 let output=git::preserve(&repository,&request,request_sha256,&roots.data.join("preserved"))?;
 drop(gate);drop(authority);
 Ok(output)
}

pub fn target_is_managed(path:&Path)->Result<bool,String>{
 if std::env::var_os("DOCKS_WORKER_CAPABILITY_FILE").is_some(){return Ok(true)}
 if !path.ancestors().any(|ancestor|fs::symlink_metadata(ancestor.join(".git")).is_ok()){return Ok(false)}
 let repository=match git::OpenedRepository::open(path){
  Ok(repository)=>repository,
  Err(error) if error.contains("not a git repository")||error.contains("not a repository")=>return Ok(false),
  Err(error)=>return Err(format!("cannot prove mutation target is outside managed mode: {error}")),
 };
 let common=Path::new(&repository.identity.common_dir_realpath);
 if common.join("docks/workspace-admission-v1.json").exists(){return Ok(true)}
 let roots=SystemAuthorityRootProvider.roots()?;
 Ok(roots.authority.join("repositories").join(&repository.identity.repository_id).exists())
}

pub fn refuse_unsupported_managed_mutation(path:&Path,read_only:bool,entrypoint:&str)->Result<(),String>{
 if read_only{return Ok(())}
 if target_is_managed(path)?{return Err(format!("{entrypoint} {MANAGED_MUTATION_REFUSAL}"))}
 Ok(())
}

pub fn start_workspace(request_file:&Path,request_sha256:&str,coordinator_capability_file:Option<&Path>)->Result<StartedWorkspace,String>{
 let roots=SystemAuthorityRootProvider.roots()?;let executable=std::env::current_exe().map_err(|e|format!("resolve relay executable: {e}"))?;let digest=resources::executable_sha256(&executable)?;
 start_workspace_with_roots_and_verified_executable(&roots,request_file,request_sha256,coordinator_capability_file,&executable,&digest)
}
pub fn start_workspace_with_roots(roots:&AuthorityRoots,request_file:&Path,request_sha256:&str,coordinator_capability_file:Option<&Path>)->Result<StartedWorkspace,String>{
 let executable=std::env::current_exe().map_err(|e|format!("resolve relay executable: {e}"))?;let digest=resources::executable_sha256(&executable)?;
 start_workspace_with_roots_and_verified_executable(roots,request_file,request_sha256,coordinator_capability_file,&executable,&digest)
}
pub fn start_workspace_with_roots_and_executable(roots:&AuthorityRoots,request_file:&Path,request_sha256:&str,coordinator_capability_file:Option<&Path>,relay_executable:&Path)->Result<StartedWorkspace,String>{
 let digest=resources::executable_sha256(relay_executable)?;
 start_workspace_with_roots_and_verified_executable(roots,request_file,request_sha256,coordinator_capability_file,relay_executable,&digest)
}
pub fn start_workspace_with_roots_and_verified_executable(roots:&AuthorityRoots,request_file:&Path,request_sha256:&str,coordinator_capability_file:Option<&Path>,relay_executable:&Path,relay_executable_sha256:&str)->Result<StartedWorkspace,String>{
 if !relay_executable.is_absolute()||fs::canonicalize(relay_executable).map_err(|e|format!("canonicalize relay executable: {e}"))?!=relay_executable{return Err("relay executable must be an absolute canonical path".into())}
 resources::verify_executable(relay_executable,relay_executable_sha256)?;
 let mut request:WorkspaceStartRequestV1=schema::read_jcs_file(request_file,Some(request_sha256))?;
 let wip:WipReceiptV1=schema::read_jcs_file(Path::new(&request.wip_receipt_path),Some(&request.wip_receipt_sha256))?;
 if wip.request_sha256.is_empty()||wip.base_commit!=request.base_commit{return Err("start request and WIP receipt base/provenance differ".into())}
 let repository=git::OpenedRepository::open(Path::new(&request.repository_path))?;
 let integration=git::OpenedRepository::open(Path::new(&request.integration_root))?;
 if repository.identity!=integration.identity||wip.repository!=repository.identity{return Err("start repository, integration root, and WIP identity differ".into())}
 repository.validate_oid(&request.base_commit)?;
 let authority=WorkspaceAuthority::new(roots.clone())?;
 let now=authority::now_timestamp()?;
 let (worktree_identity,manifest_file,capability_path,bootstrap)={
  let gate=repository_gate::RepositoryGate::acquire(roots,&repository.identity)?;gate.admit_workspace_storage(roots,&repository.identity)?;
  let fanout=crate::fanout::FanoutStore::new(crate::store::home_dir());
  let repository_dir=authority.repository_dir(&repository.identity.repository_id)?;
  let (capability_path,bootstrap)=if repository_dir.exists(){
   let path=coordinator_capability_file.ok_or_else(||{let current=authority.read_repository(&repository.identity.repository_id).ok().and_then(|record|record.current_generation.parse::<u64>().ok()).unwrap_or(1);format!("workspace authority exists; retry with --coordinator-capability-file {}",authority.capability_path(&repository.identity.repository_id,current).map(|p|p.display().to_string()).unwrap_or_default())})?;
   let (_,_capability)=authority.authenticate(&repository.identity.repository_id,path,"start")?;(path.to_path_buf(),BootstrapOutcome::Existing)
  }else{
   if coordinator_capability_file.is_some(){return Err("first workspace start must omit --coordinator-capability-file".into())}
   if !request.coordinator_owned_paths.is_empty()||!request.coordinator_owned_overrides.is_empty(){return Err("first workspace start cannot claim or override coordinator-owned paths".into())}
   if fanout.has_nonterminal_repository(&repository.identity)?{return Err("active legacy fanout authority prevents managed workspace bootstrap".into())}
   let created=authority.bootstrap_coordinator(&repository.identity,&now)?;(created.capability_file,created.bootstrap)
  };
  gate.publish_workspace_marker(&roots,&repository.identity,env!("CARGO_PKG_VERSION"),&now)?;
  let repository_dir=authority.repository_dir(&repository.identity.repository_id)?;
  let sessions=repository_dir.join("sessions");let session_dir=sessions.join(&request.request_id);
  if session_dir.exists(){return Err("workspace session already exists; use inspect or explicit recovery".into())}
  reject_overlapping_session_claims(&sessions,&request)?;
  fs::create_dir(&session_dir).map_err(|e|format!("create workspace session: {e}"))?;fs::set_permissions(&session_dir,fs::Permissions::from_mode(0o700)).map_err(|e|format!("chmod workspace session: {e}"))?;for name in ["journal","worker-capabilities","broker-replays","resources"]{fs::create_dir(session_dir.join(name)).map_err(|e|format!("create workspace {name}: {e}"))?;fs::set_permissions(session_dir.join(name),fs::Permissions::from_mode(0o700)).map_err(|e|format!("chmod workspace {name}: {e}"))?}
  let worktree_root=roots.data.join(&repository.identity.repository_id).join(format!("{}-{}",&request.request_id,&request.task_slug));authority::ensure_private_directory(worktree_root.parent().unwrap(),roots.euid)?;
  let branch_ref=format!("refs/heads/docks/{}/{}",&request.request_id,&request.task_slug);
  let manifest=manifest_for(&request,&repository,&worktree_root,&branch_ref,WorkspaceState::Reserved,None,None,None,None,&now,"0",None);
  let manifest_file=session_dir.join("manifest-v1.json");authority::atomic_create_jcs(&manifest_file,&manifest,0o600)?;
  let identity=git::provision_worktree(&repository,&worktree_root,&branch_ref,&request.request_id,&request.task_slug,&request.base_commit)?;
  let mut provisioning=manifest_for(&request,&repository,&worktree_root,&branch_ref,WorkspaceState::Provisioning,Some(&identity),None,None,None,&now,"1",None);
  // Provisioning deliberately retains null identity in its record until the lifetime lease is acquired.
  if let JcsValue::Object(object)=&mut provisioning.0{object.insert("worktree_identity".into(),JcsValue::Null);}
  authority::atomic_replace_jcs(&manifest_file,&provisioning,0o600)?;
  (identity,manifest_file,capability_path,bootstrap)
 };
 let repository_dir=authority.repository_dir(&repository.identity.repository_id)?;let leases=repository_dir.join("worktree-leases");authority::ensure_private_directory(&leases,roots.euid)?;
 let lease_path=leases.join(format!("{}.lock",worktree_identity.identity_sha256));let owner_path=leases.join(format!("{}.owner.json",worktree_identity.identity_sha256));let lease=WorkspaceLease::acquire_owned(&lease_path,&owner_path,&request.request_id,&now)?;
 let branch_ref=format!("refs/heads/docks/{}/{}",&request.request_id,&request.task_slug);let worktree_root=PathBuf::from(&worktree_identity.root_realpath);
 let gate=repository_gate::RepositoryGate::acquire(roots,&repository.identity)?;
 let applied=git::apply_wip(&repository,&worktree_root,&branch_ref,&request.base_commit,&wip,&request.created_at)?;
 let session_dir=manifest_file.parent().unwrap();
 request.coordinator_owned_paths=authority::resolve_path_policy(&worktree_root,&request.owned_paths,&request.coordinator_owned_paths,&request.coordinator_owned_overrides,matches!(bootstrap,BootstrapOutcome::Existing))?;
 let broker_socket=git::actual_private_git_dir(&worktree_root)?.join("session-relay/broker-v1.sock");let capability_file=git::actual_private_git_dir(&worktree_root)?.join("session-relay/worker-capabilities/00000000000000000001.json");
 authority::ensure_private_directory(capability_file.parent().unwrap(),roots.euid)?;let (worker_capability,record)=mint_worker(&repository.identity.repository_id,&request.request_id,1,&broker_socket,&now,"9999-12-31T23:59:59.999Z")?;
 authority::atomic_create_jcs(&capability_file,&worker_capability,0o600)?;authority::atomic_create_jcs(&session_dir.join("worker-capability-record-v1.json"),&record,0o600)?;
 let git_shim=create_git_shim(&worktree_root,&capability_file,relay_executable)?;
 let event=JournalEventV1{sequence:1,previous_sha256:None,kind:"WipApplied".into(),payload:JcsValue::Object(BTreeMap::from([("applied_wip_commit".into(),JcsValue::String(applied.clone()))])),created_at:now.clone()};let head=authority::append_journal(session_dir,&event,1,None)?;
 let manifest=manifest_for(&request,&repository,&worktree_root,&branch_ref,WorkspaceState::LeaseHeld,Some(&worktree_identity),Some(&applied),Some(&capability_file),Some(&lease_path),&now,"1",Some(&head));authority::atomic_replace_jcs(&manifest_file,&manifest,0o600)?;
 resources::preflight_tool_launch(&request.tool)?;
 let allocated=resources::allocate_resources(roots,&repository.identity.repository_id,&request.request_id,&request.resources,&session_dir.join("resources"),&now)?;
 let prepared=resources::prepare_tool_launch(&request.tool,&request.request_id,&worktree_root,&request.task,WORKSPACE_WORKER_POLICY,git_shim.parent().ok_or_else(||"Git shim has no parent".to_string())?,&capability_file,&allocated)?;
 let tool_launch_file=session_dir.join("tool-launch-v1.json");let (_tool_launch,tool_launch_sha256)=resources::persist_tool_launch_decision(&tool_launch_file,&request.request_id,&request.tool,&prepared,&now)?;
 let resource_digests=allocated.allocations.iter().flat_map(|allocation|[JcsValue::String(allocation.create_receipt_sha256.clone()),JcsValue::String(allocation.inspect_receipt_sha256.clone())]).collect();
 let allocated_event=JournalEventV1{sequence:2,previous_sha256:Some(head.clone()),kind:"ResourcesAllocated".into(),payload:JcsValue::Object(BTreeMap::from([("resource_receipt_sha256".into(),JcsValue::Array(resource_digests)),("tool_launch_sha256".into(),JcsValue::String(tool_launch_sha256.clone()))])),created_at:now.clone()};let ready_head=authority::append_journal(session_dir,&allocated_event,2,Some(&head))?;
 let ready=manifest_with_resource_allocations(manifest_for(&request,&repository,&worktree_root,&branch_ref,WorkspaceState::Ready,Some(&worktree_identity),Some(&applied),Some(&capability_file),Some(&lease_path),&now,"2",Some(&ready_head)),&allocated.allocations);authority::atomic_replace_jcs(&manifest_file,&ready,0o600)?;
 start_git_broker(roots,relay_executable,session_dir,&worktree_root,&branch_ref,&capability_file,&lease,&allocated.resource_fds)?;
 drop(gate);
 let authority_record=authority.read_repository(&repository.identity.repository_id)?;let result=WorkspaceStartResultV1{session_id:request.request_id.clone(),repository_id:repository.identity.repository_id.clone(),worktree_root:worktree_root.to_string_lossy().into_owned(),branch_ref,coordinator_capability_file:capability_path.to_string_lossy().into_owned(),coordinator_generation:authority_record.current_generation,bootstrap:match bootstrap{BootstrapOutcome::Created=>"created",BootstrapOutcome::Existing=>"existing"}.into()};
 Ok(StartedWorkspace{result,lease,manifest_file,worker_capability_file:capability_file,resources:allocated,tool_launch_file,tool_launch_sha256})
}

fn manifest_for(request:&WorkspaceStartRequestV1,repository:&git::OpenedRepository,worktree_root:&Path,branch_ref:&str,state:WorkspaceState,identity:Option<&schema::WorktreeIdentityV1>,applied:Option<&str>,worker_capability:Option<&Path>,lease_path:Option<&Path>,now:&str,journal_sequence:&str,journal_head:Option<&str>)->WorkspaceManifestRecord{
 let start=request.to_jcs().object().expect("start request object");
 let value=JcsValue::Object(BTreeMap::from([
  ("applied_wip_commit".into(),applied.map(|v|JcsValue::String(v.into())).unwrap_or(JcsValue::Null)),("base_commit".into(),JcsValue::String(request.base_commit.clone())),("branch_ref".into(),JcsValue::String(branch_ref.into())),("coordinator_owned_paths".into(),start["coordinator_owned_paths"].clone()),("created_at".into(),JcsValue::String(request.created_at.clone())),("custody_evidence".into(),JcsValue::Null),("integration_commits".into(),JcsValue::Array(Vec::new())),("integration_root".into(),JcsValue::String(request.integration_root.clone())),("journal_head_sha256".into(),journal_head.map(|v|JcsValue::String(v.into())).unwrap_or(JcsValue::Null)),("journal_sequence".into(),JcsValue::String(journal_sequence.into())),("last_error".into(),JcsValue::Null),("lease_evidence".into(),lease_path.map(|v|JcsValue::String(v.to_string_lossy().into_owned())).unwrap_or(JcsValue::Null)),("owned_paths".into(),start["owned_paths"].clone()),("produced_commits".into(),applied.map(|oid|JcsValue::Array(vec![JcsValue::Object(BTreeMap::from([("oid".into(),JcsValue::String(oid.into())),("parent_oid".into(),JcsValue::String(request.base_commit.clone())),("source".into(),JcsValue::String("applied_wip".into()))]))])).unwrap_or(JcsValue::Array(Vec::new()))),("repository".into(),repository.identity.to_jcs()),("resources".into(),start["resources"].clone()),("retention_evidence".into(),JcsValue::Null),("schema".into(),JcsValue::String(schema::SCHEMA_V1.into())),("session_id".into(),JcsValue::String(request.request_id.clone())),("state".into(),JcsValue::String(state.as_str().into())),("task".into(),JcsValue::String(request.task.clone())),("task_slug".into(),JcsValue::String(request.task_slug.clone())),("tool".into(),start["tool"].clone()),("updated_at".into(),JcsValue::String(now.into())),("wip_receipt_path".into(),JcsValue::String(request.wip_receipt_path.clone())),("wip_receipt_sha256".into(),JcsValue::String(request.wip_receipt_sha256.clone())),("worker_base_commit".into(),applied.map(|v|JcsValue::String(v.into())).unwrap_or(JcsValue::Null)),("worker_capability_file".into(),worker_capability.map(|v|JcsValue::String(v.to_string_lossy().into_owned())).unwrap_or(JcsValue::Null)),("worktree_identity".into(),identity.map(schema::WorktreeIdentityV1::value).unwrap_or(JcsValue::Null)),("worktree_root".into(),JcsValue::String(worktree_root.to_string_lossy().into_owned())),
 ]));WorkspaceManifestRecord(value)
}

fn manifest_with_resource_allocations(mut manifest:WorkspaceManifestRecord,allocations:&[schema::ResourceAllocationV1])->WorkspaceManifestRecord{
 if let JcsValue::Object(object)=&mut manifest.0{object.insert("resources".into(),JcsValue::Array(allocations.iter().map(ClosedJcs::to_jcs).collect()));}
 manifest
}

fn reject_overlapping_session_claims(sessions:&Path,request:&WorkspaceStartRequestV1)->Result<(),String>{
 if !sessions.exists(){return Ok(())}let requested=request.owned_paths.iter().map(|claim|claim.path.to_ascii_lowercase()).collect::<Vec<_>>();
 for entry in fs::read_dir(sessions).map_err(|e|format!("read workspace sessions: {e}"))?{let path=entry.map_err(|e|format!("read workspace session entry: {e}"))?.path().join("manifest-v1.json");let manifest:WorkspaceManifestRecord=schema::read_jcs_file(&path,None)?;let object=manifest.0.object()?;if object["state"].as_str()?=="Closed"{continue}let JcsValue::Array(existing)=&object["owned_paths"]else{return Err("manifest owned_paths is not an array".into())};for value in existing{let claim=value.clone().object()?;let old=claim["path"].as_str()?.to_ascii_lowercase();for new in &requested{if new==&old||new.strip_prefix(&format!("{old}/")).is_some()||old.strip_prefix(&format!("{new}/")).is_some(){return Err(format!("path claim {new} overlaps live session claim {old}"))}}}}
 Ok(())
}

fn create_git_shim(worktree:&Path,capability_file:&Path,relay_executable:&Path)->Result<PathBuf,String>{
 let private=git::actual_private_git_dir(worktree)?;let bin=private.join("session-relay/bin");authority::ensure_private_directory(&bin,unsafe{libc::geteuid()})?;let shim=bin.join("git");let quote=|value:&Path|value.to_string_lossy().replace('\'',"'\"'\"'");let body=format!("#!/bin/sh\nexec '{}' workspace __broker-client --worker-capability-file '{}' -- \"$@\"\n",quote(relay_executable),quote(capability_file));write_private_bytes(&shim,body.as_bytes())?;fs::set_permissions(&shim,fs::Permissions::from_mode(0o500)).map_err(|e|format!("chmod Git shim: {e}"))?;Ok(shim)
}

pub fn list_workspaces(repository_path:&Path,capability_file:&Path)->Result<JcsValue,String>{
 let repository=git::OpenedRepository::open(repository_path)?;let authority=WorkspaceAuthority::system()?;authority.authenticate(&repository.identity.repository_id,capability_file,"list")?;let sessions=authority.repository_dir(&repository.identity.repository_id)?.join("sessions");let mut values=Vec::new();
 if sessions.exists(){for entry in fs::read_dir(sessions).map_err(|e|format!("read workspace sessions: {e}"))?{let path=entry.map_err(|e|format!("read workspace session: {e}"))?.path().join("manifest-v1.json");let record:WorkspaceManifestRecord=schema::read_jcs_file(&path,None)?;values.push(record.0)}}
 values.sort_by(|left,right|manifest_session(left).cmp(&manifest_session(right)));Ok(JcsValue::Object(BTreeMap::from([("custody".into(),JcsValue::String("unproven".into())),("schema".into(),JcsValue::String(schema::SCHEMA_V1.into())),("workspaces".into(),JcsValue::Array(values))])))
}
fn manifest_session(value:&JcsValue)->String{match value{JcsValue::Object(object)=>object.get("session_id").and_then(|value|value.as_str().ok()).unwrap_or("").to_string(),_=>String::new()}}
pub fn inspect_workspace(session_id:&str,repository_path:&Path,capability_file:&Path)->Result<JcsValue,String>{
 LowerUuidV4::parse(session_id)?;let repository=git::OpenedRepository::open(repository_path)?;let authority=WorkspaceAuthority::system()?;authority.authenticate(&repository.identity.repository_id,capability_file,"inspect")?;let path=authority.repository_dir(&repository.identity.repository_id)?.join("sessions").join(session_id).join("manifest-v1.json");let manifest:WorkspaceManifestRecord=schema::read_jcs_file(&path,None)?;
 Ok(JcsValue::Object(BTreeMap::from([("custody".into(),JcsValue::String("unproven".into())),("schema".into(),JcsValue::String(schema::SCHEMA_V1.into())),("workspace".into(),manifest.0)])))
}

pub fn handback_workspace(request_file:&Path,request_sha256:&str,worker_capability_file:&Path)->Result<JcsValue,String>{
 let request_bytes=capability::read_secure_bytes(request_file).map_err(|e|format!("read handback request: {e}"))?;schema::Sha256Digest::parse(request_sha256)?;if !sha256::constant_time_eq(sha256::hex_digest(&request_bytes).as_bytes(),request_sha256.as_bytes()){return Err("handback request SHA-256 mismatch".into())}
 let request=schema::parse_jcs(&request_bytes,true)?;let object=closed_object(&request,&["schema","request_id","session_id","expected_head","created_at"],"HandbackRequestV1")?;if object["schema"].as_str()!=Ok(schema::SCHEMA_V1){return Err("HandbackRequestV1 schema mismatch".into())}LowerUuidV4::parse(object["request_id"].as_str()?)?;LowerUuidV4::parse(object["session_id"].as_str()?)?;schema::Timestamp::parse(object["created_at"].as_str()?)?;
 let capability:WorkerCapabilityV1=schema::read_jcs_file(worker_capability_file,None)?;if capability.session_id!=object["session_id"].as_str()?{return Err("handback capability session differs from request".into())}
 broker_exchange(&capability,"handback",vec![request_file.to_string_lossy().into_owned(),request_sha256.into()],std::env::current_dir().map_err(|e|format!("resolve handback cwd: {e}"))?)
}

fn closed_object<'a>(value:&'a JcsValue,keys:&[&str],name:&str)->Result<&'a BTreeMap<String,JcsValue>,String>{let JcsValue::Object(object)=value else{return Err(format!("{name} must be an object"))};if object.len()!=keys.len()||keys.iter().any(|key|!object.contains_key(*key)){return Err(format!("{name} keys differ from the closed schema"))}Ok(object)}

fn broker_exchange(capability:&WorkerCapabilityV1,operation:&str,argv:Vec<String>,cwd:PathBuf)->Result<JcsValue,String>{
 let request_id=crate::store::uuid_v4();let cwd=cwd.to_str().ok_or_else(||"broker cwd is not UTF-8".to_string())?.to_string();
 let bare=JcsValue::Object(BTreeMap::from([("argv".into(),JcsValue::Array(argv.iter().cloned().map(JcsValue::String).collect())),("capability_id".into(),JcsValue::String(capability.capability_id.clone())),("cwd".into(),JcsValue::String(cwd.clone())),("generation".into(),JcsValue::String(capability.generation.clone())),("operation".into(),JcsValue::String(operation.into())),("request_id".into(),JcsValue::String(request_id.clone())),("schema".into(),JcsValue::String(schema::SCHEMA_V1.into())),("session_id".into(),JcsValue::String(capability.session_id.clone()))]));
 let request_sha256=sha256::hex_digest([b"session-relay/broker-request/v1\0".as_slice(),schema::serialize_jcs(&bare).as_bytes()].concat().as_slice());
 let mut request=bare.object()?;request.insert("request_sha256".into(),JcsValue::String(request_sha256));let request=JcsValue::Object(request);
 let nonce=capability::encode_base64url(&capability::random_secret()?);let secret=capability::decode_base64url(&capability.secret_b64url)?;let message=[b"session-relay/broker-envelope/v1\0".as_slice(),schema::serialize_jcs(&request).as_bytes(),nonce.as_bytes()].concat();let mac=capability::encode_base64url(&sha256::hmac(&secret,&message));
 let envelope=JcsValue::Object(BTreeMap::from([("mac".into(),JcsValue::String(mac)),("nonce".into(),JcsValue::String(nonce)),("request".into(),request),("schema".into(),JcsValue::String(schema::SCHEMA_V1.into()))]));let mut bytes=schema::serialize_jcs(&envelope).into_bytes();bytes.push(b'\n');
 let mut stream=UnixStream::connect(&capability.broker_socket).map_err(|e|format!("connect Git broker {}: {e}",capability.broker_socket))?;stream.write_all(&bytes).map_err(|e|format!("write Git broker request: {e}"))?;stream.shutdown(std::net::Shutdown::Write).map_err(|e|format!("finish Git broker request: {e}"))?;let mut response=Vec::new();stream.take(1024*1024).read_to_end(&mut response).map_err(|e|format!("read Git broker response: {e}"))?;let value=schema::parse_jcs(&response,true)?;let object=closed_object(&value,&["schema","request_id","status","exit_code","stdout","stderr","receipt"],"GitBrokerResponseV1")?;if object["request_id"].as_str()?!=request_id{return Err("Git broker response request ID mismatch".into())}if object["status"].as_str()?=="error"{return Err(object["stderr"].as_str()?.to_string())}Ok(value)
}
pub fn execute(command: WorkspaceCommand) -> Result<String, String> {
    let value=match command {
        WorkspaceCommand::Preserve { request_file,request_sha256 } => {
            let result=preserve_workspace(&request_file,&request_sha256)?;
            JcsValue::Object(BTreeMap::from([
                ("receipt_file".into(),JcsValue::String(result.receipt_file.to_string_lossy().into_owned())),
                ("receipt_sha256".into(),JcsValue::String(result.receipt_sha256)),
                ("schema".into(),JcsValue::String(schema::SCHEMA_V1.into())),
            ]))
        }
        WorkspaceCommand::Start { request_file,request_sha256,coordinator_capability_file } => {
            let started=start_workspace(&request_file,&request_sha256,coordinator_capability_file.as_deref())?;
            started.result.to_jcs()
        }
        WorkspaceCommand::List { repository,coordinator_capability_file } => list_workspaces(&repository,&coordinator_capability_file)?,
        WorkspaceCommand::Inspect { session_id,repository,coordinator_capability_file } => inspect_workspace(&session_id,&repository,&coordinator_capability_file)?,
        WorkspaceCommand::Handback { request_file,request_sha256,worker_capability_file } => handback_workspace(&request_file,&request_sha256,&worker_capability_file)?,
        WorkspaceCommand::Coordinator { operation, .. } => return Err(format!("workspace {} requires the S6 lifecycle coordinator and refuses without it",operation.action())),
    };
    Ok(format!("{}\n",schema::serialize_jcs(&value)))
}

fn start_git_broker(roots:&AuthorityRoots,relay_executable:&Path,session_dir:&Path,worktree:&Path,branch_ref:&str,capability_file:&Path,lease:&WorkspaceLease,resource_fds:&[RawFd])->Result<(),String>{
 let authority_root=roots.authority.to_str().ok_or_else(||"authority root is not UTF-8".to_string())?;
 let data_root=roots.data.to_str().ok_or_else(||"data root is not UTF-8".to_string())?;
 let session_dir_arg=session_dir.to_str().ok_or_else(||"session dir is not UTF-8".to_string())?;
 let worktree_arg=worktree.to_str().ok_or_else(||"worktree is not UTF-8".to_string())?;
 let capability_arg=capability_file.to_str().ok_or_else(||"capability path is not UTF-8".to_string())?;
 let capability:WorkerCapabilityV1=schema::read_jcs_file(capability_file,None)?;
 let resource_fd_arg=resource_fds.iter().map(ToString::to_string).collect::<Vec<_>>().join(",");
 let lease_fd=lease.as_raw_fd();
 let mut inherited:Vec<(RawFd,libc::c_int)>=Vec::with_capacity(resource_fds.len()+1);
 for fd in std::iter::once(lease_fd).chain(resource_fds.iter().copied()){
  let old=unsafe{libc::fcntl(fd,libc::F_GETFD)};
  if old<0{for (set_fd,flags) in inherited.iter().copied(){unsafe{libc::fcntl(set_fd,libc::F_SETFD,flags);}}return Err(format!("inspect inherited descriptor {fd} flags: {}",std::io::Error::last_os_error()))}
  if unsafe{libc::fcntl(fd,libc::F_SETFD,old&!libc::FD_CLOEXEC)}<0{for (set_fd,flags) in inherited.iter().copied(){unsafe{libc::fcntl(set_fd,libc::F_SETFD,flags);}}return Err(format!("make descriptor {fd} inheritable for broker: {}",std::io::Error::last_os_error()))}
  inherited.push((fd,old));
 }
 let lease_fd_arg=lease_fd.to_string();
 let spawned=Command::new(relay_executable).args(["workspace","__broker","--authority-root",authority_root,"--data-root",data_root,"--session-dir",session_dir_arg,"--worktree",worktree_arg,"--branch-ref",branch_ref,"--worker-capability-file",capability_arg,"--lease-fd",&lease_fd_arg,"--resource-fds",&resource_fd_arg]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
 let mut restore_error=None;
 for (fd,flags) in inherited{if unsafe{libc::fcntl(fd,libc::F_SETFD,flags)}<0&&restore_error.is_none(){restore_error=Some(format!("restore descriptor {fd} flags after broker spawn: {}",std::io::Error::last_os_error()))}}
 let mut child=spawned.map_err(|error|format!("spawn Git broker: {error}"))?;
 if let Some(error)=restore_error{let _=child.kill();let _=child.wait();return Err(error)}
 let deadline=Instant::now()+Duration::from_secs(3);
 while Instant::now()<deadline{if UnixStream::connect(&capability.broker_socket).is_ok(){return Ok(())}if child.try_wait().map_err(|error|format!("inspect Git broker: {error}"))?.is_some(){return Err("Git broker exited before publishing its socket".into())}std::thread::sleep(Duration::from_millis(10))}
 let _=child.kill();let _=child.wait();Err("Git broker did not publish its socket within three seconds".into())
}
fn run_broker(raw:&[String])->Result<(),String>{
 let flags=Flags::parse(raw,&["--authority-root","--data-root","--session-dir","--worktree","--branch-ref","--worker-capability-file","--lease-fd","--resource-fds"])?;let roots=AuthorityRoots{authority:flags.absolute("--authority-root")?,data:flags.absolute("--data-root")?,euid:unsafe{libc::geteuid()}};let session_dir=flags.absolute("--session-dir")?;let worktree=flags.absolute("--worktree")?;let branch_ref=flags.value("--branch-ref")?.to_string();if !branch_ref.starts_with("refs/heads/docks/"){return Err("broker branch ref is invalid".into())}let capability_file=flags.absolute("--worker-capability-file")?;let lease_fd:i32=flags.value("--lease-fd")?.parse().map_err(|_|"broker lease fd is not decimal".to_string())?;
 let resource_fds=if flags.value("--resource-fds")?.is_empty(){Vec::new()}else{flags.value("--resource-fds")?.split(',').map(|value|value.parse::<RawFd>().map_err(|_|"broker resource FD list is not canonical decimal".to_string())).collect::<Result<Vec<_>,_>>()?};
 if resource_fds.iter().any(|fd|*fd==lease_fd){return Err("broker lease FD collides with a held resource FD".into())}
 resources::validate_held_resource_fds(&session_dir.join("resources"),&resource_fds)?;
 let _lease=unsafe{File::from_raw_fd(lease_fd)};let _held_resources=resource_fds.into_iter().map(|fd|unsafe{File::from_raw_fd(fd)}).collect::<Vec<_>>();
 let capability:WorkerCapabilityV1=schema::read_jcs_file(&capability_file,None)?;let record:schema::CapabilityRecordV1=schema::read_jcs_file(&session_dir.join("worker-capability-record-v1.json"),None)?;let socket=PathBuf::from(&capability.broker_socket);if socket.exists(){return Err("broker socket already exists; refusing replacement".into())}let listener=UnixListener::bind(&socket).map_err(|e|format!("bind Git broker {}: {e}",socket.display()))?;fs::set_permissions(&socket,fs::Permissions::from_mode(0o600)).map_err(|e|format!("chmod Git broker socket: {e}"))?;
 for connection in listener.incoming(){let mut stream=match connection{Ok(stream)=>stream,Err(error)=>return Err(format!("accept Git broker connection: {error}"))};let response=match read_broker_request(&mut stream).and_then(|envelope|handle_broker_request(&roots,&session_dir,&worktree,&branch_ref,&capability,&record,envelope)){Ok(response)=>response,Err(error)=>broker_response("00000000-0000-4000-8000-000000000000","error",1,"",&error,JcsValue::Null)};let mut bytes=schema::serialize_jcs(&response).into_bytes();bytes.push(b'\n');stream.write_all(&bytes).map_err(|e|format!("write Git broker response: {e}"))?;}
 Ok(())
}

fn read_broker_request(stream:&mut UnixStream)->Result<JcsValue,String>{let mut bytes=Vec::new();stream.take(1024*1024).read_to_end(&mut bytes).map_err(|e|format!("read Git broker request: {e}"))?;schema::parse_jcs(&bytes,true)}
fn handle_broker_request(roots:&AuthorityRoots,session_dir:&Path,worktree:&Path,branch_ref:&str,capability:&WorkerCapabilityV1,record:&schema::CapabilityRecordV1,envelope:JcsValue)->Result<JcsValue,String>{
 let envelope=closed_object(&envelope,&["schema","request","nonce","mac"],"GitBrokerEnvelopeV1")?;let request=envelope["request"].clone();let object=closed_object(&request,&["schema","request_id","session_id","generation","operation","argv","cwd","capability_id","request_sha256"],"GitBrokerRequestV1")?;let request_id=object["request_id"].as_str()?.to_string();LowerUuidV4::parse(&request_id)?;if object["session_id"].as_str()?!=capability.session_id||object["capability_id"].as_str()?!=capability.capability_id{return Err("broker request capability identity mismatch".into())}let operation=object["operation"].as_str()?;if !matches!(operation,"git_index"|"git_commit"|"handback"){return Err("broker operation is outside the closed set".into())}
 let mut bare=object.clone();let supplied_digest=bare.remove("request_sha256").unwrap().as_str()?.to_string();let bare=JcsValue::Object(bare);let expected_digest=sha256::hex_digest([b"session-relay/broker-request/v1\0".as_slice(),schema::serialize_jcs(&bare).as_bytes()].concat().as_slice());if !sha256::constant_time_eq(supplied_digest.as_bytes(),expected_digest.as_bytes()){return Err("broker request digest mismatch".into())}
 let nonce=envelope["nonce"].as_str()?;let mac=envelope["mac"].as_str()?;let secret=capability::decode_base64url(&capability.secret_b64url)?;let message=[b"session-relay/broker-envelope/v1\0".as_slice(),schema::serialize_jcs(&request).as_bytes(),nonce.as_bytes()].concat();let expected_mac=capability::encode_base64url(&sha256::hmac(&secret,&message));if !sha256::constant_time_eq(mac.as_bytes(),expected_mac.as_bytes()){return Err("broker envelope MAC mismatch".into())}
 capability::authenticate_worker(capability,record,&capability.repository_id,&capability.session_id,capability.generation.parse().map_err(|_|"broker generation overflow".to_string())?,operation,&authority::now_timestamp()?)?;
 let replay=session_dir.join("broker-replays").join(format!("{request_id}.json"));let replay_digest=session_dir.join("broker-replays").join(format!("{request_id}.sha256"));if replay.exists(){let existing=capability::read_secure_bytes(&replay_digest)?;if existing!=format!("{supplied_digest}\n").as_bytes(){return Err("changed broker request replay is refused".into())}return schema::parse_jcs(&capability::read_secure_bytes(&replay)?,true)}
 let argv=match &object["argv"]{JcsValue::Array(values)=>values.iter().map(|value|value.as_str().map(str::to_string)).collect::<Result<Vec<_>,_>>()?,_=>return Err("broker argv must be an array".into())};let cwd=PathBuf::from(object["cwd"].as_str()?);if fs::canonicalize(&cwd).map_err(|e|format!("canonicalize broker cwd: {e}"))?!=worktree{return Err("broker cwd differs from the exact workspace root".into())}
 let repository=git::OpenedRepository::open(worktree)?;let gate=repository_gate::RepositoryGate::acquire(roots,&repository.identity)?;let response=match operation{"git_index"=>broker_git_index(&repository,worktree,branch_ref,session_dir,&request_id,&argv),"git_commit"=>broker_git_commit(&repository,worktree,branch_ref,session_dir,&request_id,&argv),"handback"=>broker_handback(&repository,worktree,branch_ref,session_dir,&request_id,&argv),_=>unreachable!()}?;drop(gate);
 let response_record=CanonicalRecord(response.clone());authority::atomic_create_jcs(&replay,&response_record,0o600)?;write_private_bytes(&replay_digest,format!("{supplied_digest}\n").as_bytes())?;Ok(response)
}

fn manifest_claims(session_dir:&Path)->Result<(WorkspaceManifestRecord,Vec<schema::PathClaimRequestV1>),String>{let path=session_dir.join("manifest-v1.json");let manifest:WorkspaceManifestRecord=schema::read_jcs_file(&path,None)?;let object=match &manifest.0{JcsValue::Object(object)=>object,_=>unreachable!()};let claims=match &object["owned_paths"]{JcsValue::Array(values)=>values.iter().map(|value|{let object=value.clone().object()?;Ok(schema::PathClaimRequestV1{path:object["path"].as_str()?.into(),path_type:object["path_type"].as_str()?.into(),mode:object["mode"].as_str()?.into()})}).collect::<Result<Vec<_>,String>>()?,_=>return Err("manifest owned_paths is not an array".into())};Ok((manifest,claims))}

fn broker_git_index(_repository:&git::OpenedRepository,worktree:&Path,branch_ref:&str,session_dir:&Path,request_id:&str,argv:&[String])->Result<JcsValue,String>{
 let (_,claims)=manifest_claims(session_dir)?;
 let operation=argv.first().map(String::as_str).ok_or_else(||"Git broker index operation is missing".to_string())?;if !matches!(operation,"add"|"rm"|"mv"){return Err("Git broker permits only add|rm|mv index operations".into())}
 if argv.iter().skip(1).any(|arg|arg.starts_with('-')&&arg!="--"){return Err("Git broker index flags are outside the closed set".into())}
 let literal_paths=argv.iter().skip(1).filter(|arg|arg.as_str()!="--").collect::<Vec<_>>();if literal_paths.is_empty()||(operation=="mv"&&literal_paths.len()!=2){return Err("Git broker index operation has invalid path arity".into())}
 for path in &literal_paths{schema::RelPath::parse(path)?;if path.starts_with(':')||path.bytes().any(|byte|matches!(byte,b'*'|b'?'|b'['|b']'|b'\\')){return Err("Git broker requires literal relative path arguments".into())}let exact_file=claims.iter().any(|claim|claim.path_type=="file"&&claim.path.eq_ignore_ascii_case(path));if exact_file&&worktree.join(path).is_dir(){return Err("file claim cannot address a directory pathspec".into())}}
 let paths=literal_paths.iter().map(|path|git::NameStatusChange{status:"A".into(),source:None,destination:(**path).clone()}).collect::<Vec<_>>();git::validate_changed_paths(&paths,&claims)?;
 if git::run_git_text(worktree,&["symbolic-ref","-q","HEAD"])?!=branch_ref{return Err("workspace branch changed before broker index mutation".into())}
 let mut args=vec![operation,"--"];args.extend(literal_paths.iter().map(|path|path.as_str()));let output=git::run_git(worktree,&args)?;
 if !output.status.success(){return Ok(broker_response(request_id,"error",output.status.code().unwrap_or(1),&String::from_utf8_lossy(&output.stdout),&String::from_utf8_lossy(&output.stderr),JcsValue::Null))}
 let changed=git::parse_name_status_z(&git::run_git_bytes(worktree,&["diff","--cached","--name-status","-z","--find-renames","--find-copies"])? )?;git::validate_changed_paths(&changed,&claims)?;
 Ok(broker_response(request_id,"ok",0,"","",JcsValue::Null))
}

fn broker_git_commit(repository:&git::OpenedRepository,worktree:&Path,branch_ref:&str,session_dir:&Path,request_id:&str,argv:&[String])->Result<JcsValue,String>{
 let (_,claims)=manifest_claims(session_dir)?;
 if argv.len()!=3||argv[0]!="commit"||argv[1]!="-m"{return Err("Git broker commit grammar is exactly commit -m <message>".into())}
 let changed=git::parse_name_status_z(&git::run_git_bytes(worktree,&["diff","--cached","--name-status","-z","--find-renames","--find-copies"])? )?;
 if changed.is_empty(){return Err("Git broker refuses an empty worker commit".into())}
 git::validate_changed_paths(&changed,&claims)?;
 let commit=git::create_worker_commit(repository,worktree,branch_ref,&argv[2],&authority::now_timestamp()?)?;
 Ok(broker_response(request_id,"ok",0,&format!("{commit}\n"),"",JcsValue::Null))
}

fn broker_handback(repository:&git::OpenedRepository,worktree:&Path,branch_ref:&str,session_dir:&Path,request_id:&str,argv:&[String])->Result<JcsValue,String>{
 if argv.len()!=2{return Err("broker handback requires request path and digest".into())}
 let request_path=Path::new(&argv[0]);let bytes=capability::read_secure_bytes(request_path).map_err(|e|format!("read handback request: {e}"))?;let request=schema::parse_jcs(&bytes,true)?;
 let request_object=closed_object(&request,&["schema","request_id","session_id","expected_head","created_at"],"HandbackRequestV1")?;
 if request_object["session_id"].as_str()?!=session_dir.file_name().and_then(|v|v.to_str()).unwrap_or(""){return Err("handback request session differs from broker".into())}
 if sha256::hex_digest(&bytes)!=argv[1]{return Err("handback request digest mismatch".into())}
 if git::run_git_text(worktree,&["symbolic-ref","-q","HEAD"])?!=branch_ref{return Err("workspace branch changed before handback".into())}
 if !git::run_git_bytes(worktree,&["status","--porcelain=v2","-z"] )?.is_empty(){return Err("workspace is dirty at handback".into())}
 let head=git::run_git_text(worktree,&["rev-parse","--verify","HEAD"])?;if head!=request_object["expected_head"].as_str()?{return Err("handback expected HEAD differs from workspace HEAD".into())}repository.validate_oid(&head)?;
 let (manifest,claims)=manifest_claims(session_dir)?;let manifest_object=match &manifest.0{JcsValue::Object(object)=>object,_=>unreachable!()};let base=manifest_object["worker_base_commit"].as_str()?;
 let changes=git::parse_name_status_z(&git::run_git_bytes(worktree,&["diff","--name-status","-z","--find-renames","--find-copies",&format!("{base}..{head}")])?)?;git::validate_changed_paths(&changes,&claims)?;
 let commits=git::run_git_text(worktree,&["rev-list","--reverse",&format!("{base}..{head}")])?.lines().map(str::to_string).filter(|value|!value.is_empty()).collect::<Vec<_>>();
 for commit in &commits{let parents=git::run_git_text(worktree,&["rev-list","--parents","-n","1",commit])?;if parents.split_whitespace().count()!=2{return Err("handback produced history is not linear and merge-free".into())}}
 let receipt=JcsValue::Object(BTreeMap::from([("created_at".into(),JcsValue::String(request_object["created_at"].as_str()?.into())),("head_oid".into(),JcsValue::String(head)),("outcome".into(),JcsValue::String("validated".into())),("produced_commits".into(),JcsValue::Array(commits.into_iter().map(JcsValue::String).collect())),("request_id".into(),JcsValue::String(request_object["request_id"].as_str()?.into())),("schema".into(),JcsValue::String(schema::SCHEMA_V1.into())),("session_id".into(),JcsValue::String(request_object["session_id"].as_str()?.into()))]));
 authority::atomic_create_jcs(&session_dir.join("handback-receipt-v1.json"),&CanonicalRecord(receipt.clone()),0o600)?;
 Ok(broker_response(request_id,"ok",0,"","",receipt))
}

fn broker_response(request_id:&str,status:&str,exit_code:i32,stdout:&str,stderr:&str,receipt:JcsValue)->JcsValue{
 JcsValue::Object(BTreeMap::from([("exit_code".into(),JcsValue::String(exit_code.to_string())),("receipt".into(),receipt),("request_id".into(),JcsValue::String(request_id.into())),("schema".into(),JcsValue::String(schema::SCHEMA_V1.into())),("status".into(),JcsValue::String(status.into())),("stderr".into(),JcsValue::String(stderr.into())),("stdout".into(),JcsValue::String(stdout.into()))]))
}

fn run_broker_client(raw:&[String])->Result<i32,String>{
 let divider=raw.iter().position(|arg|arg=="--").ok_or_else(||"broker client requires -- before Git arguments".to_string())?;let flags=Flags::parse(&raw[..divider],&["--worker-capability-file"])?;let capability_file=flags.absolute("--worker-capability-file")?;let argv=raw[divider+1..].to_vec();
 let operation=match argv.first().map(String::as_str){Some("add"|"rm"|"mv")=>"git_index",Some("commit")=>"git_commit",Some(other)=>return Err(format!("Git operation {other} is refused by the closed broker")),None=>return Err("Git broker requires an operation".into())};
 let capability:WorkerCapabilityV1=schema::read_jcs_file(&capability_file,None)?;let response=broker_exchange(&capability,operation,argv,std::env::current_dir().map_err(|e|format!("resolve broker client cwd: {e}"))?)?;let object=match response{JcsValue::Object(object)=>object,_=>return Err("broker response is not an object".into())};
 print!("{}",object["stdout"].as_str()?);eprint!("{}",object["stderr"].as_str()?);object["exit_code"].as_str()?.parse().map_err(|_|"broker exit code is invalid".to_string())
}
pub fn run(raw: Vec<String>) -> ! {
 if raw.first().map(String::as_str)==Some("__broker"){
  match run_broker(&raw[1..]){Ok(())=>std::process::exit(0),Err(error)=>{eprintln!("{error}");std::process::exit(1)}}
 }
 if raw.first().map(String::as_str)==Some("__broker-client"){
  match run_broker_client(&raw[1..]){Ok(code)=>std::process::exit(code),Err(error)=>{eprintln!("{error}");std::process::exit(1)}}
 }
 let result=parse_command(&raw).and_then(execute);
 match result { Ok(output)=>{print!("{output}");std::process::exit(0)},Err(error)=>{eprintln!("{error}");std::process::exit(1)} }
}

#[cfg(test)] mod tests { use super::*; #[test] fn exact_router_is_closed(){let args=vec!["preserve".into(),"--request-file".into(),"/tmp/request.json".into(),"--request-sha256".into(),"a".repeat(64)];assert!(matches!(parse_command(&args),Ok(WorkspaceCommand::Preserve{..})));let mut bad=args;bad.extend(["--extra".into(),"x".into()]);assert!(parse_command(&bad).is_err());} }
