pub mod authority;
pub mod capability;
pub mod custody;
pub mod git;
pub mod repository_gate;
pub mod platform;
pub mod schema;

use schema::{AbsPath, LowerUuidV4, Sha256Digest};
use std::path::PathBuf;

pub const WORKSPACE_HELP: &str = "usage:\n  session-relay workspace preserve --request-file <absolute-file> --request-sha256 <sha256>\n  session-relay workspace start --request-file <absolute-file> --request-sha256 <sha256> [--coordinator-capability-file <absolute-file>]\n  session-relay workspace list --repository <canonical-root> --coordinator-capability-file <absolute-file>\n  session-relay workspace inspect <session-id> --repository <canonical-root> --coordinator-capability-file <absolute-file>\n  session-relay workspace handback --request-file <absolute-file> --request-sha256 <sha256> --worker-capability-file <absolute-file>\n  session-relay workspace integrate|recover|finish|abort --request-file <absolute-file> --request-sha256 <sha256> --coordinator-capability-file <absolute-file>";

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

pub fn execute(command: WorkspaceCommand) -> Result<String, String> {
    match command {
        WorkspaceCommand::Preserve { .. } => Err("workspace preserve is disabled until repository admission is complete".to_string()),
        WorkspaceCommand::Start { .. } => Err("workspace start is disabled until repository admission is complete".to_string()),
        WorkspaceCommand::List { .. } => Err("workspace list is disabled until repository admission is complete".to_string()),
        WorkspaceCommand::Inspect { .. } => Err("workspace inspect is disabled until repository admission is complete".to_string()),
        WorkspaceCommand::Handback { .. } => Err("workspace handback is disabled until broker admission is complete".to_string()),
        WorkspaceCommand::Coordinator { operation, .. } => Err(format!("workspace {} is disabled until coordinator integration is complete",operation.action())),
    }
}

pub fn run(raw: Vec<String>) -> ! {
    let result=parse_command(&raw).and_then(execute);
    match result { Ok(output)=>{print!("{output}");std::process::exit(0)},Err(error)=>{eprintln!("{error}");std::process::exit(1)} }
}

#[cfg(test)] mod tests { use super::*; #[test] fn exact_router_is_closed(){let args=vec!["preserve".into(),"--request-file".into(),"/tmp/request.json".into(),"--request-sha256".into(),"a".repeat(64)];assert!(matches!(parse_command(&args),Ok(WorkspaceCommand::Preserve{..})));let mut bad=args;bad.extend(["--extra".into(),"x".into()]);assert!(parse_command(&bad).is_err());} }
