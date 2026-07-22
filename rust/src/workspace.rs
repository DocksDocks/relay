pub mod authority;
pub mod capability;
pub mod custody;
pub mod git;
pub mod repository_gate;
pub mod platform;
pub mod resources;
pub mod schema;

use authority::{
    AuthorityRootProvider, AuthorityRoots, BootstrapOutcome, JournalEventV1, LeaseIdentity,
    SystemAuthorityRootProvider, WorkspaceAuthority, WorkspaceJournalLock, WorkspaceLease,
    WorkspaceLeaseProbe,
};
use capability::{mint_worker, WorkerCapabilityV1};
use crate::sha256;
use schema::{
    AbortRequestV1, AbsPath, CleanupReceiptV1, ClosedJcs, FinishRequestV1, HandbackReceiptV1,
    HandbackRequestV1, HashedFileV1, IntegrateRequestV1, IntegrationReceiptV1, JcsValue,
    LowerUuidV4, RecoverRequestV1, RetentionProofV1, Sha256Digest, WorkspaceStartRequestV1,
    WorkspaceStartResultV1, WorkspaceState, WipReceiptV1,
};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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
  validate_manifest_wip_nullability(early,&object["applied_wip_commit"],&object["worker_base_commit"])?;
  Ok(Self(value))
 }
 fn to_jcs(&self)->JcsValue{self.0.clone()}
}

fn validate_manifest_wip_nullability(early:bool,applied:&JcsValue,worker_base:&JcsValue)->Result<(),String>{
 let applied_null=matches!(applied,JcsValue::Null);let worker_null=matches!(worker_base,JcsValue::Null);
 if applied_null!=worker_null||early!=applied_null{return Err("applied WIP nullability differs from state".into())}
 Ok(())
}

struct CanonicalRecord(JcsValue);
impl ClosedJcs for CanonicalRecord {
 fn from_jcs(value:JcsValue)->Result<Self,String>{Ok(Self(value))}
 fn to_jcs(&self)->JcsValue{self.0.clone()}
}

impl WorkspaceManifestRecord {
    fn object(&self) -> Result<&BTreeMap<String, JcsValue>, String> {
        match &self.0 {
            JcsValue::Object(object) => Ok(object),
            _ => Err("WorkspaceManifestV1 must be an object".into()),
        }
    }

    fn object_mut(&mut self) -> Result<&mut BTreeMap<String, JcsValue>, String> {
        match &mut self.0 {
            JcsValue::Object(object) => Ok(object),
            _ => Err("WorkspaceManifestV1 must be an object".into()),
        }
    }

    fn state(&self) -> Result<WorkspaceState, String> {
        WorkspaceState::parse(self.object()?["state"].as_str()?)
    }

    fn journal_position(&self) -> Result<(u64, Option<String>), String> {
        let object = self.object()?;
        let sequence = object["journal_sequence"]
            .as_str()?
            .parse::<u64>()
            .map_err(|_| "manifest journal sequence overflows u64".to_string())?;
        let head = match &object["journal_head_sha256"] {
            JcsValue::Null => None,
            JcsValue::String(value) => {
                Sha256Digest::parse(value)?;
                Some(value.clone())
            }
            _ => return Err("manifest journal head has invalid nullability".into()),
        };
        if (sequence == 0) != head.is_none() {
            return Err("manifest journal sequence/head nullability mismatch".into());
        }
        Ok((sequence, head))
    }
}

fn read_manifest(path: &Path) -> Result<WorkspaceManifestRecord, String> {
    schema::read_jcs_file(path, None)
}

fn mutate_manifest_event<F>(
    session_dir: &Path,
    manifest_file: &Path,
    expected: WorkspaceState,
    next: WorkspaceState,
    kind: &str,
    payload: JcsValue,
    created_at: &str,
    mutate: F,
) -> Result<WorkspaceManifestRecord, String>
where
    F: FnOnce(&mut BTreeMap<String, JcsValue>) -> Result<(), String>,
{
    schema::Timestamp::parse(created_at)?;
    if !expected.may_transition_to(next) {
        return Err(format!(
            "workspace state transition {} -> {} is not admitted",
            expected.as_str(),
            next.as_str()
        ));
    }
    repository_gate::with_relay_store_rank(|| {
        let journal_lock = WorkspaceJournalLock::acquire(session_dir)?;
        let current = read_manifest(manifest_file)?;
        if current.state()? != expected {
            return Err(format!(
                "workspace state changed before {} publication",
                kind
            ));
        }
        let (sequence, head) = current.journal_position()?;
        let event = JournalEventV1 {
            sequence: sequence
                .checked_add(1)
                .ok_or_else(|| "workspace journal sequence exhausted".to_string())?,
            previous_sha256: head.clone(),
            kind: kind.to_string(),
            payload,
            created_at: created_at.to_string(),
        };
        let next_head = authority::append_journal_cas(
            &journal_lock,
            &event,
            sequence,
            head.as_deref(),
        )?;
        let mut replacement = current.clone();
        {
            let object = replacement.object_mut()?;
            object.insert("state".into(), JcsValue::String(next.as_str().into()));
            object.insert(
                "journal_sequence".into(),
                JcsValue::String(event.sequence.to_string()),
            );
            object.insert(
                "journal_head_sha256".into(),
                JcsValue::String(next_head),
            );
            object.insert("updated_at".into(), JcsValue::String(created_at.into()));
            mutate(object)?;
        }
        WorkspaceManifestRecord::from_jcs(replacement.0.clone())?;
        authority::replace_manifest_cas(
            &journal_lock,
            manifest_file,
            expected.as_str(),
            sequence,
            head.as_deref(),
            &replacement,
        )?;
        Ok(replacement)
    })
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
 platform::admit_writable_custody()?;
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
  fs::create_dir(&session_dir).map_err(|e|format!("create workspace session: {e}"))?;fs::set_permissions(&session_dir,fs::Permissions::from_mode(0o700)).map_err(|e|format!("chmod workspace session: {e}"))?;for name in ["journal","worker-capabilities","broker-replays","broker-intents","broker-plans","resources"]{fs::create_dir(session_dir.join(name)).map_err(|e|format!("create workspace {name}: {e}"))?;fs::set_permissions(session_dir.join(name),fs::Permissions::from_mode(0o700)).map_err(|e|format!("chmod workspace {name}: {e}"))?}
  authority::atomic_create_jcs(&session_dir.join("start-request-v1.json"),&request,0o600)?;
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
 let lease_identity=lease.identity()?;
 let lease_identity_record=CanonicalRecord(JcsValue::Object(BTreeMap::from([
  ("device".into(),JcsValue::String(lease_identity.device.to_string())),
  ("inode".into(),JcsValue::String(lease_identity.inode.to_string())),
  ("path".into(),JcsValue::String(lease_path.to_string_lossy().into_owned())),
  ("schema".into(),JcsValue::String("WorkspaceLeaseIdentityV1".into())),
 ])));
 authority::atomic_create_jcs(&manifest_file.parent().unwrap().join("lease-identity-v1.json"),&lease_identity_record,0o600)?;
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
 drop(gate);
 start_git_broker(roots,relay_executable,session_dir,&worktree_root,&branch_ref,&capability_file,&lease,&allocated.resource_fds)?;
 let custody_active_sha256=start_custody_runtime(
  relay_executable,
  session_dir,
  &request.request_id,
  &tool_launch_file,
  &lease,
  &allocated.resource_fds,
 )?;
 let gate=repository_gate::RepositoryGate::acquire(roots,&repository.identity)?;
 let running=mutate_manifest_event(
  session_dir,
  &manifest_file,
  WorkspaceState::Ready,
  WorkspaceState::Running,
  "CustodyActivated",
  JcsValue::Object(BTreeMap::from([
   ("backend".into(),JcsValue::String(platform::LINUX_BACKEND.into())),
   ("custody_active_sha256".into(),JcsValue::String(custody_active_sha256.clone())),
  ])),
  &authority::now_timestamp()?,
  |object|{
   object.insert("custody_evidence".into(),JcsValue::Object(BTreeMap::from([
    ("active_sha256".into(),JcsValue::String(custody_active_sha256.clone())),
    ("backend".into(),JcsValue::String(platform::LINUX_BACKEND.into())),
    ("empty_sha256".into(),JcsValue::Null),
   ])));
   Ok(())
  },
 )?;
 if running.state()?!=WorkspaceState::Running{return Err("custody activation did not durably publish Running".into())}
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
 let private=git::actual_private_git_dir(worktree)?;
 let private_fd=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY).open(&private).map_err(|e|format!("securely open private Git directory for shim: {e}"))?;
 let session_relay=open_or_create_private_dir_at(&private_fd,"session-relay")?;
 let bin=open_or_create_private_dir_at(&session_relay,"bin")?;
 let quote=|value:&Path|value.to_string_lossy().replace('\'',"'\"'\"'");
 let body=format!("#!/bin/sh\nexec '{}' workspace __broker-client --worker-capability-file '{}' -- \"$@\"\n",quote(relay_executable),quote(capability_file));
 let name=std::ffi::CString::new("git").unwrap();let fd=unsafe{libc::openat(bin.as_raw_fd(),name.as_ptr(),libc::O_WRONLY|libc::O_CREAT|libc::O_EXCL|libc::O_CLOEXEC|libc::O_NOFOLLOW,0o500)};
 if fd<0{return Err(format!("create Git shim: {}",std::io::Error::last_os_error()))}
 let mut shim_file=unsafe{File::from_raw_fd(fd)};let metadata=shim_file.metadata().map_err(|e|format!("fstat Git shim: {e}"))?;if !metadata.is_file()||metadata.uid()!=unsafe{libc::geteuid()}||metadata.nlink()!=1||metadata.mode()&0o777!=0o500{return Err("Git shim is not an EUID-owned single-link mode-0500 regular file".into())}
 shim_file.write_all(body.as_bytes()).and_then(|_|shim_file.sync_all()).map_err(|e|format!("persist Git shim: {e}"))?;bin.sync_all().map_err(|e|format!("fsync Git shim directory: {e}"))?;
 Ok(private.join("session-relay/bin/git"))
}

fn open_or_create_private_dir_at(parent:&File,name:&str)->Result<File,String>{
 let name=std::ffi::CString::new(name).map_err(|_|"private directory component contains NUL".to_string())?;
 let created=unsafe{libc::mkdirat(parent.as_raw_fd(),name.as_ptr(),0o700)};
 if created!=0{let error=std::io::Error::last_os_error();if error.kind()!=std::io::ErrorKind::AlreadyExists{return Err(format!("create private directory component: {error}"))}}
 let fd=unsafe{libc::openat(parent.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY,0)};
 if fd<0{return Err(format!("securely open private directory component: {}",std::io::Error::last_os_error()))}
 let file=unsafe{File::from_raw_fd(fd)};let metadata=file.metadata().map_err(|e|format!("fstat private directory component: {e}"))?;if !metadata.is_dir()||metadata.uid()!=unsafe{libc::geteuid()}||metadata.mode()&0o777!=0o700{return Err("Git shim directory is not an EUID-owned mode-0700 real directory".into())}
 if created==0{parent.sync_all().map_err(|e|format!("fsync parent after Git shim directory creation: {e}"))?}
 Ok(file)
}

pub fn list_workspaces(repository_path:&Path,capability_file:&Path)->Result<JcsValue,String>{
 let repository=git::OpenedRepository::open(repository_path)?;let authority=WorkspaceAuthority::system()?;authority.authenticate(&repository.identity.repository_id,capability_file,"list")?;let sessions=authority.roots().data.join("repositories").join(&repository.identity.repository_id).join("sessions");let mut values=Vec::new();
 if sessions.exists(){for entry in fs::read_dir(sessions).map_err(|e|format!("read workspace sessions: {e}"))?{let entry=entry.map_err(|e|format!("read workspace session: {e}"))?;if !entry.file_type().map_err(|e|format!("inspect workspace session entry: {e}"))?.is_dir(){continue}let path=entry.path().join("manifest-v1.json");let record:WorkspaceManifestRecord=schema::read_jcs_file(&path,None)?;values.push(record.0)}}
 values.sort_by(|left,right|manifest_session(left).cmp(&manifest_session(right)));Ok(JcsValue::Object(BTreeMap::from([("custody".into(),JcsValue::String("unproven".into())),("schema".into(),JcsValue::String(schema::SCHEMA_V1.into())),("workspaces".into(),JcsValue::Array(values))])))
}
fn manifest_session(value:&JcsValue)->String{match value{JcsValue::Object(object)=>object.get("session_id").and_then(|value|value.as_str().ok()).unwrap_or("").to_string(),_=>String::new()}}
pub fn inspect_workspace(session_id:&str,repository_path:&Path,capability_file:&Path)->Result<JcsValue,String>{
 LowerUuidV4::parse(session_id)?;let repository=git::OpenedRepository::open(repository_path)?;let authority=WorkspaceAuthority::system()?;authority.authenticate(&repository.identity.repository_id,capability_file,"inspect")?;let path=authority.roots().data.join("repositories").join(&repository.identity.repository_id).join("sessions").join(session_id).join("manifest-v1.json");let manifest:WorkspaceManifestRecord=schema::read_jcs_file(&path,None)?;
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
fn session_dir_for(
    roots: &AuthorityRoots,
    repository_id: &str,
    session_id: &str,
) -> Result<PathBuf, String> {
    Sha256Digest::parse(repository_id)?;
    LowerUuidV4::parse(session_id)?;
    let path = roots
        .data
        .join("repositories")
        .join(repository_id)
        .join("sessions")
        .join(session_id);
    authority::ensure_private_directory(
        path.parent()
            .ok_or_else(|| "session path has no parent".to_string())?,
        roots.euid,
    )?;
    if !path.exists() {
        return Err("workspace session is not durably present".into());
    }
    Ok(path)
}

fn manifest_produced_head(manifest: &WorkspaceManifestRecord) -> Result<String, String> {
    let object = manifest.object()?;
    if let JcsValue::Array(values) = &object["produced_commits"] {
        if let Some(last) = values.last() {
            return last.clone().object()?["oid"].as_str().map(str::to_string);
        }
    }
    object["worker_base_commit"].as_str().map(str::to_string)
}

fn ordered_request_from_manifest(
    manifest: &WorkspaceManifestRecord,
    integration_root: &Path,
    integration_branch_ref: &str,
    expected_integration_head: &str,
) -> Result<git::OrderedIntegrationRequest, String> {
    let object = manifest.object()?;
    let produced_commits = match &object["produced_commits"] {
        JcsValue::Array(values) => values
            .iter()
            .map(|value| {
                let object = value.clone().object()?;
                Ok(git::OrderedProducedCommit {
                    oid: object["oid"].as_str()?.into(),
                    parent_oid: object["parent_oid"].as_str()?.into(),
                    source: object["source"].as_str()?.into(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?,
        _ => return Err("workspace produced_commits is not an array".into()),
    };
    if produced_commits.is_empty() {
        return Err("workspace has no produced commit chain".into());
    }
    let expected_worker_head = produced_commits
        .last()
        .expect("nonempty produced commit chain")
        .oid
        .clone();
    Ok(git::OrderedIntegrationRequest {
        integration_root: integration_root.to_path_buf(),
        integration_branch_ref: integration_branch_ref.into(),
        expected_integration_head: expected_integration_head.into(),
        worker_root: PathBuf::from(object["worktree_root"].as_str()?),
        worker_branch_ref: object["branch_ref"].as_str()?.into(),
        expected_worker_head,
        base_commit: object["base_commit"].as_str()?.into(),
        produced_commits,
    })
}

fn integration_receipt_path(session_dir: &Path) -> PathBuf {
    session_dir.join("integration-receipt-v1.json")
}

pub fn integrate_workspace_with_roots(
    roots: &AuthorityRoots,
    request_file: &Path,
    request_sha256: &str,
    coordinator_capability_file: &Path,
) -> Result<IntegrationReceiptV1, String> {
    let request: IntegrateRequestV1 =
        schema::read_jcs_file(request_file, Some(request_sha256))?;
    let integration_root = PathBuf::from(&request.repository_path);
    let repository = git::OpenedRepository::open(&integration_root)?;
    let authority = WorkspaceAuthority::new(roots.clone())?;
    if request.repository_id != repository.identity.repository_id {
        return Err("integration request repository identity differs from its canonical path".into());
    }
    authority.authenticate(
        &repository.identity.repository_id,
        coordinator_capability_file,
        "integrate",
    )?;
    let session_dir = session_dir_for(
        roots,
        &repository.identity.repository_id,
        &request.session_id,
    )?;
    let receipt_path = integration_receipt_path(&session_dir);
    if receipt_path.exists() {
        let receipt: IntegrationReceiptV1 = schema::read_jcs_file(&receipt_path, None)?;
        if receipt.request_id != request.request_id || receipt.session_id != request.session_id {
            return Err("integration receipt belongs to a different coordinator request".into());
        }
        if receipt.outcome == "needs_user_action" {
            return Ok(receipt);
        }
        return Err("integration coordinator request was already durably settled".into());
    }
    let queue =
        authority.acquire_integration_queue(&repository.identity.repository_id)?;
    let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
    let manifest_path = session_dir.join("manifest-v1.json");
    let manifest: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    if manifest.state()? == WorkspaceState::IntegrationBlocked {
        return Err(
            "integration is durably blocked and will not be retried without explicit recovery"
                .into(),
        );
    }
    let manifest_object = manifest.object()?;
    if manifest_object["session_id"].as_str()? != request.session_id
        || manifest.state()? != request.expected_state
        || manifest_object["journal_head_sha256"].as_str()?
            != request.expected_journal_head_sha256
    {
        return Err("integration request CAS differs from the durable workspace manifest".into());
    }
    if schema::RepositoryIdentityV1::from_jcs(manifest.object()?["repository"].clone())?
        != repository.identity
    {
        return Err("integration repository differs from the workspace manifest".into());
    }
    repository.validate_unchanged()?;
    let integration_branch =
        git::run_git_text(&integration_root, &["symbolic-ref", "-q", "HEAD"])?;
    let ordered = ordered_request_from_manifest(
        &manifest,
        &integration_root,
        &integration_branch,
        &request.expected_head,
    )?;
    let intent = CanonicalRecord(JcsValue::Object(BTreeMap::from([
        (
            "action".into(),
            JcsValue::String(request.disposition.clone()),
        ),
        (
            "request_sha256".into(),
            JcsValue::String(request_sha256.into()),
        ),
        (
            "schema".into(),
            JcsValue::String("IntegrationIntentV1".into()),
        ),
        (
            "session_id".into(),
            JcsValue::String(request.session_id.clone()),
        ),
    ])));
    let intent_path = session_dir.join("integration-intent-v1.json");
    if intent_path.exists() {
        let existing = schema::parse_jcs(&capability::read_secure_bytes(&intent_path)?, true)?;
        if existing != intent.0 {
            return Err("integration intent differs from the durable request".into());
        }
    } else {
        authority::atomic_create_jcs(&intent_path, &intent, 0o600)?;
    }
    mutate_manifest_event(
        &session_dir,
        &manifest_path,
        WorkspaceState::HandbackReady,
        WorkspaceState::IntegrationQueued,
        "IntegrationQueued",
        JcsValue::Object(BTreeMap::from([(
            "request_sha256".into(),
            JcsValue::String(request_sha256.into()),
        )])),
        &request.created_at,
        |_| Ok(()),
    )?;
    let result = match request.disposition.as_str() {
        "integrate" => git::integrate_ordered(&repository, &ordered)?,
        "reject" => git::reject_ordered(&repository, &ordered)?,
        _ => return Err("integration disposition is outside the closed set".into()),
    };
    let outcome = result.outcome.as_str().to_string();
    let receipt = IntegrationReceiptV1 {
        request_id: request.request_id.clone(),
        session_id: request.session_id.clone(),
        outcome: outcome.clone(),
        pre_integration_head: result.pre_head.clone(),
        worker_commits: ordered
            .produced_commits
            .iter()
            .map(|commit| commit.oid.clone())
            .collect(),
        integration_commits: result.output_oids.clone(),
        post_integration_head: result.post_head.clone(),
        conflict_paths: result.conflict_paths.clone(),
        created_at: request.created_at.clone(),
    };
    receipt.validate()?;
    authority::atomic_create_jcs(&receipt_path, &receipt, 0o600)?;
    let next = match result.outcome {
        git::CoordinatorIntegrationOutcome::Integrated => WorkspaceState::Integrated,
        git::CoordinatorIntegrationOutcome::Rejected => WorkspaceState::Rejected,
        git::CoordinatorIntegrationOutcome::NeedsUserAction => {
            WorkspaceState::IntegrationBlocked
        }
    };
    let receipt_sha256 = sha256::hex_digest(&capability::read_secure_bytes(&receipt_path)?);
    let integration_commits = result
        .output_oids
        .iter()
        .cloned()
        .map(JcsValue::String)
        .collect::<Vec<_>>();
    mutate_manifest_event(
        &session_dir,
        &manifest_path,
        WorkspaceState::IntegrationQueued,
        next,
        "IntegrationSettled",
        JcsValue::Object(BTreeMap::from([
            ("outcome".into(), JcsValue::String(outcome)),
            (
                "receipt_sha256".into(),
                JcsValue::String(receipt_sha256),
            ),
        ])),
        &request.created_at,
        |object| {
            object.insert(
                "integration_commits".into(),

                JcsValue::Array(integration_commits),
            );
            Ok(())
        },
    )?;
    drop(gate);
    drop(queue);
    Ok(receipt)
}

fn hashed_file(path: &Path) -> Result<HashedFileV1, String> {
    let canonical =
        fs::canonicalize(path).map_err(|error| format!("canonicalize artifact: {error}"))?;
    let bytes =
        fs::read(&canonical).map_err(|error| format!("read {}: {error}", canonical.display()))?;
    let file = HashedFileV1 {
        path: canonical.to_string_lossy().into_owned(),
        sha256: sha256::hex_digest(&bytes),
        size: bytes.len().to_string(),
    };
    file.validate()?;
    Ok(file)
}

fn sync_file_and_parent(path: &Path) -> Result<(), String> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("fsync {}: {error}", path.display()))?;
    let parent = path
        .parent()
        .ok_or_else(|| "artifact path has no parent".to_string())?;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("fsync {}: {error}", parent.display()))
}

fn create_retention_proof(
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    reason: &str,
    proven_at: &str,
) -> Result<(RetentionProofV1, String), String> {
    let proof_path = session_dir.join("retention-proof-v1.json");
    if proof_path.exists() {
        let proof: RetentionProofV1 = schema::read_jcs_file(&proof_path, None)?;
        if proof.reason != reason {
            return Err("durable retention proof has a different reason".into());
        }
        return Ok((
            proof,
            sha256::hex_digest(&capability::read_secure_bytes(&proof_path)?),
        ));
    }
    let object = manifest.object()?;
    let worktree = PathBuf::from(object["worktree_root"].as_str()?);
    let branch_ref = object["branch_ref"].as_str()?.to_string();
    let head = git::run_git_text(&worktree, &["rev-parse", "--verify", "HEAD"])?;
    let bundle_path = session_dir.join("retained-work.bundle");
    if bundle_path.exists() {
        return Err("unreceipted retention bundle already exists".into());
    }
    let output = Command::new("git")
        .args(["bundle", "create"])
        .arg(&bundle_path)
        .arg(&branch_ref)
        .current_dir(&worktree)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("create retention bundle: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "create retention bundle failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    sync_file_and_parent(&bundle_path)?;
    let verify = Command::new("git")
        .args(["bundle", "verify"])
        .arg(&bundle_path)
        .current_dir(&worktree)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("verify retention bundle: {error}"))?;
    if !verify.status.success() {
        return Err(format!(
            "verify retention bundle failed: {}",
            String::from_utf8_lossy(&verify.stderr).trim()
        ));
    }
    let dirty =
        !git::run_git_bytes(&worktree, &["status", "--porcelain=v2", "-z"])?.is_empty();
    let dirty_artifact = if dirty {
        if reason != "abort" {
            return Err("dirty retention requires explicit abort".into());
        }
        let artifact = session_dir.join("dirty-worktree.tar");
        if artifact.exists() {
            return Err("unreceipted dirty retention artifact already exists".into());
        }
        let output = Command::new("tar")
            .args(["--format=posix", "-cf"])
            .arg(&artifact)
            .arg("-C")
            .arg(&worktree)
            .args(["--exclude=.git", "."])
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("create dirty retention artifact: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "create dirty retention artifact failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        sync_file_and_parent(&artifact)?;
        Some(hashed_file(&artifact)?)
    } else {
        None
    };
    let proof = RetentionProofV1 {
        session_id: object["session_id"].as_str()?.into(),
        branch_ref,
        head_oid: head.clone(),
        bundle: hashed_file(&bundle_path)?,
        dirty_artifact,
        reachable_oids: git::run_git_text(&worktree, &["rev-list", &head])?
            .lines()
            .map(str::to_string)
            .filter(|value| !value.is_empty())
            .collect(),
        reason: reason.into(),
        proven_at: proven_at.into(),
    };
    proof.validate()?;
    authority::atomic_create_jcs(&proof_path, &proof, 0o600)?;
    let digest = sha256::hex_digest(&capability::read_secure_bytes(&proof_path)?);
    Ok((proof, digest))
}

fn revoke_worker_if_needed(
    roots: &AuthorityRoots,
    repository: &git::OpenedRepository,
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    revoked_at: &str,
) -> Result<(), String> {
    let record_path = session_dir.join("worker-capability-record-v1.json");
    let record: schema::CapabilityRecordV1 = schema::read_jcs_file(&record_path, None)?;
    if record.revoked_at.is_some() {
        return Ok(());
    }
    let worker_path = PathBuf::from(manifest.object()?["worker_capability_file"].as_str()?);
    let worker: WorkerCapabilityV1 = schema::read_jcs_file(&worker_path, None)?;
    let authority = WorkspaceAuthority::new(roots.clone())?;
    let exclusion =
        authority.acquire_authority_exclusion(&repository.identity.repository_id)?;
    capability::revoke_worker_durable(
        &exclusion,
        &record_path,
        &worker.capability_id,
        worker
            .generation
            .parse()
            .map_err(|_| "worker capability generation overflow".to_string())?,
        revoked_at,
    )?;
    Ok(())
}

fn wait_for_durable_file(path: &Path, label: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(format!(
                "{label} was not published within fifteen seconds; work retained"
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn evidence_field(
    path: &Path,
    keys: &[&str],
    schema_name: &str,
    field: &str,
) -> Result<String, String> {
    let value = schema::parse_jcs(&capability::read_secure_bytes(path)?, true)?;
    let object = closed_object(&value, keys, schema_name)?;
    let digest = object[field].as_str()?.to_string();
    Sha256Digest::parse(&digest)?;
    Ok(digest)
}

fn close_custody_and_resources(
    roots: &AuthorityRoots,
    repository: &git::OpenedRepository,
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    now: &str,
) -> Result<(String, Vec<String>), String> {
    revoke_worker_if_needed(roots, repository, session_dir, manifest, now)?;
    let empty_path = session_dir.join("custody-empty-v1.json");
    if !empty_path.exists() {
        runtime_exchange(session_dir, "terminate", None)?;
    }
    wait_for_durable_file(&empty_path, "custody EMPTY proof")?;
    let broker_close_path = session_dir.join("broker-close-v1.json");
    wait_for_durable_file(&broker_close_path, "broker close proof")?;
    let lease_close_path = session_dir.join("custody-lease-closed-v1.json");
    if !lease_close_path.exists() {
        runtime_exchange(session_dir, "close_lease", None)?;
    }
    wait_for_durable_file(&lease_close_path, "custody lease close proof")?;

    let lease_value = schema::parse_jcs(
        &capability::read_secure_bytes(&session_dir.join("lease-identity-v1.json"))?,
        true,
    )?;
    let lease = closed_object(
        &lease_value,
        &["schema", "path", "device", "inode"],
        "WorkspaceLeaseIdentityV1",
    )?;
    let expected = LeaseIdentity {
        device: lease["device"]
            .as_str()?
            .parse()
            .map_err(|_| "lease device overflow".to_string())?,
        inode: lease["inode"]
            .as_str()?
            .parse()
            .map_err(|_| "lease inode overflow".to_string())?,
    };
    let authority = WorkspaceAuthority::new(roots.clone())?;
    let exclusion =
        authority.acquire_authority_exclusion(&repository.identity.repository_id)?;
    let probe = WorkspaceLeaseProbe::acquire(Path::new(lease["path"].as_str()?), expected)?;
    probe.revalidate()?;
    exclusion.revalidate()?;
    drop(probe);
    drop(exclusion);

    let broker_close_sha256 = evidence_field(
        &broker_close_path,
        &[
            "schema",
            "session_id",
            "capability_id",
            "revoked_at",
            "evidence_sha256",
        ],
        "BrokerCloseV1",
        "evidence_sha256",
    )?;
    let runtime_empty_sha256 = evidence_field(
        &empty_path,
        &["schema", "empty_sha256", "mode"],
        "CustodyEmptyV1",
        "empty_sha256",
    )?;
    let receipts = resources::release_resources(
        roots,
        &repository.identity.repository_id,
        manifest.object()?["session_id"].as_str()?,
        &session_dir.join("resources"),
        &resources::ResourceReleaseEvidenceV1 {
            broker_close_sha256,
            runtime_empty_sha256: runtime_empty_sha256.clone(),
        },
        now,
    )?;
    Ok((runtime_empty_sha256, receipts))
}

fn validate_lifecycle_cas(
    manifest: &WorkspaceManifestRecord,
    session_id: &str,
    expected_state: WorkspaceState,
    expected_journal_head: &str,
) -> Result<(), String> {
    let object = manifest.object()?;
    if object["session_id"].as_str()? != session_id
        || manifest.state()? != expected_state
        || object["journal_head_sha256"].as_str()? != expected_journal_head
    {
        return Err("coordinator lifecycle CAS differs from the durable manifest".into());
    }
    Ok(())
}

fn finalize_closed(
    roots: &AuthorityRoots,
    repository: &git::OpenedRepository,
    session_dir: &Path,
    request_id: &str,
    expected_state: WorkspaceState,
    retention_sha256: Option<String>,
    created_at: &str,
) -> Result<CleanupReceiptV1, String> {
    let manifest_path = session_dir.join("manifest-v1.json");
    let manifest: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    if manifest.state()? == WorkspaceState::Closed {
        return schema::read_jcs_file(&session_dir.join("cleanup-receipt-v1.json"), None);
    }
    if manifest.state()? != expected_state {
        return Err("cleanup state differs from the exact settled outcome".into());
    }
    let (custody_empty_sha256, mut resource_receipts) =
        close_custody_and_resources(roots, repository, session_dir, &manifest, created_at)?;
    resource_receipts.sort();
    resource_receipts.dedup();
    let authority = WorkspaceAuthority::new(roots.clone())?;
    let queue =
        authority.acquire_integration_queue(&repository.identity.repository_id)?;
    let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
    let locked: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    if locked.state()? != expected_state {
        return Err("cleanup state changed while acquiring integration locks".into());
    }
    repository.validate_unchanged()?;
    let object = locked.object()?;
    let worker_root = PathBuf::from(object["worktree_root"].as_str()?);
    let worker_branch_ref = object["branch_ref"].as_str()?;
    let expected_worker_head = if expected_state == WorkspaceState::AbortedRetained {
        let proof: RetentionProofV1 =
            schema::read_jcs_file(&session_dir.join("retention-proof-v1.json"), None)?;
        proof.head_oid
    } else {
        manifest_produced_head(&locked)?
    };
    match expected_state {
        WorkspaceState::Integrated => {
            let integration_receipt: IntegrationReceiptV1 =
                schema::read_jcs_file(&integration_receipt_path(session_dir), None)?;
            let integration_branch =
                git::run_git_text(&repository.root, &["symbolic-ref", "-q", "HEAD"])?;
            let ordered = ordered_request_from_manifest(
                &locked,
                &repository.root,
                &integration_branch,
                &integration_receipt.pre_integration_head,
            )?;
            let proof = git::GitCleanupProof::Integrated(git::CoordinatorIntegrationResult {
                outcome: git::CoordinatorIntegrationOutcome::Integrated,
                pre_head: integration_receipt.pre_integration_head,
                post_head: integration_receipt.post_integration_head,
                output_oids: integration_receipt.integration_commits,
                conflict_paths: Vec::new(),
            });
            git::cleanup_ordered_worktree(repository, &ordered, &proof)?;
        }
        WorkspaceState::Rejected | WorkspaceState::AbortedRetained => {
            let retention = retention_sha256
                .as_deref()
                .ok_or_else(|| "retained cleanup requires retention proof".to_string())?;
            git::cleanup_retained_worktree(
                repository,
                &worker_root,
                worker_branch_ref,
                &expected_worker_head,
                retention,
            )?;
        }
        _ => return Err("cleanup is outside the settled outcome states".into()),
    }
    let receipt = CleanupReceiptV1 {
        request_id: request_id.into(),
        session_id: object["session_id"].as_str()?.into(),
        retention_sha256,
        resource_receipts,
        worktree_removed: true,
        branch_removed: true,
        capabilities_revoked: true,
        custody_empty_sha256,
        lease_released: true,
        outcome: "closed".into(),
        created_at: created_at.into(),
    };
    receipt.validate()?;
    let receipt_path = session_dir.join("cleanup-receipt-v1.json");
    authority::atomic_create_jcs(&receipt_path, &receipt, 0o600)?;
    let receipt_sha256 = sha256::hex_digest(&capability::read_secure_bytes(&receipt_path)?);
    mutate_manifest_event(
        session_dir,
        &manifest_path,
        expected_state,
        WorkspaceState::Releasing,
        "Releasing",
        JcsValue::Object(BTreeMap::from([(
            "cleanup_receipt_sha256".into(),
            JcsValue::String(receipt_sha256.clone()),
        )])),
        created_at,
        |_| Ok(()),
    )?;
    mutate_manifest_event(
        session_dir,
        &manifest_path,
        WorkspaceState::Releasing,
        WorkspaceState::Closed,
        "Closed",
        JcsValue::Object(BTreeMap::from([(
            "cleanup_receipt_sha256".into(),
            JcsValue::String(receipt_sha256.clone()),
        )])),
        created_at,
        |object| {
            object.insert(
                "custody_evidence".into(),
                JcsValue::Object(BTreeMap::from([
                    (
                        "active_sha256".into(),
                        object["custody_evidence"].clone().object()?["active_sha256"]
                            .clone(),
                    ),
                    (
                        "empty_sha256".into(),
                        JcsValue::String(receipt.custody_empty_sha256.clone()),
                    ),
                ])),
            );
            Ok(())
        },
    )?;
    drop(gate);
    drop(queue);
    runtime_exchange(session_dir, "closed_committed", Some(&receipt_sha256))?;
    Ok(receipt)
}

pub fn finish_workspace_with_roots(
    roots: &AuthorityRoots,
    request_file: &Path,
    request_sha256: &str,
    coordinator_capability_file: &Path,
) -> Result<CleanupReceiptV1, String> {
    let request: FinishRequestV1 =
        schema::read_jcs_file(request_file, Some(request_sha256))?;
    let repository = git::OpenedRepository::open(Path::new(&request.repository_path))?;
    if repository.identity.repository_id != request.repository_id {
        return Err("finish repository identity differs from its canonical path".into());
    }
    let authority = WorkspaceAuthority::new(roots.clone())?;
    authority.authenticate(
        &request.repository_id,
        coordinator_capability_file,
        "finish",
    )?;
    let session_dir = session_dir_for(roots, &request.repository_id, &request.session_id)?;
    let manifest: WorkspaceManifestRecord =
        schema::read_jcs_file(&session_dir.join("manifest-v1.json"), None)?;
    validate_lifecycle_cas(
        &manifest,
        &request.session_id,
        request.expected_state,
        &request.expected_journal_head_sha256,
    )?;
    let repository_head =
        git::run_git_text(&repository.root, &["rev-parse", "--verify", "HEAD"])?;
    if repository_head != request.expected_head {
        return Err("finish expected HEAD differs from the integration checkout".into());
    }
    let retention = if request.expected_state == WorkspaceState::Rejected {
        Some(create_retention_proof(
            &session_dir,
            &manifest,
            "rejected",
            &request.created_at,
        )?
        .1)
    } else if request.expected_state == WorkspaceState::AbortedRetained {
        Some(
            sha256::hex_digest(&capability::read_secure_bytes(
                &session_dir.join("retention-proof-v1.json"),
            )?),
        )
    } else {
        None
    };
    finalize_closed(
        roots,
        &repository,
        &session_dir,
        &request.request_id,
        request.expected_state,
        retention,
        &request.created_at,
    )
}

pub fn abort_workspace_with_roots(
    roots: &AuthorityRoots,
    request_file: &Path,
    request_sha256: &str,
    coordinator_capability_file: &Path,
) -> Result<CleanupReceiptV1, String> {
    let request: AbortRequestV1 =
        schema::read_jcs_file(request_file, Some(request_sha256))?;
    let repository = git::OpenedRepository::open(Path::new(&request.repository_path))?;
    if repository.identity.repository_id != request.repository_id {
        return Err("abort repository identity differs from its canonical path".into());
    }
    let authority = WorkspaceAuthority::new(roots.clone())?;
    authority.authenticate(
        &request.repository_id,
        coordinator_capability_file,
        "abort",
    )?;
    let session_dir = session_dir_for(roots, &request.repository_id, &request.session_id)?;
    let manifest_path = session_dir.join("manifest-v1.json");
    let manifest: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    validate_lifecycle_cas(
        &manifest,
        &request.session_id,
        request.expected_state,
        &request.expected_journal_head_sha256,
    )?;
    if git::run_git_text(
        Path::new(manifest.object()?["worktree_root"].as_str()?),
        &["rev-parse", "--verify", "HEAD"],
    )? != request.expected_head
    {
        return Err("abort expected worker HEAD differs from the exact checkout".into());
    }
    revoke_worker_if_needed(
        roots,
        &repository,
        &session_dir,
        &manifest,
        &request.created_at,
    )?;
    if !session_dir.join("custody-empty-v1.json").exists() {
        runtime_exchange(&session_dir, "terminate", None)?;
    }
    wait_for_durable_file(
        &session_dir.join("custody-empty-v1.json"),
        "abort custody EMPTY proof",
    )?;
    let retention = create_retention_proof(
        &session_dir,
        &manifest,
        "abort",
        &request.created_at,
    )?
    .1;
    mutate_manifest_event(
        &session_dir,
        &manifest_path,
        request.expected_state,
        WorkspaceState::AbortedRetained,
        "AbortedRetained",
        JcsValue::Object(BTreeMap::from([
            ("reason".into(), JcsValue::String(request.reason.clone())),
            (
                "retention_sha256".into(),
                JcsValue::String(retention.clone()),
            ),
        ])),
        &request.created_at,
        |_| Ok(()),
    )?;
    finalize_closed(
        roots,
        &repository,
        &session_dir,
        &request.request_id,
        WorkspaceState::AbortedRetained,
        Some(retention),
        &request.created_at,
    )
}

fn recovery_inspect_value(
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
) -> Result<JcsValue, String> {
    let evidence = |name: &str| -> Result<JcsValue, String> {
        let path = session_dir.join(name);
        if path.exists() {
            Ok(JcsValue::String(sha256::hex_digest(
                &capability::read_secure_bytes(&path)?,
            )))
        } else {
            Ok(JcsValue::Null)
        }
    };
    Ok(JcsValue::Object(BTreeMap::from([
        (
            "broker_close_sha256".into(),
            evidence("broker-close-v1.json")?,
        ),
        (
            "custody_active_sha256".into(),
            evidence("custody-active-v1.json")?,
        ),
        (
            "custody_empty_sha256".into(),
            evidence("custody-empty-v1.json")?,
        ),
        (
            "custody_status".into(),
            JcsValue::String("unproven".into()),
        ),
        ("manifest".into(), manifest.to_jcs()),
        (
            "retention_sha256".into(),
            evidence("retention-proof-v1.json")?,
        ),
        (
            "schema".into(),
            JcsValue::String("RecoveryInspectV1".into()),
        ),
    ])))
}

fn resume_prelaunch(
    roots: &AuthorityRoots,
    repository: &git::OpenedRepository,
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    coordinator_capability_file: &Path,
) -> Result<WorkspaceStartResultV1, String> {
    let state = manifest.state()?;
    if !matches!(
        state,
        WorkspaceState::Reserved | WorkspaceState::Provisioning | WorkspaceState::LeaseHeld
    ) {
        return Err(
            "resume_prelaunch requires exact Reserved, Provisioning, or LeaseHeld proof; work retained"
                .into(),
        );
    }
    if session_dir.join("custody-active-v1.json").exists()
        || session_dir.join("custody-command-v1.sock").exists()
        || session_dir.join("tool-launch-v1.json").exists()
        || session_dir.join("broker-close-v1.json").exists()
    {
        return Err("resume_prelaunch found runtime/resource side effects; work retained".into());
    }
    let object = manifest.object()?;
    let worktree = PathBuf::from(object["worktree_root"].as_str()?);
    if worktree.exists() {
        let branch_ref = object["branch_ref"].as_str()?;
        let head = git::run_git_text(&worktree, &["rev-parse", "--verify", "HEAD"])?;
        if state == WorkspaceState::LeaseHeld && object["worker_base_commit"].as_str()? != head {
            return Err("prelaunch worker HEAD drifted; work retained".into());
        }
        let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
        git::cleanup_prelaunch_worktree(repository, &worktree, branch_ref, &head)?;
        drop(gate);
    }
    if session_dir.join("lease-identity-v1.json").exists() {
        let lease_value = schema::parse_jcs(
            &capability::read_secure_bytes(&session_dir.join("lease-identity-v1.json"))?,
            true,
        )?;
        let lease = closed_object(
            &lease_value,
            &["schema", "path", "device", "inode"],
            "WorkspaceLeaseIdentityV1",
        )?;
        let probe = WorkspaceLeaseProbe::acquire(
            Path::new(lease["path"].as_str()?),
            LeaseIdentity {
                device: lease["device"]
                    .as_str()?
                    .parse()
                    .map_err(|_| "lease device overflow".to_string())?,
                inode: lease["inode"]
                    .as_str()?
                    .parse()
                    .map_err(|_| "lease inode overflow".to_string())?,
            },
        )?;
        probe.revalidate()?;
    }
    let start_bytes =
        capability::read_secure_bytes(&session_dir.join("start-request-v1.json"))?;
    let start_sha256 = sha256::hex_digest(&start_bytes);
    let restart_file = session_dir
        .parent()
        .ok_or_else(|| "session directory has no parent".to_string())?
        .join(format!(
            ".resume-{}-start-request-v1.json",
            manifest.object()?["session_id"].as_str()?
        ));
    write_private_bytes(&restart_file, &start_bytes)?;
    fs::remove_dir_all(session_dir)
        .map_err(|error| format!("remove proven prelaunch session: {error}"))?;
    match start_workspace_with_roots(
        roots,
        &restart_file,
        &start_sha256,
        Some(coordinator_capability_file),
    ) {
        Ok(started) => {
            fs::remove_file(&restart_file)
                .map_err(|error| format!("remove consumed resume request: {error}"))?;
            Ok(started.result)
        }
        Err(error) => Err(format!(
            "prelaunch session was exactly reset but restart failed: {error}; request retained at {}",
            restart_file.display()
        )),
    }
}

pub fn recover_workspace_with_roots(
    roots: &AuthorityRoots,
    request_file: &Path,
    request_sha256: &str,
    coordinator_capability_file: &Path,
) -> Result<JcsValue, String> {
    let request: RecoverRequestV1 =
        schema::read_jcs_file(request_file, Some(request_sha256))?;
    let repository = git::OpenedRepository::open(Path::new(&request.repository_path))?;
    if repository.identity.repository_id != request.repository_id {
        return Err("recover repository identity differs from its canonical path".into());
    }
    let authority = WorkspaceAuthority::new(roots.clone())?;
    authority.authenticate(
        &request.repository_id,
        coordinator_capability_file,
        "recover",
    )?;
    let session_dir = session_dir_for(roots, &request.repository_id, &request.session_id)?;
    let manifest_path = session_dir.join("manifest-v1.json");
    let manifest: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    validate_lifecycle_cas(
        &manifest,
        &request.session_id,
        request.expected_state,
        &request.expected_journal_head_sha256,
    )?;
    match request.action.as_str() {
        "inspect" => recovery_inspect_value(&session_dir, &manifest),
        "rotate_coordinator" => {
            let exclusion = authority.acquire_authority_exclusion(&request.repository_id)?;
            let rotated = authority.rotate_coordinator_cas(
                &exclusion,
                &request.repository_id,
                coordinator_capability_file,
                &request.created_at,
            )?;
            Ok(JcsValue::Object(BTreeMap::from([
                (
                    "coordinator_capability_file".into(),
                    JcsValue::String(rotated.capability_file.to_string_lossy().into_owned()),
                ),
                (
                    "generation".into(),
                    JcsValue::String(rotated.capability.generation),
                ),
                (
                    "schema".into(),
                    JcsValue::String("CoordinatorRotationV1".into()),
                ),
            ])))
        }
        "resume_prelaunch" => Ok(resume_prelaunch(
            roots,
            &repository,
            &session_dir,
            &manifest,
            coordinator_capability_file,
        )?
        .to_jcs()),
        "retain_abort" => {
            let synthetic = AbortRequestV1 {
                request_id: request.request_id,
                repository_path: request.repository_path,
                repository_id: request.repository_id,
                session_id: request.session_id,
                expected_state: request.expected_state,
                expected_journal_head_sha256: request.expected_journal_head_sha256,
                expected_head: request.expected_head,
                reason: "recover retain_abort".into(),
                created_at: request.created_at,
            };
            let path = session_dir.join("recover-abort-request-v1.json");
            authority::atomic_create_jcs(&path, &synthetic, 0o600)?;
            let digest = sha256::hex_digest(&capability::read_secure_bytes(&path)?);
            Ok(
                abort_workspace_with_roots(
                    roots,
                    &path,
                    &digest,
                    coordinator_capability_file,
                )?
                .to_jcs(),
            )
        }
        _ => Err("recover action is outside the closed set".into()),
    }
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
        WorkspaceCommand::Coordinator { operation,request_file,request_sha256,coordinator_capability_file } => {
            let roots=SystemAuthorityRootProvider.roots()?;
            match operation {
                CoordinatorMutation::Integrate=>integrate_workspace_with_roots(&roots,&request_file,&request_sha256,&coordinator_capability_file)?.to_jcs(),
                CoordinatorMutation::Recover=>recover_workspace_with_roots(&roots,&request_file,&request_sha256,&coordinator_capability_file)?,
                CoordinatorMutation::Finish=>finish_workspace_with_roots(&roots,&request_file,&request_sha256,&coordinator_capability_file)?.to_jcs(),
                CoordinatorMutation::Abort=>abort_workspace_with_roots(&roots,&request_file,&request_sha256,&coordinator_capability_file)?.to_jcs(),
            }
        },
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

#[cfg(target_os = "linux")]
fn set_spawn_inheritance(fds: &[RawFd], inheritable: bool) -> Result<Vec<(RawFd, i32)>, String> {
    let mut prior = Vec::with_capacity(fds.len());
    for fd in fds {
        let flags = unsafe { libc::fcntl(*fd, libc::F_GETFD) };
        if flags < 0 {
            restore_spawn_inheritance(&prior)?;
            return Err(format!(
                "inspect custody descriptor {fd}: {}",
                std::io::Error::last_os_error()
            ));
        }
        let next = if inheritable {
            flags & !libc::FD_CLOEXEC
        } else {
            flags | libc::FD_CLOEXEC
        };
        if unsafe { libc::fcntl(*fd, libc::F_SETFD, next) } < 0 {
            restore_spawn_inheritance(&prior)?;
            return Err(format!(
                "set custody descriptor {fd} inheritance: {}",
                std::io::Error::last_os_error()
            ));
        }
        prior.push((*fd, flags));
    }
    Ok(prior)
}

#[cfg(target_os = "linux")]
fn restore_spawn_inheritance(prior: &[(RawFd, i32)]) -> Result<(), String> {
    let mut error = None;
    for (fd, flags) in prior.iter().copied() {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 && error.is_none() {
            error = Some(format!(
                "restore custody descriptor {fd} flags: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(target_os = "linux")]
fn wait_pipe_record(mut read_end: File, mut child: Child) -> Result<String, String> {
    let mut poll = libc::pollfd {
        fd: read_end.as_raw_fd(),
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("inspect custody guardian: {error}"))?
        {
            return Err(format!(
                "custody guardian exited before activation ({status})"
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(
                "custody activation was not proven within fifteen seconds; lease and work retained"
                    .into(),
            );
        }
        let milliseconds = remaining.as_millis().min(100) as i32;
        let rc = unsafe { libc::poll(&mut poll, 1, milliseconds) };
        if rc < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!("wait for custody activation: {error}"));
        }
        if rc > 0 {
            let mut bytes = Vec::new();
            read_end
                .read_to_end(&mut bytes)
                .map_err(|error| format!("read custody activation proof: {error}"))?;
            if bytes.len() != 65 || bytes.last() != Some(&b'\n') {
                return Err("custody guardian activation proof is not exact SHA-256+LF".into());
            }
            let digest = std::str::from_utf8(&bytes[..64])
                .map_err(|_| "custody activation digest is not UTF-8".to_string())?;
            Sha256Digest::parse(digest)?;
            return Ok(digest.to_string());
        }
    }
}

fn start_custody_runtime(
    relay_executable: &Path,
    session_dir: &Path,
    session_id: &str,
    tool_launch_file: &Path,
    lease: &WorkspaceLease,
    resource_fds: &[RawFd],
) -> Result<String, String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (
            relay_executable,
            session_dir,
            session_id,
            tool_launch_file,
            lease,
            resource_fds,
        );
        return Err(platform::MACOS_STOP_REASON.into());
    }
    #[cfg(target_os = "linux")]
    {
        LowerUuidV4::parse(session_id)?;
        let runtime_key = capability::random_secret()?;
        write_private_bytes(&session_dir.join("runtime-control-key-v1"), &runtime_key)?;
        let mut pipe = [-1; 2];
        if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(format!(
                "create custody activation pipe: {}",
                std::io::Error::last_os_error()
            ));
        }
        let read_end = unsafe { File::from_raw_fd(pipe[0]) };
        let write_end = unsafe { OwnedFd::from_raw_fd(pipe[1]) };
        let lease_fd = lease.as_raw_fd();
        let mut inherited = vec![lease_fd, write_end.as_raw_fd()];
        inherited.extend_from_slice(resource_fds);
        let prior = set_spawn_inheritance(&inherited, true)?;
        let resource_text = resource_fds
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let spawned = Command::new(relay_executable)
            .args([
                "workspace",
                "__guardian",
                "--session-dir",
                session_dir
                    .to_str()
                    .ok_or_else(|| "session dir is not UTF-8".to_string())?,
                "--session-id",
                session_id,
                "--tool-launch-file",
                tool_launch_file
                    .to_str()
                    .ok_or_else(|| "tool launch path is not UTF-8".to_string())?,
                "--relay-executable",
                relay_executable
                    .to_str()
                    .ok_or_else(|| "relay executable path is not UTF-8".to_string())?,
                "--lease-fd",
                &lease_fd.to_string(),
                "--resource-fds",
                &resource_text,
                "--ready-fd",
                &write_end.as_raw_fd().to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let restore = restore_spawn_inheritance(&prior);
        let child = spawned.map_err(|error| format!("spawn custody guardian: {error}"))?;
        if let Err(error) = restore {
            return Err(format!(
                "{error}; custody guardian may hold retained proof and requires recovery"
            ));
        }
        drop(write_end);
        wait_pipe_record(read_end, child)
    }
}

#[cfg(target_os = "linux")]
fn parse_fd_list(value: &str) -> Result<Vec<RawFd>, String> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let mut fds = value
        .split(',')
        .map(|value| {
            let fd = value
                .parse::<RawFd>()
                .map_err(|_| "custody FD list is not decimal".to_string())?;
            if fd < 3 || fd.to_string() != value {
                return Err("custody FD list is not canonical".into());
            }
            Ok(fd)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let before = fds.len();
    fds.sort_unstable();
    fds.dedup();
    if fds.len() != before {
        return Err("custody FD list contains duplicates".into());
    }
    Ok(fds)
}

#[cfg(target_os = "linux")]
fn runtime_key(session_dir: &Path) -> Result<[u8; 32], String> {
    let bytes = capability::read_secure_bytes(&session_dir.join("runtime-control-key-v1"))?;
    bytes
        .try_into()
        .map_err(|_| "runtime control key is not exactly 32 bytes".to_string())
}

#[cfg(target_os = "linux")]
fn persist_runtime_record(
    session_dir: &Path,
    name: &str,
    value: JcsValue,
) -> Result<String, String> {
    let path = session_dir.join(name);
    let record = CanonicalRecord(value);
    if path.exists() {
        let existing = schema::parse_jcs(&capability::read_secure_bytes(&path)?, true)?;
        if existing != record.0 {
            return Err(format!("{name} differs from the durable custody evidence"));
        }
    } else {
        authority::atomic_create_jcs(&path, &record, 0o600)?;
    }
    Ok(schema::jcs_sha256(&record))
}

#[cfg(target_os = "linux")]
fn process_peer(pid: libc::pid_t) -> Result<custody::PeerIdentity, String> {
    Ok(custody::PeerIdentity {
        pid,
        euid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        start_token: platform::linux::process_start_token(pid)?,
    })
}

#[cfg(target_os = "linux")]
fn run_guardian(raw: &[String]) -> Result<(), String> {
    use custody::{ControlEndpoint, CustodyController, LeaseReference, PayloadValue, Sender};
    use platform::linux::DelegatedCgroup;

    let flags = Flags::parse(
        raw,
        &[
            "--session-dir",
            "--session-id",
            "--tool-launch-file",
            "--relay-executable",
            "--lease-fd",
            "--resource-fds",
            "--ready-fd",
        ],
    )?;
    let session_dir = flags.absolute("--session-dir")?;
    let session_id = flags.value("--session-id")?.to_string();
    LowerUuidV4::parse(&session_id)?;
    let tool_launch_file = flags.absolute("--tool-launch-file")?;
    let relay_executable = flags.absolute("--relay-executable")?;
    let lease_fd = flags
        .value("--lease-fd")?
        .parse::<RawFd>()
        .map_err(|_| "guardian lease FD is not decimal".to_string())?;
    let resource_fds = parse_fd_list(flags.value("--resource-fds")?)?;
    let ready_fd = flags
        .value("--ready-fd")?
        .parse::<RawFd>()
        .map_err(|_| "guardian ready FD is not decimal".to_string())?;
    if lease_fd < 3 || ready_fd < 3 || resource_fds.contains(&lease_fd) {
        return Err("guardian inherited FD inventory is invalid".into());
    }
    let lease_fd = unsafe { OwnedFd::from_raw_fd(lease_fd) };
    let resource_files = resource_fds
        .iter()
        .map(|fd| unsafe { File::from_raw_fd(*fd) })
        .collect::<Vec<_>>();
    let mut ready = unsafe { File::from_raw_fd(ready_fd) };
    resources::validate_held_resource_fds(&session_dir.join("resources"), &resource_fds)?;

    let socket = session_dir.join("custody-command-v1.sock");
    if socket.exists() {
        return Err("custody command socket already exists; explicit recovery is required".into());
    }
    let listener = UnixListener::bind(&socket)
        .map_err(|error| format!("bind custody command socket {}: {error}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod custody command socket: {error}"))?;

    let cgroup = DelegatedCgroup::create(&session_id)?;
    let guardian_lease = LeaseReference::from_owned_fd(lease_fd)?;
    let (guardian_fd, supervisor_fd) = ControlEndpoint::pair()?;
    let (key_fd, key) = custody::create_control_key_memfd()?;
    let guardian_identity = custody::PeerIdentity::current()?;
    let inherited_fds = std::iter::once(supervisor_fd.as_raw_fd())
        .chain(std::iter::once(key_fd.as_raw_fd()))
        .chain(resource_fds.iter().copied())
        .collect::<Vec<_>>();
    let prior = set_spawn_inheritance(&inherited_fds, true)?;
    let resource_text = resource_fds
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let spawned = Command::new(&relay_executable)
        .args([
            "workspace",
            "__custody-supervisor",
            "--session-id",
            &session_id,
            "--tool-launch-file",
            tool_launch_file
                .to_str()
                .ok_or_else(|| "tool launch path is not UTF-8".to_string())?,
            "--control-fd",
            &supervisor_fd.as_raw_fd().to_string(),
            "--key-fd",
            &key_fd.as_raw_fd().to_string(),
            "--resource-fds",
            &resource_text,
            "--guardian-pid",
            &guardian_identity.pid.to_string(),
            "--guardian-euid",
            &guardian_identity.euid.to_string(),
            "--guardian-gid",
            &guardian_identity.gid.to_string(),
            "--guardian-start-token",
            &guardian_identity.start_token,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let restore = restore_spawn_inheritance(&prior);
    let mut supervisor = spawned.map_err(|error| format!("spawn custody supervisor: {error}"))?;
    restore?;
    drop(supervisor_fd);
    drop(key_fd);
    drop(resource_files);
    let supervisor_identity = process_peer(supervisor.id() as libc::pid_t)?;
    let endpoint = ControlEndpoint::new(
        guardian_fd,
        key,
        session_id.clone(),
        1,
        Sender::Guardian,
        supervisor_identity.clone(),
    )?;
    let mut controller = CustodyController::new(endpoint);
    let activation = crate::supervisor::run_workspace_guardian_bootstrap(
        &mut controller,
        &cgroup,
        &guardian_lease,
    )?;
    let active = JcsValue::Object(BTreeMap::from([
        (
            "activated_sha256".into(),
            match activation.activated.get("evidence_sha256") {
                Some(PayloadValue::String(value)) => JcsValue::String(value.clone()),
                _ => return Err("ACTIVATED evidence payload is not exact".into()),
            },
        ),
        (
            "backend".into(),
            JcsValue::String(platform::LINUX_BACKEND.into()),
        ),
        (
            "cgroup_membership".into(),
            JcsValue::String(cgroup.membership().into()),
        ),
        (
            "guardian_pid".into(),
            JcsValue::String(guardian_identity.pid.to_string()),
        ),
        (
            "guardian_start_token".into(),
            JcsValue::String(guardian_identity.start_token),
        ),
        (
            "prepared_sha256".into(),
            JcsValue::String(activation.prepared_evidence_sha256),
        ),
        ("schema".into(), JcsValue::String("CustodyActiveV1".into())),
        ("session_id".into(), JcsValue::String(session_id.clone())),
        (
            "supervisor_pid".into(),
            JcsValue::String(supervisor_identity.pid.to_string()),
        ),
        (
            "supervisor_start_token".into(),
            JcsValue::String(supervisor_identity.start_token),
        ),
    ]));
    let active_sha256 =
        persist_runtime_record(&session_dir, "custody-active-v1.json", active)?;
    ready
        .write_all(format!("{active_sha256}\n").as_bytes())
        .and_then(|_| ready.flush())
        .map_err(|error| format!("publish custody activation proof: {error}"))?;
    drop(ready);
    run_guardian_commands(
        &session_dir,
        listener,
        &mut controller,
        guardian_lease,
        &mut supervisor,
    )
}

#[cfg(target_os = "linux")]
fn launch_from_record(
    path: &Path,
    resource_fds: &[RawFd],
) -> Result<platform::linux::WorkerLaunch, String> {
    let decision: resources::ToolLaunchDecisionV1 = schema::read_jcs_file(path, None)?;
    let recorded = decision
        .resource_fds
        .iter()
        .map(|value| {
            value
                .parse::<RawFd>()
                .map_err(|_| "tool launch resource FD is not decimal".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if recorded != resource_fds {
        return Err("custody inherited resource FDs differ from the durable launch decision".into());
    }
    resources::verify_executable(
        Path::new(&decision.executable_path),
        &decision.executable_sha256,
    )?;
    Ok(platform::linux::WorkerLaunch {
        executable: PathBuf::from(decision.executable_path),
        arguments: decision.arguments,
        environment: decision
            .environment
            .into_iter()
            .map(|value| (value.name, value.value))
            .collect(),
        cwd: PathBuf::from(&decision.cwd),
        resource_fds: resource_fds.to_vec(),
        sandbox: platform::linux::LandlockPolicy {
            workspace: PathBuf::from(decision.cwd),
            readable: Vec::new(),
            writable_resources: decision
                .writable_resources
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        },
    })
}

#[cfg(target_os = "linux")]
fn run_custody_supervisor(raw: &[String]) -> Result<(), String> {
    use custody::{ControlEndpoint, CustodianServer, PeerIdentity, Sender};

    let flags = Flags::parse(
        raw,
        &[
            "--session-id",
            "--tool-launch-file",
            "--control-fd",
            "--key-fd",
            "--resource-fds",
            "--guardian-pid",
            "--guardian-euid",
            "--guardian-gid",
            "--guardian-start-token",
        ],
    )?;
    let session_id = flags.value("--session-id")?.to_string();
    LowerUuidV4::parse(&session_id)?;
    let control_fd = flags
        .value("--control-fd")?
        .parse::<RawFd>()
        .map_err(|_| "supervisor control FD is not decimal".to_string())?;
    let key_fd = flags
        .value("--key-fd")?
        .parse::<RawFd>()
        .map_err(|_| "supervisor key FD is not decimal".to_string())?;
    let resource_fds = parse_fd_list(flags.value("--resource-fds")?)?;
    let resource_files = resource_fds
        .iter()
        .map(|fd| unsafe { File::from_raw_fd(*fd) })
        .collect::<Vec<_>>();
    let key = custody::read_control_key_memfd(key_fd)?;
    unsafe { libc::close(key_fd) };
    let guardian = PeerIdentity {
        pid: flags
            .value("--guardian-pid")?
            .parse()
            .map_err(|_| "guardian PID is not decimal".to_string())?,
        euid: flags
            .value("--guardian-euid")?
            .parse()
            .map_err(|_| "guardian EUID is not decimal".to_string())?,
        gid: flags
            .value("--guardian-gid")?
            .parse()
            .map_err(|_| "guardian GID is not decimal".to_string())?,
        start_token: flags.value("--guardian-start-token")?.to_string(),
    };
    let actual = process_peer(guardian.pid)?;
    if actual != guardian {
        return Err("guardian peer identity drifted before supervisor bootstrap".into());
    }
    let endpoint = ControlEndpoint::new(
        unsafe { OwnedFd::from_raw_fd(control_fd) },
        key,
        session_id,
        1,
        Sender::Supervisor,
        guardian,
    )?;
    let launch = launch_from_record(&flags.absolute("--tool-launch-file")?, &resource_fds)?;
    let mut server = CustodianServer::new(endpoint);
    let mut fault = |_error: &str| {};
    let custody = crate::supervisor::run_workspace_supervisor_entrypoint(
        &mut server,
        &launch,
        &mut fault,
    )?;
    let duplicate_pidfd = unsafe {
        libc::fcntl(
            custody.process.pidfd.as_raw_fd(),
            libc::F_DUPFD_CLOEXEC,
            3,
        )
    };
    if duplicate_pidfd < 0 {
        return Err(format!(
            "duplicate worker pidfd for resource close: {}",
            std::io::Error::last_os_error()
        ));
    }
    let closer = thread::spawn(move || {
        let pidfd = unsafe { OwnedFd::from_raw_fd(duplicate_pidfd) };
        let mut poll = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let rc = unsafe { libc::poll(&mut poll, 1, -1) };
            if rc > 0 {
                break;
            }
            if rc < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
        drop(resource_files);
        drop(pidfd);
    });
    let released = crate::supervisor::run_workspace_supervisor_release_protocol(
        &mut server,
        custody,
        &mut fault,
    )?;
    closer
        .join()
        .map_err(|_| "resource closer thread panicked".to_string())?;
    released.cgroup.remove()
}

#[cfg(target_os = "linux")]
fn runtime_bare_request(
    request_id: &str,
    action: &str,
    evidence_sha256: Option<&str>,
    nonce: &str,
) -> JcsValue {
    JcsValue::Object(BTreeMap::from([
        ("action".into(), JcsValue::String(action.into())),
        (
            "evidence_sha256".into(),
            evidence_sha256
                .map(|value| JcsValue::String(value.into()))
                .unwrap_or(JcsValue::Null),
        ),
        ("nonce".into(), JcsValue::String(nonce.into())),
        ("request_id".into(), JcsValue::String(request_id.into())),
        (
            "schema".into(),
            JcsValue::String("WorkspaceRuntimeCommandV1".into()),
        ),
    ]))
}

#[cfg(target_os = "linux")]
fn runtime_exchange(
    session_dir: &Path,
    action: &str,
    evidence_sha256: Option<&str>,
) -> Result<String, String> {
    if !matches!(
        action,
        "quiesce" | "terminate" | "close_lease" | "closed_committed"
    ) {
        return Err("runtime action is outside the closed set".into());
    }
    if let Some(value) = evidence_sha256 {
        Sha256Digest::parse(value)?;
    }
    let request_id = crate::store::uuid_v4();
    let nonce = capability::encode_base64url(&capability::random_secret()?);
    let bare = runtime_bare_request(&request_id, action, evidence_sha256, &nonce);
    let key = runtime_key(session_dir)?;
    let message = [
        b"session-relay/runtime-command/v1\0".as_slice(),
        schema::serialize_jcs(&bare).as_bytes(),
    ]
    .concat();
    let mac = sha256::hex_digest(&sha256::hmac(&key, &message));
    let mut object = bare.object()?;
    object.insert("mac".into(), JcsValue::String(mac));
    let mut bytes = schema::serialize_jcs(&JcsValue::Object(object)).into_bytes();
    bytes.push(b'\n');
    let mut stream = UnixStream::connect(session_dir.join("custody-command-v1.sock"))
        .map_err(|error| format!("connect custody runtime: {error}"))?;
    stream
        .write_all(&bytes)
        .map_err(|error| format!("write custody runtime command: {error}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|error| format!("finish custody runtime command: {error}"))?;
    let mut response = Vec::new();
    stream
        .take(64 * 1024)
        .read_to_end(&mut response)
        .map_err(|error| format!("read custody runtime response: {error}"))?;
    let response = schema::parse_jcs(&response, true)?;
    let object = closed_object(
        &response,
        &[
            "schema",
            "request_id",
            "status",
            "evidence_sha256",
            "error",
        ],
        "WorkspaceRuntimeResponseV1",
    )?;
    if object["request_id"].as_str()? != request_id {
        return Err("custody runtime response request ID mismatch".into());
    }
    if object["status"].as_str()? != "ok" {
        return Err(format!(
            "custody runtime refused {action}: {}",
            object["error"].as_str()?
        ));
    }
    let digest = object["evidence_sha256"].as_str()?;
    Sha256Digest::parse(digest)?;
    Ok(digest.into())
}

#[cfg(not(target_os = "linux"))]
fn runtime_exchange(
    _session_dir: &Path,
    _action: &str,
    _evidence_sha256: Option<&str>,
) -> Result<String, String> {
    Err(platform::MACOS_STOP_REASON.into())
}

#[cfg(target_os = "linux")]
fn runtime_response(
    request_id: &str,
    result: Result<String, String>,
) -> JcsValue {
    match result {
        Ok(evidence) => JcsValue::Object(BTreeMap::from([
            ("error".into(), JcsValue::String(String::new())),
            ("evidence_sha256".into(), JcsValue::String(evidence)),
            ("request_id".into(), JcsValue::String(request_id.into())),
            (
                "schema".into(),
                JcsValue::String("WorkspaceRuntimeResponseV1".into()),
            ),
            ("status".into(), JcsValue::String("ok".into())),
        ])),
        Err(error) => JcsValue::Object(BTreeMap::from([
            ("error".into(), JcsValue::String(error)),
            (
                "evidence_sha256".into(),
                JcsValue::String("0".repeat(64)),
            ),
            ("request_id".into(), JcsValue::String(request_id.into())),
            (
                "schema".into(),
                JcsValue::String("WorkspaceRuntimeResponseV1".into()),
            ),
            ("status".into(), JcsValue::String("error".into())),
        ])),
    }
}

#[cfg(target_os = "linux")]
fn peer_is_current_euid(stream: &UnixStream) -> Result<(), String> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
        || length as usize != std::mem::size_of::<libc::ucred>()
        || credentials.uid != unsafe { libc::geteuid() }
    {
        return Err("custody runtime peer credential mismatch".into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_guardian_commands(
    session_dir: &Path,
    listener: UnixListener,
    controller: &mut custody::CustodyController,
    guardian_lease: custody::LeaseReference,
    supervisor: &mut Child,
) -> Result<(), String> {
    use custody::PayloadValue;
    let key = runtime_key(session_dir)?;
    let mut guardian_lease = Some(guardian_lease);
    for incoming in listener.incoming() {
        let mut stream =
            incoming.map_err(|error| format!("accept custody runtime command: {error}"))?;
        peer_is_current_euid(&stream)?;
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut stream)
            .take(64 * 1024)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read custody runtime command: {error}"))?;
        let value = schema::parse_jcs(&bytes, true)?;
        let object = closed_object(
            &value,
            &[
                "schema",
                "request_id",
                "action",
                "evidence_sha256",
                "nonce",
                "mac",
            ],
            "WorkspaceRuntimeCommandV1",
        )?;
        let request_id = object["request_id"].as_str()?.to_string();
        LowerUuidV4::parse(&request_id)?;
        let action = object["action"].as_str()?.to_string();
        let evidence = match &object["evidence_sha256"] {
            JcsValue::Null => None,
            JcsValue::String(value) => {
                Sha256Digest::parse(value)?;
                Some(value.clone())
            }
            _ => return Err("runtime command evidence has invalid nullability".into()),
        };
        let bare = runtime_bare_request(
            &request_id,
            &action,
            evidence.as_deref(),
            object["nonce"].as_str()?,
        );
        let message = [
            b"session-relay/runtime-command/v1\0".as_slice(),
            schema::serialize_jcs(&bare).as_bytes(),
        ]
        .concat();
        let expected = sha256::hex_digest(&sha256::hmac(&key, &message));
        if !sha256::constant_time_eq(
            expected.as_bytes(),
            object["mac"].as_str()?.as_bytes(),
        ) {
            return Err("runtime command MAC mismatch".into());
        }
        let result = match action.as_str() {
            "quiesce" => {
                let payload = controller.quiesce()?;
                let empty = match payload.get("evidence_sha256") {
                    Some(PayloadValue::String(value)) => value.clone(),
                    _ => return Err("QUIESCED evidence payload is not exact".into()),
                };
                controller.confirm_empty(&empty)?;
                persist_runtime_record(
                    session_dir,
                    "custody-empty-v1.json",
                    JcsValue::Object(BTreeMap::from([
                        ("empty_sha256".into(), JcsValue::String(empty)),
                        ("mode".into(), JcsValue::String("quiesce".into())),
                        ("schema".into(), JcsValue::String("CustodyEmptyV1".into())),
                    ])),
                )
            }
            "terminate" => {
                let payload = controller.terminate()?;
                let empty = match payload.get("evidence_sha256") {
                    Some(PayloadValue::String(value)) => value.clone(),
                    _ => return Err("EMPTY evidence payload is not exact".into()),
                };
                persist_runtime_record(
                    session_dir,
                    "custody-empty-v1.json",
                    JcsValue::Object(BTreeMap::from([
                        ("empty_sha256".into(), JcsValue::String(empty)),
                        ("mode".into(), JcsValue::String("terminate".into())),
                        ("schema".into(), JcsValue::String("CustodyEmptyV1".into())),
                    ])),
                )
            }
            "close_lease" => {
                if !session_dir.join("custody-empty-v1.json").exists() {
                    return Err("custody EMPTY proof is missing before lease close".into());
                }
                controller.prepare_release()?;
                let supervisor_close = controller.close_lease()?;
                let supervisor_close = match supervisor_close.get("evidence_sha256") {
                    Some(PayloadValue::String(value)) => value.clone(),
                    _ => return Err("supervisor LEASE_CLOSED evidence is not exact".into()),
                };
                let guardian_close = guardian_lease
                    .take()
                    .ok_or_else(|| "guardian lease was already closed".to_string())?
                    .close();
                persist_runtime_record(
                    session_dir,
                    "custody-lease-closed-v1.json",
                    JcsValue::Object(BTreeMap::from([
                        (
                            "guardian_close_sha256".into(),
                            JcsValue::String(guardian_close.evidence_sha256),
                        ),
                        (
                            "supervisor_close_sha256".into(),
                            JcsValue::String(supervisor_close),
                        ),
                        (
                            "schema".into(),
                            JcsValue::String("CustodyLeaseClosedV1".into()),
                        ),
                    ])),
                )
            }
            "closed_committed" => {
                let evidence = evidence
                    .as_deref()
                    .ok_or_else(|| "CLOSED_COMMITTED requires evidence".to_string())?;
                controller.closed_committed(evidence)?;
                let status = supervisor
                    .wait()
                    .map_err(|error| format!("wait custody supervisor: {error}"))?;
                if !status.success() {
                    return Err(format!(
                        "custody supervisor failed after CLOSED_COMMITTED ({status})"
                    ));
                }
                Ok(evidence.to_string())
            }
            _ => Err("runtime action is outside the closed set".into()),
        };
        let close = action == "closed_committed" && result.is_ok();
        let response = runtime_response(&request_id, result);
        let mut response_bytes = schema::serialize_jcs(&response).into_bytes();
        response_bytes.push(b'\n');
        stream
            .write_all(&response_bytes)
            .map_err(|error| format!("write custody runtime response: {error}"))?;
        if close {
            fs::remove_file(session_dir.join("custody-command-v1.sock"))
                .map_err(|error| format!("remove custody command socket: {error}"))?;
            return Ok(());
        }
    }
    Err("custody command listener ended before CLOSED_COMMITTED".into())
}
fn produced_commit_values(
    repository: &git::OpenedRepository,
    receipt: &HandbackReceiptV1,
    applied_wip_commit: &str,
) -> Result<JcsValue, String> {
    let mut values = Vec::with_capacity(receipt.produced_commits.len());
    for oid in &receipt.produced_commits {
        repository.validate_oid(oid)?;
        let parent_text =
            git::run_git_text(&repository.root, &["show", "-s", "--format=%P", oid])?;
        let parents = parent_text.split_whitespace().collect::<Vec<_>>();
        if parents.len() != 1 {
            return Err("handback produced commit is not single-parent linear history".into());
        }
        values.push(JcsValue::Object(BTreeMap::from([
            ("oid".into(), JcsValue::String(oid.clone())),
            ("parent_oid".into(), JcsValue::String(parents[0].into())),
            (
                "source".into(),
                JcsValue::String(
                    if oid == applied_wip_commit {
                        "preserved_wip"
                    } else {
                        "worker"
                    }
                    .into(),
                ),
            ),
        ])));
    }
    Ok(JcsValue::Array(values))
}

fn complete_handback_quiescence(
    roots: &AuthorityRoots,
    session_dir: &Path,
    worktree: &Path,
) -> Result<(), String> {
    let manifest_path = session_dir.join("manifest-v1.json");
    let current: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    if current.state()? == WorkspaceState::HandbackReady {
        return Ok(());
    }
    if current.state()? != WorkspaceState::Running {
        return Err("handback quiescence requires Running or durable HandbackReady".into());
    }
    let receipt_path = session_dir.join("handback-receipt-v1.json");
    let receipt: HandbackReceiptV1 = schema::read_jcs_file(&receipt_path, None)?;
    let receipt_sha256 = sha256::hex_digest(&capability::read_secure_bytes(&receipt_path)?);
    let empty_sha256 = if session_dir.join("custody-empty-v1.json").exists() {
        let value = schema::parse_jcs(
            &capability::read_secure_bytes(&session_dir.join("custody-empty-v1.json"))?,
            true,
        )?;
        let object = closed_object(
            &value,
            &["schema", "empty_sha256", "mode"],
            "CustodyEmptyV1",
        )?;
        object["empty_sha256"].as_str()?.to_string()
    } else {
        runtime_exchange(session_dir, "quiesce", None)?
    };
    let repository = git::OpenedRepository::open(worktree)?;
    let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
    let locked: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    if locked.state()? == WorkspaceState::HandbackReady {
        return Ok(());
    }
    if locked.state()? != WorkspaceState::Running {
        return Err("handback state changed while acquiring the lifecycle locks".into());
    }
    if locked.object()?["session_id"].as_str()? != receipt.session_id
        || schema::RepositoryIdentityV1::from_jcs(locked.object()?["repository"].clone())?
            != repository.identity
    {
        return Err("handback receipt identity differs from the durable workspace".into());
    }
    repository.validate_unchanged()?;
    let object = locked.object()?;
    let branch_ref = object["branch_ref"].as_str()?;
    if git::run_git_text(worktree, &["symbolic-ref", "-q", "HEAD"])? != branch_ref {
        return Err("workspace branch changed during handback quiescence".into());
    }
    if git::run_git_text(worktree, &["rev-parse", "--verify", "HEAD"])? != receipt.head_oid {
        return Err("workspace HEAD changed during handback quiescence".into());
    }
    let applied = object["applied_wip_commit"].as_str()?;
    let produced = produced_commit_values(&repository, &receipt, applied)?;
    let now = authority::now_timestamp()?;
    let payload = JcsValue::Object(BTreeMap::from([
        (
            "custody_empty_sha256".into(),
            JcsValue::String(empty_sha256.clone()),
        ),
        (
            "receipt_sha256".into(),
            JcsValue::String(receipt_sha256),
        ),
    ]));
    mutate_manifest_event(
        session_dir,
        &manifest_path,
        WorkspaceState::Running,
        WorkspaceState::HandbackReady,
        "HandbackReady",
        payload,
        &now,
        |object| {
            object.insert("produced_commits".into(), produced.clone());
            let custody = object["custody_evidence"].clone().object()?;
            object.insert(
                "custody_evidence".into(),
                JcsValue::Object(BTreeMap::from([
                    (
                        "active_sha256".into(),
                        custody["active_sha256"].clone(),
                    ),
                    (
                        "empty_sha256".into(),
                        JcsValue::String(empty_sha256.clone()),
                    ),
                ])),
            );
            Ok(())
        },
    )?;
    drop(gate);
    Ok(())
}

fn run_broker(raw: &[String]) -> Result<(), String> {
    let flags = Flags::parse(
        raw,
        &[
            "--authority-root",
            "--data-root",
            "--session-dir",
            "--worktree",
            "--branch-ref",
            "--worker-capability-file",
            "--lease-fd",
            "--resource-fds",
        ],
    )?;
    let roots = AuthorityRoots {
        authority: flags.absolute("--authority-root")?,
        data: flags.absolute("--data-root")?,
        euid: unsafe { libc::geteuid() },
    };
    let session_dir = flags.absolute("--session-dir")?;
    let worktree = flags.absolute("--worktree")?;
    let branch_ref = flags.value("--branch-ref")?.to_string();
    if !branch_ref.starts_with("refs/heads/docks/") {
        return Err("broker branch ref is invalid".into());
    }
    let capability_file = flags.absolute("--worker-capability-file")?;
    let lease_fd = flags
        .value("--lease-fd")?
        .parse::<RawFd>()
        .map_err(|_| "broker lease fd is not decimal".to_string())?;
    let resource_fds = parse_fd_list(flags.value("--resource-fds")?)?;
    if resource_fds.contains(&lease_fd) {
        return Err("broker lease FD collides with a held resource FD".into());
    }
    resources::validate_held_resource_fds(&session_dir.join("resources"), &resource_fds)?;
    let lease = unsafe { File::from_raw_fd(lease_fd) };
    let held_resources = resource_fds
        .into_iter()
        .map(|fd| unsafe { File::from_raw_fd(fd) })
        .collect::<Vec<_>>();
    let capability: WorkerCapabilityV1 = schema::read_jcs_file(&capability_file, None)?;
    let record_path = session_dir.join("worker-capability-record-v1.json");
    for name in ["broker-replays", "broker-intents", "broker-plans"] {
        authority::ensure_private_directory(&session_dir.join(name), roots.euid)?;
    }
    let socket = PathBuf::from(&capability.broker_socket);
    if socket.exists() {
        return Err("broker socket already exists; refusing replacement".into());
    }
    let listener = UnixListener::bind(&socket)
        .map_err(|error| format!("bind Git broker {}: {error}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod Git broker socket: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set Git broker nonblocking: {error}"))?;

    let revoked_at = loop {
        let record: schema::CapabilityRecordV1 = schema::read_jcs_file(&record_path, None)?;
        if let Some(revoked_at) = record.revoked_at {
            break revoked_at;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let handled = read_broker_request(&mut stream).and_then(|envelope| {
                    handle_broker_request(
                        &roots,
                        &session_dir,
                        &worktree,
                        &branch_ref,
                        &capability,
                        envelope,
                    )
                });
                let (response, accepted_handback) = match handled {
                    Ok(value) => value,
                    Err(error) => (
                        broker_response(
                            "00000000-0000-4000-8000-000000000000",
                            "error",
                            1,
                            "",
                            &error,
                            JcsValue::Null,
                        ),
                        false,
                    ),
                };
                let mut bytes = schema::serialize_jcs(&response).into_bytes();
                bytes.push(b'\n');
                stream
                    .write_all(&bytes)
                    .map_err(|error| format!("write Git broker response: {error}"))?;
                drop(stream);
                if accepted_handback {
                    if let Err(error) =
                        complete_handback_quiescence(&roots, &session_dir, &worktree)
                    {
                        let fault = CanonicalRecord(JcsValue::Object(BTreeMap::from([
                            ("error".into(), JcsValue::String(error)),
                            (
                                "schema".into(),
                                JcsValue::String("HandbackQuiescenceFaultV1".into()),
                            ),
                        ])));
                        if !session_dir.join("handback-quiescence-fault-v1.json").exists() {
                            authority::atomic_create_jcs(
                                &session_dir.join("handback-quiescence-fault-v1.json"),
                                &fault,
                                0o600,
                            )?;
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(format!("accept Git broker connection: {error}")),
        }
    };
    drop(listener);
    fs::remove_file(&socket)
        .map_err(|error| format!("remove drained Git broker socket: {error}"))?;
    drop(held_resources);
    drop(lease);
    let evidence = sha256::hex_digest(
        format!(
            "workspace-broker-close-v1\0{}\0{}\0{}",
            capability.session_id, capability.capability_id, revoked_at
        )
        .as_bytes(),
    );
    persist_runtime_record(
        &session_dir,
        "broker-close-v1.json",
        JcsValue::Object(BTreeMap::from([
            (
                "capability_id".into(),
                JcsValue::String(capability.capability_id),
            ),
            ("evidence_sha256".into(), JcsValue::String(evidence)),
            ("revoked_at".into(), JcsValue::String(revoked_at)),
            ("schema".into(), JcsValue::String("BrokerCloseV1".into())),
            ("session_id".into(), JcsValue::String(capability.session_id)),
        ])),
    )?;
    Ok(())
}

fn read_broker_request(stream:&mut UnixStream)->Result<JcsValue,String>{let mut bytes=Vec::new();stream.take(1024*1024).read_to_end(&mut bytes).map_err(|e|format!("read Git broker request: {e}"))?;schema::parse_jcs(&bytes,true)}
fn handle_broker_request(
    roots: &AuthorityRoots,
    session_dir: &Path,
    worktree: &Path,
    branch_ref: &str,
    capability: &WorkerCapabilityV1,
    envelope: JcsValue,
) -> Result<(JcsValue, bool), String> {
    let envelope = closed_object(
        &envelope,
        &["schema", "request", "nonce", "mac"],
        "GitBrokerEnvelopeV1",
    )?;
    let request = envelope["request"].clone();
    let object = closed_object(
        &request,
        &[
            "schema",
            "request_id",
            "session_id",
            "generation",
            "operation",
            "argv",
            "cwd",
            "capability_id",
            "request_sha256",
        ],
        "GitBrokerRequestV1",
    )?;
    let request_id = object["request_id"].as_str()?.to_string();
    LowerUuidV4::parse(&request_id)?;
    if object["session_id"].as_str()? != capability.session_id
        || object["capability_id"].as_str()? != capability.capability_id
    {
        return Err("broker request capability identity mismatch".into());
    }
    let operation = object["operation"].as_str()?;
    if !matches!(operation, "git_index" | "git_commit" | "handback") {
        return Err("broker operation is outside the closed set".into());
    }
    let mut bare = object.clone();
    let supplied_digest = bare.remove("request_sha256").unwrap().as_str()?.to_string();
    let bare = JcsValue::Object(bare);
    let expected_digest = sha256::hex_digest(
        [
            b"session-relay/broker-request/v1\0".as_slice(),
            schema::serialize_jcs(&bare).as_bytes(),
        ]
        .concat()
        .as_slice(),
    );
    if !sha256::constant_time_eq(supplied_digest.as_bytes(), expected_digest.as_bytes()) {
        return Err("broker request digest mismatch".into());
    }
    let nonce = envelope["nonce"].as_str()?;
    let mac = envelope["mac"].as_str()?;
    let secret = capability::decode_base64url(&capability.secret_b64url)?;
    let message = [
        b"session-relay/broker-envelope/v1\0".as_slice(),
        schema::serialize_jcs(&request).as_bytes(),
        nonce.as_bytes(),
    ]
    .concat();
    let expected_mac = capability::encode_base64url(&sha256::hmac(&secret, &message));
    if !sha256::constant_time_eq(mac.as_bytes(), expected_mac.as_bytes()) {
        return Err("broker envelope MAC mismatch".into());
    }
    let replay = session_dir
        .join("broker-replays")
        .join(format!("{request_id}.json"));
    let replay_digest = session_dir
        .join("broker-replays")
        .join(format!("{request_id}.sha256"));
    if replay.exists() {
        let existing = capability::read_secure_bytes(&replay_digest)?;
        if existing != format!("{supplied_digest}\n").as_bytes() {
            return Err("changed broker request replay is refused".into());
        }
        return Ok((
            schema::parse_jcs(&capability::read_secure_bytes(&replay)?, true)?,
            operation == "handback",
        ));
    }
    let record_path = session_dir.join("worker-capability-record-v1.json");
    let record: schema::CapabilityRecordV1 = schema::read_jcs_file(&record_path, None)?;
    capability::authenticate_worker(
        capability,
        &record,
        &capability.repository_id,
        &capability.session_id,
        capability
            .generation
            .parse()
            .map_err(|_| "broker generation overflow".to_string())?,
        operation,
        &authority::now_timestamp()?,
    )?;
    let argv = match &object["argv"] {
        JcsValue::Array(values) => values
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err("broker argv must be an array".into()),
    };
    let cwd = PathBuf::from(object["cwd"].as_str()?);
    if fs::canonicalize(&cwd).map_err(|error| format!("canonicalize broker cwd: {error}"))?
        != worktree
    {
        return Err("broker cwd differs from the exact workspace root".into());
    }
    let repository = git::OpenedRepository::open(worktree)?;
    let (manifest, _) = manifest_claims(session_dir)?;
    validate_broker_repository_binding(
        capability,
        &manifest,
        &repository,
        worktree,
        branch_ref,
        operation,
    )?;
    let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
    let response = match operation {
        "git_index" => broker_git_index(
            &repository,
            worktree,
            branch_ref,
            session_dir,
            &request_id,
            &supplied_digest,
            &argv,
        ),
        "git_commit" => broker_git_commit(
            &repository,
            worktree,
            branch_ref,
            session_dir,
            &request_id,
            &supplied_digest,
            &argv,
        ),
        "handback" => broker_handback(
            &repository,
            worktree,
            branch_ref,
            session_dir,
            &request_id,
            &supplied_digest,
            &argv,
        ),
        _ => unreachable!(),
    }?;
    if operation == "handback" {
        let authority = WorkspaceAuthority::new(roots.clone())?;
        let exclusion = authority.acquire_authority_exclusion(&repository.identity.repository_id)?;
        capability::revoke_worker_durable(
            &exclusion,
            &record_path,
            &capability.capability_id,
            capability
                .generation
                .parse()
                .map_err(|_| "worker generation overflow".to_string())?,
            &authority::now_timestamp()?,
        )?;
    }
    drop(gate);
    let response_record = CanonicalRecord(response.clone());
    authority::atomic_create_jcs(&replay, &response_record, 0o600)?;
    write_private_bytes(
        &replay_digest,
        format!("{supplied_digest}\n").as_bytes(),
    )?;
    Ok((response, operation == "handback"))
}

fn manifest_claims(session_dir:&Path)->Result<(WorkspaceManifestRecord,Vec<schema::PathClaimRequestV1>),String>{let path=session_dir.join("manifest-v1.json");let manifest:WorkspaceManifestRecord=schema::read_jcs_file(&path,None)?;let object=match &manifest.0{JcsValue::Object(object)=>object,_=>unreachable!()};let claims=match &object["owned_paths"]{JcsValue::Array(values)=>values.iter().map(|value|{let object=value.clone().object()?;Ok(schema::PathClaimRequestV1{path:object["path"].as_str()?.into(),path_type:object["path_type"].as_str()?.into(),mode:object["mode"].as_str()?.into()})}).collect::<Result<Vec<_>,String>>()?,_=>return Err("manifest owned_paths is not an array".into())};Ok((manifest,claims))}

fn validate_broker_identity_values(capability_repository_id:&str,actual_repository:&schema::RepositoryIdentityV1,expected_repository:&schema::RepositoryIdentityV1,actual_worktree:&schema::WorktreeIdentityV1,expected_worktree:&schema::WorktreeIdentityV1)->Result<(),String>{
 if capability_repository_id!=actual_repository.repository_id||expected_repository!=actual_repository{return Err("broker repository identity differs from capability or manifest".into())}
 if actual_worktree!=expected_worktree{return Err("broker worktree or private Git identity differs from manifest".into())}
 Ok(())
}
fn validate_broker_repository_binding(
    capability: &WorkerCapabilityV1,
    manifest: &WorkspaceManifestRecord,
    repository: &git::OpenedRepository,
    worktree: &Path,
    branch_ref: &str,
    operation: &str,
) -> Result<(), String> {
    if manifest.state()? != WorkspaceState::Running {
        return Err(format!(
            "broker {operation} requires a durably Running workspace"
        ));
    }
    let object = manifest.object()?;
    let expected_repository =
        schema::RepositoryIdentityV1::from_jcs(object["repository"].clone())?;
    let expected_worktree =
        schema::WorktreeIdentityV1::from_value(object["worktree_identity"].clone())?;
    let actual_worktree = git::worktree_identity(worktree, branch_ref)?;
    validate_broker_identity_values(
        &capability.repository_id,
        &repository.identity,
        &expected_repository,
        &actual_worktree,
        &expected_worktree,
    )
}

#[derive(Clone,Debug,Eq,PartialEq)]
struct BrokerIndexCommand{git_args:Vec<String>,paths:Vec<String>}
fn parse_broker_index_argv(argv:&[String])->Result<BrokerIndexCommand,String>{
 let (mut git_args,rest)=match argv{
  [operation,rest @ ..] if operation=="add"=>(vec!["add".into()],rest),
  [operation,cached,rest @ ..] if operation=="rm"&&cached=="--cached"=>(vec!["rm".into(),"--cached".into()],rest),
  [operation,staged,rest @ ..] if operation=="restore"&&staged=="--staged"=>(vec!["restore".into(),"--staged".into()],rest),
  _=>return Err("Git broker permits exactly add, rm --cached, or restore --staged".into()),
 };
 let paths=if rest.first().is_some_and(|arg|arg=="--"){&rest[1..]}else{rest};
 if paths.is_empty()||paths.iter().any(|arg|arg.starts_with('-')){return Err("Git broker index operation has invalid path grammar".into())}
 for path in paths{schema::RelPath::parse(path)?;if path.starts_with(':')||path.bytes().any(|byte|matches!(byte,b'*'|b'?'|b'['|b']'|b'\\')){return Err("Git broker requires literal relative path arguments".into())}}
 git_args.push("--".into());git_args.extend(paths.iter().cloned());
 Ok(BrokerIndexCommand{git_args,paths:paths.to_vec()})
}

fn broker_intent<F>(session_dir:&Path,request_id:&str,request_sha256:&str,operation:&str,argv:&[String],build_details:F)->Result<(JcsValue,bool),String>
where F:FnOnce()->Result<JcsValue,String>{
 let path=session_dir.join("broker-intents").join(format!("{request_id}.json"));
 let expected_argv=JcsValue::Array(argv.iter().cloned().map(JcsValue::String).collect());
 if path.exists(){
  let record=schema::parse_jcs(&capability::read_secure_bytes(&path)?,true)?;let object=closed_object(&record,&["schema","request_id","request_sha256","operation","argv","details"],"GitBrokerIntentV1")?;
  if object["schema"].as_str()!=Ok(schema::SCHEMA_V1)||object["request_id"].as_str()!=Ok(request_id)||object["request_sha256"].as_str()!=Ok(request_sha256)||object["operation"].as_str()!=Ok(operation)||object["argv"]!=expected_argv{return Err("changed broker request conflicts with durable mutation intent".into())}
  return Ok((object["details"].clone(),true))
 }
 let details=build_details()?;let record=CanonicalRecord(JcsValue::Object(BTreeMap::from([("argv".into(),expected_argv),("details".into(),details.clone()),("operation".into(),JcsValue::String(operation.into())),("request_id".into(),JcsValue::String(request_id.into())),("request_sha256".into(),JcsValue::String(request_sha256.into())),("schema".into(),JcsValue::String(schema::SCHEMA_V1.into()))])));
 authority::atomic_create_jcs(&path,&record,0o600)?;Ok((details,false))
}

fn read_optional_git_file(path:&Path)->Result<Option<Vec<u8>>,String>{
 let mut file=match OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(path){Ok(file)=>file,Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return Ok(None),Err(error)=>return Err(format!("securely open Git file {}: {error}",path.display()))};
 let metadata=file.metadata().map_err(|e|format!("fstat Git file {}: {e}",path.display()))?;if !metadata.is_file()||metadata.uid()!=unsafe{libc::geteuid()}||metadata.nlink()!=1{return Err(format!("Git file {} is not an EUID-owned single-link regular file",path.display()))}
 let mut bytes=Vec::new();file.read_to_end(&mut bytes).map_err(|e|format!("read Git file {}: {e}",path.display()))?;Ok(Some(bytes))
}
fn optional_digest(bytes:&Option<Vec<u8>>)->JcsValue{bytes.as_ref().map(|bytes|JcsValue::String(sha256::hex_digest(bytes))).unwrap_or(JcsValue::Null)}
fn digest_matches(value:&JcsValue,bytes:&Option<Vec<u8>>)->Result<bool,String>{match (value,bytes){(JcsValue::Null,None)=>Ok(true),(JcsValue::String(expected),Some(bytes))=>{Sha256Digest::parse(expected)?;Ok(sha256::hex_digest(bytes)==*expected)},(JcsValue::Null,Some(_))|(JcsValue::String(_),None)=>Ok(false),_=>Err("broker intent index digest has invalid nullability".into())}}

fn prepare_index_plan(worktree:&Path,plan:&Path,index_before:&Option<Vec<u8>>,command:&BrokerIndexCommand)->Result<String,String>{
 if let Some(bytes)=index_before{write_private_bytes(plan,bytes)?}else{
  let output=Command::new("git").args(["read-tree","HEAD"]).env("GIT_INDEX_FILE",plan).current_dir(worktree).stdin(Stdio::null()).output().map_err(|e|format!("prepare empty Git index plan: {e}"))?;if !output.status.success(){return Err(format!("prepare empty Git index plan failed: {}",String::from_utf8_lossy(&output.stderr).trim()))}
 }
 let args=command.git_args.iter().map(String::as_str).collect::<Vec<_>>();let output=Command::new("git").args(&args).env("GIT_INDEX_FILE",plan).current_dir(worktree).stdin(Stdio::null()).output().map_err(|e|format!("prepare Git index mutation: {e}"))?;if !output.status.success(){fs::remove_file(plan).ok();return Err(format!("Git index operation failed before publication: {}",String::from_utf8_lossy(&output.stderr).trim()))}
 fs::set_permissions(plan,fs::Permissions::from_mode(0o600)).map_err(|e|format!("chmod Git index plan: {e}"))?;let file=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(plan).map_err(|e|format!("open Git index plan: {e}"))?;file.sync_all().map_err(|e|format!("fsync Git index plan: {e}"))?;let bytes=read_optional_git_file(plan)?.ok_or_else(||"Git index plan disappeared".to_string())?;Ok(sha256::hex_digest(&bytes))
}

fn publish_index_plan(index:&Path,plan:&Path,before:&JcsValue,planned_sha256:&str)->Result<(),String>{
 let current=read_optional_git_file(index)?;if current.as_ref().is_some_and(|bytes|sha256::hex_digest(bytes)==planned_sha256){return Ok(())}if !digest_matches(before,&current)?{return Err("Git index differs from both durable intent precondition and planned result; refusing ambiguous replay".into())}
 let planned=read_optional_git_file(plan)?.ok_or_else(||"durable Git index plan is missing".to_string())?;if sha256::hex_digest(&planned)!=planned_sha256{return Err("durable Git index plan digest differs from intent".into())}
 let parent=index.parent().ok_or_else(||"Git index has no parent".to_string())?;let staging=parent.join(format!(".session-relay-index-{}",crate::store::uuid_v4()));write_private_bytes(&staging,&planned)?;
 let still_current=read_optional_git_file(index)?;if !digest_matches(before,&still_current)?{fs::remove_file(&staging).ok();return Err("Git index changed while publishing durable broker intent".into())}
 fs::rename(&staging,index).map_err(|e|format!("publish planned Git index: {e}"))?;let directory=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY).open(parent).map_err(|e|format!("open Git index directory for fsync: {e}"))?;directory.sync_all().map_err(|e|format!("fsync Git index directory: {e}"))?;
 let published=read_optional_git_file(index)?.ok_or_else(||"published Git index disappeared".to_string())?;if sha256::hex_digest(&published)!=planned_sha256{return Err("published Git index differs from durable plan".into())}Ok(())
}

fn broker_git_index(_repository:&git::OpenedRepository,worktree:&Path,branch_ref:&str,session_dir:&Path,request_id:&str,request_sha256:&str,argv:&[String])->Result<JcsValue,String>{
 let (_,claims)=manifest_claims(session_dir)?;
 let command=parse_broker_index_argv(argv)?;
 for path in &command.paths{let exact_file=claims.iter().any(|claim|claim.path_type=="file"&&claim.path==*path);if exact_file&&worktree.join(path).is_dir(){return Err("file claim cannot address a directory pathspec".into())}}
 let paths=command.paths.iter().map(|path|git::NameStatusChange{status:"A".into(),source:None,destination:path.clone()}).collect::<Vec<_>>();git::validate_changed_paths(&paths,&claims)?;
 if git::run_git_text(worktree,&["symbolic-ref","-q","HEAD"])?!=branch_ref{return Err("workspace branch changed before broker index mutation".into())}
 let index=PathBuf::from(git::run_git_text(worktree,&["rev-parse","--path-format=absolute","--git-path","index"])?);let plan=session_dir.join("broker-plans").join(format!("{request_id}.index"));
 let (details,_)=broker_intent(session_dir,request_id,request_sha256,"git_index",argv,||{
  let before=read_optional_git_file(&index)?;let planned_sha256=prepare_index_plan(worktree,&plan,&before,&command)?;
  Ok(JcsValue::Object(BTreeMap::from([("planned_index_sha256".into(),JcsValue::String(planned_sha256)),("pre_index_sha256".into(),optional_digest(&before))])))
 })?;
 let details=closed_object(&details,&["pre_index_sha256","planned_index_sha256"],"GitIndexIntentV1")?;let planned_sha256=details["planned_index_sha256"].as_str()?;Sha256Digest::parse(planned_sha256)?;
 publish_index_plan(&index,&plan,&details["pre_index_sha256"],planned_sha256)?;
 let changed=git::parse_name_status_z(&git::run_git_bytes(worktree,&["diff","--cached","--name-status","-z","--find-renames","--find-copies"])? )?;git::validate_changed_paths(&changed,&claims)?;
 Ok(broker_response(request_id,"ok",0,"","",JcsValue::Null))
}

fn validate_planned_commit(worktree:&Path,commit:&str,pre_head:&str,tree:&str,timestamp:&str,message:&str)->Result<(),String>{
 if git::run_git_text(worktree,&["rev-parse","--verify",&format!("{commit}^")])?!=pre_head||git::run_git_text(worktree,&["rev-parse","--verify",&format!("{commit}^{{tree}}")])?!=tree{return Err("advanced broker branch does not match the durable commit intent".into())}
 for (format,expected) in [("%an","Session Relay"),("%ae","session-relay@localhost"),("%cn","Session Relay"),("%ce","session-relay@localhost")]{if git::run_git_text(worktree,&["show","-s",&format!("--format={format}"),commit])?!=expected{return Err("advanced broker commit identity differs from durable intent".into())}}
 let expected_time=format!("{}+00:00",&timestamp[..19]);for format in ["%aI","%cI"]{if git::run_git_text(worktree,&["show","-s",&format!("--format={format}"),commit])?!=expected_time{return Err("advanced broker commit timestamp differs from durable intent".into())}}
 if git::run_git_text(worktree,&["show","-s","--format=%B",commit])?!=message.trim_end_matches(['\r','\n']){return Err("advanced broker commit message differs from durable intent".into())}Ok(())
}

fn broker_git_commit(repository:&git::OpenedRepository,worktree:&Path,branch_ref:&str,session_dir:&Path,request_id:&str,request_sha256:&str,argv:&[String])->Result<JcsValue,String>{
 let (_,claims)=manifest_claims(session_dir)?;
 if argv.len()!=3||argv[0]!="commit"||argv[1]!="-m"{return Err("Git broker commit grammar is exactly commit -m <message>".into())}
 let changed=git::parse_name_status_z(&git::run_git_bytes(worktree,&["diff","--cached","--name-status","-z","--find-renames","--find-copies"])? )?;
 if changed.is_empty(){return Err("Git broker refuses an empty worker commit".into())}
 git::validate_changed_paths(&changed,&claims)?;
 if git::run_git_text(worktree,&["symbolic-ref","-q","HEAD"])?!=branch_ref{return Err("workspace branch changed before broker commit".into())}
 let (details,_)=broker_intent(session_dir,request_id,request_sha256,"git_commit",argv,||{
  let pre_head=git::run_git_text(worktree,&["rev-parse","--verify","HEAD"])?;let tree=git::run_git_text(worktree,&["write-tree"])?;let timestamp=authority::now_timestamp()?;
  Ok(JcsValue::Object(BTreeMap::from([("message".into(),JcsValue::String(argv[2].clone())),("pre_head".into(),JcsValue::String(pre_head)),("timestamp".into(),JcsValue::String(timestamp)),("tree".into(),JcsValue::String(tree))])))
 })?;
 let details=closed_object(&details,&["message","pre_head","timestamp","tree"],"GitCommitIntentV1")?;let pre_head=details["pre_head"].as_str()?;let tree=details["tree"].as_str()?;let timestamp=details["timestamp"].as_str()?;let message=details["message"].as_str()?;schema::Timestamp::parse(timestamp)?;repository.validate_oid(pre_head)?;repository.validate_oid(tree)?;
 if git::run_git_text(worktree,&["write-tree"])?!=tree{return Err("Git index tree changed after durable commit intent".into())}
 let head=git::run_git_text(worktree,&["rev-parse","--verify","HEAD"])?;let commit=if head==pre_head{git::create_worker_commit(repository,worktree,branch_ref,message,timestamp)?}else{validate_planned_commit(worktree,&head,pre_head,tree,timestamp,message)?;head};
 Ok(broker_response(request_id,"ok",0,&format!("{commit}\n"),"",JcsValue::Null))
}

fn broker_handback(
    repository: &git::OpenedRepository,
    worktree: &Path,
    branch_ref: &str,
    session_dir: &Path,
    request_id: &str,
    request_sha256: &str,
    argv: &[String],
) -> Result<JcsValue, String> {
    if argv.len() != 2 {
        return Err("broker handback requires request path and digest".into());
    }
    let request_path = Path::new(&argv[0]);
    let bytes = capability::read_secure_bytes(request_path)
        .map_err(|error| format!("read handback request: {error}"))?;
    if sha256::hex_digest(&bytes) != argv[1] {
        return Err("handback request digest mismatch".into());
    }
    let request = HandbackRequestV1::from_jcs(schema::parse_jcs(&bytes, true)?)?;
    if request.session_id
        != session_dir
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("")
    {
        return Err("handback request session differs from broker".into());
    }
    if git::run_git_text(worktree, &["symbolic-ref", "-q", "HEAD"])? != branch_ref {
        return Err("workspace branch changed before handback".into());
    }
    if !git::run_git_bytes(worktree, &["status", "--porcelain=v2", "-z"])?.is_empty() {
        return Err("workspace is dirty at handback".into());
    }
    let head = git::run_git_text(worktree, &["rev-parse", "--verify", "HEAD"])?;
    if head != request.expected_head {
        return Err("handback expected HEAD differs from workspace HEAD".into());
    }
    repository.validate_oid(&head)?;
    let (manifest, claims) = manifest_claims(session_dir)?;
    if manifest.state()? != WorkspaceState::Running {
        return Err("handback requires a durably Running workspace".into());
    }
    let manifest_object = manifest.object()?;
    let base = manifest_object["worker_base_commit"].as_str()?;
    let changes = git::parse_name_status_z(&git::run_git_bytes(
        worktree,
        &[
            "diff",
            "--name-status",
            "-z",
            "--find-renames",
            "--find-copies",
            &format!("{base}..{head}"),
        ],
    )?)?;
    git::validate_changed_paths(&changes, &claims)?;
    let worker_commits = git::run_git_text(
        worktree,
        &["rev-list", "--reverse", &format!("{base}..{head}")],
    )?
    .lines()
    .map(str::to_string)
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>();
    for commit in &worker_commits {
        let parents =
            git::run_git_text(worktree, &["rev-list", "--parents", "-n", "1", commit])?;
        if parents.split_whitespace().count() != 2 {
            return Err("handback produced history is not linear and merge-free".into());
        }
    }
    let applied = manifest_object["applied_wip_commit"].as_str()?.to_string();
    let mut produced_commits = Vec::with_capacity(worker_commits.len() + 1);
    produced_commits.push(applied);
    produced_commits.extend(worker_commits);
    let receipt = HandbackReceiptV1 {
        request_id: request.request_id,
        session_id: request.session_id,
        head_oid: head,
        outcome: "validated".into(),
        produced_commits,
        created_at: request.created_at,
    };
    receipt.validate()?;
    let receipt_path = session_dir.join("handback-receipt-v1.json");
    let (_, existing_intent) = broker_intent(
        session_dir,
        request_id,
        request_sha256,
        "handback",
        argv,
        || Ok(JcsValue::Object(BTreeMap::new())),
    )?;
    if receipt_path.exists() {
        if !existing_intent {
            return Err("handback receipt predates its durable broker intent".into());
        }
        let existing: HandbackReceiptV1 = schema::read_jcs_file(&receipt_path, None)?;
        if existing != receipt {
            return Err("handback receipt differs from durable replay result".into());
        }
        return Ok(broker_response(
            request_id,
            "ok",
            0,
            "",
            "",
            existing.to_jcs(),
        ));
    }
    authority::atomic_create_jcs(&receipt_path, &receipt, 0o600)?;
    Ok(broker_response(
        request_id,
        "ok",
        0,
        "",
        "",
        receipt.to_jcs(),
    ))
}

fn broker_response(request_id:&str,status:&str,exit_code:i32,stdout:&str,stderr:&str,receipt:JcsValue)->JcsValue{
 JcsValue::Object(BTreeMap::from([("exit_code".into(),JcsValue::String(exit_code.to_string())),("receipt".into(),receipt),("request_id".into(),JcsValue::String(request_id.into())),("schema".into(),JcsValue::String(schema::SCHEMA_V1.into())),("status".into(),JcsValue::String(status.into())),("stderr".into(),JcsValue::String(stderr.into())),("stdout".into(),JcsValue::String(stdout.into()))]))
}

fn run_broker_client(raw:&[String])->Result<i32,String>{
 let divider=raw.iter().position(|arg|arg=="--").ok_or_else(||"broker client requires -- before Git arguments".to_string())?;let flags=Flags::parse(&raw[..divider],&["--worker-capability-file"])?;let capability_file=flags.absolute("--worker-capability-file")?;let argv=raw[divider+1..].to_vec();
 let operation=match argv.first().map(String::as_str){Some("add"|"rm"|"restore")=>{parse_broker_index_argv(&argv)?;"git_index"},Some("commit")=>"git_commit",Some(other)=>return Err(format!("Git operation {other} is refused by the closed broker")),None=>return Err("Git broker requires an operation".into())};
 let capability:WorkerCapabilityV1=schema::read_jcs_file(&capability_file,None)?;let response=broker_exchange(&capability,operation,argv,std::env::current_dir().map_err(|e|format!("resolve broker client cwd: {e}"))?)?;let object=match response{JcsValue::Object(object)=>object,_=>return Err("broker response is not an object".into())};
 print!("{}",object["stdout"].as_str()?);eprint!("{}",object["stderr"].as_str()?);object["exit_code"].as_str()?.parse().map_err(|_|"broker exit code is invalid".to_string())
}
pub fn run(raw: Vec<String>) -> ! {
 if raw.first().map(String::as_str)==Some("__guardian"){
  match run_guardian(&raw[1..]){Ok(())=>std::process::exit(0),Err(error)=>{eprintln!("{error}");std::process::exit(1)}}
 }
 if raw.first().map(String::as_str)==Some("__custody-supervisor"){
  match run_custody_supervisor(&raw[1..]){Ok(())=>std::process::exit(0),Err(error)=>{eprintln!("{error}");std::process::exit(1)}}
 }
 if raw.first().map(String::as_str)==Some("__broker"){
  match run_broker(&raw[1..]){Ok(())=>std::process::exit(0),Err(error)=>{eprintln!("{error}");std::process::exit(1)}}
 }
 if raw.first().map(String::as_str)==Some("__broker-client"){
  match run_broker_client(&raw[1..]){Ok(code)=>std::process::exit(code),Err(error)=>{eprintln!("{error}");std::process::exit(1)}}
 }
 let result=parse_command(&raw).and_then(execute);
 match result { Ok(output)=>{print!("{output}");std::process::exit(0)},Err(error)=>{eprintln!("{error}");std::process::exit(1)} }
}

#[cfg(test)]
mod tests {
 use super::*;
 #[test] fn exact_router_is_closed(){let args=vec!["preserve".into(),"--request-file".into(),"/tmp/request.json".into(),"--request-sha256".into(),"a".repeat(64)];assert!(matches!(parse_command(&args),Ok(WorkspaceCommand::Preserve{..})));let mut bad=args;bad.extend(["--extra".into(),"x".into()]);assert!(parse_command(&bad).is_err());}
 #[test] fn manifest_wip_fields_are_state_coupled(){
  let oid=JcsValue::String("a".repeat(40));
  assert!(validate_manifest_wip_nullability(true,&JcsValue::Null,&JcsValue::Null).is_ok());
  assert!(validate_manifest_wip_nullability(false,&oid,&oid).is_ok());
  assert!(validate_manifest_wip_nullability(true,&JcsValue::Null,&oid).is_err());
  assert!(validate_manifest_wip_nullability(true,&oid,&JcsValue::Null).is_err());
  assert!(validate_manifest_wip_nullability(false,&JcsValue::Null,&JcsValue::Null).is_err());
 }
 #[test] fn broker_index_grammar_is_exact(){
  let strings=|values:&[&str]|values.iter().map(|value|value.to_string()).collect::<Vec<_>>();
  assert_eq!(parse_broker_index_argv(&strings(&["add","--","src/a.rs"])).unwrap().git_args,strings(&["add","--","src/a.rs"]));
  assert_eq!(parse_broker_index_argv(&strings(&["rm","--cached","src/a.rs"])).unwrap().git_args,strings(&["rm","--cached","--","src/a.rs"]));
  assert_eq!(parse_broker_index_argv(&strings(&["restore","--staged","--","src/a.rs"])).unwrap().git_args,strings(&["restore","--staged","--","src/a.rs"]));
  for refused in [strings(&["mv","a","b"]),strings(&["rm","a"]),strings(&["restore","a"]),strings(&["add","-A"])]{assert!(parse_broker_index_argv(&refused).is_err())}
 }
 #[test] fn broker_identity_binds_repository_and_private_git_inode(){
  use schema::{ObjectFormat,RepositoryIdentityV1,WorktreeIdentityV1};
  let repository=RepositoryIdentityV1{repository_id:"a".repeat(64),common_dir_realpath:"/repo/.git".into(),common_dir_dev:"1".into(),common_dir_ino:"2".into(),common_dir_owner_euid:"3".into(),euid:"3".into(),object_format:ObjectFormat::Sha1};
  let worktree=WorktreeIdentityV1{identity_sha256:"b".repeat(64),root_realpath:"/repo/w".into(),root_dev:"4".into(),root_ino:"5".into(),root_owner_euid:"3".into(),private_git_dir_realpath:"/repo/.git/worktrees/w".into(),private_git_dir_dev:"1".into(),private_git_dir_ino:"6".into(),branch_ref:"refs/heads/docks/x/task".into()};
  assert!(validate_broker_identity_values(&repository.repository_id,&repository,&repository,&worktree,&worktree).is_ok());
  let mut replaced=worktree.clone();replaced.private_git_dir_ino="7".into();assert!(validate_broker_identity_values(&repository.repository_id,&repository,&repository,&replaced,&worktree).is_err());
  assert!(validate_broker_identity_values(&"c".repeat(64),&repository,&repository,&worktree,&worktree).is_err());
 }
 #[test] fn durable_broker_intent_precedes_and_authenticates_replay(){
  let root=std::env::temp_dir().join(format!("session-relay-intent-{}",crate::store::uuid_v4()));authority::ensure_private_directory(&root.join("broker-intents"),unsafe{libc::geteuid()}).unwrap();
  let request_id="11111111-1111-4111-8111-111111111111";let argv=vec!["commit".into(),"-m".into(),"message".into()];let details=JcsValue::Object(BTreeMap::from([("pre_head".into(),JcsValue::String("a".repeat(40)))]));
  let (created,existing)=broker_intent(&root,request_id,&"b".repeat(64),"git_commit",&argv,||Ok(details.clone())).unwrap();assert_eq!(created,details);assert!(!existing);assert!(root.join("broker-intents").join(format!("{request_id}.json")).exists());
  let (replayed,existing)=broker_intent(&root,request_id,&"b".repeat(64),"git_commit",&argv,||panic!("durable replay must not rebuild intent")).unwrap();assert_eq!(replayed,details);assert!(existing);
  assert!(broker_intent(&root,request_id,&"c".repeat(64),"git_commit",&argv,||Ok(JcsValue::Null)).is_err());
  fs::remove_dir_all(root).unwrap();
 }
 #[test] fn durable_index_plan_reconciles_pre_and_post_mutation_states(){
  let root=std::env::temp_dir().join(format!("session-relay-index-plan-{}",crate::store::uuid_v4()));fs::create_dir(&root).unwrap();let index=root.join("index");let plan=root.join("plan");fs::write(&index,b"before").unwrap();fs::write(&plan,b"planned").unwrap();
  let before=JcsValue::String(sha256::hex_digest(b"before"));let planned=sha256::hex_digest(b"planned");publish_index_plan(&index,&plan,&before,&planned).unwrap();assert_eq!(fs::read(&index).unwrap(),b"planned");
  publish_index_plan(&index,&plan,&before,&planned).unwrap();fs::write(&index,b"unrelated").unwrap();assert!(publish_index_plan(&index,&plan,&before,&planned).is_err());fs::remove_dir_all(root).unwrap();
 }
 #[test] fn git_shim_is_create_new_no_follow_and_fsynced_at_final_mode(){
  use std::os::unix::fs::symlink;
  let root=std::env::temp_dir().join(format!("session-relay-shim-{}",crate::store::uuid_v4()));fs::create_dir(&root).unwrap();let output=Command::new("git").args(["init","-q"]).current_dir(&root).output().unwrap();assert!(output.status.success());
  let capability=root.join("capability.json");let shim=create_git_shim(&root,&capability,Path::new("/bin/true")).unwrap();let metadata=fs::symlink_metadata(&shim).unwrap();assert!(metadata.is_file());assert_eq!(metadata.mode()&0o777,0o500);assert_eq!(metadata.nlink(),1);
  fs::remove_file(&shim).unwrap();let outside=root.join("outside");fs::write(&outside,b"unchanged").unwrap();symlink(&outside,&shim).unwrap();assert!(create_git_shim(&root,&capability,Path::new("/bin/true")).is_err());assert_eq!(fs::read(&outside).unwrap(),b"unchanged");
  fs::remove_dir_all(root).unwrap();
 }
}
