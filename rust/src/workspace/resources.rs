use super::authority::{self, AuthorityRoots};
use super::capability;
use super::schema::{
    self, ClosedJcs, EnvProjectionV1, ProviderReceiptV1, ProviderRequestV1, ResourceAllocationV1,
    ResourceDecisionV1, ResourceProviderRegistrationV1, ResourceProviderRegistryV1, ToolLaunchV1,
};
use crate::sha256;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::{Child, Command, Stdio};
#[cfg(target_os = "linux")]
use std::thread;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

pub const PROVIDER_REGISTRY_FILE: &str = "resource-provider-registry-v1.json";
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(5);
const PROVIDER_TERMINATION_GRACE: Duration = Duration::from_millis(100);
const PROVIDER_PIPE_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const PROVIDER_OUTPUT_MAX: u64 = 64 * 1024;
const BUILTIN_PROVIDER: &str = "builtin";

#[derive(Debug)]
pub struct ResourceSet {
    pub decisions: Vec<ResourceDecisionV1>,
    pub allocations: Vec<ResourceAllocationV1>,
    pub environment: BTreeMap<String, String>,
    pub resource_fds: Vec<RawFd>,
    receipt_dir: PathBuf,
    resource_root: PathBuf,
    session_id: String,
    inventory_created_at: String,
    ports: BTreeMap<String, TcpListener>,
    provider_registrations: BTreeMap<String, ResourceProviderRegistrationV1>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceReleaseEvidenceV1 {
    pub broker_close_sha256: String,
    pub runtime_empty_sha256: String,
}

impl ResourceReleaseEvidenceV1 {
    fn validate(&self) -> Result<(), String> {
        schema::Sha256Digest::parse(&self.broker_close_sha256)?;
        schema::Sha256Digest::parse(&self.runtime_empty_sha256)?;
        if self.broker_close_sha256 == self.runtime_empty_sha256 {
            return Err("broker-close and runtime-empty evidence must be distinct".into());
        }
        Ok(())
    }
}

pub struct ToolLaunchContext<'a> {
    pub tool: &'a ToolLaunchV1,
    pub session_id: &'a str,
    pub workspace: &'a Path,
    pub prompt: &'a str,
    pub generated_policy: &'a str,
    pub git_shim_dir: &'a Path,
    pub worker_capability_file: &'a Path,
    pub resources: &'a ResourceSet,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedToolLaunch {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub resource_fds: Vec<RawFd>,
    pub cwd: PathBuf,
    pub writable_resources: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolLaunchDecisionV1 {
    pub session_id: String,
    pub kind: String,
    pub executable_path: String,
    pub executable_sha256: String,
    pub arguments: Vec<String>,
    pub cwd: String,
    pub environment: Vec<EnvProjectionV1>,
    pub resource_fds: Vec<String>,
    pub writable_resources: Vec<String>,
    pub created_at: String,
}

impl ClosedJcs for ToolLaunchDecisionV1 {
    fn from_jcs(value: schema::JcsValue) -> Result<Self, String> {
        let object = value.object()?;
        let keys = [
            "schema",
            "session_id",
            "kind",
            "executable_path",
            "executable_sha256",
            "arguments",
            "cwd",
            "environment",
            "resource_fds",
            "writable_resources",
            "created_at",
        ];
        if object.len() != keys.len() || keys.iter().any(|key| !object.contains_key(*key)) {
            return Err("ToolLaunchDecisionV1 keys differ from the closed schema".into());
        }
        if object["schema"].as_str()? != "ToolLaunchDecisionV1" {
            return Err("ToolLaunchDecisionV1 schema mismatch".into());
        }
        let session_id = object["session_id"].as_str()?.to_string();
        schema::LowerUuidV4::parse(&session_id)?;
        let kind = object["kind"].as_str()?.to_string();
        if !matches!(kind.as_str(), "claude" | "codex" | "omp") {
            return Err("launch decision kind is invalid".into());
        }
        let executable_path = object["executable_path"].as_str()?.to_string();
        schema::AbsPath::parse(&executable_path)?;
        let executable_sha256 = object["executable_sha256"].as_str()?.to_string();
        schema::Sha256Digest::parse(&executable_sha256)?;
        let arguments = string_array(&object["arguments"], "launch arguments")?;
        let cwd = object["cwd"].as_str()?.to_string();
        schema::AbsPath::parse(&cwd)?;
        let environment = match &object["environment"] {
            schema::JcsValue::Array(values) => values
                .iter()
                .cloned()
                .map(EnvProjectionV1::from_value)
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("launch environment must be an array".into()),
        };
        let resource_fds = string_array(&object["resource_fds"], "launch resource_fds")?;
        for value in &resource_fds {
            let fd = value
                .parse::<RawFd>()
                .map_err(|_| "launch resource FD is not decimal".to_string())?;
            if fd < 3 || fd.to_string() != *value {
                return Err("launch resource FD is not canonical".into());
            }
        }
        let writable_resources =
            string_array(&object["writable_resources"], "launch writable_resources")?;
        for value in &writable_resources {
            schema::AbsPath::parse(value)?;
        }
        let created_at = object["created_at"].as_str()?.to_string();
        schema::Timestamp::parse(&created_at)?;
        Ok(Self {
            session_id,
            kind,
            executable_path,
            executable_sha256,
            arguments,
            cwd,
            environment,
            resource_fds,
            writable_resources,
            created_at,
        })
    }
    fn to_jcs(&self) -> schema::JcsValue {
        schema::JcsValue::Object(BTreeMap::from([
            ("arguments".into(), strings_value(&self.arguments)),
            (
                "created_at".into(),
                schema::JcsValue::String(self.created_at.clone()),
            ),
            ("cwd".into(), schema::JcsValue::String(self.cwd.clone())),
            (
                "environment".into(),
                schema::JcsValue::Array(
                    self.environment
                        .iter()
                        .map(EnvProjectionV1::value)
                        .collect(),
                ),
            ),
            (
                "executable_path".into(),
                schema::JcsValue::String(self.executable_path.clone()),
            ),
            (
                "executable_sha256".into(),
                schema::JcsValue::String(self.executable_sha256.clone()),
            ),
            ("kind".into(), schema::JcsValue::String(self.kind.clone())),
            ("resource_fds".into(), strings_value(&self.resource_fds)),
            (
                "schema".into(),
                schema::JcsValue::String("ToolLaunchDecisionV1".into()),
            ),
            (
                "session_id".into(),
                schema::JcsValue::String(self.session_id.clone()),
            ),
            (
                "writable_resources".into(),
                strings_value(&self.writable_resources),
            ),
        ]))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AllocationInventoryV1 {
    session_id: String,
    decisions: Vec<ResourceDecisionV1>,
    allocations: Vec<ResourceAllocationV1>,
    created_at: String,
}

impl ClosedJcs for AllocationInventoryV1 {
    fn from_jcs(value: schema::JcsValue) -> Result<Self, String> {
        let object = value.object()?;
        let expected = [
            "schema",
            "session_id",
            "decisions",
            "allocations",
            "created_at",
        ];
        if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
            return Err("ResourceInventoryV1 keys differ from the closed schema".into());
        }
        if object["schema"].as_str()? != "ResourceInventoryV1" {
            return Err("ResourceInventoryV1 schema mismatch".into());
        }
        let session_id = object["session_id"].as_str()?.to_string();
        schema::LowerUuidV4::parse(&session_id)?;
        let decisions = match &object["decisions"] {
            schema::JcsValue::Array(values) => values
                .iter()
                .cloned()
                .map(ResourceDecisionV1::from_value)
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("resource inventory decisions must be an array".into()),
        };
        let allocations = match &object["allocations"] {
            schema::JcsValue::Array(values) => values
                .iter()
                .cloned()
                .map(ResourceAllocationV1::from_jcs)
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("resource inventory allocations must be an array".into()),
        };
        let created_at = object["created_at"].as_str()?.to_string();
        schema::Timestamp::parse(&created_at)?;
        Ok(Self {
            session_id,
            decisions,
            allocations,
            created_at,
        })
    }

    fn to_jcs(&self) -> schema::JcsValue {
        schema::JcsValue::Object(BTreeMap::from([
            (
                "allocations".into(),
                schema::JcsValue::Array(self.allocations.iter().map(ClosedJcs::to_jcs).collect()),
            ),
            (
                "created_at".into(),
                schema::JcsValue::String(self.created_at.clone()),
            ),
            (
                "decisions".into(),
                schema::JcsValue::Array(
                    self.decisions
                        .iter()
                        .map(ResourceDecisionV1::value)
                        .collect(),
                ),
            ),
            (
                "schema".into(),
                schema::JcsValue::String("ResourceInventoryV1".into()),
            ),
            (
                "session_id".into(),
                schema::JcsValue::String(self.session_id.clone()),
            ),
        ]))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DeletionIntentV1 {
    allocation_id: String,
    kind: String,
    provider_id: String,
    value: String,
    provider_evidence_sha256: String,
    prior_receipt_sha256: String,
    request_id: String,
    mode: String,
    released_at: String,
    broker_close_sha256: Option<String>,
    runtime_empty_sha256: Option<String>,
}

impl ClosedJcs for DeletionIntentV1 {
    fn from_jcs(value: schema::JcsValue) -> Result<Self, String> {
        let object = value.object()?;
        let expected = [
            "schema",
            "allocation_id",
            "kind",
            "provider_id",
            "value",
            "provider_evidence_sha256",
            "prior_receipt_sha256",
            "request_id",
            "mode",
            "released_at",
            "broker_close_sha256",
            "runtime_empty_sha256",
        ];
        if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
            return Err("ResourceDeletionIntentV1 keys differ from the closed schema".into());
        }
        if object["schema"].as_str()? != "ResourceDeletionIntentV1" {
            return Err("ResourceDeletionIntentV1 schema mismatch".into());
        }
        let allocation_id = object["allocation_id"].as_str()?.to_string();
        let request_id = object["request_id"].as_str()?.to_string();
        schema::LowerUuidV4::parse(&allocation_id)?;
        schema::LowerUuidV4::parse(&request_id)?;
        let kind = object["kind"].as_str()?.to_string();
        if !matches!(
            kind.as_str(),
            "port" | "temp_dir" | "build_dir" | "log_dir" | "cache_dir" | "database_schema"
        ) {
            return Err("resource deletion intent kind is invalid".into());
        }
        let provider_id = object["provider_id"].as_str()?.to_string();
        if provider_id.is_empty() {
            return Err("resource deletion intent provider_id is empty".into());
        }
        let value = object["value"].as_str()?.to_string();
        if value.is_empty() || value.as_bytes().contains(&0) {
            return Err("resource deletion intent value is invalid".into());
        }
        let provider_evidence_sha256 = object["provider_evidence_sha256"].as_str()?.to_string();
        let prior_receipt_sha256 = object["prior_receipt_sha256"].as_str()?.to_string();
        schema::Sha256Digest::parse(&provider_evidence_sha256)?;
        schema::Sha256Digest::parse(&prior_receipt_sha256)?;
        let mode = object["mode"].as_str()?.to_string();
        if !matches!(mode.as_str(), "release" | "legacy_release" | "rollback") {
            return Err("resource deletion intent mode is invalid".into());
        }
        let released_at = object["released_at"].as_str()?.to_string();
        schema::Timestamp::parse(&released_at)?;
        let optional_digest = |key: &str| -> Result<Option<String>, String> {
            match &object[key] {
                schema::JcsValue::Null => Ok(None),
                schema::JcsValue::String(value) => {
                    schema::Sha256Digest::parse(value)?;
                    Ok(Some(value.clone()))
                }
                _ => Err(format!("{key} must be a SHA-256 string or null")),
            }
        };
        let broker_close_sha256 = optional_digest("broker_close_sha256")?;
        let runtime_empty_sha256 = optional_digest("runtime_empty_sha256")?;
        if (mode == "release") != (broker_close_sha256.is_some() && runtime_empty_sha256.is_some())
        {
            return Err("resource deletion intent close evidence disagrees with mode".into());
        }
        if broker_close_sha256 == runtime_empty_sha256 && broker_close_sha256.is_some() {
            return Err("resource deletion intent close evidence must be distinct".into());
        }
        Ok(Self {
            allocation_id,
            kind,
            provider_id,
            value,
            provider_evidence_sha256,
            prior_receipt_sha256,
            request_id,
            mode,
            released_at,
            broker_close_sha256,
            runtime_empty_sha256,
        })
    }

    fn to_jcs(&self) -> schema::JcsValue {
        schema::JcsValue::Object(BTreeMap::from([
            (
                "allocation_id".into(),
                schema::JcsValue::String(self.allocation_id.clone()),
            ),
            (
                "broker_close_sha256".into(),
                self.broker_close_sha256
                    .clone()
                    .map(schema::JcsValue::String)
                    .unwrap_or(schema::JcsValue::Null),
            ),
            ("kind".into(), schema::JcsValue::String(self.kind.clone())),
            ("mode".into(), schema::JcsValue::String(self.mode.clone())),
            (
                "prior_receipt_sha256".into(),
                schema::JcsValue::String(self.prior_receipt_sha256.clone()),
            ),
            (
                "provider_evidence_sha256".into(),
                schema::JcsValue::String(self.provider_evidence_sha256.clone()),
            ),
            (
                "provider_id".into(),
                schema::JcsValue::String(self.provider_id.clone()),
            ),
            (
                "released_at".into(),
                schema::JcsValue::String(self.released_at.clone()),
            ),
            (
                "request_id".into(),
                schema::JcsValue::String(self.request_id.clone()),
            ),
            (
                "runtime_empty_sha256".into(),
                self.runtime_empty_sha256
                    .clone()
                    .map(schema::JcsValue::String)
                    .unwrap_or(schema::JcsValue::Null),
            ),
            (
                "schema".into(),
                schema::JcsValue::String("ResourceDeletionIntentV1".into()),
            ),
            ("value".into(), schema::JcsValue::String(self.value.clone())),
        ]))
    }
}

struct VerifiedProviderFiles {
    executable: File,
    config: File,
}

pub fn provider_registry_path(roots: &AuthorityRoots) -> PathBuf {
    roots.authority.join(PROVIDER_REGISTRY_FILE)
}

pub fn allocate_resources(
    roots: &AuthorityRoots,
    repository_id: &str,
    session_id: &str,
    decisions: &[ResourceDecisionV1],
    receipt_dir: &Path,
    created_at: &str,
) -> Result<ResourceSet, String> {
    schema::Sha256Digest::parse(repository_id)?;
    schema::LowerUuidV4::parse(session_id)?;
    schema::Timestamp::parse(created_at)?;
    preflight_resources(roots, decisions)?;
    authority::ensure_private_directory(receipt_dir, roots.euid)?;
    let resource_root = roots
        .data
        .join(repository_id)
        .join("resources")
        .join(session_id);
    if resource_root.exists() || receipt_dir.join("resource-inventory-v1.json").exists() {
        return Err("resource allocation already exists; inspect it before recovery".into());
    }
    let registry = load_registry_if_needed(roots, decisions)?;
    validate_registry_for_decisions(registry.as_ref(), decisions)?;
    let resource_parent = roots.data.join(repository_id).join("resources");
    ensure_private_ancestors(&resource_parent, &roots.data, roots.euid)?;
    fs::create_dir(&resource_root).map_err(|error| {
        format!(
            "create session resource root {}: {error}",
            resource_root.display()
        )
    })?;
    fs::set_permissions(&resource_root, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("chmod session resource root: {error}"))?;
    authority::ensure_private_directory(&resource_root, roots.euid)?;
    let mut set = ResourceSet {
        decisions: decisions.to_vec(),
        allocations: Vec::new(),
        environment: BTreeMap::new(),
        resource_fds: Vec::new(),
        receipt_dir: receipt_dir.to_path_buf(),
        resource_root,
        session_id: session_id.to_string(),
        inventory_created_at: created_at.to_string(),
        ports: BTreeMap::new(),
        provider_registrations: BTreeMap::new(),
    };
    for decision in decisions
        .iter()
        .filter(|decision| decision.state == "requested")
    {
        let result = match decision.kind.as_str() {
            "port" => set.allocate_port(session_id, decision, created_at),
            "temp_dir" | "build_dir" | "log_dir" | "cache_dir" => {
                set.allocate_directory(session_id, decision, created_at)
            }
            "database_schema" => {
                let registry = registry
                    .as_ref()
                    .ok_or_else(|| "database provider registry is unavailable".to_string())?;
                set.allocate_database(session_id, decision, registry, created_at)
            }
            _ => Err("unknown requested resource kind".into()),
        };
        if let Err(error) = result {
            let rollback = set.rollback_unpublished(roots.euid);
            return Err(match rollback {
                Ok(()) => error,
                Err(rollback) => {
                    format!("{error}; resource rollback could not be proven: {rollback}")
                }
            });
        }
    }
    let publication = set.validate_projection().and_then(|()| {
        let inventory = AllocationInventoryV1 {
            session_id: session_id.to_string(),
            decisions: set.decisions.clone(),
            allocations: set.allocations.clone(),
            created_at: created_at.to_string(),
        };
        authority::atomic_create_jcs(
            &receipt_dir.join("resource-inventory-v1.json"),
            &inventory,
            0o600,
        )
    });
    if let Err(error) = publication {
        let rollback = set.rollback_unpublished(roots.euid);
        return Err(match rollback {
            Ok(()) => error,
            Err(rollback) => {
                format!("{error}; resource rollback could not be proven: {rollback}")
            }
        });
    }
    Ok(set)
}

pub fn load_resources(
    roots: &AuthorityRoots,
    repository_id: &str,
    session_id: &str,
    receipt_dir: &Path,
) -> Result<ResourceSet, String> {
    schema::Sha256Digest::parse(repository_id)?;
    schema::LowerUuidV4::parse(session_id)?;
    if roots.euid != unsafe { libc::geteuid() } {
        return Err("resource authority EUID differs from the process EUID".into());
    }
    require_existing_private_directory(receipt_dir, roots.euid)?;
    let inventory: AllocationInventoryV1 =
        read_secure_jcs(&receipt_dir.join("resource-inventory-v1.json"), None)?;
    if inventory.session_id != session_id {
        return Err(
            "resource inventory session identity differs from the requested session".into(),
        );
    }
    validate_inventory(&inventory)?;
    validate_inventory_receipts(receipt_dir, &inventory)?;
    let provider_registrations = load_pinned_providers(receipt_dir, &inventory.allocations)?;

    let resource_root = roots
        .data
        .join(repository_id)
        .join("resources")
        .join(session_id);
    let has_allocated = inventory
        .allocations
        .iter()
        .any(|allocation| allocation.state == "allocated");
    match fs::symlink_metadata(&resource_root) {
        Ok(_) => authority::ensure_private_directory(&resource_root, roots.euid)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !has_allocated => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("allocated resource inventory has no session resource root".into());
        }
        Err(error) => {
            return Err(format!(
                "inspect session resource root {}: {error}",
                resource_root.display()
            ));
        }
    }
    validate_resource_paths(&resource_root, &inventory.allocations)?;

    let mut environment = BTreeMap::new();
    for allocation in inventory
        .allocations
        .iter()
        .filter(|allocation| allocation.state == "allocated" && allocation.kind != "port")
    {
        for projection in &allocation.env {
            if environment
                .insert(projection.name.clone(), projection.value.clone())
                .is_some()
            {
                return Err(format!(
                    "resource environment projection {} collides",
                    projection.name
                ));
            }
        }
    }
    Ok(ResourceSet {
        decisions: inventory.decisions,
        allocations: inventory.allocations,
        environment,
        resource_fds: Vec::new(),
        receipt_dir: receipt_dir.to_path_buf(),
        resource_root,
        session_id: inventory.session_id,
        inventory_created_at: inventory.created_at,
        ports: BTreeMap::new(),
        provider_registrations,
    })
}

pub fn inspect_resources(
    roots: &AuthorityRoots,
    repository_id: &str,
    session_id: &str,
    receipt_dir: &Path,
) -> Result<Vec<ProviderReceiptV1>, String> {
    load_resources(roots, repository_id, session_id, receipt_dir)?.inspect(roots)
}

pub fn release_resources(
    roots: &AuthorityRoots,
    repository_id: &str,
    session_id: &str,
    receipt_dir: &Path,
    close_evidence: &ResourceReleaseEvidenceV1,
    released_at: &str,
) -> Result<Vec<String>, String> {
    close_evidence.validate()?;
    schema::Timestamp::parse(released_at)?;
    let mut resources = load_resources(roots, repository_id, session_id, receipt_dir)?;
    resources.release_after_close(roots, close_evidence, released_at)
}

impl ResourceSet {
    fn allocate_port(
        &mut self,
        session_id: &str,
        decision: &ResourceDecisionV1,
        at: &str,
    ) -> Result<(), String> {
        if decision.provider_id.is_some() {
            return Err("port must use the builtin provider".into());
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| format!("bind held loopback port: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("inspect held loopback port: {error}"))?;
        if !matches!(address, SocketAddr::V4(value) if *value.ip() == Ipv4Addr::LOCALHOST) {
            return Err("held port did not bind IPv4 loopback".into());
        }
        ensure_cloexec(listener.as_raw_fd())?;
        let allocation_id = crate::store::uuid_v4();
        let value = address.to_string();
        let env_name = format!("DOCKS_RESOURCE_{}_FD", decision.name.to_ascii_uppercase());
        let env = vec![EnvProjectionV1 {
            name: env_name,
            value: listener.as_raw_fd().to_string(),
        }];
        let evidence = builtin_evidence("port", &allocation_id, &value, None);
        let create = builtin_receipt("create", "allocated", &allocation_id, &value, &evidence, at);
        let inspect = builtin_receipt("inspect", "exists", &allocation_id, &value, &evidence, at);
        let create_digest = self.persist_receipt(&allocation_id, "create", &create)?;
        let inspect_digest = self.persist_receipt(&allocation_id, "inspect", &inspect)?;
        let allocation = ResourceAllocationV1 {
            allocation_id: allocation_id.clone(),
            session_id: session_id.into(),
            kind: decision.kind.clone(),
            name: decision.name.clone(),
            provider_id: BUILTIN_PROVIDER.into(),
            state: "allocated".into(),
            value,
            env,
            create_receipt_sha256: create_digest,
            inspect_receipt_sha256: inspect_digest,
            delete_receipt_sha256: None,
            created_at: at.into(),
            released_at: None,
        };
        ResourceAllocationV1::from_jcs(allocation.to_jcs())?;
        self.insert_projection(&allocation)?;
        self.resource_fds.push(listener.as_raw_fd());
        self.ports.insert(allocation_id, listener);
        self.allocations.push(allocation);
        Ok(())
    }

    fn allocate_directory(
        &mut self,
        session_id: &str,
        decision: &ResourceDecisionV1,
        at: &str,
    ) -> Result<(), String> {
        if decision.provider_id.is_some() {
            return Err(format!("{} must use the builtin provider", decision.kind));
        }
        let allocation_id = crate::store::uuid_v4();
        let path = self
            .resource_root
            .join(format!("{}-{}", decision.kind, decision.name));
        fs::create_dir(&path).map_err(|error| {
            format!(
                "create {} resource {}: {error}",
                decision.kind,
                path.display()
            )
        })?;
        let result = (|| {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("chmod resource directory: {error}"))?;
            let evidence = directory_evidence(&path)?;
            let value = path
                .to_str()
                .ok_or_else(|| "resource directory path is not UTF-8".to_string())?
                .to_string();
            let env = directory_env(&decision.kind, &value)?;
            let create =
                builtin_receipt("create", "allocated", &allocation_id, &value, &evidence, at);
            let inspect_evidence = directory_evidence(&path)?;
            if inspect_evidence != evidence {
                return Err("resource directory identity changed after creation".into());
            }
            let inspect = builtin_receipt(
                "inspect",
                "exists",
                &allocation_id,
                &value,
                &inspect_evidence,
                at,
            );
            let create_digest = self.persist_receipt(&allocation_id, "create", &create)?;
            let inspect_digest = self.persist_receipt(&allocation_id, "inspect", &inspect)?;
            let allocation = ResourceAllocationV1 {
                allocation_id: allocation_id.clone(),
                session_id: session_id.into(),
                kind: decision.kind.clone(),
                name: decision.name.clone(),
                provider_id: BUILTIN_PROVIDER.into(),
                state: "allocated".into(),
                value,
                env,
                create_receipt_sha256: create_digest,
                inspect_receipt_sha256: inspect_digest,
                delete_receipt_sha256: None,
                created_at: at.into(),
                released_at: None,
            };
            ResourceAllocationV1::from_jcs(allocation.to_jcs())?;
            self.insert_projection(&allocation)?;
            self.allocations.push(allocation);
            Ok(())
        })();
        if let Err(error) = result {
            return match remove_private_tree(&path, unsafe { libc::geteuid() }) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!(
                    "{error}; created directory cleanup could not be proven: {cleanup}"
                )),
            };
        }
        Ok(())
    }

    fn allocate_database(
        &mut self,
        session_id: &str,
        decision: &ResourceDecisionV1,
        registry: &ResourceProviderRegistryV1,
        at: &str,
    ) -> Result<(), String> {
        let provider_id = decision
            .provider_id
            .as_deref()
            .ok_or_else(|| "database_schema requires a registered provider_id".to_string())?;
        let provider = registry
            .providers
            .iter()
            .find(|provider| provider.provider_id == provider_id)
            .ok_or_else(|| format!("database provider {provider_id} is not registered"))?;
        let allocation_id = crate::store::uuid_v4();
        self.persist_pinned_provider(&allocation_id, provider, &registry.updated_at)?;
        self.provider_registrations
            .insert(allocation_id.clone(), provider.clone());

        let create = match invoke_provider(
            provider,
            provider_request("create", &allocation_id, session_id, &decision.name, None),
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                self.remove_unused_provider_pin(&allocation_id);
                return Err(error);
            }
        };
        validate_provider_receipt(&create, "create", "allocated", &allocation_id, None)?;
        let create_digest = schema::jcs_sha256(&create);
        if let Err(error) = self.persist_receipt(&allocation_id, "create", &create) {
            let rollback = rollback_created_database(
                provider,
                &allocation_id,
                session_id,
                &decision.name,
                &create.value,
                &create_digest,
            )
            .and_then(|receipt| self.persist_partial_rollback_receipt(&allocation_id, &receipt));
            return Err(combine_rollback_error(error, rollback));
        }
        let inspect = match invoke_provider(
            provider,
            provider_request(
                "inspect",
                &allocation_id,
                session_id,
                &decision.name,
                Some(&create_digest),
            ),
        )
        .and_then(|receipt| {
            validate_provider_receipt(
                &receipt,
                "inspect",
                "exists",
                &allocation_id,
                Some(&create.value),
            )?;
            if receipt.provider_evidence_sha256 != create.provider_evidence_sha256 {
                return Err("database provider stable evidence changed during allocation".into());
            }
            Ok(receipt)
        }) {
            Ok(receipt) => receipt,
            Err(error) => {
                let rollback = rollback_created_database(
                    provider,
                    &allocation_id,
                    session_id,
                    &decision.name,
                    &create.value,
                    &create_digest,
                )
                .and_then(|receipt| {
                    self.persist_partial_rollback_receipt(&allocation_id, &receipt)
                });
                return Err(combine_rollback_error(error, rollback));
            }
        };
        let inspect_digest = schema::jcs_sha256(&inspect);
        if let Err(error) = self.persist_receipt(&allocation_id, "inspect", &inspect) {
            let rollback = rollback_created_database(
                provider,
                &allocation_id,
                session_id,
                &decision.name,
                &create.value,
                &inspect_digest,
            )
            .and_then(|receipt| self.persist_partial_rollback_receipt(&allocation_id, &receipt));
            return Err(combine_rollback_error(error, rollback));
        }
        let env = vec![EnvProjectionV1 {
            name: format!(
                "DOCKS_RESOURCE_{}_DATABASE_SCHEMA",
                decision.name.to_ascii_uppercase()
            ),
            value: create.value.clone(),
        }];
        let allocation = ResourceAllocationV1 {
            allocation_id,
            session_id: session_id.into(),
            kind: decision.kind.clone(),
            name: decision.name.clone(),
            provider_id: provider_id.into(),
            state: "allocated".into(),
            value: create.value,
            env,
            create_receipt_sha256: create_digest,
            inspect_receipt_sha256: inspect_digest,
            delete_receipt_sha256: None,
            created_at: at.into(),
            released_at: None,
        };
        ResourceAllocationV1::from_jcs(allocation.to_jcs())?;
        if let Err(error) = self.insert_projection(&allocation) {
            self.allocations.push(allocation);
            return Err(error);
        }
        self.allocations.push(allocation);
        Ok(())
    }

    fn persist_receipt(
        &self,
        allocation_id: &str,
        operation: &str,
        receipt: &ProviderReceiptV1,
    ) -> Result<String, String> {
        let path = self
            .receipt_dir
            .join(format!("{allocation_id}-{operation}-receipt-v1.json"));
        authority::atomic_create_jcs(&path, receipt, 0o600)?;
        Ok(schema::jcs_sha256(receipt))
    }

    fn insert_projection(&mut self, allocation: &ResourceAllocationV1) -> Result<(), String> {
        for projection in &allocation.env {
            if self
                .environment
                .insert(projection.name.clone(), projection.value.clone())
                .is_some()
            {
                return Err(format!(
                    "resource environment projection {} collides",
                    projection.name
                ));
            }
        }
        Ok(())
    }

    fn validate_projection(&self) -> Result<(), String> {
        let mut projected_fds = Vec::new();
        for (name, value) in &self.environment {
            if name.starts_with("DOCKS_RESOURCE_") && name.ends_with("_FD") {
                let fd = value
                    .parse::<RawFd>()
                    .map_err(|_| "resource FD projection is not decimal".to_string())?;
                if fd.to_string() != *value {
                    return Err("resource FD projection is not canonical decimal".into());
                }
                projected_fds.push(fd);
            }
        }
        let mut actual = self.resource_fds.clone();
        projected_fds.sort_unstable();
        actual.sort_unstable();
        if projected_fds != actual || actual.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err("resource FD projection is not one-to-one".into());
        }
        Ok(())
    }

    fn rollback_unpublished(&mut self, euid: u32) -> Result<(), String> {
        let mut failures = Vec::new();
        for index in (0..self.allocations.len()).rev() {
            if self.allocations[index].state == "released" {
                continue;
            }
            let allocation = self.allocations[index].clone();
            let result = self
                .persist_deletion_intent(&allocation, "rollback", None, &allocation.created_at)
                .and_then(|intent| self.execute_deletion_intent(&allocation, &intent, euid))
                .and_then(|receipt| {
                    let digest = self.persist_delete_receipt(&allocation, &receipt)?;
                    self.allocations[index].state = "released".into();
                    self.allocations[index].delete_receipt_sha256 = Some(digest);
                    self.allocations[index].released_at = Some(receipt.at);
                    ResourceAllocationV1::from_jcs(self.allocations[index].to_jcs())?;
                    self.persist_inventory()
                });
            if let Err(error) = result {
                failures.push(format!("rollback resource {}: {error}", allocation.name));
            }
        }
        if !failures.is_empty() {
            return Err(failures.join("; "));
        }
        self.persist_inventory()?;
        match fs::remove_dir(&self.resource_root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "remove rolled-back session resource root {}: {error}",
                    self.resource_root.display()
                ));
            }
        }
        Ok(())
    }

    pub fn writable_paths(&self) -> Vec<PathBuf> {
        self.allocations
            .iter()
            .filter(|allocation| {
                allocation.state == "allocated"
                    && matches!(
                        allocation.kind.as_str(),
                        "temp_dir" | "build_dir" | "log_dir" | "cache_dir"
                    )
            })
            .map(|allocation| PathBuf::from(&allocation.value))
            .collect()
    }

    pub fn inspect(&self, _roots: &AuthorityRoots) -> Result<Vec<ProviderReceiptV1>, String> {
        self.allocations
            .iter()
            .map(|allocation| self.inspect_allocation(allocation))
            .collect()
    }

    pub fn release(
        &mut self,
        roots: &AuthorityRoots,
        released_at: &str,
    ) -> Result<Vec<String>, String> {
        schema::Timestamp::parse(released_at)?;
        self.inspect(roots)?;
        let mut digests = Vec::new();
        for index in 0..self.allocations.len() {
            if self.allocations[index].state == "released" {
                digests.push(
                    self.allocations[index]
                        .delete_receipt_sha256
                        .clone()
                        .ok_or_else(|| "released resource has no delete receipt".to_string())?,
                );
                continue;
            }
            let allocation = self.allocations[index].clone();
            let intent =
                self.persist_deletion_intent(&allocation, "legacy_release", None, released_at)?;
            let receipt = self.execute_deletion_intent(&allocation, &intent, roots.euid)?;
            let digest = self.persist_delete_receipt(&allocation, &receipt)?;
            self.allocations[index].state = "released".into();
            self.allocations[index].delete_receipt_sha256 = Some(digest.clone());
            self.allocations[index].released_at = Some(released_at.into());
            ResourceAllocationV1::from_jcs(self.allocations[index].to_jcs())?;
            self.persist_inventory()?;
            digests.push(digest);
        }
        self.environment.clear();
        self.resource_fds.clear();
        match fs::remove_dir(&self.resource_root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove empty session resource root: {error}")),
        }
        Ok(digests)
    }

    pub fn release_after_close(
        &mut self,
        roots: &AuthorityRoots,
        close_evidence: &ResourceReleaseEvidenceV1,
        released_at: &str,
    ) -> Result<Vec<String>, String> {
        close_evidence.validate()?;
        schema::Timestamp::parse(released_at)?;
        self.reconcile_durable_delete_receipts(close_evidence)?;
        self.reconcile_deletion_intents(close_evidence, roots.euid)?;
        for allocation in self
            .allocations
            .iter()
            .filter(|allocation| allocation.kind == "port" && allocation.state == "released")
        {
            let digest = allocation
                .delete_receipt_sha256
                .as_deref()
                .ok_or_else(|| "released held port has no delete receipt".to_string())?;
            let receipt = read_receipt(
                &self.receipt_dir.join(format!(
                    "{}-delete-receipt-v1.json",
                    allocation.allocation_id
                )),
                digest,
            )?;
            let close_binding = format!(
                "{}:{}",
                close_evidence.broker_close_sha256, close_evidence.runtime_empty_sha256
            );
            let evidence = builtin_evidence(
                "port-release",
                &allocation.allocation_id,
                &allocation.value,
                Some(&close_binding),
            );
            let expected = deterministic_builtin_receipt(
                "delete",
                "released",
                &allocation.allocation_id,
                &allocation.value,
                &evidence,
                &receipt.at,
            );
            if receipt != expected {
                return Err(format!(
                    "held port {} release receipt is bound to different close evidence",
                    allocation.name
                ));
            }
        }

        for allocation in self
            .allocations
            .iter()
            .filter(|allocation| allocation.state == "allocated")
        {
            self.inspect_allocation(allocation)?;
            match allocation.kind.as_str() {
                "port" => prove_recorded_port_released(&allocation.value)?,
                "temp_dir" | "build_dir" | "log_dir" | "cache_dir" => {
                    preflight_private_tree(Path::new(&allocation.value), roots.euid)?
                }
                "database_schema" => {}
                _ => return Err("resource release encountered an unknown kind".into()),
            }
        }

        let mut digests = self
            .allocations
            .iter()
            .map(|allocation| allocation.delete_receipt_sha256.clone())
            .collect::<Vec<_>>();
        let mut release_order = self
            .allocations
            .iter()
            .enumerate()
            .filter_map(|(index, allocation)| (allocation.state == "allocated").then_some(index))
            .collect::<Vec<_>>();
        release_order.sort_by_key(|index| match self.allocations[*index].kind.as_str() {
            "database_schema" => 0,
            "port" => 1,
            _ => 2,
        });
        for index in release_order {
            let allocation = self.allocations[index].clone();
            let intent = self.persist_deletion_intent(
                &allocation,
                "release",
                Some(close_evidence),
                released_at,
            )?;
            let receipt = self.execute_deletion_intent(&allocation, &intent, roots.euid)?;
            let digest = self.persist_delete_receipt(&allocation, &receipt)?;
            self.allocations[index].state = "released".into();
            self.allocations[index].delete_receipt_sha256 = Some(digest.clone());
            self.allocations[index].released_at = Some(receipt.at);
            ResourceAllocationV1::from_jcs(self.allocations[index].to_jcs())?;
            self.persist_inventory()?;
            digests[index] = Some(digest);
        }
        let digests = digests
            .into_iter()
            .map(|digest| {
                digest.ok_or_else(|| "released resource has no delete receipt".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;

        self.environment.clear();
        self.resource_fds.clear();
        match fs::remove_dir(&self.resource_root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "remove empty session resource root {}: {error}",
                    self.resource_root.display()
                ));
            }
        }
        Ok(digests)
    }

    fn reconcile_durable_delete_receipts(
        &mut self,
        close_evidence: &ResourceReleaseEvidenceV1,
    ) -> Result<(), String> {
        for index in 0..self.allocations.len() {
            if self.allocations[index].state == "released" {
                continue;
            }
            let allocation = self.allocations[index].clone();
            let path = self.receipt_dir.join(format!(
                "{}-delete-receipt-v1.json",
                allocation.allocation_id
            ));
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    let receipt: ProviderReceiptV1 = read_secure_jcs(&path, None)?;
                    validate_provider_receipt(
                        &receipt,
                        "delete",
                        "released",
                        &allocation.allocation_id,
                        Some(&allocation.value),
                    )?;
                    match allocation.kind.as_str() {
                        "port" => {
                            prove_recorded_port_released(&allocation.value)?;
                            let close_binding = format!(
                                "{}:{}",
                                close_evidence.broker_close_sha256,
                                close_evidence.runtime_empty_sha256
                            );
                            let expected_evidence = builtin_evidence(
                                "port-release",
                                &allocation.allocation_id,
                                &allocation.value,
                                Some(&close_binding),
                            );
                            let expected = deterministic_builtin_receipt(
                                "delete",
                                "released",
                                &allocation.allocation_id,
                                &allocation.value,
                                &expected_evidence,
                                &receipt.at,
                            );
                            if receipt != expected {
                                return Err(format!(
                                    "held port {} delete receipt is not bound to close evidence",
                                    allocation.name
                                ));
                            }
                        }
                        "temp_dir" | "build_dir" | "log_dir" | "cache_dir" => {
                            let inspect = read_receipt(
                                &self.receipt_dir.join(format!(
                                    "{}-inspect-receipt-v1.json",
                                    allocation.allocation_id
                                )),
                                &allocation.inspect_receipt_sha256,
                            )?;
                            if receipt.provider_evidence_sha256 != inspect.provider_evidence_sha256
                            {
                                return Err(format!(
                                    "resource directory {} delete identity drifted",
                                    allocation.name
                                ));
                            }
                        }
                        "database_schema" => {
                            let inspect = read_receipt(
                                &self.receipt_dir.join(format!(
                                    "{}-inspect-receipt-v1.json",
                                    allocation.allocation_id
                                )),
                                &allocation.inspect_receipt_sha256,
                            )?;
                            if receipt.provider_evidence_sha256 != inspect.provider_evidence_sha256
                            {
                                return Err(format!(
                                    "database resource {} delete identity drifted",
                                    allocation.name
                                ));
                            }
                        }
                        _ => {
                            return Err(
                                "resource delete receipt has an unknown allocation kind".into()
                            );
                        }
                    }
                    let digest = schema::jcs_sha256(&receipt);
                    self.allocations[index].state = "released".into();
                    self.allocations[index].delete_receipt_sha256 = Some(digest);
                    self.allocations[index].released_at = Some(receipt.at);
                    ResourceAllocationV1::from_jcs(self.allocations[index].to_jcs())?;
                    self.persist_inventory()?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "inspect delete receipt {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    }

    fn persist_pinned_provider(
        &self,
        allocation_id: &str,
        provider: &ResourceProviderRegistrationV1,
        updated_at: &str,
    ) -> Result<(), String> {
        let pin = ResourceProviderRegistryV1 {
            providers: vec![provider.clone()],
            updated_at: updated_at.to_string(),
        };
        authority::atomic_create_jcs(
            &self
                .receipt_dir
                .join(format!("{allocation_id}-provider-registration-v1.json")),
            &pin,
            0o600,
        )
    }

    fn remove_unused_provider_pin(&mut self, allocation_id: &str) {
        self.provider_registrations.remove(allocation_id);
        let _ = fs::remove_file(
            self.receipt_dir
                .join(format!("{allocation_id}-provider-registration-v1.json")),
        );
    }

    fn persist_partial_rollback_receipt(
        &self,
        allocation_id: &str,
        receipt: &ProviderReceiptV1,
    ) -> Result<(), String> {
        let path = self
            .receipt_dir
            .join(format!("{allocation_id}-delete-receipt-v1.json"));
        match authority::atomic_create_jcs(&path, receipt, 0o600) {
            Ok(()) => Ok(()),
            Err(create_error) => {
                let durable: ProviderReceiptV1 = read_secure_jcs(&path, None)
                    .map_err(|read_error| format!("{create_error}; {read_error}"))?;
                if durable == *receipt {
                    Ok(())
                } else {
                    Err("partial rollback delete receipt has different durable bytes".into())
                }
            }
        }
    }

    fn persist_deletion_intent(
        &self,
        allocation: &ResourceAllocationV1,
        mode: &str,
        close_evidence: Option<&ResourceReleaseEvidenceV1>,
        released_at: &str,
    ) -> Result<DeletionIntentV1, String> {
        let durable_inspect = read_receipt(
            &self.receipt_dir.join(format!(
                "{}-inspect-receipt-v1.json",
                allocation.allocation_id
            )),
            &allocation.inspect_receipt_sha256,
        )?;
        let provider_evidence_sha256 = if allocation.kind == "port" && mode == "release" {
            let close = close_evidence
                .ok_or_else(|| "held-port release intent requires close evidence".to_string())?;
            let binding = format!(
                "{}:{}",
                close.broker_close_sha256, close.runtime_empty_sha256
            );
            builtin_evidence(
                "port-release",
                &allocation.allocation_id,
                &allocation.value,
                Some(&binding),
            )
        } else {
            durable_inspect.provider_evidence_sha256
        };
        let request_id = deterministic_uuid(
            format!(
                "session-relay/resource-deletion-intent/v1\0{mode}\0{}\0{}\0{}\0{}\0{}",
                allocation.allocation_id,
                allocation.inspect_receipt_sha256,
                allocation.provider_id,
                close_evidence
                    .map(|value| value.broker_close_sha256.as_str())
                    .unwrap_or(""),
                close_evidence
                    .map(|value| value.runtime_empty_sha256.as_str())
                    .unwrap_or("")
            )
            .as_bytes(),
        );
        let intent = DeletionIntentV1 {
            allocation_id: allocation.allocation_id.clone(),
            kind: allocation.kind.clone(),
            provider_id: allocation.provider_id.clone(),
            value: allocation.value.clone(),
            provider_evidence_sha256,
            prior_receipt_sha256: allocation.inspect_receipt_sha256.clone(),
            request_id,
            mode: mode.to_string(),
            released_at: released_at.to_string(),
            broker_close_sha256: close_evidence.map(|value| value.broker_close_sha256.clone()),
            runtime_empty_sha256: close_evidence.map(|value| value.runtime_empty_sha256.clone()),
        };
        DeletionIntentV1::from_jcs(intent.to_jcs())?;
        let path = self.receipt_dir.join(format!(
            "{}-delete-intent-v1.json",
            allocation.allocation_id
        ));
        match authority::atomic_create_jcs(&path, &intent, 0o600) {
            Ok(()) => Ok(intent),
            Err(create_error) => {
                let durable: DeletionIntentV1 = read_secure_jcs(&path, None)
                    .map_err(|read_error| format!("{create_error}; {read_error}"))?;
                if durable != intent {
                    return Err(format!(
                        "resource {} deletion intent already exists with different bytes",
                        allocation.name
                    ));
                }
                Ok(durable)
            }
        }
    }

    fn execute_deletion_intent(
        &mut self,
        allocation: &ResourceAllocationV1,
        intent: &DeletionIntentV1,
        euid: u32,
    ) -> Result<ProviderReceiptV1, String> {
        if intent.allocation_id != allocation.allocation_id
            || intent.kind != allocation.kind
            || intent.provider_id != allocation.provider_id
            || intent.value != allocation.value
            || intent.prior_receipt_sha256 != allocation.inspect_receipt_sha256
        {
            return Err(format!(
                "resource {} deletion intent identity differs from allocation",
                allocation.name
            ));
        }
        match allocation.kind.as_str() {
            "port" => {
                if intent.mode != "release" {
                    if let Some(listener) = self.ports.remove(&allocation.allocation_id) {
                        let address = listener
                            .local_addr()
                            .map_err(|error| format!("inspect held port before rollback: {error}"))?
                            .to_string();
                        if address != allocation.value {
                            return Err(format!(
                                "held port {} identity drifted before rollback",
                                allocation.name
                            ));
                        }
                        drop(listener);
                    }
                    prove_recorded_port_released(&allocation.value)?;
                } else {
                    prove_recorded_port_released(&allocation.value)?;
                }
                Ok(deterministic_builtin_receipt(
                    "delete",
                    "released",
                    &allocation.allocation_id,
                    &allocation.value,
                    &intent.provider_evidence_sha256,
                    &intent.released_at,
                ))
            }
            "temp_dir" | "build_dir" | "log_dir" | "cache_dir" => {
                match fs::symlink_metadata(&allocation.value) {
                    Ok(_) => {
                        let current = directory_evidence(Path::new(&allocation.value))?;
                        if current != intent.provider_evidence_sha256 {
                            return Err(format!(
                                "resource directory {} identity drifted after deletion intent",
                                allocation.name
                            ));
                        }
                        preflight_private_tree(Path::new(&allocation.value), euid)?;
                        remove_private_tree(Path::new(&allocation.value), euid)?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "inspect resource directory {} after deletion intent: {error}",
                            allocation.name
                        ));
                    }
                }
                Ok(deterministic_builtin_receipt(
                    "delete",
                    "released",
                    &allocation.allocation_id,
                    &allocation.value,
                    &intent.provider_evidence_sha256,
                    &intent.released_at,
                ))
            }
            "database_schema" => {
                let provider = self
                    .provider_registrations
                    .get(&allocation.allocation_id)
                    .ok_or_else(|| {
                        format!(
                            "pinned database provider {} is unavailable",
                            allocation.provider_id
                        )
                    })?;
                let mut request = provider_request(
                    "delete",
                    &allocation.allocation_id,
                    &allocation.session_id,
                    &allocation.name,
                    Some(&intent.prior_receipt_sha256),
                );
                request.request_id = intent.request_id.clone();
                let receipt = invoke_provider(provider, request)?;
                validate_provider_receipt(
                    &receipt,
                    "delete",
                    "released",
                    &allocation.allocation_id,
                    Some(&allocation.value),
                )?;
                if receipt.provider_evidence_sha256 != intent.provider_evidence_sha256 {
                    return Err(format!(
                        "database resource {} delete identity drifted",
                        allocation.name
                    ));
                }
                Ok(receipt)
            }
            _ => Err("resource deletion intent has an unknown allocation kind".into()),
        }
    }

    fn reconcile_deletion_intents(
        &mut self,
        close_evidence: &ResourceReleaseEvidenceV1,
        euid: u32,
    ) -> Result<(), String> {
        for index in 0..self.allocations.len() {
            if self.allocations[index].state == "released" {
                continue;
            }
            let allocation = self.allocations[index].clone();
            let path = self.receipt_dir.join(format!(
                "{}-delete-intent-v1.json",
                allocation.allocation_id
            ));
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    let intent: DeletionIntentV1 = read_secure_jcs(&path, None)?;
                    let expected = self.persist_deletion_intent(
                        &allocation,
                        "release",
                        Some(close_evidence),
                        &intent.released_at,
                    )?;
                    if intent != expected {
                        return Err(format!(
                            "resource {} deletion intent changed during reconciliation",
                            allocation.name
                        ));
                    }
                    let receipt = self.execute_deletion_intent(&allocation, &intent, euid)?;
                    let digest = self.persist_delete_receipt(&allocation, &receipt)?;
                    self.allocations[index].state = "released".into();
                    self.allocations[index].delete_receipt_sha256 = Some(digest);
                    self.allocations[index].released_at = Some(receipt.at);
                    ResourceAllocationV1::from_jcs(self.allocations[index].to_jcs())?;
                    self.persist_inventory()?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "inspect resource deletion intent {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    }

    fn persist_delete_receipt(
        &self,
        allocation: &ResourceAllocationV1,
        receipt: &ProviderReceiptV1,
    ) -> Result<String, String> {
        let path = self.receipt_dir.join(format!(
            "{}-delete-receipt-v1.json",
            allocation.allocation_id
        ));
        match authority::atomic_create_jcs(&path, receipt, 0o600) {
            Ok(()) => Ok(schema::jcs_sha256(receipt)),
            Err(create_error) => {
                let durable: ProviderReceiptV1 = read_secure_jcs(&path, None)
                    .map_err(|read_error| format!("{create_error}; {read_error}"))?;
                if durable != *receipt {
                    return Err(format!(
                        "delete receipt for resource {} already exists with different bytes",
                        allocation.name
                    ));
                }
                Ok(schema::jcs_sha256(&durable))
            }
        }
    }

    fn inspect_allocation(
        &self,
        allocation: &ResourceAllocationV1,
    ) -> Result<ProviderReceiptV1, String> {
        if allocation.state == "released" {
            let digest = allocation
                .delete_receipt_sha256
                .as_deref()
                .ok_or_else(|| "released allocation has no delete receipt".to_string())?;
            return read_receipt(
                &self.receipt_dir.join(format!(
                    "{}-delete-receipt-v1.json",
                    allocation.allocation_id
                )),
                digest,
            );
        }
        let durable = read_receipt(
            &self.receipt_dir.join(format!(
                "{}-inspect-receipt-v1.json",
                allocation.allocation_id
            )),
            &allocation.inspect_receipt_sha256,
        )?;
        match allocation.kind.as_str() {
            "port" => {
                let expected =
                    builtin_evidence("port", &allocation.allocation_id, &allocation.value, None);
                let address: SocketAddr = allocation
                    .value
                    .parse()
                    .map_err(|_| format!("held port {} address is invalid", allocation.name))?;
                if !matches!(address,SocketAddr::V4(value) if *value.ip()==Ipv4Addr::LOCALHOST)
                    || durable.provider_evidence_sha256 != expected
                {
                    return Err(format!("held port {} identity drifted", allocation.name));
                }
                if let Some(listener) = self.ports.get(&allocation.allocation_id) {
                    let value = listener
                        .local_addr()
                        .map_err(|error| format!("inspect held port: {error}"))?
                        .to_string();
                    if value != allocation.value {
                        return Err(format!("held port {} identity drifted", allocation.name));
                    }
                }
                Ok(durable)
            }
            "temp_dir" | "build_dir" | "log_dir" | "cache_dir" => {
                let evidence = directory_evidence(Path::new(&allocation.value))?;
                if durable.provider_evidence_sha256 != evidence {
                    return Err(format!(
                        "resource directory {} identity drifted",
                        allocation.name
                    ));
                }
                Ok(durable)
            }
            "database_schema" => {
                let provider = self
                    .provider_registrations
                    .get(&allocation.allocation_id)
                    .ok_or_else(|| {
                        format!(
                            "pinned database provider {} is unavailable",
                            allocation.provider_id
                        )
                    })?;
                let request = provider_request(
                    "inspect",
                    &allocation.allocation_id,
                    &allocation.session_id,
                    &allocation.name,
                    Some(&allocation.create_receipt_sha256),
                );
                let receipt = invoke_provider(provider, request)?;
                validate_provider_receipt(
                    &receipt,
                    "inspect",
                    "exists",
                    &allocation.allocation_id,
                    Some(&allocation.value),
                )?;
                if receipt.provider_evidence_sha256 != durable.provider_evidence_sha256 {
                    return Err(format!(
                        "database resource {} provider identity drifted",
                        allocation.name
                    ));
                }
                Ok(receipt)
            }
            _ => Err("resource inspection encountered an unknown kind".into()),
        }
    }

    fn persist_inventory(&self) -> Result<(), String> {
        let inventory = AllocationInventoryV1 {
            session_id: self.session_id.clone(),
            decisions: self.decisions.clone(),
            allocations: self.allocations.clone(),
            created_at: self.inventory_created_at.clone(),
        };
        authority::atomic_replace_jcs(
            &self.receipt_dir.join("resource-inventory-v1.json"),
            &inventory,
            0o600,
        )
    }
}

pub(crate) fn validate_held_resource_fds(receipt_dir: &Path, fds: &[RawFd]) -> Result<(), String> {
    let inventory: AllocationInventoryV1 =
        read_secure_jcs(&receipt_dir.join("resource-inventory-v1.json"), None)?;
    validate_inventory(&inventory)?;
    validate_inventory_receipts(receipt_dir, &inventory)?;
    let mut expected = Vec::new();
    for allocation in inventory
        .allocations
        .iter()
        .filter(|allocation| allocation.state == "allocated" && allocation.kind == "port")
    {
        let projection = allocation
            .env
            .iter()
            .find(|projection| {
                projection.name
                    == format!("DOCKS_RESOURCE_{}_FD", allocation.name.to_ascii_uppercase())
            })
            .ok_or_else(|| format!("held port {} has no FD projection", allocation.name))?;
        let fd: RawFd = projection
            .value
            .parse()
            .map_err(|_| format!("held port {} FD is not decimal", allocation.name))?;
        expected.push((fd, allocation.value.as_str()));
    }
    let mut actual = fds.to_vec();
    actual.sort_unstable();
    let mut expected_numbers = expected.iter().map(|(fd, _)| *fd).collect::<Vec<_>>();
    expected_numbers.sort_unstable();
    if actual != expected_numbers || actual.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("broker held resource FD inventory mismatch".into());
    }
    for (fd, address) in expected {
        if fd <= 2 {
            return Err("held resource FD overlaps stdio".into());
        }
        let duplicate = unsafe { libc::dup(fd) };
        if duplicate < 0 {
            return Err(format!(
                "duplicate held resource FD {fd}: {}",
                std::io::Error::last_os_error()
            ));
        }
        let listener = unsafe { TcpListener::from_raw_fd(duplicate) };
        let local = listener
            .local_addr()
            .map_err(|error| format!("inspect held resource FD {fd}: {error}"))?;
        let mut accepts = 0i32;
        let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ACCEPTCONN,
                (&mut accepts as *mut i32).cast(),
                &mut length,
            )
        } < 0
            || accepts != 1
        {
            return Err(format!("held resource FD {fd} is not a listening socket"));
        }
        if local.to_string() != address
            || !matches!(local,SocketAddr::V4(value) if *value.ip()==Ipv4Addr::LOCALHOST)
        {
            return Err(format!("held resource FD {fd} identity drifted"));
        }
    }
    Ok(())
}

pub fn preflight_tool_launch(tool: &ToolLaunchV1) -> Result<(), String> {
    verify_executable(Path::new(&tool.executable_path), &tool.executable_sha256)?;
    for (name, value) in [
        ("model", tool.model.as_deref()),
        ("effort", tool.effort.as_deref()),
    ] {
        if value.is_some_and(|value| value.is_empty() || value.as_bytes().contains(&0)) {
            return Err(format!("tool {name} is invalid"));
        }
    }
    match tool.kind.as_str() {
        "claude" | "omp" if tool.service_tier.is_some() => Err(format!(
            "{} does not support service_tier in ToolLaunchV1",
            tool.kind
        )),
        "claude" | "codex" | "omp" => Ok(()),
        _ => Err("tool kind must be claude|codex|omp".into()),
    }
}

pub fn prepare_tool_launch(context: ToolLaunchContext<'_>) -> Result<PreparedToolLaunch, String> {
    let ToolLaunchContext {
        tool,
        session_id,
        workspace,
        prompt,
        generated_policy,
        git_shim_dir,
        worker_capability_file,
        resources,
    } = context;
    schema::LowerUuidV4::parse(session_id)?;
    preflight_tool_launch(tool)?;
    if prompt.as_bytes().contains(&0) || generated_policy.as_bytes().contains(&0) {
        return Err("tool prompt or generated policy contains NUL".into());
    }
    let mut arguments = match tool.kind.as_str() {
        "claude" => {
            if tool.service_tier.is_some() {
                return Err("Claude does not support service_tier in ToolLaunchV1".into());
            }
            let mut values = vec![
                "-p".into(),
                "--session-id".into(),
                session_id.into(),
                "--permission-mode".into(),
                "auto".into(),
            ];
            append_option(&mut values, "--model", tool.model.as_deref());
            append_option(&mut values, "--effort", tool.effort.as_deref());
            values
        }
        "codex" => {
            let mut values = vec!["exec".into(), "--sandbox".into(), "workspace-write".into()];
            append_option(&mut values, "-m", tool.model.as_deref());
            if let Some(effort) = &tool.effort {
                values.extend(["-c".into(), format!("model_reasoning_effort={effort}")]);
            }
            if let Some(tier) = &tool.service_tier {
                values.extend(["-c".into(), format!("service_tier={tier}")]);
            }
            values
        }
        "omp" => {
            if tool.service_tier.is_some() {
                return Err("OMP does not support service_tier in ToolLaunchV1".into());
            }
            let root = workspace
                .to_str()
                .ok_or_else(|| "workspace path is not UTF-8".to_string())?;
            let mut values = vec![
                "-p".into(),
                "--cwd".into(),
                root.into(),
                "--approval-mode".into(),
                "write".into(),
                "--append-system-prompt".into(),
                generated_policy.into(),
            ];
            append_option(&mut values, "--model", tool.model.as_deref());
            append_option(&mut values, "--thinking", tool.effort.as_deref());
            values
        }
        _ => return Err("tool kind must be claude|codex|omp".into()),
    };
    arguments.extend(["--".into(), prompt.into()]);
    let shim = git_shim_dir
        .to_str()
        .ok_or_else(|| "Git shim directory is not UTF-8".to_string())?;
    let mut environment = resources.environment.clone();
    environment.insert(
        "PATH".into(),
        format!("{shim}:/usr/local/bin:/usr/bin:/bin"),
    );
    environment.insert(
        "DOCKS_WORKER_CAPABILITY_FILE".into(),
        worker_capability_file
            .to_str()
            .ok_or_else(|| "worker capability path is not UTF-8".to_string())?
            .into(),
    );
    Ok(PreparedToolLaunch {
        executable: PathBuf::from(&tool.executable_path),
        arguments,
        environment,
        resource_fds: resources.resource_fds.clone(),
        cwd: workspace.to_path_buf(),
        writable_resources: resources.writable_paths(),
    })
}

pub fn persist_tool_launch_decision(
    path: &Path,
    session_id: &str,
    tool: &ToolLaunchV1,
    prepared: &PreparedToolLaunch,
    created_at: &str,
) -> Result<(ToolLaunchDecisionV1, String), String> {
    schema::Timestamp::parse(created_at)?;
    let mut resource_fds = prepared
        .resource_fds
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    resource_fds.sort();
    let decision = ToolLaunchDecisionV1 {
        session_id: session_id.into(),
        kind: tool.kind.clone(),
        executable_path: prepared
            .executable
            .to_str()
            .ok_or_else(|| "launch executable path is not UTF-8".to_string())?
            .into(),
        executable_sha256: tool.executable_sha256.clone(),
        arguments: prepared.arguments.clone(),
        cwd: prepared
            .cwd
            .to_str()
            .ok_or_else(|| "launch cwd is not UTF-8".to_string())?
            .into(),
        environment: prepared
            .environment
            .iter()
            .map(|(name, value)| EnvProjectionV1 {
                name: name.clone(),
                value: value.clone(),
            })
            .collect(),
        resource_fds,
        writable_resources: prepared
            .writable_resources
            .iter()
            .map(|path| {
                path.to_str()
                    .ok_or_else(|| "writable resource path is not UTF-8".to_string())
                    .map(str::to_string)
            })
            .collect::<Result<Vec<_>, _>>()?,
        created_at: created_at.into(),
    };
    ToolLaunchDecisionV1::from_jcs(decision.to_jcs())?;
    authority::atomic_create_jcs(path, &decision, 0o600)?;
    let digest = schema::jcs_sha256(&decision);
    Ok((decision, digest))
}

pub fn executable_sha256(path: &Path) -> Result<String, String> {
    let bytes = read_verified_file(path, false)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect executable {}: {error}", path.display()))?;
    if metadata.mode() & 0o111 == 0 {
        return Err(format!(
            "tool executable {} has no execute bit",
            path.display()
        ));
    }
    Ok(sha256::hex_digest(&bytes))
}

pub fn verify_executable(path: &Path, expected_sha256: &str) -> Result<(), String> {
    schema::Sha256Digest::parse(expected_sha256)?;
    let actual = executable_sha256(path)?;
    if !sha256::constant_time_eq(actual.as_bytes(), expected_sha256.as_bytes()) {
        return Err(format!("executable digest drift for {}", path.display()));
    }
    Ok(())
}

pub fn open_verified_executable(path: &Path, expected_sha256: &str) -> Result<File, String> {
    schema::Sha256Digest::parse(expected_sha256)?;
    let (file, bytes) = open_verified_file(path, false)?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect executable {}: {error}", path.display()))?;
    if metadata.mode() & 0o111 == 0 {
        return Err(format!(
            "tool executable {} has no execute bit",
            path.display()
        ));
    }
    let actual = sha256::hex_digest(&bytes);
    if !sha256::constant_time_eq(actual.as_bytes(), expected_sha256.as_bytes()) {
        return Err(format!("executable digest drift for {}", path.display()));
    }
    relocate_file_above_stdio(file)
}

pub fn preflight_resources(
    roots: &AuthorityRoots,
    decisions: &[ResourceDecisionV1],
) -> Result<(), String> {
    validate_decisions(decisions)?;
    let registry = load_registry_if_needed(roots, decisions)?;
    validate_registry_for_decisions(registry.as_ref(), decisions)
}

fn validate_decisions(decisions: &[ResourceDecisionV1]) -> Result<(), String> {
    let expected = BTreeSet::from([
        "port",
        "temp_dir",
        "build_dir",
        "database_schema",
        "log_dir",
        "cache_dir",
    ]);
    let actual = decisions
        .iter()
        .map(|decision| decision.kind.as_str())
        .collect::<BTreeSet<_>>();
    if decisions.len() != 6 || actual != expected {
        return Err("resource decisions must contain all six kinds exactly once".into());
    }
    let mut requested_names = BTreeSet::new();
    for decision in decisions {
        schema::validate_resource_name(&decision.name)?;
        if decision.state == "requested"
            && !requested_names.insert((decision.kind.as_str(), decision.name.as_str()))
        {
            return Err("requested resource names must be unique per kind".into());
        }
        if decision.kind == "database_schema"
            && decision.state == "requested"
            && decision.provider_id.is_none()
        {
            return Err("database_schema requires provider_id".into());
        }
        if decision.kind != "database_schema" && decision.provider_id.is_some() {
            return Err(format!(
                "{} cannot select an external provider",
                decision.kind
            ));
        }
    }
    Ok(())
}

fn load_registry_if_needed(
    roots: &AuthorityRoots,
    decisions: &[ResourceDecisionV1],
) -> Result<Option<ResourceProviderRegistryV1>, String> {
    if !decisions
        .iter()
        .any(|decision| decision.kind == "database_schema" && decision.state == "requested")
    {
        return Ok(None);
    }
    schema::read_jcs_file(&provider_registry_path(roots), None).map(Some)
}

fn validate_registry_for_decisions(
    registry: Option<&ResourceProviderRegistryV1>,
    decisions: &[ResourceDecisionV1],
) -> Result<(), String> {
    for decision in decisions
        .iter()
        .filter(|decision| decision.kind == "database_schema" && decision.state == "requested")
    {
        let provider_id = decision
            .provider_id
            .as_deref()
            .ok_or_else(|| "database_schema requires provider_id".to_string())?;
        let provider = registry
            .and_then(|registry| {
                registry
                    .providers
                    .iter()
                    .find(|provider| provider.provider_id == provider_id)
            })
            .ok_or_else(|| format!("database provider {provider_id} is not registered"))?;
        if !provider
            .supported_kinds
            .iter()
            .any(|kind| kind == "database_schema")
        {
            return Err(format!(
                "database provider {provider_id} does not support database_schema"
            ));
        }
        verify_provider_files(provider)?;
    }
    Ok(())
}

fn load_pinned_providers(
    receipt_dir: &Path,
    allocations: &[ResourceAllocationV1],
) -> Result<BTreeMap<String, ResourceProviderRegistrationV1>, String> {
    let mut providers = BTreeMap::new();
    for allocation in allocations
        .iter()
        .filter(|allocation| allocation.kind == "database_schema")
    {
        let pin: ResourceProviderRegistryV1 = read_secure_jcs(
            &receipt_dir.join(format!(
                "{}-provider-registration-v1.json",
                allocation.allocation_id
            )),
            None,
        )?;
        if pin.providers.len() != 1 || pin.providers[0].provider_id != allocation.provider_id {
            return Err(format!(
                "database resource {} pinned provider registration is invalid",
                allocation.name
            ));
        }
        if !pin.providers[0]
            .supported_kinds
            .iter()
            .any(|kind| kind == "database_schema")
        {
            return Err(format!(
                "database resource {} pinned provider does not support database_schema",
                allocation.name
            ));
        }
        providers.insert(allocation.allocation_id.clone(), pin.providers[0].clone());
    }
    Ok(providers)
}

fn ensure_private_ancestors(path: &Path, trusted_root: &Path, euid: u32) -> Result<(), String> {
    if !path.starts_with(trusted_root) {
        return Err("resource path escapes trusted data root".into());
    }
    let relative = path
        .strip_prefix(trusted_root)
        .map_err(|_| "resource path escapes trusted root".to_string())?;
    let mut current = trusted_root.to_path_buf();
    authority::ensure_private_directory(&current, euid)?;
    for component in relative.components() {
        current.push(component);
        if !current.exists() {
            fs::create_dir(&current).map_err(|error| {
                format!(
                    "create private resource component {}: {error}",
                    current.display()
                )
            })?;
            fs::set_permissions(&current, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("chmod private resource component: {error}"))?;
        }
        authority::ensure_private_directory(&current, euid)?;
    }
    Ok(())
}

fn ensure_cloexec(fd: RawFd) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } != 0 {
        return Err(format!(
            "set resource FD_CLOEXEC: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn directory_env(kind: &str, value: &str) -> Result<Vec<EnvProjectionV1>, String> {
    let names: &[&str] = match kind {
        "temp_dir" => &["TMPDIR", "TMP", "TEMP"],
        "build_dir" => &["DOCKS_BUILD_DIR"],
        "log_dir" => &["DOCKS_LOG_DIR"],
        "cache_dir" => &["DOCKS_CACHE_DIR"],
        _ => return Err("directory resource kind is invalid".into()),
    };
    Ok(names
        .iter()
        .map(|name| EnvProjectionV1 {
            name: (*name).into(),
            value: value.into(),
        })
        .collect())
}

fn directory_evidence(path: &Path) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect resource directory {}: {error}", path.display()))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(format!(
            "resource directory {} is not EUID-owned mode-0700",
            path.display()
        ));
    }
    Ok(sha256::hex_digest(
        format!(
            "session-relay/resource-directory/v1\0{}\0{}\0{}\0{}",
            path.display(),
            metadata.dev(),
            metadata.ino(),
            metadata.uid()
        )
        .as_bytes(),
    ))
}

fn builtin_evidence(kind: &str, allocation_id: &str, value: &str, extra: Option<&str>) -> String {
    sha256::hex_digest(
        format!(
            "session-relay/resource-builtin/v1\0{kind}\0{allocation_id}\0{value}\0{}",
            extra.unwrap_or("")
        )
        .as_bytes(),
    )
}

fn builtin_receipt(
    operation: &str,
    outcome: &str,
    allocation_id: &str,
    value: &str,
    evidence: &str,
    at: &str,
) -> ProviderReceiptV1 {
    ProviderReceiptV1 {
        request_id: crate::store::uuid_v4(),
        allocation_id: allocation_id.into(),
        operation: operation.into(),
        outcome: outcome.into(),
        value: value.into(),
        provider_evidence_sha256: evidence.into(),
        at: at.into(),
    }
}

fn provider_request(
    operation: &str,
    allocation_id: &str,
    session_id: &str,
    name: &str,
    prior: Option<&str>,
) -> ProviderRequestV1 {
    ProviderRequestV1 {
        operation: operation.into(),
        request_id: crate::store::uuid_v4(),
        allocation_id: allocation_id.into(),
        session_id: session_id.into(),
        kind: "database_schema".into(),
        name: name.into(),
        prior_receipt_sha256: prior.map(str::to_string),
    }
}

fn verify_provider_files(provider: &ResourceProviderRegistrationV1) -> Result<(), String> {
    open_verified_provider_files(provider).map(|_| ())
}

fn read_verified_file(path: &Path, private: bool) -> Result<Vec<u8>, String> {
    open_verified_file(path, private).map(|(_, bytes)| bytes)
}

fn invoke_provider(
    provider: &ResourceProviderRegistrationV1,
    request: ProviderRequestV1,
) -> Result<ProviderReceiptV1, String> {
    let files = open_verified_provider_files(provider)?;
    invoke_provider_with_files(provider, request, files)
}

fn open_verified_file(path: &Path, private: bool) -> Result<(File, Vec<u8>), String> {
    if !path.is_absolute() {
        return Err(format!("{} is not absolute", path.display()));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("securely open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(format!(
            "{} is not a single-link regular file",
            path.display()
        ));
    }
    if private && (metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o600)
    {
        return Err(format!(
            "{} is not an EUID-owned mode-0600 provider config",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("rewind {}: {error}", path.display()))?;
    Ok((file, bytes))
}

fn relocate_file_above_stdio(file: File) -> Result<File, String> {
    if file.as_raw_fd() >= 4 {
        return Ok(file);
    }
    let duplicate = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 4) };
    if duplicate < 0 {
        return Err(format!(
            "relocate verified provider FD above stdio: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { File::from_raw_fd(duplicate) })
}

fn open_verified_provider_files(
    provider: &ResourceProviderRegistrationV1,
) -> Result<VerifiedProviderFiles, String> {
    schema::Sha256Digest::parse(&provider.executable_sha256)?;
    schema::Sha256Digest::parse(&provider.config_sha256)?;
    let (config, config_bytes) = open_verified_file(Path::new(&provider.config_path), true)?;
    let (executable, executable_bytes) =
        open_verified_file(Path::new(&provider.executable_path), false)?;
    let executable_metadata = executable
        .metadata()
        .map_err(|error| format!("inspect provider executable: {error}"))?;
    if executable_metadata.mode() & 0o111 == 0 {
        return Err(format!(
            "provider executable {} has no execute bit",
            provider.executable_path
        ));
    }
    let executable_digest = sha256::hex_digest(&executable_bytes);
    if !sha256::constant_time_eq(
        executable_digest.as_bytes(),
        provider.executable_sha256.as_bytes(),
    ) {
        return Err(format!(
            "provider {} executable digest drift",
            provider.provider_id
        ));
    }
    let config_digest = sha256::hex_digest(&config_bytes);
    if !sha256::constant_time_eq(config_digest.as_bytes(), provider.config_sha256.as_bytes()) {
        return Err(format!(
            "provider {} config digest drift",
            provider.provider_id
        ));
    }
    Ok(VerifiedProviderFiles {
        executable: relocate_file_above_stdio(executable)?,
        config,
    })
}

#[cfg(not(target_os = "linux"))]
fn invoke_provider_with_files(
    provider: &ResourceProviderRegistrationV1,
    request: ProviderRequestV1,
    files: VerifiedProviderFiles,
) -> Result<ProviderReceiptV1, String> {
    let _ = (provider, request, files);
    Err("external resource provider execution requires Linux process-tree containment".into())
}

#[cfg(target_os = "linux")]
fn invoke_provider_with_files(
    provider: &ResourceProviderRegistrationV1,
    request: ProviderRequestV1,
    files: VerifiedProviderFiles,
) -> Result<ProviderReceiptV1, String> {
    let executable_fd = files.executable.as_raw_fd();
    let config_fd = files.config.as_raw_fd();
    let executable_path = format!("/proc/self/fd/{executable_fd}");
    let mut command = Command::new(executable_path);
    command
        .args([
            "--session-relay-resource-provider-v1",
            request.operation.as_str(),
        ])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    unsafe {
        command.pre_exec(move || {
            if config_fd != 3 && libc::dup2(config_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in [3, executable_fd] {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if libc::setpgid(0, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            install_provider_daemon_fence()
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn resource provider {}: {error}", provider.provider_id))?;
    let provider_group = child.id() as libc::pid_t;
    let mut stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            let _ = terminate_provider_group(&mut child, provider_group);
            return Err("provider stdin pipe missing".into());
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = terminate_provider_group(&mut child, provider_group);
            return Err("provider stdout pipe missing".into());
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = terminate_provider_group(&mut child, provider_group);
            return Err("provider stderr pipe missing".into());
        }
    };
    let mut stdout = match ProviderOutput::new(stdout) {
        Ok(stdout) => stdout,
        Err(error) => {
            let _ = terminate_provider_group(&mut child, provider_group);
            return Err(error);
        }
    };
    let mut stderr = match ProviderOutput::new(stderr) {
        Ok(stderr) => stderr,
        Err(error) => {
            let _ = terminate_provider_group(&mut child, provider_group);
            return Err(error);
        }
    };
    let request_bytes = schema::serialize_jcs_lf(&request);
    if let Err(error) = stdin.write_all(&request_bytes) {
        drop(stdin);
        let _ = terminate_provider_group(&mut child, provider_group);
        return Err(format!("write provider request: {error}"));
    }
    drop(stdin);

    let deadline = Instant::now() + PROVIDER_TIMEOUT;
    let mut status = None;
    let status = loop {
        stdout.drain_available();
        stderr.drain_available();
        if status.is_none() {
            match child.try_wait() {
                Ok(observed) => status = observed,
                Err(error) => {
                    let _ = terminate_provider_group(&mut child, provider_group);
                    return Err(format!("poll resource provider: {error}"));
                }
            }
        }
        if Instant::now() >= deadline {
            let mut cleanup_errors = terminate_provider_group(&mut child, provider_group);
            let drain_deadline = Instant::now() + PROVIDER_PIPE_DRAIN_TIMEOUT;
            while (!stdout.is_closed() || !stderr.is_closed()) && Instant::now() < drain_deadline {
                stdout.drain_available();
                stderr.drain_available();
                if !stdout.is_closed() || !stderr.is_closed() {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            if !stdout.is_closed() {
                cleanup_errors.push("provider stdout pipe remained open".into());
            }
            if !stderr.is_closed() {
                cleanup_errors.push("provider stderr pipe remained open".into());
            }
            let detail = if cleanup_errors.is_empty() {
                String::new()
            } else {
                format!("; cleanup: {}", cleanup_errors.join("; "))
            };
            return Err(format!(
                "resource provider {} timed out{detail}",
                provider.provider_id
            ));
        }
        if stdout.is_closed() && stderr.is_closed() {
            if let Some(status) = status {
                match provider_group_exists(provider_group) {
                    Ok(false) => break status,
                    Ok(true) => {
                        let cleanup_errors = terminate_provider_group(&mut child, provider_group);
                        let detail = if cleanup_errors.is_empty() {
                            String::new()
                        } else {
                            format!("; cleanup: {}", cleanup_errors.join("; "))
                        };
                        return Err(format!(
                            "resource provider {} left descendants running{detail}",
                            provider.provider_id
                        ));
                    }
                    Err(error) => {
                        let _ = terminate_provider_group(&mut child, provider_group);
                        return Err(error);
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout.finish()?;
    let stderr = stderr.finish()?;
    if !status.success() {
        return Err(format!(
            "resource provider {} failed: {}",
            provider.provider_id,
            String::from_utf8_lossy(&stderr)
        ));
    }
    let receipt = ProviderReceiptV1::from_jcs(schema::parse_jcs(&stdout, true)?)?;
    if receipt.request_id != request.request_id
        || receipt.allocation_id != request.allocation_id
        || receipt.operation != request.operation
    {
        return Err("resource provider receipt identity differs from request".into());
    }
    Ok(receipt)
}

#[cfg(target_os = "linux")]
fn install_provider_daemon_fence() -> std::io::Result<()> {
    const BPF_LOAD_ABSOLUTE_WORD: u16 = 0x20;
    const BPF_JUMP_IF_EQUAL: u16 = 0x15;
    const BPF_JUMP_IF_BITS_SET: u16 = 0x45;
    const BPF_RETURN: u16 = 0x06;
    const SECCOMP_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_ERRNO: u32 = 0x0005_0000;
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;

    let native_audit_arch = if cfg!(target_arch = "x86_64") {
        0xc000_003e
    } else if cfg!(target_arch = "aarch64") {
        0xc000_00b7
    } else {
        return Err(std::io::Error::from_raw_os_error(libc::ENOTSUP));
    };
    let filters = [
        libc::sock_filter {
            code: BPF_LOAD_ABSOLUTE_WORD,
            jt: 0,
            jf: 0,
            k: 4,
        },
        libc::sock_filter {
            code: BPF_JUMP_IF_EQUAL,
            jt: 1,
            jf: 0,
            k: native_audit_arch,
        },
        libc::sock_filter {
            code: BPF_RETURN,
            jt: 0,
            jf: 0,
            k: SECCOMP_KILL_PROCESS,
        },
        libc::sock_filter {
            code: BPF_LOAD_ABSOLUTE_WORD,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: BPF_JUMP_IF_BITS_SET,
            jt: 0,
            jf: 1,
            k: X32_SYSCALL_BIT,
        },
        libc::sock_filter {
            code: BPF_RETURN,
            jt: 0,
            jf: 0,
            k: SECCOMP_KILL_PROCESS,
        },
        libc::sock_filter {
            code: BPF_JUMP_IF_EQUAL,
            jt: 0,
            jf: 1,
            k: libc::SYS_setsid as u32,
        },
        libc::sock_filter {
            code: BPF_RETURN,
            jt: 0,
            jf: 0,
            k: SECCOMP_ERRNO | libc::EPERM as u32,
        },
        libc::sock_filter {
            code: BPF_JUMP_IF_EQUAL,
            jt: 0,
            jf: 1,
            k: libc::SYS_setpgid as u32,
        },
        libc::sock_filter {
            code: BPF_RETURN,
            jt: 0,
            jf: 0,
            k: SECCOMP_ERRNO | libc::EPERM as u32,
        },
        libc::sock_filter {
            code: BPF_RETURN,
            jt: 0,
            jf: 0,
            k: SECCOMP_ALLOW,
        },
    ];
    let program = libc::sock_fprog {
        len: filters.len() as u16,
        filter: filters.as_ptr().cast_mut(),
    };
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &program as *const libc::sock_fprog,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn signal_provider_group(group: libc::pid_t, signal: libc::c_int) -> Result<(), String> {
    if unsafe { libc::kill(-group, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(format!("signal provider process group: {error}"))
}

#[cfg(target_os = "linux")]
fn provider_group_exists(group: libc::pid_t) -> Result<bool, String> {
    if unsafe { libc::kill(-group, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(format!("inspect provider process group: {error}")),
    }
}

#[cfg(target_os = "linux")]
fn terminate_provider_group(child: &mut Child, group: libc::pid_t) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(error) = signal_provider_group(group, libc::SIGTERM) {
        errors.push(error);
    }
    let termination_deadline = Instant::now() + PROVIDER_TERMINATION_GRACE;
    loop {
        match provider_group_exists(group) {
            Ok(false) => break,
            Ok(true) if Instant::now() < termination_deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(true) => {
                if let Err(error) = signal_provider_group(group, libc::SIGKILL) {
                    errors.push(error);
                }
                break;
            }
            Err(error) => {
                errors.push(error);
                break;
            }
        }
    }
    if let Err(error) = child.wait() {
        errors.push(format!("reap provider process: {error}"));
    }
    errors
}

#[cfg(target_os = "linux")]
struct ProviderOutput<R> {
    reader: Option<R>,
    bytes: Vec<u8>,
    error: Option<String>,
}

#[cfg(target_os = "linux")]
impl<R: Read + AsRawFd> ProviderOutput<R> {
    fn new(reader: R) -> Result<Self, String> {
        let fd = reader.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0 {
            return Err(format!(
                "set provider output nonblocking: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            reader: Some(reader),
            bytes: Vec::new(),
            error: None,
        })
    }

    fn drain_available(&mut self) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    self.reader = None;
                    return;
                }
                Ok(count) => {
                    let remaining =
                        (PROVIDER_OUTPUT_MAX as usize + 1).saturating_sub(self.bytes.len());
                    self.bytes
                        .extend_from_slice(&buffer[..count.min(remaining)]);
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(error) => {
                    self.error = Some(format!("read provider output: {error}"));
                    self.reader = None;
                    return;
                }
            }
        }
    }

    fn is_closed(&self) -> bool {
        self.reader.is_none()
    }

    fn finish(self) -> Result<Vec<u8>, String> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.bytes.len() as u64 > PROVIDER_OUTPUT_MAX {
            return Err("resource provider output exceeded 64 KiB".into());
        }
        Ok(self.bytes)
    }
}

fn validate_provider_receipt(
    receipt: &ProviderReceiptV1,
    operation: &str,
    outcome: &str,
    allocation_id: &str,
    expected_value: Option<&str>,
) -> Result<(), String> {
    if receipt.operation != operation
        || receipt.outcome != outcome
        || receipt.allocation_id != allocation_id
    {
        return Err("resource provider returned an unexpected receipt variant".into());
    }
    if receipt.value.is_empty() || receipt.value.as_bytes().contains(&0) {
        return Err("resource provider returned an invalid value".into());
    }
    if expected_value.is_some_and(|value| value != receipt.value) {
        return Err("resource provider inspect value drifted".into());
    }
    Ok(())
}

fn read_receipt(path: &Path, expected_sha256: &str) -> Result<ProviderReceiptV1, String> {
    read_secure_jcs(path, Some(expected_sha256))
}

fn rollback_created_database(
    provider: &ResourceProviderRegistrationV1,
    allocation_id: &str,
    session_id: &str,
    name: &str,
    value: &str,
    prior_receipt_sha256: &str,
) -> Result<ProviderReceiptV1, String> {
    let request = provider_request(
        "delete",
        allocation_id,
        session_id,
        name,
        Some(prior_receipt_sha256),
    );
    let receipt = invoke_provider(provider, request)?;
    validate_provider_receipt(&receipt, "delete", "released", allocation_id, Some(value))?;
    Ok(receipt)
}

fn combine_rollback_error(error: String, rollback: Result<(), String>) -> String {
    match rollback {
        Ok(()) => error,
        Err(rollback) => {
            format!("{error}; database allocation cleanup could not be proven: {rollback}")
        }
    }
}

fn read_secure_jcs<T: ClosedJcs>(path: &Path, expected_sha256: Option<&str>) -> Result<T, String> {
    let bytes = capability::read_secure_bytes(path)?;
    if let Some(expected) = expected_sha256 {
        schema::Sha256Digest::parse(expected)?;
        let actual = sha256::hex_digest(&bytes);
        if !sha256::constant_time_eq(actual.as_bytes(), expected.as_bytes()) {
            return Err(format!("digest mismatch for {}", path.display()));
        }
    }
    T::from_jcs(schema::parse_jcs(&bytes, true)?)
}

fn require_existing_private_directory(path: &Path, euid: u32) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(format!("{} is not a real directory", path.display())),
        Err(error) => {
            return Err(format!(
                "inspect existing private directory {}: {error}",
                path.display()
            ));
        }
    }
    authority::ensure_private_directory(path, euid)
}

fn validate_inventory(inventory: &AllocationInventoryV1) -> Result<(), String> {
    validate_decisions(&inventory.decisions)?;
    let requested = inventory
        .decisions
        .iter()
        .filter(|decision| decision.state == "requested")
        .map(|decision| ((decision.kind.as_str(), decision.name.as_str()), decision))
        .collect::<BTreeMap<_, _>>();
    if requested.len() != inventory.allocations.len() {
        return Err("resource inventory allocations do not match requested decisions".into());
    }
    let mut allocation_ids = BTreeSet::new();
    let mut allocation_keys = BTreeSet::new();
    for allocation in &inventory.allocations {
        if allocation.session_id != inventory.session_id
            || allocation.created_at != inventory.created_at
        {
            return Err("resource allocation identity differs from its inventory".into());
        }
        if !allocation_ids.insert(allocation.allocation_id.as_str())
            || !allocation_keys.insert((allocation.kind.as_str(), allocation.name.as_str()))
        {
            return Err("resource inventory contains duplicate allocations".into());
        }
        let decision = requested
            .get(&(allocation.kind.as_str(), allocation.name.as_str()))
            .ok_or_else(|| "resource allocation has no matching requested decision".to_string())?;
        let expected_provider = if allocation.kind == "database_schema" {
            decision
                .provider_id
                .as_deref()
                .ok_or_else(|| "database resource decision has no provider".to_string())?
        } else {
            BUILTIN_PROVIDER
        };
        if allocation.provider_id != expected_provider {
            return Err(format!(
                "resource {} provider differs from its decision",
                allocation.name
            ));
        }
    }
    Ok(())
}

fn validate_inventory_receipts(
    receipt_dir: &Path,
    inventory: &AllocationInventoryV1,
) -> Result<(), String> {
    for allocation in &inventory.allocations {
        let create: ProviderReceiptV1 = read_secure_jcs(
            &receipt_dir.join(format!(
                "{}-create-receipt-v1.json",
                allocation.allocation_id
            )),
            Some(&allocation.create_receipt_sha256),
        )?;
        validate_provider_receipt(
            &create,
            "create",
            "allocated",
            &allocation.allocation_id,
            Some(&allocation.value),
        )?;
        let inspect: ProviderReceiptV1 = read_secure_jcs(
            &receipt_dir.join(format!(
                "{}-inspect-receipt-v1.json",
                allocation.allocation_id
            )),
            Some(&allocation.inspect_receipt_sha256),
        )?;
        validate_provider_receipt(
            &inspect,
            "inspect",
            "exists",
            &allocation.allocation_id,
            Some(&allocation.value),
        )?;
        if allocation.provider_id == BUILTIN_PROVIDER {
            let evidence_matches = if allocation.kind == "port" {
                let expected =
                    builtin_evidence("port", &allocation.allocation_id, &allocation.value, None);
                create.provider_evidence_sha256 == expected
                    && inspect.provider_evidence_sha256 == expected
            } else {
                create.provider_evidence_sha256 == inspect.provider_evidence_sha256
            };
            if !evidence_matches {
                return Err(format!(
                    "builtin resource {} receipt identity drifted",
                    allocation.name
                ));
            }
        }
        if allocation.state == "released" {
            let digest = allocation
                .delete_receipt_sha256
                .as_deref()
                .ok_or_else(|| "released resource has no delete receipt".to_string())?;
            let delete: ProviderReceiptV1 = read_secure_jcs(
                &receipt_dir.join(format!(
                    "{}-delete-receipt-v1.json",
                    allocation.allocation_id
                )),
                Some(digest),
            )?;
            validate_provider_receipt(
                &delete,
                "delete",
                "released",
                &allocation.allocation_id,
                Some(&allocation.value),
            )?;
            if allocation.released_at.as_deref() != Some(delete.at.as_str()) {
                return Err(format!(
                    "resource {} release time differs from its receipt",
                    allocation.name
                ));
            }
        }
    }
    Ok(())
}

fn validate_resource_paths(
    resource_root: &Path,
    allocations: &[ResourceAllocationV1],
) -> Result<(), String> {
    for allocation in allocations.iter().filter(|allocation| {
        matches!(
            allocation.kind.as_str(),
            "temp_dir" | "build_dir" | "log_dir" | "cache_dir"
        )
    }) {
        let expected = resource_root.join(format!("{}-{}", allocation.kind, allocation.name));
        if Path::new(&allocation.value) != expected {
            return Err(format!(
                "resource directory {} escapes its deterministic root",
                allocation.name
            ));
        }
    }
    Ok(())
}

fn deterministic_builtin_receipt(
    operation: &str,
    outcome: &str,
    allocation_id: &str,
    value: &str,
    evidence: &str,
    at: &str,
) -> ProviderReceiptV1 {
    let request_id = deterministic_uuid(
        format!(
            "session-relay/resource-receipt-request/v1\0{operation}\0{outcome}\0{allocation_id}\0{value}\0{evidence}\0{at}"
        )
        .as_bytes(),
    );
    ProviderReceiptV1 {
        request_id,
        allocation_id: allocation_id.into(),
        operation: operation.into(),
        outcome: outcome.into(),
        value: value.into(),
        provider_evidence_sha256: evidence.into(),
        at: at.into(),
    }
}

fn deterministic_uuid(context: &[u8]) -> String {
    let digest = sha256::hex_digest(context);
    let mut uuid = digest.into_bytes();
    uuid[12] = b'4';
    uuid[16] = b'8';
    format!(
        "{}-{}-{}-{}-{}",
        std::str::from_utf8(&uuid[0..8]).expect("hex digest is UTF-8"),
        std::str::from_utf8(&uuid[8..12]).expect("hex digest is UTF-8"),
        std::str::from_utf8(&uuid[12..16]).expect("hex digest is UTF-8"),
        std::str::from_utf8(&uuid[16..20]).expect("hex digest is UTF-8"),
        std::str::from_utf8(&uuid[20..32]).expect("hex digest is UTF-8"),
    )
}

fn preflight_private_tree(path: &Path, euid: u32) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect resource path {}: {error}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != euid {
        return Err(format!(
            "resource path {} identity is not deletable",
            path.display()
        ));
    }
    for entry in fs::read_dir(path)
        .map_err(|error| format!("read resource path {}: {error}", path.display()))?
    {
        let entry = entry.map_err(|error| format!("read resource entry: {error}"))?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)
            .map_err(|error| format!("inspect resource entry {}: {error}", child.display()))?;
        if metadata.uid() != euid || metadata.file_type().is_symlink() {
            return Err(format!(
                "resource entry {} is not safely removable",
                child.display()
            ));
        }
        if metadata.is_dir() {
            preflight_private_tree(&child, euid)?;
        } else if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(format!(
                "resource entry {} has unsupported type or link count",
                child.display()
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn prove_recorded_port_released(value: &str) -> Result<(), String> {
    const PROC_TCP_MAX: u64 = 1024 * 1024;
    let address: SocketAddr = value
        .parse()
        .map_err(|_| "recorded held-port address is invalid".to_string())?;
    let SocketAddr::V4(address) = address else {
        return Err("recorded held-port address is not IPv4".into());
    };
    if *address.ip() != Ipv4Addr::LOCALHOST || address.port() == 0 {
        return Err("recorded held-port address is not canonical IPv4 loopback".into());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open("/proc/net/tcp")
        .map_err(|error| format!("open kernel TCP inventory: {error}"))?;
    let mut bytes = Vec::new();
    file.take(PROC_TCP_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read kernel TCP inventory: {error}"))?;
    if bytes.len() as u64 > PROC_TCP_MAX {
        return Err("kernel TCP inventory exceeded 1 MiB".into());
    }
    let inventory =
        std::str::from_utf8(&bytes).map_err(|_| "kernel TCP inventory is not UTF-8".to_string())?;
    for line in inventory.lines().skip(1) {
        let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
        if fields.len() < 4 || fields[3] != "0A" {
            continue;
        }
        let Some((ip_hex, port_hex)) = fields[1].split_once(':') else {
            return Err("kernel TCP inventory has a malformed local address".into());
        };
        let raw_ip = u32::from_str_radix(ip_hex, 16)
            .map_err(|_| "kernel TCP inventory has a malformed IPv4 address".to_string())?;
        let port = u16::from_str_radix(port_hex, 16)
            .map_err(|_| "kernel TCP inventory has a malformed port".to_string())?;
        if Ipv4Addr::from(raw_ip.to_ne_bytes()) == *address.ip() && port == address.port() {
            return Err(format!(
                "held port {} remains present after broker/runtime close",
                value
            ));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn prove_recorded_port_released(_value: &str) -> Result<(), String> {
    Err("held-port release proof is available only on Linux".into())
}

fn remove_private_tree(path: &Path, euid: u32) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect resource path {}: {error}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != euid {
        return Err(format!(
            "resource path {} identity is not deletable",
            path.display()
        ));
    }
    for entry in fs::read_dir(path)
        .map_err(|error| format!("read resource path {}: {error}", path.display()))?
    {
        let entry = entry.map_err(|error| format!("read resource entry: {error}"))?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)
            .map_err(|error| format!("inspect resource entry {}: {error}", child.display()))?;
        if metadata.uid() != euid || metadata.file_type().is_symlink() {
            return Err(format!(
                "resource entry {} is not safely removable",
                child.display()
            ));
        }
        if metadata.is_dir() {
            remove_private_tree(&child, euid)?;
        } else if metadata.is_file() && metadata.nlink() == 1 {
            fs::remove_file(&child)
                .map_err(|error| format!("remove resource file {}: {error}", child.display()))?;
        } else {
            return Err(format!(
                "resource entry {} has unsupported type or link count",
                child.display()
            ));
        }
    }
    fs::remove_dir(path)
        .map_err(|error| format!("remove resource directory {}: {error}", path.display()))
}

fn string_array(value: &schema::JcsValue, name: &str) -> Result<Vec<String>, String> {
    match value {
        schema::JcsValue::Array(values) => values
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect(),
        _ => Err(format!("{name} must be an array")),
    }
}

fn strings_value(values: &[String]) -> schema::JcsValue {
    schema::JcsValue::Array(
        values
            .iter()
            .cloned()
            .map(schema::JcsValue::String)
            .collect(),
    )
}

fn append_option(arguments: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = value {
        arguments.extend([flag.into(), value.into()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPOSITORY_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SESSION_ID: &str = "123e4567-e89b-42d3-a456-426614174000";
    const CREATED_AT: &str = "2026-07-22T00:00:00.000Z";

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "session-relay-resources-{label}-{}",
                crate::store::uuid_v4()
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_roots(temp: &TestRoot) -> (AuthorityRoots, PathBuf) {
        let authority = temp.0.join("authority");
        let data = temp.0.join("data");
        let receipts = temp.0.join("receipts");
        for path in [&authority, &data, &receipts] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        (
            AuthorityRoots {
                authority,
                data,
                euid: unsafe { libc::geteuid() },
            },
            receipts,
        )
    }

    fn decisions(requested: &[&str]) -> Vec<ResourceDecisionV1> {
        [
            "port",
            "temp_dir",
            "build_dir",
            "log_dir",
            "cache_dir",
            "database_schema",
        ]
        .into_iter()
        .map(|kind| {
            let is_requested = requested.contains(&kind);
            ResourceDecisionV1 {
                kind: kind.into(),
                name: format!("{kind}_test"),
                state: if is_requested { "requested" } else { "unused" }.into(),
                provider_id: (kind == "database_schema" && is_requested)
                    .then(|| "test_provider".to_string()),
                reason: (!is_requested).then(|| "task_does_not_use_resource".to_string()),
            }
        })
        .collect()
    }

    fn provider_fixture(
        temp: &TestRoot,
        label: &str,
        evidence_path: &Path,
        collision_path: Option<&Path>,
    ) -> ResourceProviderRegistrationV1 {
        let executable = temp.0.join(format!("provider-{label}.py"));
        fs::write(
            &executable,
            r#"#!/usr/bin/python3
import json
import os
import sys

config = json.load(os.fdopen(3))
request = json.load(sys.stdin)
if request["operation"] == "inspect" and config.get("collision"):
    try:
        fd = os.open(config["collision"], os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        os.write(fd, b"publication collision\n")
        os.fsync(fd)
        os.close(fd)
    except FileExistsError:
        pass
with open(config["evidence"], "r", encoding="ascii") as source:
    evidence = source.read().strip()
outcomes = {"create": "allocated", "inspect": "exists", "delete": "released"}
receipt = {
    "allocation_id": request["allocation_id"],
    "at": "2026-07-22T00:00:00.000Z",
    "operation": request["operation"],
    "outcome": outcomes[request["operation"]],
    "provider_evidence_sha256": evidence,
    "request_id": request["request_id"],
    "schema": "ProviderReceiptV1",
    "value": "schema_" + request["allocation_id"].replace("-", ""),
}
sys.stdout.write(json.dumps(receipt, sort_keys=True, separators=(",", ":")) + "\n")
"#,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let config = temp.0.join(format!("provider-{label}.json"));
        let collision = collision_path
            .map(|path| format!("\"{}\"", path.display()))
            .unwrap_or_else(|| "null".into());
        fs::write(
            &config,
            format!(
                "{{\"collision\":{collision},\"evidence\":\"{}\"}}\n",
                evidence_path.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        ResourceProviderRegistrationV1 {
            provider_id: "test_provider".into(),
            executable_path: executable.to_string_lossy().into_owned(),
            executable_sha256: sha256::hex_digest(&fs::read(&executable).unwrap()),
            config_path: config.to_string_lossy().into_owned(),
            config_sha256: sha256::hex_digest(&fs::read(&config).unwrap()),
            supported_kinds: vec!["database_schema".into()],
        }
    }

    fn install_registry(
        roots: &AuthorityRoots,
        provider: ResourceProviderRegistrationV1,
        replace: bool,
    ) {
        let registry = ResourceProviderRegistryV1 {
            providers: vec![provider],
            updated_at: CREATED_AT.into(),
        };
        let path = provider_registry_path(roots);
        if replace {
            authority::atomic_replace_jcs(&path, &registry, 0o600).unwrap();
        } else {
            authority::atomic_create_jcs(&path, &registry, 0o600).unwrap();
        }
    }

    fn close_evidence() -> ResourceReleaseEvidenceV1 {
        ResourceReleaseEvidenceV1 {
            broker_close_sha256: "b".repeat(64),
            runtime_empty_sha256: "c".repeat(64),
        }
    }

    #[test]
    fn verified_provider_fds_survive_executable_and_config_rename() {
        let temp = TestRoot::new("provider-fd-race");
        let evidence = temp.0.join("evidence");
        fs::write(&evidence, "b".repeat(64)).unwrap();
        let provider = provider_fixture(&temp, "trusted", &evidence, None);
        let files = open_verified_provider_files(&provider).unwrap();
        let executable = PathBuf::from(&provider.executable_path);
        let config = PathBuf::from(&provider.config_path);
        fs::rename(&executable, temp.0.join("verified-provider")).unwrap();
        fs::rename(&config, temp.0.join("verified-config")).unwrap();
        fs::write(&executable, "#!/bin/sh\nexit 97\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&config, b"{\"evidence\":\"malicious\"}\n").unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        let request = provider_request(
            "create",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            SESSION_ID,
            "database_schema_test",
            None,
        );
        let receipt = invoke_provider_with_files(&provider, request, files).unwrap();
        assert_eq!(receipt.provider_evidence_sha256, "b".repeat(64));
    }

    #[test]
    fn invalid_registry_does_not_publish_resource_root_and_retry_is_possible() {
        let temp = TestRoot::new("invalid-registry");
        let (roots, receipts) = fixture_roots(&temp);
        assert!(
            allocate_resources(
                &roots,
                REPOSITORY_ID,
                SESSION_ID,
                &decisions(&["database_schema"]),
                &receipts,
                CREATED_AT,
            )
            .is_err()
        );
        let resource_root = roots
            .data
            .join(REPOSITORY_ID)
            .join("resources")
            .join(SESSION_ID);
        assert!(!resource_root.exists());
        let evidence = temp.0.join("retry-evidence");
        fs::write(&evidence, "b".repeat(64)).unwrap();
        install_registry(
            &roots,
            provider_fixture(&temp, "retry", &evidence, None),
            false,
        );
        let resources = allocate_resources(
            &roots,
            REPOSITORY_ID,
            SESSION_ID,
            &decisions(&["database_schema"]),
            &receipts,
            CREATED_AT,
        )
        .unwrap();
        assert_eq!(resources.allocations.len(), 1);
    }

    #[test]
    fn publication_failure_rolls_back_every_allocation() {
        let temp = TestRoot::new("publication-rollback");
        let (roots, receipts) = fixture_roots(&temp);
        let evidence = temp.0.join("publication-evidence");
        fs::write(&evidence, "b".repeat(64)).unwrap();
        install_registry(
            &roots,
            provider_fixture(
                &temp,
                "publication",
                &evidence,
                Some(&receipts.join("resource-inventory-v1.json")),
            ),
            false,
        );
        let error = allocate_resources(
            &roots,
            REPOSITORY_ID,
            SESSION_ID,
            &decisions(&[
                "port",
                "temp_dir",
                "build_dir",
                "log_dir",
                "cache_dir",
                "database_schema",
            ]),
            &receipts,
            CREATED_AT,
        )
        .unwrap_err();
        assert!(error.contains("create-once publish"));
        let inventory: AllocationInventoryV1 =
            read_secure_jcs(&receipts.join("resource-inventory-v1.json"), None).unwrap();
        assert_eq!(inventory.allocations.len(), 6);
        assert!(
            inventory
                .allocations
                .iter()
                .all(|allocation| allocation.state == "released")
        );
        assert!(
            !roots
                .data
                .join(REPOSITORY_ID)
                .join("resources")
                .join(SESSION_ID)
                .exists()
        );
    }

    #[test]
    fn deletion_intent_reconciles_crash_after_delete_before_receipt() {
        let temp = TestRoot::new("delete-intent-crash");
        let (roots, receipts) = fixture_roots(&temp);
        let resources = allocate_resources(
            &roots,
            REPOSITORY_ID,
            SESSION_ID,
            &decisions(&["temp_dir"]),
            &receipts,
            CREATED_AT,
        )
        .unwrap();
        let allocation = resources.allocations[0].clone();
        resources
            .persist_deletion_intent(&allocation, "release", Some(&close_evidence()), CREATED_AT)
            .unwrap();
        remove_private_tree(Path::new(&allocation.value), roots.euid).unwrap();
        drop(resources);
        let first = release_resources(
            &roots,
            REPOSITORY_ID,
            SESSION_ID,
            &receipts,
            &close_evidence(),
            CREATED_AT,
        )
        .unwrap();
        let second = release_resources(
            &roots,
            REPOSITORY_ID,
            SESSION_ID,
            &receipts,
            &close_evidence(),
            CREATED_AT,
        )
        .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn provider_registration_is_pinned_for_live_session() {
        let temp = TestRoot::new("provider-pin");
        let (roots, receipts) = fixture_roots(&temp);
        let original_evidence = temp.0.join("original-evidence");
        fs::write(&original_evidence, "b".repeat(64)).unwrap();
        install_registry(
            &roots,
            provider_fixture(&temp, "original", &original_evidence, None),
            false,
        );
        let resources = allocate_resources(
            &roots,
            REPOSITORY_ID,
            SESSION_ID,
            &decisions(&["database_schema"]),
            &receipts,
            CREATED_AT,
        )
        .unwrap();
        let replacement_evidence = temp.0.join("replacement-evidence");
        fs::write(&replacement_evidence, "c".repeat(64)).unwrap();
        install_registry(
            &roots,
            provider_fixture(&temp, "replacement", &replacement_evidence, None),
            true,
        );
        drop(resources);
        let resources = load_resources(&roots, REPOSITORY_ID, SESSION_ID, &receipts).unwrap();
        let inspected = resources.inspect(&roots).unwrap();
        assert_eq!(inspected[0].provider_evidence_sha256, "b".repeat(64));
    }

    #[test]
    fn database_inspect_rejects_stable_evidence_mismatch() {
        let temp = TestRoot::new("database-evidence");
        let (roots, receipts) = fixture_roots(&temp);
        let evidence = temp.0.join("mutable-provider-evidence");
        fs::write(&evidence, "b".repeat(64)).unwrap();
        install_registry(
            &roots,
            provider_fixture(&temp, "evidence", &evidence, None),
            false,
        );
        let resources = allocate_resources(
            &roots,
            REPOSITORY_ID,
            SESSION_ID,
            &decisions(&["database_schema"]),
            &receipts,
            CREATED_AT,
        )
        .unwrap();
        fs::write(&evidence, "c".repeat(64)).unwrap();
        let error = resources.inspect(&roots).unwrap_err();
        assert!(error.contains("provider identity drifted"));
    }

    #[test]
    fn rollback_delete_receipts_and_terminal_inventory_are_durable() {
        let temp = TestRoot::new("rollback-proof");
        let (roots, receipts) = fixture_roots(&temp);
        let mut resources = allocate_resources(
            &roots,
            REPOSITORY_ID,
            SESSION_ID,
            &decisions(&["temp_dir", "build_dir"]),
            &receipts,
            CREATED_AT,
        )
        .unwrap();
        resources.rollback_unpublished(roots.euid).unwrap();
        let inventory: AllocationInventoryV1 =
            read_secure_jcs(&receipts.join("resource-inventory-v1.json"), None).unwrap();
        for allocation in &inventory.allocations {
            assert_eq!(allocation.state, "released");
            let digest = allocation.delete_receipt_sha256.as_deref().unwrap();
            read_receipt(
                &receipts.join(format!(
                    "{}-delete-receipt-v1.json",
                    allocation.allocation_id
                )),
                digest,
            )
            .unwrap();
        }
        assert!(
            !roots
                .data
                .join(REPOSITORY_ID)
                .join("resources")
                .join(SESSION_ID)
                .exists()
        );
    }

    #[test]
    fn close_bound_release_is_idempotent_and_rejects_evidence_drift() {
        let temp = TestRoot::new("close-bound-release");
        let (roots, receipts) = fixture_roots(&temp);
        let resources = allocate_resources(
            &roots,
            REPOSITORY_ID,
            SESSION_ID,
            &decisions(&["port"]),
            &receipts,
            CREATED_AT,
        )
        .unwrap();
        drop(resources);
        let evidence = close_evidence();
        let first = release_resources(
            &roots,
            REPOSITORY_ID,
            SESSION_ID,
            &receipts,
            &evidence,
            CREATED_AT,
        )
        .unwrap();
        let second = release_resources(
            &roots,
            REPOSITORY_ID,
            SESSION_ID,
            &receipts,
            &evidence,
            CREATED_AT,
        )
        .unwrap();
        assert_eq!(first, second);
        let drifted = ResourceReleaseEvidenceV1 {
            broker_close_sha256: "d".repeat(64),
            runtime_empty_sha256: evidence.runtime_empty_sha256,
        };
        assert!(
            release_resources(
                &roots,
                REPOSITORY_ID,
                SESSION_ID,
                &receipts,
                &drifted,
                CREATED_AT,
            )
            .unwrap_err()
            .contains("different close evidence")
        );
    }
}
