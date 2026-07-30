//! Atomic, package-private compute state store.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use super::model::*;

const LOCK_WAIT: Duration = Duration::from_secs(5);
const STALE_LOCK_AGE: Duration = Duration::from_secs(30);

/// Result of one canonical managed-surface lifecycle pass.
///
/// Orphaned bindings are retained as failed records rather than deleted. This
/// keeps worktree/session identities and user metadata available for diagnosis
/// or an explicit recovery flow.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ManagedBindingReconcileReport {
    pub examined: usize,
    pub failed_binding_ids: Vec<String>,
    pub preserved_live_binding_ids: Vec<String>,
    pub expired_lease_ids: Vec<String>,
}

/// Terminal identities cross WinRT, JSON, PowerShell and CLI boundaries.
/// WinRT formats GUIDs with braces and lower-case hex while the COM protocol
/// commonly returns upper-case GUIDs without braces. They identify the same
/// workspace/surface and must not fork lifecycle ownership in the store.
pub fn terminal_identity_eq(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    fn trim_terminal_guid(value: &str) -> &str {
        value.trim().trim_matches(['{', '}'])
    }
    match (
        Uuid::parse_str(trim_terminal_guid(left)),
        Uuid::parse_str(trim_terminal_guid(right)),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct TargetDocument {
    schema_version: u16,
    #[serde(default)]
    targets: BTreeMap<String, ComputeTarget>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct BindingDocument {
    schema_version: u16,
    #[serde(default)]
    bindings: BTreeMap<String, SurfaceBinding>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct LeaseDocument {
    schema_version: u16,
    #[serde(default)]
    leases: BTreeMap<String, Lease>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PolicyDocument {
    schema_version: u16,
    #[serde(default)]
    policies: BTreeMap<String, WorkspaceComputePolicy>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct RemoteWorkspaceDocument {
    schema_version: u16,
    #[serde(default)]
    workspaces: BTreeMap<String, RemoteWorkspaceSession>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct RemoteSurfaceDocument {
    schema_version: u16,
    #[serde(default)]
    surfaces: BTreeMap<String, RemoteSurfaceSession>,
}

pub struct ComputeStore {
    root: PathBuf,
}

impl ComputeStore {
    pub fn package_default() -> Result<Self> {
        let root = crate::runtime_paths::intelligent_terminal_root()
            .context("LOCALAPPDATA/APPDATA is unavailable")?
            .join("compute")
            .join("v1");
        Self::at(root)
    }

    pub fn at(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("jobs"))?;
        fs::create_dir_all(root.join("snapshots"))?;
        fs::create_dir_all(root.join("transfers"))?;
        fs::create_dir_all(root.join("proxies"))?;
        fs::create_dir_all(root.join("browsers"))?;
        fs::create_dir_all(root.join("browser-profiles"))?;
        fs::create_dir_all(root.join("file-root-policies"))?;
        fs::create_dir_all(root.join("environments"))?;
        fs::create_dir_all(root.join("endpoints"))?;
        fs::create_dir_all(root.join("connection-supervisors"))?;
        fs::create_dir_all(root.join("restore"))?;
        fs::create_dir_all(root.join("migrations"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list_targets(&self) -> Result<Vec<ComputeTarget>> {
        Ok(self.read_targets()?.targets.into_values().collect())
    }

    pub fn get_target(&self, id: &str) -> Result<ComputeTarget> {
        self.read_targets()?
            .targets
            .remove(id)
            .with_context(|| format!("unknown compute target: {id}"))
    }

    pub fn upsert_target(&self, actor: &str, mut target: ComputeTarget) -> Result<ComputeTarget> {
        validate_id("target id", &target.id)?;
        validate_target(&target)?;
        target.schema_version = COMPUTE_SCHEMA_VERSION;
        let _lock = self.acquire_lock()?;
        let mut document = self.read_targets()?;
        let created = !document.targets.contains_key(&target.id);
        document.targets.insert(target.id.clone(), target.clone());
        self.write_json(&self.targets_path(), &document)?;
        self.append_event(ComputeEvent::new(
            if created {
                "target.created"
            } else {
                "target.updated"
            },
            actor,
            Some(target.id.clone()),
            None,
            json!({"provider": target.provider, "disabled": target.disabled}),
        ))?;
        Ok(target)
    }

    pub fn remove_target(&self, actor: &str, id: &str) -> Result<ComputeTarget> {
        let _lock = self.acquire_lock()?;
        let mut document = self.read_targets()?;
        let target = document
            .targets
            .remove(id)
            .with_context(|| format!("unknown compute target: {id}"))?;
        let bindings = self.read_bindings()?;
        if bindings
            .bindings
            .values()
            .any(|binding| binding.home_target_id.as_deref() == Some(id))
        {
            bail!("target {id} is referenced by a surface binding; disable it instead");
        }
        if self.read_leases()?.leases.values().any(|lease| {
            lease.target_id.as_deref() == Some(id) && lease.state == LeaseState::Active
        }) {
            bail!("target {id} has an active lease; revoke or release it first");
        }
        if self
            .read_remote_workspaces()?
            .workspaces
            .values()
            .any(|workspace| {
                workspace.target_id == id && workspace.state != RemoteWorkspaceState::Closed
            })
        {
            bail!("target {id} is referenced by a remote workspace; close it first");
        }
        if self.list_environments()?.iter().any(|environment| {
            environment.target_id == id
                && environment.lifecycle_state != EnvironmentLifecycleState::Retired
        }) {
            bail!("target {id} is referenced by an execution environment; retire it first");
        }
        self.write_json(&self.targets_path(), &document)?;
        self.append_event(ComputeEvent::new(
            "target.removed",
            actor,
            Some(id.to_string()),
            None,
            Value::Null,
        ))?;
        Ok(target)
    }

    pub fn set_target_disabled(
        &self,
        actor: &str,
        id: &str,
        disabled: bool,
    ) -> Result<ComputeTarget> {
        let mut target = self.get_target(id)?;
        target.disabled = disabled;
        self.upsert_target(actor, target)
    }

    pub fn save_environment(
        &self,
        actor: &str,
        mut environment: ExecutionEnvironment,
    ) -> Result<ExecutionEnvironment> {
        validate_id("environment id", &environment.environment_id)?;
        validate_id("environment target id", &environment.target_id)?;
        self.get_target(&environment.target_id)?;
        if environment.runtime_version.trim().is_empty() {
            bail!("execution environment runtime version cannot be empty");
        }
        if environment.protocol_version == 0 {
            bail!("execution environment protocol version must be non-zero");
        }
        if environment.os.trim().is_empty() || environment.arch.trim().is_empty() {
            bail!("execution environment OS and architecture are required");
        }
        environment.schema_version = COMPUTE_SCHEMA_VERSION;
        environment.capabilities.sort();
        environment.capabilities.dedup();
        let now = now_ms();
        let path = self
            .root
            .join("environments")
            .join(format!("{}.json", environment.environment_id));
        let created = !path.exists();
        if created || environment.created_at_ms == 0 {
            environment.created_at_ms = now;
        } else if let Ok(previous) = self.read_json::<ExecutionEnvironment>(&path) {
            environment.created_at_ms = previous.created_at_ms;
        }
        environment.updated_at_ms = now;
        let _lock = self.acquire_lock()?;
        self.write_json(&path, &environment)?;
        self.append_event(ComputeEvent::new(
            if created {
                "environment.created"
            } else {
                "environment.updated"
            },
            actor,
            Some(environment.environment_id.clone()),
            None,
            json!({
                "target_id": environment.target_id,
                "state": environment.lifecycle_state,
                "launch_method": environment.launch_method,
            }),
        ))?;
        Ok(environment)
    }

    pub fn get_environment(&self, id: &str) -> Result<ExecutionEnvironment> {
        validate_id("environment id", id)?;
        self.read_json(&self.root.join("environments").join(format!("{id}.json")))
            .with_context(|| format!("unknown or invalid execution environment: {id}"))
    }

    pub fn list_environments(&self) -> Result<Vec<ExecutionEnvironment>> {
        let mut environments = self.read_json_directory("environments")?;
        environments.sort_by(|left: &ExecutionEnvironment, right| {
            left.environment_id.cmp(&right.environment_id)
        });
        Ok(environments)
    }

    pub fn remove_environment(&self, actor: &str, id: &str) -> Result<ExecutionEnvironment> {
        let environment = self.get_environment(id)?;
        if self
            .list_endpoints(Some(id))?
            .iter()
            .any(|endpoint| endpoint.enabled)
        {
            bail!("execution environment {id} has enabled endpoints");
        }
        if let Ok(supervisor) = self.get_connection_supervisor(id) {
            if !matches!(
                supervisor.state,
                EnvironmentConnectionState::Disconnected
                    | EnvironmentConnectionState::Offline
                    | EnvironmentConnectionState::Failed
            ) {
                bail!("execution environment {id} has an active connection supervisor");
            }
        }
        let _lock = self.acquire_lock()?;
        fs::remove_file(self.root.join("environments").join(format!("{id}.json")))?;
        let supervisor_path = self
            .root
            .join("connection-supervisors")
            .join(format!("{id}.json"));
        if supervisor_path.exists() {
            fs::remove_file(supervisor_path)?;
        }
        self.append_event(ComputeEvent::new(
            "environment.removed",
            actor,
            Some(id.to_string()),
            None,
            Value::Null,
        ))?;
        Ok(environment)
    }

    pub fn save_endpoint(
        &self,
        actor: &str,
        mut endpoint: AccessEndpoint,
    ) -> Result<AccessEndpoint> {
        validate_id("endpoint id", &endpoint.endpoint_id)?;
        validate_id("endpoint environment id", &endpoint.environment_id)?;
        self.get_environment(&endpoint.environment_id)?;
        if endpoint.enabled
            && matches!(
                endpoint.kind,
                AccessEndpointKind::Tailscale
                    | AccessEndpointKind::AuthenticatedWss
                    | AccessEndpointKind::Relay
            )
        {
            bail!(
                "endpoint kind {:?} is a future contract and cannot be enabled in this release",
                endpoint.kind
            );
        }
        if endpoint.kind == AccessEndpointKind::SshForward
            && endpoint.reachability != EndpointReachability::SshRequired
        {
            bail!("SSH-forward endpoints must declare ssh_required reachability");
        }
        endpoint.schema_version = COMPUTE_SCHEMA_VERSION;
        let now = now_ms();
        let path = self
            .root
            .join("endpoints")
            .join(format!("{}.json", endpoint.endpoint_id));
        let created = !path.exists();
        if created || endpoint.created_at_ms == 0 {
            endpoint.created_at_ms = now;
        } else if let Ok(previous) = self.read_json::<AccessEndpoint>(&path) {
            endpoint.created_at_ms = previous.created_at_ms;
        }
        endpoint.updated_at_ms = now;
        let _lock = self.acquire_lock()?;
        self.write_json(&path, &endpoint)?;
        self.append_event(ComputeEvent::new(
            if created {
                "endpoint.created"
            } else {
                "endpoint.updated"
            },
            actor,
            Some(endpoint.endpoint_id.clone()),
            None,
            json!({
                "environment_id": endpoint.environment_id,
                "kind": endpoint.kind,
                "health": endpoint.health,
                "enabled": endpoint.enabled,
            }),
        ))?;
        Ok(endpoint)
    }

    pub fn get_endpoint(&self, id: &str) -> Result<AccessEndpoint> {
        validate_id("endpoint id", id)?;
        self.read_json(&self.root.join("endpoints").join(format!("{id}.json")))
            .with_context(|| format!("unknown or invalid access endpoint: {id}"))
    }

    pub fn list_endpoints(&self, environment_id: Option<&str>) -> Result<Vec<AccessEndpoint>> {
        let mut endpoints = self
            .read_json_directory::<AccessEndpoint>("endpoints")?
            .into_iter()
            .filter(|endpoint| environment_id.is_none_or(|value| endpoint.environment_id == value))
            .collect::<Vec<_>>();
        endpoints.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.endpoint_id.cmp(&right.endpoint_id))
        });
        Ok(endpoints)
    }

    pub fn remove_endpoint(&self, actor: &str, id: &str) -> Result<AccessEndpoint> {
        let endpoint = self.get_endpoint(id)?;
        if let Ok(supervisor) = self.get_connection_supervisor(&endpoint.environment_id) {
            if supervisor.current_endpoint_id.as_deref() == Some(id)
                && supervisor.state == EnvironmentConnectionState::Connected
            {
                bail!("endpoint {id} is owned by a connected supervisor");
            }
        }
        let _lock = self.acquire_lock()?;
        fs::remove_file(self.root.join("endpoints").join(format!("{id}.json")))?;
        self.append_event(ComputeEvent::new(
            "endpoint.removed",
            actor,
            Some(id.to_string()),
            None,
            json!({"environment_id": endpoint.environment_id}),
        ))?;
        Ok(endpoint)
    }

    pub fn save_connection_supervisor(
        &self,
        actor: &str,
        mut supervisor: EnvironmentConnectionSupervisor,
    ) -> Result<EnvironmentConnectionSupervisor> {
        validate_id("connection environment id", &supervisor.environment_id)?;
        self.get_environment(&supervisor.environment_id)?;
        if let Some(endpoint_id) = supervisor.current_endpoint_id.as_deref() {
            let endpoint = self.get_endpoint(endpoint_id)?;
            if endpoint.environment_id != supervisor.environment_id {
                bail!("connection supervisor endpoint belongs to another environment");
            }
        }
        supervisor.schema_version = COMPUTE_SCHEMA_VERSION;
        let now = now_ms();
        let path = self
            .root
            .join("connection-supervisors")
            .join(format!("{}.json", supervisor.environment_id));
        let created = !path.exists();
        if created || supervisor.created_at_ms == 0 {
            supervisor.created_at_ms = now;
        } else if let Ok(previous) = self.read_json::<EnvironmentConnectionSupervisor>(&path) {
            supervisor.created_at_ms = previous.created_at_ms;
            if supervisor.generation < previous.generation {
                bail!("connection supervisor generation cannot move backwards");
            }
        }
        supervisor.updated_at_ms = now;
        let _lock = self.acquire_lock()?;
        self.write_json(&path, &supervisor)?;
        self.append_event(ComputeEvent::new(
            if created {
                "connection.created"
            } else {
                "connection.updated"
            },
            actor,
            Some(supervisor.environment_id.clone()),
            None,
            json!({
                "state": supervisor.state,
                "endpoint_id": supervisor.current_endpoint_id,
                "retry_attempt": supervisor.retry_attempt,
                "generation": supervisor.generation,
            }),
        ))?;
        Ok(supervisor)
    }

    pub fn get_connection_supervisor(
        &self,
        environment_id: &str,
    ) -> Result<EnvironmentConnectionSupervisor> {
        validate_id("connection environment id", environment_id)?;
        self.read_json(
            &self
                .root
                .join("connection-supervisors")
                .join(format!("{environment_id}.json")),
        )
        .with_context(|| {
            format!("unknown or invalid environment connection supervisor: {environment_id}")
        })
    }

    pub fn list_connection_supervisors(&self) -> Result<Vec<EnvironmentConnectionSupervisor>> {
        let mut supervisors = self.read_json_directory("connection-supervisors")?;
        supervisors.sort_by(|left: &EnvironmentConnectionSupervisor, right| {
            left.environment_id.cmp(&right.environment_id)
        });
        Ok(supervisors)
    }

    pub fn list_bindings(&self) -> Result<Vec<SurfaceBinding>> {
        Ok(self.read_bindings()?.bindings.into_values().collect())
    }

    pub fn list_remote_workspaces(&self) -> Result<Vec<RemoteWorkspaceSession>> {
        Ok(self
            .read_remote_workspaces()?
            .workspaces
            .into_values()
            .collect())
    }

    pub fn get_remote_workspace(&self, id: &str) -> Result<RemoteWorkspaceSession> {
        self.read_remote_workspaces()?
            .workspaces
            .remove(id)
            .with_context(|| format!("unknown remote workspace: {id}"))
    }

    pub fn upsert_remote_workspace(
        &self,
        actor: &str,
        mut workspace: RemoteWorkspaceSession,
    ) -> Result<RemoteWorkspaceSession> {
        validate_id("remote workspace id", &workspace.remote_workspace_id)?;
        validate_id("workspace id", &workspace.workspace_id)?;
        validate_id("window id", &workspace.window_id)?;
        let target = self.get_target(&workspace.target_id)?;
        if !matches!(target.provider, ProviderKind::Ssh | ProviderKind::Azure) {
            bail!("remote workspace target must be SSH-backed");
        }
        if let Some(environment_id) = workspace.environment_id.as_deref() {
            let environment = self.get_environment(environment_id)?;
            if environment.target_id != workspace.target_id {
                bail!("remote workspace environment does not match its target");
            }
            if workspace.preferred_endpoint_kind.is_none() {
                bail!("remote workspace environment requires a preferred endpoint kind");
            }
        } else if workspace.preferred_endpoint_kind.is_some() {
            bail!("remote workspace endpoint preference requires an environment");
        }
        if workspace.reconnect_policy.delays_seconds.is_empty()
            || workspace.reconnect_policy.ceiling_seconds == 0
            || workspace
                .reconnect_policy
                .delays_seconds
                .iter()
                .any(|delay| *delay == 0 || *delay > workspace.reconnect_policy.ceiling_seconds)
        {
            bail!("remote workspace reconnect policy is invalid");
        }
        let now = now_ms();
        workspace.schema_version = COMPUTE_SCHEMA_VERSION;
        if workspace.created_at_ms == 0 {
            workspace.created_at_ms = now;
        }
        workspace.updated_at_ms = now;
        let _lock = self.acquire_lock()?;
        let mut document = self.read_remote_workspaces()?;
        if let Some(existing) = document.workspaces.values().find(|candidate| {
            candidate.remote_workspace_id != workspace.remote_workspace_id
                && candidate.window_id == workspace.window_id
                && candidate.workspace_id == workspace.workspace_id
                && candidate.state != RemoteWorkspaceState::Closed
        }) {
            bail!(
                "native workspace is already owned by remote workspace {}",
                existing.remote_workspace_id
            );
        }
        let created = !document
            .workspaces
            .contains_key(&workspace.remote_workspace_id);
        document
            .workspaces
            .insert(workspace.remote_workspace_id.clone(), workspace.clone());
        self.write_json(&self.remote_workspaces_path(), &document)?;
        self.append_event(ComputeEvent::new(
            if created {
                "remote_workspace.created"
            } else {
                "remote_workspace.updated"
            },
            actor,
            Some(workspace.remote_workspace_id.clone()),
            Some(workspace.workspace_id.clone()),
            json!({
                "target_id": workspace.target_id,
                "state": workspace.state,
                "reconnect_attempt": workspace.reconnect_attempt,
            }),
        ))?;
        Ok(workspace)
    }

    pub fn remove_remote_workspace(&self, actor: &str, id: &str) -> Result<RemoteWorkspaceSession> {
        let _lock = self.acquire_lock()?;
        let mut workspaces = self.read_remote_workspaces()?;
        let workspace = workspaces
            .workspaces
            .remove(id)
            .with_context(|| format!("unknown remote workspace: {id}"))?;
        let surfaces = self.read_remote_surfaces()?;
        if surfaces
            .surfaces
            .values()
            .any(|surface| surface.remote_workspace_id == id)
        {
            bail!("remote workspace {id} still owns remote surfaces");
        }
        self.write_json(&self.remote_workspaces_path(), &workspaces)?;
        self.append_event(ComputeEvent::new(
            "remote_workspace.removed",
            actor,
            Some(id.to_string()),
            Some(workspace.workspace_id.clone()),
            Value::Null,
        ))?;
        Ok(workspace)
    }

    pub fn list_remote_surfaces(
        &self,
        remote_workspace_id: Option<&str>,
    ) -> Result<Vec<RemoteSurfaceSession>> {
        Ok(self
            .read_remote_surfaces()?
            .surfaces
            .into_values()
            .filter(|surface| {
                remote_workspace_id.is_none_or(|workspace| surface.remote_workspace_id == workspace)
            })
            .collect())
    }

    pub fn upsert_remote_surface(
        &self,
        actor: &str,
        mut surface: RemoteSurfaceSession,
    ) -> Result<RemoteSurfaceSession> {
        validate_id("remote surface id", &surface.remote_surface_id)?;
        validate_id("remote workspace id", &surface.remote_workspace_id)?;
        validate_id("binding id", &surface.binding_id)?;
        validate_id("pty session id", &surface.pty_session_id)?;
        self.get_remote_workspace(&surface.remote_workspace_id)?;
        self.get_binding(&surface.binding_id)?;
        let now = now_ms();
        surface.schema_version = COMPUTE_SCHEMA_VERSION;
        if surface.created_at_ms == 0 {
            surface.created_at_ms = now;
        }
        surface.updated_at_ms = now;
        let _lock = self.acquire_lock()?;
        let mut document = self.read_remote_surfaces()?;
        let created = !document.surfaces.contains_key(&surface.remote_surface_id);
        document
            .surfaces
            .insert(surface.remote_surface_id.clone(), surface.clone());
        self.write_json(&self.remote_surfaces_path(), &document)?;
        self.append_event(ComputeEvent::new(
            if created {
                "remote_surface.created"
            } else {
                "remote_surface.updated"
            },
            actor,
            Some(surface.remote_surface_id.clone()),
            Some(surface.remote_workspace_id.clone()),
            json!({"kind": surface.kind, "state": surface.state}),
        ))?;
        Ok(surface)
    }

    pub fn remove_remote_surface(&self, actor: &str, id: &str) -> Result<RemoteSurfaceSession> {
        let _lock = self.acquire_lock()?;
        let mut document = self.read_remote_surfaces()?;
        let surface = document
            .surfaces
            .remove(id)
            .with_context(|| format!("unknown remote surface: {id}"))?;
        self.write_json(&self.remote_surfaces_path(), &document)?;
        self.append_event(ComputeEvent::new(
            "remote_surface.removed",
            actor,
            Some(id.to_string()),
            Some(surface.remote_workspace_id.clone()),
            Value::Null,
        ))?;
        Ok(surface)
    }

    pub fn find_surface_binding(
        &self,
        window_id: &str,
        workspace_id: &str,
        surface_id: &str,
    ) -> Result<Option<SurfaceBinding>> {
        Ok(self
            .read_bindings()?
            .bindings
            .into_values()
            .find(|binding| {
                terminal_identity_eq(&binding.window_id, window_id)
                    && terminal_identity_eq(&binding.workspace_id, workspace_id)
                    && terminal_identity_eq(&binding.surface_id, surface_id)
            }))
    }

    pub fn list_policies(&self) -> Result<Vec<WorkspaceComputePolicy>> {
        Ok(self.read_policies()?.policies.into_values().collect())
    }

    pub fn get_policy(&self, workspace_id: &str) -> Result<WorkspaceComputePolicy> {
        self.read_policies()?
            .policies
            .remove(workspace_id)
            .with_context(|| format!("no compute policy for workspace: {workspace_id}"))
    }

    pub fn upsert_policy(
        &self,
        actor: &str,
        mut policy: WorkspaceComputePolicy,
    ) -> Result<WorkspaceComputePolicy> {
        validate_id("workspace id", &policy.workspace_id)?;
        if policy.production_targets_allowed && policy.required_trust_tier != TrustTier::Production
        {
            bail!("production_targets_allowed requires required_trust_tier=production");
        }
        for target in &policy.eligible_target_ids {
            self.get_target(target)?;
        }
        policy.schema_version = COMPUTE_SCHEMA_VERSION;
        let _lock = self.acquire_lock()?;
        let mut document = self.read_policies()?;
        let created = !document.policies.contains_key(&policy.workspace_id);
        document
            .policies
            .insert(policy.workspace_id.clone(), policy.clone());
        self.write_json(&self.policies_path(), &document)?;
        self.append_event(ComputeEvent::new(
            if created {
                "policy.created"
            } else {
                "policy.updated"
            },
            actor,
            Some(policy.workspace_id.clone()),
            Some(policy.workspace_id.clone()),
            serde_json::to_value(&policy)?,
        ))?;
        Ok(policy)
    }

    pub fn remove_policy(&self, actor: &str, workspace_id: &str) -> Result<WorkspaceComputePolicy> {
        let _lock = self.acquire_lock()?;
        let mut document = self.read_policies()?;
        let policy = document
            .policies
            .remove(workspace_id)
            .with_context(|| format!("no compute policy for workspace: {workspace_id}"))?;
        self.write_json(&self.policies_path(), &document)?;
        self.append_event(ComputeEvent::new(
            "policy.removed",
            actor,
            Some(workspace_id.to_string()),
            Some(workspace_id.to_string()),
            Value::Null,
        ))?;
        Ok(policy)
    }

    pub fn get_binding(&self, id: &str) -> Result<SurfaceBinding> {
        self.read_bindings()?
            .bindings
            .remove(id)
            .with_context(|| format!("unknown surface binding: {id}"))
    }

    pub fn upsert_binding(
        &self,
        actor: &str,
        mut binding: SurfaceBinding,
    ) -> Result<SurfaceBinding> {
        validate_id("binding id", &binding.binding_id)?;
        if binding.workspace_id.trim().is_empty()
            || binding.pane_id.trim().is_empty()
            || binding.surface_id.trim().is_empty()
        {
            bail!("workspace_id, pane_id and surface_id are required");
        }
        if binding.kind == BindingKind::ManagedAgent && binding.home_target_id.is_none() {
            bail!("managed_agent binding requires home_target_id");
        }
        if binding.kind == BindingKind::ManagedAgent
            && binding
                .agent_id
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            bail!("managed_agent binding requires agent_id");
        }
        if binding.kind == BindingKind::ManagedAgent
            && binding
                .adapter_kind
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            bail!("managed_agent binding requires adapter_kind");
        }
        if let Some(target) = binding.home_target_id.as_deref() {
            self.get_target(target)?;
        }
        if let Some(environment_id) = binding.environment_id.as_deref() {
            let environment = self.get_environment(environment_id)?;
            if binding.home_target_id.as_deref() != Some(environment.target_id.as_str()) {
                bail!("surface binding environment does not match its HomeTarget");
            }
            if binding.preferred_endpoint_kind.is_none() {
                bail!("surface binding environment requires a preferred endpoint kind");
            }
        } else if binding.preferred_endpoint_kind.is_some() {
            bail!("surface binding endpoint preference requires an environment");
        }
        let now = now_ms();
        binding.schema_version = COMPUTE_SCHEMA_VERSION;
        if binding.created_at_ms == 0 {
            binding.created_at_ms = now;
        }
        binding.updated_at_ms = now;
        let _lock = self.acquire_lock()?;
        let mut document = self.read_bindings()?;
        if let Some(existing) = document.bindings.values().find(|candidate| {
            candidate.binding_id != binding.binding_id
                && candidate.window_id == binding.window_id
                && candidate.workspace_id == binding.workspace_id
                && candidate.surface_id == binding.surface_id
        }) {
            bail!(
                "surface is already bound by {} (surface identity must be unique)",
                existing.binding_id
            );
        }
        let created = !document.bindings.contains_key(&binding.binding_id);
        document
            .bindings
            .insert(binding.binding_id.clone(), binding.clone());
        self.write_json(&self.bindings_path(), &document)?;
        self.append_event(ComputeEvent::new(
            if created {
                "binding.created"
            } else {
                "binding.updated"
            },
            actor,
            Some(binding.binding_id.clone()),
            Some(binding.workspace_id.clone()),
            json!({
                "surface_id": binding.surface_id,
                "kind": binding.kind,
                "state": binding.state,
                "home_target_id": binding.home_target_id,
            }),
        ))?;
        Ok(binding)
    }

    pub fn remove_binding(&self, actor: &str, id: &str) -> Result<SurfaceBinding> {
        let _lock = self.acquire_lock()?;
        let mut document = self.read_bindings()?;
        let binding = document
            .bindings
            .remove(id)
            .with_context(|| format!("unknown surface binding: {id}"))?;

        // Binding lifecycle owns every lease whose subject is the binding,
        // not only the writer lease copied onto SurfaceBinding. Agent-slot
        // leases are intentionally separate records; leaving one active after
        // its surface closes consumes target capacity until TTL expiry and can
        // make the scheduler reject an otherwise idle machine.
        let lease_ids = self
            .read_leases()?
            .leases
            .values()
            .filter(|lease| lease.subject_id == id && lease.state == LeaseState::Active)
            .map(|lease| lease.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in lease_ids {
            self.revoke_lease_unlocked(actor, &lease_id, "binding removed")?;
        }
        self.write_json(&self.bindings_path(), &document)?;
        self.append_event(ComputeEvent::new(
            "binding.removed",
            actor,
            Some(id.to_string()),
            Some(binding.workspace_id.clone()),
            json!({"surface_id": binding.surface_id}),
        ))?;
        Ok(binding)
    }

    /// Remove the binding owned by a concrete terminal surface. This is the
    /// presentation-lifecycle entry point used when no ACP pane/master is
    /// running. It is intentionally idempotent because a live master may
    /// observe the same close event and race this direct host cleanup.
    pub fn remove_surface_binding(
        &self,
        actor: &str,
        window_id: &str,
        workspace_id: &str,
        surface_id: &str,
    ) -> Result<Option<SurfaceBinding>> {
        let Some(binding) = self.find_surface_binding(window_id, workspace_id, surface_id)? else {
            return Ok(None);
        };
        self.remove_binding(actor, &binding.binding_id).map(Some)
    }

    /// Record proof that the runtime owned by a managed binding is still alive.
    ///
    /// The heartbeat is persisted on the binding itself so a later process can
    /// distinguish an interrupted create transaction from a slow but live
    /// runtime. Callers should heartbeat only after positively observing the
    /// exact process/runtime represented by this binding.
    pub fn heartbeat_binding_runtime(&self, actor: &str, id: &str) -> Result<SurfaceBinding> {
        self.heartbeat_binding_runtime_at(actor, id, now_ms())
    }

    fn heartbeat_binding_runtime_at(
        &self,
        actor: &str,
        id: &str,
        timestamp_ms: u64,
    ) -> Result<SurfaceBinding> {
        let _lock = self.acquire_lock()?;
        let mut document = self.read_bindings()?;
        let binding = document
            .bindings
            .get_mut(id)
            .with_context(|| format!("unknown surface binding: {id}"))?;
        if binding.kind != BindingKind::ManagedAgent {
            bail!("binding {id} is not a managed agent");
        }
        let prior = runtime_heartbeat_at_ms(binding);
        if prior.is_some_and(|prior| prior >= timestamp_ms) {
            return Ok(binding.clone());
        }
        ensure_object(&mut binding.metadata);
        binding.metadata["runtime_heartbeat_at_ms"] = json!(timestamp_ms);
        binding.updated_at_ms = binding.updated_at_ms.max(timestamp_ms);
        let result = binding.clone();
        self.write_json(&self.bindings_path(), &document)?;
        self.append_event(ComputeEvent::new(
            "binding.runtime_heartbeat",
            actor,
            Some(id.to_string()),
            Some(result.workspace_id.clone()),
            json!({"heartbeat_at_ms": timestamp_ms}),
        ))?;
        Ok(result)
    }

    /// Reconcile managed bindings left in `creating` after an interrupted
    /// transaction.
    ///
    /// A binding is considered live when at least one exact ownership signal
    /// remains current:
    ///
    /// * an unexpired lease whose subject/owner is the binding;
    /// * a non-terminal job requested by the binding (or using its lease);
    /// * a recent persisted runtime heartbeat.
    ///
    /// A grace period protects normal startup races. Once it elapses without
    /// evidence, the binding transitions to `failed` and remains in the store
    /// with all worktree, ACP, runtime and user metadata intact. The operation
    /// is idempotent because only `creating` records are candidates.
    pub fn reconcile_stale_managed_bindings(
        &self,
        actor: &str,
        stale_after_ms: u64,
    ) -> Result<ManagedBindingReconcileReport> {
        self.reconcile_stale_managed_bindings_at(actor, now_ms(), stale_after_ms)
    }

    fn reconcile_stale_managed_bindings_at(
        &self,
        actor: &str,
        now: u64,
        stale_after_ms: u64,
    ) -> Result<ManagedBindingReconcileReport> {
        if stale_after_ms == 0 {
            bail!("managed binding stale threshold must be greater than zero");
        }

        // Jobs are independent per-directory documents. Snapshot them before
        // taking the aggregate state lock; save_job does not participate in
        // that lock and lifecycle reconciliation must never block job output.
        let jobs = self.list_jobs()?;
        let _lock = self.acquire_lock()?;
        let mut bindings = self.read_bindings()?;
        let mut leases = self.read_leases()?;
        let mut report = ManagedBindingReconcileReport::default();

        // Persist expiration here. list_leases intentionally projects expiry
        // without writing, which is useful for reads but insufficient for an
        // authoritative lifecycle decision.
        for lease in leases.leases.values_mut() {
            if lease.state == LeaseState::Active && lease.expires_at_ms <= now {
                lease.state = LeaseState::Expired;
                report.expired_lease_ids.push(lease.lease_id.clone());
            }
        }

        for binding in bindings.bindings.values_mut() {
            if binding.kind != BindingKind::ManagedAgent || binding.state != BindingState::Creating
            {
                continue;
            }
            report.examined += 1;
            if now.saturating_sub(binding.updated_at_ms) < stale_after_ms {
                continue;
            }

            let has_live_lease = leases.leases.values().any(|lease| {
                lease.state == LeaseState::Active
                    && lease.expires_at_ms > now
                    && (lease.subject_id == binding.binding_id
                        || lease.owner == binding.binding_id
                        || binding.writer_lease_id.as_deref() == Some(lease.lease_id.as_str()))
            });
            let owned_lease_ids = leases
                .leases
                .values()
                .filter(|lease| {
                    lease.subject_id == binding.binding_id
                        || lease.owner == binding.binding_id
                        || binding.writer_lease_id.as_deref() == Some(lease.lease_id.as_str())
                })
                .map(|lease| lease.lease_id.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let has_live_job = jobs.iter().any(|job| {
                !job.state.is_terminal()
                    && (job.request.requested_by == binding.binding_id
                        || job
                            .lease_id
                            .as_deref()
                            .is_some_and(|lease_id| owned_lease_ids.contains(lease_id)))
            });
            let has_process_heartbeat = runtime_heartbeat_at_ms(binding)
                .is_some_and(|heartbeat| now.saturating_sub(heartbeat) < stale_after_ms);

            if has_live_lease || has_live_job || has_process_heartbeat {
                report
                    .preserved_live_binding_ids
                    .push(binding.binding_id.clone());
                continue;
            }

            binding.state = BindingState::Failed;
            binding.updated_at_ms = now;
            ensure_object(&mut binding.metadata);
            binding.metadata["lifecycle_reconcile"] = json!({
                "at_ms": now,
                "reason": "creating_without_liveness",
                "previous_state": "creating",
                "stale_after_ms": stale_after_ms,
            });
            report.failed_binding_ids.push(binding.binding_id.clone());
        }

        report.expired_lease_ids.sort();
        report.failed_binding_ids.sort();
        report.preserved_live_binding_ids.sort();
        if !report.expired_lease_ids.is_empty() {
            self.write_json(&self.leases_path(), &leases)?;
        }
        if !report.failed_binding_ids.is_empty() {
            self.write_json(&self.bindings_path(), &bindings)?;
            for id in &report.failed_binding_ids {
                let binding = bindings
                    .bindings
                    .get(id)
                    .expect("failed binding came from the binding document");
                self.append_event(ComputeEvent::new(
                    "binding.reconcile_failed",
                    actor,
                    Some(id.clone()),
                    Some(binding.workspace_id.clone()),
                    json!({
                        "reason": "creating_without_liveness",
                        "state": binding.state,
                        "stale_after_ms": stale_after_ms,
                    }),
                ))?;
            }
        }
        Ok(report)
    }

    pub fn list_leases(&self) -> Result<Vec<Lease>> {
        let mut document = self.read_leases()?;
        let now = now_ms();
        for lease in document.leases.values_mut() {
            if lease.state == LeaseState::Active && lease.expires_at_ms <= now {
                lease.state = LeaseState::Expired;
            }
        }
        Ok(document.leases.into_values().collect())
    }

    pub fn acquire_lease(
        &self,
        actor: &str,
        kind: LeaseKind,
        subject_id: &str,
        target_id: Option<&str>,
        workspace_id: &str,
        owner: &str,
        ttl_ms: u64,
    ) -> Result<Lease> {
        if ttl_ms < 1_000 {
            bail!("lease ttl must be at least 1000ms");
        }
        if let Some(target_id) = target_id {
            self.get_target(target_id)?;
        }
        let _lock = self.acquire_lock()?;
        let mut document = self.read_leases()?;
        let now = now_ms();
        for existing in document.leases.values_mut() {
            if existing.state == LeaseState::Active && existing.expires_at_ms <= now {
                existing.state = LeaseState::Expired;
            }
        }
        if kind == LeaseKind::Writer
            && document.leases.values().any(|lease| {
                lease.kind == LeaseKind::Writer
                    && lease.subject_id == subject_id
                    && lease.state == LeaseState::Active
            })
        {
            bail!("writer lease already exists for {subject_id}");
        }
        if matches!(kind, LeaseKind::AgentSlot | LeaseKind::BuildSlot) {
            let target_id = target_id.context("slot lease requires target_id")?;
            let target = self.get_target(target_id)?;
            let used = document
                .leases
                .values()
                .filter(|lease| {
                    lease.state == LeaseState::Active
                        && lease.target_id.as_deref() == Some(target_id)
                        && lease.kind == kind
                })
                .count() as u32;
            let capacity = if kind == LeaseKind::AgentSlot {
                target.agent_slots
            } else {
                target.build_slots
            };
            if used >= capacity {
                bail!("target {target_id} has no free {:?} capacity", kind);
            }
        }
        let lease = Lease {
            schema_version: COMPUTE_SCHEMA_VERSION,
            lease_id: Uuid::new_v4().to_string(),
            kind,
            subject_id: subject_id.to_string(),
            target_id: target_id.map(str::to_string),
            workspace_id: workspace_id.to_string(),
            owner: owner.to_string(),
            issued_at_ms: now,
            expires_at_ms: now.saturating_add(ttl_ms),
            heartbeat_at_ms: now,
            state: LeaseState::Active,
        };
        document
            .leases
            .insert(lease.lease_id.clone(), lease.clone());
        self.write_json(&self.leases_path(), &document)?;
        self.append_event(ComputeEvent::new(
            "lease.acquired",
            actor,
            Some(lease.lease_id.clone()),
            Some(workspace_id.to_string()),
            json!({"kind": kind, "subject_id": subject_id, "target_id": target_id}),
        ))?;
        Ok(lease)
    }

    pub fn heartbeat_lease(&self, actor: &str, id: &str, ttl_ms: u64) -> Result<Lease> {
        let _lock = self.acquire_lock()?;
        let mut document = self.read_leases()?;
        let lease = document
            .leases
            .get_mut(id)
            .with_context(|| format!("unknown lease: {id}"))?;
        if lease.state != LeaseState::Active {
            bail!("lease {id} is not active");
        }
        let now = now_ms();
        lease.heartbeat_at_ms = now;
        lease.expires_at_ms = now.saturating_add(ttl_ms);
        let result = lease.clone();
        self.write_json(&self.leases_path(), &document)?;
        self.append_event(ComputeEvent::new(
            "lease.heartbeat",
            actor,
            Some(id.to_string()),
            Some(result.workspace_id.clone()),
            Value::Null,
        ))?;
        Ok(result)
    }

    pub fn revoke_lease(&self, actor: &str, id: &str, reason: &str) -> Result<Lease> {
        let _lock = self.acquire_lock()?;
        self.revoke_lease_unlocked(actor, id, reason)
    }

    fn revoke_lease_unlocked(&self, actor: &str, id: &str, reason: &str) -> Result<Lease> {
        let mut document = self.read_leases()?;
        let lease = document
            .leases
            .get_mut(id)
            .with_context(|| format!("unknown lease: {id}"))?;
        lease.state = LeaseState::Revoked;
        let result = lease.clone();
        self.write_json(&self.leases_path(), &document)?;
        self.append_event(ComputeEvent::new(
            "lease.revoked",
            actor,
            Some(id.to_string()),
            Some(result.workspace_id.clone()),
            json!({"reason": reason}),
        ))?;
        Ok(result)
    }

    pub fn save_job(&self, actor: &str, job: &ExecutionJob) -> Result<()> {
        validate_id("job id", &job.job_id)?;
        let path = self.job_path(&job.job_id).join("state.json");
        self.write_json(&path, job)?;
        self.append_event(ComputeEvent::new(
            "job.state",
            actor,
            Some(job.job_id.clone()),
            Some(job.request.workspace_id.clone()),
            json!({"state": job.state, "target_id": job.target_id, "attempt": job.attempt}),
        ))
    }

    pub fn get_job(&self, id: &str) -> Result<ExecutionJob> {
        self.read_json(&self.job_path(id).join("state.json"))
            .with_context(|| format!("unknown or invalid job: {id}"))
    }

    pub fn list_jobs(&self) -> Result<Vec<ExecutionJob>> {
        let mut jobs = Vec::new();
        for entry in fs::read_dir(self.root.join("jobs"))? {
            let path = entry?.path().join("state.json");
            if path.is_file() {
                jobs.push(self.read_json(&path)?);
            }
        }
        jobs.sort_by(|left: &ExecutionJob, right: &ExecutionJob| {
            right
                .request
                .request_id
                .cmp(&left.request.request_id)
                .then_with(|| left.job_id.cmp(&right.job_id))
        });
        Ok(jobs)
    }

    pub fn delete_job(&self, actor: &str, id: &str) -> Result<ExecutionJob> {
        let job = self.get_job(id)?;
        if !job.state.is_terminal() {
            bail!("job {id} is not terminal");
        }
        let path = self.job_path(id);
        fs::remove_dir_all(&path)?;
        self.append_event(ComputeEvent::new(
            "job.deleted",
            actor,
            Some(id.to_string()),
            Some(job.request.workspace_id.clone()),
            Value::Null,
        ))?;
        Ok(job)
    }

    pub fn job_path(&self, id: &str) -> PathBuf {
        self.root.join("jobs").join(id)
    }

    pub fn save_snapshot(&self, actor: &str, manifest: &SnapshotManifest) -> Result<()> {
        validate_id("snapshot id", &manifest.snapshot_id)?;
        self.write_json(
            &self
                .root
                .join("snapshots")
                .join(&manifest.snapshot_id)
                .join("manifest.json"),
            manifest,
        )?;
        self.append_event(ComputeEvent::new(
            "snapshot.created",
            actor,
            Some(manifest.snapshot_id.clone()),
            None,
            json!({"digest": manifest.overall_digest}),
        ))
    }

    pub fn snapshot_path(&self, id: &str) -> PathBuf {
        self.root.join("snapshots").join(id)
    }

    pub fn get_snapshot(&self, id: &str) -> Result<SnapshotManifest> {
        self.read_json(&self.root.join("snapshots").join(id).join("manifest.json"))
            .with_context(|| format!("unknown or invalid snapshot: {id}"))
    }

    pub fn list_snapshots(&self) -> Result<Vec<SnapshotManifest>> {
        let mut manifests = Vec::new();
        for entry in fs::read_dir(self.root.join("snapshots"))? {
            let path = entry?.path().join("manifest.json");
            if path.is_file() {
                manifests.push(self.read_json(&path)?);
            }
        }
        manifests
            .sort_by_key(|manifest: &SnapshotManifest| std::cmp::Reverse(manifest.created_at_ms));
        Ok(manifests)
    }

    pub fn delete_snapshot(&self, actor: &str, id: &str) -> Result<SnapshotManifest> {
        let manifest = self.get_snapshot(id)?;
        if self
            .list_jobs()?
            .iter()
            .any(|job| job.request.snapshot_id.as_deref() == Some(id))
        {
            bail!("snapshot {id} is referenced by a job");
        }
        fs::remove_dir_all(self.root.join("snapshots").join(id))?;
        self.append_event(ComputeEvent::new(
            "snapshot.deleted",
            actor,
            Some(id.to_string()),
            None,
            Value::Null,
        ))?;
        Ok(manifest)
    }

    pub fn save_transfer(&self, actor: &str, transfer: &FileTransfer) -> Result<()> {
        validate_id("transfer id", &transfer.transfer_id)?;
        self.write_json(
            &self
                .root
                .join("transfers")
                .join(format!("{}.json", transfer.transfer_id)),
            transfer,
        )?;
        self.append_event(ComputeEvent::new(
            "transfer.updated",
            actor,
            Some(transfer.transfer_id.clone()),
            transfer.workspace_id.clone(),
            json!({
                "target_id": transfer.target_id,
                "surface_id": transfer.surface_id,
                "state": transfer.state,
                "size_bytes": transfer.size_bytes,
                "bytes_transferred": transfer.bytes_transferred,
            }),
        ))
    }

    pub fn get_transfer(&self, id: &str) -> Result<FileTransfer> {
        validate_id("transfer id", id)?;
        self.read_json(&self.root.join("transfers").join(format!("{id}.json")))
            .with_context(|| format!("unknown or invalid transfer: {id}"))
    }

    pub fn list_transfers(&self) -> Result<Vec<FileTransfer>> {
        let mut transfers: Vec<FileTransfer> = Vec::new();
        for entry in fs::read_dir(self.root.join("transfers"))? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                transfers.push(self.read_json(&path)?);
            }
        }
        transfers.sort_by_key(|transfer| std::cmp::Reverse(transfer.created_at_ms));
        Ok(transfers)
    }

    pub fn request_transfer_cancel(&self, actor: &str, id: &str) -> Result<FileTransfer> {
        let mut transfer = self.get_transfer(id)?;
        if matches!(
            transfer.state,
            TransferState::Succeeded | TransferState::Failed | TransferState::Cancelled
        ) {
            return Ok(transfer);
        }
        transfer.state = TransferState::Cancelling;
        transfer.updated_at_ms = now_ms();
        self.write_json(
            &self.root.join("transfers").join(format!("{id}.json")),
            &transfer,
        )?;
        let marker = self.root.join("transfers").join(format!("{id}.cancel"));
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(marker)?;
        file.write_all(b"cancel\n")?;
        file.flush()?;
        file.sync_all()?;
        self.append_event(ComputeEvent::new(
            "transfer.cancel_requested",
            actor,
            Some(id.to_string()),
            transfer.workspace_id.clone(),
            json!({
                "target_id": transfer.target_id,
                "surface_id": transfer.surface_id,
            }),
        ))?;
        Ok(transfer)
    }

    pub fn transfer_cancel_requested(&self, id: &str) -> Result<bool> {
        validate_id("transfer id", id)?;
        Ok(self
            .root
            .join("transfers")
            .join(format!("{id}.cancel"))
            .is_file())
    }

    pub fn clear_transfer_cancel(&self, id: &str) -> Result<()> {
        validate_id("transfer id", id)?;
        let marker = self.root.join("transfers").join(format!("{id}.cancel"));
        if marker.exists() {
            fs::remove_file(marker)?;
        }
        Ok(())
    }

    pub fn remove_transfer(&self, actor: &str, id: &str) -> Result<FileTransfer> {
        let transfer = self.get_transfer(id)?;
        if !matches!(
            transfer.state,
            TransferState::Succeeded | TransferState::Failed | TransferState::Cancelled
        ) {
            bail!("transfer {id} is still active");
        }
        fs::remove_file(self.root.join("transfers").join(format!("{id}.json")))?;
        self.clear_transfer_cancel(id)?;
        self.append_event(ComputeEvent::new(
            "transfer.deleted",
            actor,
            Some(id.to_string()),
            transfer.workspace_id.clone(),
            Value::Null,
        ))?;
        Ok(transfer)
    }

    pub fn save_proxy(&self, actor: &str, proxy: &RemoteProxySession) -> Result<()> {
        validate_id("proxy id", &proxy.proxy_id)?;
        validate_id("target id", &proxy.target_id)?;
        validate_id("workspace id", &proxy.workspace_id)?;
        if let Some(surface_id) = proxy.surface_id.as_deref() {
            validate_id("surface id", surface_id)?;
        }
        if proxy.local_address != "127.0.0.1" || proxy.local_port == 0 {
            bail!("remote proxy must bind a non-zero port on 127.0.0.1");
        }
        self.get_target(&proxy.target_id)?;
        if let Some(environment_id) = proxy.environment_id.as_deref() {
            let environment = self.get_environment(environment_id)?;
            if environment.target_id != proxy.target_id {
                bail!("remote proxy environment does not match its target");
            }
        }
        if let Some(endpoint_id) = proxy.endpoint_id.as_deref() {
            let endpoint = self.get_endpoint(endpoint_id)?;
            if proxy.environment_id.as_deref() != Some(endpoint.environment_id.as_str()) {
                bail!("remote proxy endpoint does not match its environment");
            }
        }
        let _lock = self.acquire_lock()?;
        let path = self
            .root
            .join("proxies")
            .join(format!("{}.json", proxy.proxy_id));
        let created = !path.exists();
        self.write_json(&path, proxy)?;
        self.append_event(ComputeEvent::new(
            if created {
                "proxy.created"
            } else {
                "proxy.updated"
            },
            actor,
            Some(proxy.proxy_id.clone()),
            Some(proxy.workspace_id.clone()),
            json!({
                "target_id": proxy.target_id,
                "surface_id": proxy.surface_id,
                "state": proxy.state,
                "local_address": proxy.local_address,
                "local_port": proxy.local_port,
            }),
        ))
    }

    pub fn get_proxy(&self, id: &str) -> Result<RemoteProxySession> {
        validate_id("proxy id", id)?;
        self.read_json(&self.root.join("proxies").join(format!("{id}.json")))
            .with_context(|| format!("unknown or invalid remote proxy: {id}"))
    }

    pub fn list_proxies(&self) -> Result<Vec<RemoteProxySession>> {
        let mut proxies = Vec::new();
        for entry in fs::read_dir(self.root.join("proxies"))? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                proxies.push(self.read_json(&path)?);
            }
        }
        proxies.sort_by_key(|proxy: &RemoteProxySession| std::cmp::Reverse(proxy.created_at_ms));
        Ok(proxies)
    }

    pub fn request_proxy_stop(&self, actor: &str, id: &str) -> Result<RemoteProxySession> {
        let mut proxy = self.get_proxy(id)?;
        if proxy.state.is_terminal() {
            return Ok(proxy);
        }
        proxy.state = RemoteProxyState::Stopping;
        proxy.updated_at_ms = now_ms();
        self.save_proxy(actor, &proxy)?;
        let marker = self.root.join("proxies").join(format!("{id}.stop"));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(marker)?;
        file.write_all(b"stop\n")?;
        file.flush()?;
        file.sync_all()?;
        Ok(proxy)
    }

    pub fn proxy_stop_requested(&self, id: &str) -> Result<bool> {
        validate_id("proxy id", id)?;
        Ok(self
            .root
            .join("proxies")
            .join(format!("{id}.stop"))
            .is_file())
    }

    pub fn clear_proxy_stop(&self, id: &str) -> Result<()> {
        validate_id("proxy id", id)?;
        let marker = self.root.join("proxies").join(format!("{id}.stop"));
        if marker.exists() {
            fs::remove_file(marker)?;
        }
        Ok(())
    }

    pub fn remove_proxy(&self, actor: &str, id: &str) -> Result<RemoteProxySession> {
        let proxy = self.get_proxy(id)?;
        if !proxy.state.is_terminal() {
            bail!("remote proxy {id} is still active");
        }
        fs::remove_file(self.root.join("proxies").join(format!("{id}.json")))?;
        self.clear_proxy_stop(id)?;
        self.append_event(ComputeEvent::new(
            "proxy.deleted",
            actor,
            Some(id.to_string()),
            Some(proxy.workspace_id.clone()),
            Value::Null,
        ))?;
        Ok(proxy)
    }

    pub fn browser_profile_path(&self, profile_id: &str) -> Result<PathBuf> {
        validate_id("browser profile id", profile_id)?;
        Ok(self.root.join("browser-profiles").join(profile_id))
    }

    pub fn save_browser(
        &self,
        actor: &str,
        mut browser: BrowserSurfaceSession,
    ) -> Result<BrowserSurfaceSession> {
        validate_id("browser surface id", &browser.browser_surface_id)?;
        validate_id("remote workspace id", &browser.remote_workspace_id)?;
        validate_id("workspace id", &browser.workspace_id)?;
        validate_id("surface id", &browser.surface_id)?;
        validate_id("target id", &browser.target_id)?;
        validate_id("proxy id", &browser.proxy_id)?;
        validate_id("browser profile id", &browser.profile_id)?;
        let workspace = self.get_remote_workspace(&browser.remote_workspace_id)?;
        if workspace.workspace_id != browser.workspace_id
            || workspace.target_id != browser.target_id
            || workspace.environment_id != browser.environment_id
        {
            bail!(
                "browser surface does not match its remote workspace target, environment or identity"
            );
        }
        let proxy = self.get_proxy(&browser.proxy_id)?;
        if proxy.target_id != browser.target_id
            || proxy.workspace_id != browser.workspace_id
            || proxy.surface_id.as_deref() != Some(browser.surface_id.as_str())
            || proxy.environment_id != browser.environment_id
        {
            bail!(
                "browser surface proxy is not scoped to the same environment, workspace and surface"
            );
        }
        let expected_profile = self.browser_profile_path(&browser.profile_id)?;
        if PathBuf::from(&browser.user_data_folder) != expected_profile {
            bail!("browser user data folder must be owned by the compute store");
        }
        if browser.navigation_history.is_empty() {
            browser.navigation_history.push(browser.current_url.clone());
            browser.history_index = 0;
        }
        if browser.history_index >= browser.navigation_history.len() {
            bail!("browser history index is outside the navigation history");
        }
        browser.schema_version = COMPUTE_SCHEMA_VERSION;
        let now = now_ms();
        if browser.created_at_ms == 0 {
            browser.created_at_ms = now;
        }
        browser.updated_at_ms = now;

        let _lock = self.acquire_lock()?;
        if let Some(existing) = self.list_browsers()?.into_iter().find(|candidate| {
            candidate.browser_surface_id != browser.browser_surface_id
                && terminal_identity_eq(&candidate.workspace_id, &browser.workspace_id)
                && terminal_identity_eq(&candidate.surface_id, &browser.surface_id)
                && !candidate.state.is_terminal()
        }) {
            bail!(
                "native surface is already owned by browser surface {}",
                existing.browser_surface_id
            );
        }
        fs::create_dir_all(&expected_profile)?;
        let path = self
            .root
            .join("browsers")
            .join(format!("{}.json", browser.browser_surface_id));
        let created = !path.exists();
        self.write_json(&path, &browser)?;
        self.append_event(ComputeEvent::new(
            if created {
                "browser.created"
            } else {
                "browser.updated"
            },
            actor,
            Some(browser.browser_surface_id.clone()),
            Some(browser.workspace_id.clone()),
            json!({
                "remote_workspace_id": browser.remote_workspace_id,
                "surface_id": browser.surface_id,
                "target_id": browser.target_id,
                "proxy_id": browser.proxy_id,
                "state": browser.state,
                "url": browser.current_url,
            }),
        ))?;
        Ok(browser)
    }

    pub fn get_browser(&self, id: &str) -> Result<BrowserSurfaceSession> {
        validate_id("browser surface id", id)?;
        self.read_json(&self.root.join("browsers").join(format!("{id}.json")))
            .with_context(|| format!("unknown or invalid browser surface: {id}"))
    }

    pub fn list_browsers(&self) -> Result<Vec<BrowserSurfaceSession>> {
        let mut browsers = Vec::new();
        for entry in fs::read_dir(self.root.join("browsers"))? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                browsers.push(self.read_json(&path)?);
            }
        }
        browsers.sort_by_key(|browser: &BrowserSurfaceSession| {
            std::cmp::Reverse(browser.created_at_ms)
        });
        Ok(browsers)
    }

    pub fn remove_browser(
        &self,
        actor: &str,
        id: &str,
        delete_profile: bool,
    ) -> Result<BrowserSurfaceSession> {
        let browser = self.get_browser(id)?;
        if !browser.state.is_terminal() {
            bail!("browser surface {id} is still active");
        }
        let _lock = self.acquire_lock()?;
        fs::remove_file(self.root.join("browsers").join(format!("{id}.json")))?;
        if delete_profile {
            let profile = self.browser_profile_path(&browser.profile_id)?;
            if profile.exists() {
                fs::remove_dir_all(&profile)?;
            }
        }
        self.append_event(ComputeEvent::new(
            "browser.deleted",
            actor,
            Some(id.to_string()),
            Some(browser.workspace_id.clone()),
            json!({"profile_deleted": delete_profile}),
        ))?;
        Ok(browser)
    }

    pub fn save_file_root_policy(
        &self,
        actor: &str,
        mut policy: RemoteFileRootPolicy,
    ) -> Result<RemoteFileRootPolicy> {
        validate_id("remote file root id", &policy.root_id)?;
        validate_id("workspace id", &policy.workspace_id)?;
        validate_id("target id", &policy.target_id)?;
        if let Some(binding_id) = policy.binding_id.as_deref() {
            validate_id("binding id", binding_id)?;
            let binding = self.get_binding(binding_id)?;
            if !terminal_identity_eq(&binding.workspace_id, &policy.workspace_id)
                || binding.home_target_id.as_deref() != Some(policy.target_id.as_str())
            {
                bail!("remote file root binding does not match its workspace and target");
            }
        }
        if policy.label.trim().is_empty() || policy.label.len() > 120 {
            bail!("remote file root label must contain 1-120 characters");
        }
        if policy.canonical_path.trim().is_empty()
            || policy.canonical_path.len() > 4096
            || policy.canonical_path.chars().any(char::is_control)
        {
            bail!("remote file root path is empty, too long, or contains control characters");
        }
        if !policy.readable && (policy.writable || policy.deletable) {
            bail!("remote file mutation capabilities require readable=true");
        }
        if policy.deletable && !policy.writable {
            bail!("remote file delete capability requires writable=true");
        }
        if matches!(
            policy.source,
            RemoteFileRootSource::ExplicitHome | RemoteFileRootSource::Admin
        ) && !policy.wide_scope_acknowledged
        {
            bail!("broad remote file roots require an explicit wide-scope acknowledgement");
        }

        let target = self.get_target(&policy.target_id)?;
        if target.trust_tier != policy.trust_tier {
            bail!("remote file root trust tier must match its compute target");
        }
        if !policy.active {
            policy.revoked_at_ms.get_or_insert_with(now_ms);
        } else {
            policy.revoked_at_ms = None;
        }
        policy.schema_version = COMPUTE_SCHEMA_VERSION;
        let now = now_ms();
        if policy.created_at_ms == 0 {
            policy.created_at_ms = now;
        }
        policy.updated_at_ms = now;

        let _lock = self.acquire_lock()?;
        let path = self
            .root
            .join("file-root-policies")
            .join(format!("{}.json", policy.root_id));
        let created = !path.exists();
        self.write_json(&path, &policy)?;
        self.append_event(ComputeEvent::new(
            if created {
                "file_root.created"
            } else if policy.active {
                "file_root.updated"
            } else {
                "file_root.revoked"
            },
            actor,
            Some(policy.root_id.clone()),
            Some(policy.workspace_id.clone()),
            json!({
                "target_id": policy.target_id,
                "binding_id": policy.binding_id,
                "source": policy.source,
                "readable": policy.readable,
                "writable": policy.writable,
                "deletable": policy.deletable,
                "active": policy.active,
            }),
        ))?;
        Ok(policy)
    }

    pub fn get_file_root_policy(&self, id: &str) -> Result<RemoteFileRootPolicy> {
        validate_id("remote file root id", id)?;
        self.read_json(
            &self
                .root
                .join("file-root-policies")
                .join(format!("{id}.json")),
        )
        .with_context(|| format!("unknown or invalid remote file root policy: {id}"))
    }

    pub fn list_file_root_policies(
        &self,
        workspace_id: Option<&str>,
        target_id: Option<&str>,
        binding_id: Option<&str>,
        include_revoked: bool,
    ) -> Result<Vec<RemoteFileRootPolicy>> {
        let mut policies = Vec::new();
        for entry in fs::read_dir(self.root.join("file-root-policies"))? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let policy: RemoteFileRootPolicy = self.read_json(&path)?;
            if !include_revoked && !policy.active {
                continue;
            }
            if workspace_id.is_some_and(|value| !terminal_identity_eq(&policy.workspace_id, value))
                || target_id.is_some_and(|value| policy.target_id != value)
                || binding_id.is_some_and(|value| policy.binding_id.as_deref() != Some(value))
            {
                continue;
            }
            policies.push(policy);
        }
        policies.sort_by(|left, right| {
            left.label
                .to_lowercase()
                .cmp(&right.label.to_lowercase())
                .then_with(|| left.root_id.cmp(&right.root_id))
        });
        Ok(policies)
    }

    pub fn revoke_file_root_policy(&self, actor: &str, id: &str) -> Result<RemoteFileRootPolicy> {
        let mut policy = self.get_file_root_policy(id)?;
        policy.active = false;
        policy.revoked_at_ms = Some(now_ms());
        self.save_file_root_policy(actor, policy)
    }

    pub fn save_restore_snapshot(
        &self,
        actor: &str,
        mut snapshot: RuntimeRestoreSnapshot,
    ) -> Result<RuntimeRestoreSnapshot> {
        validate_id("restore id", &snapshot.restore_id)?;
        validate_id("window id", &snapshot.window_id)?;
        validate_id("workspace id", &snapshot.workspace_id)?;
        if let Some(surface_id) = snapshot.focused_surface_id.as_deref() {
            validate_id("focused surface id", surface_id)?;
        }
        for id in &snapshot.remote_workspace_ids {
            validate_id("remote workspace id", id)?;
        }
        for id in &snapshot.binding_ids {
            validate_id("binding id", id)?;
        }
        for id in &snapshot.browser_surface_ids {
            validate_id("browser surface id", id)?;
        }
        for reference in &snapshot.runtime_references {
            validate_id("restore environment id", &reference.environment_id)?;
            validate_id("restore target id", &reference.target_id)?;
            if let Some(binding_id) = reference.binding_id.as_deref() {
                validate_id("restore binding id", binding_id)?;
            }
            let environment = self.get_environment(&reference.environment_id)?;
            if environment.target_id != reference.target_id {
                bail!("restore environment and target identities do not match");
            }
            if matches!(
                reference.preferred_endpoint_kind,
                AccessEndpointKind::Tailscale
                    | AccessEndpointKind::AuthenticatedWss
                    | AccessEndpointKind::Relay
            ) {
                bail!("restore cannot prefer an endpoint kind disabled in this release");
            }
        }
        snapshot.schema_version = COMPUTE_SCHEMA_VERSION;
        if snapshot.captured_at_ms == 0 {
            snapshot.captured_at_ms = now_ms();
        }
        snapshot.remote_workspace_ids.sort();
        snapshot.remote_workspace_ids.dedup();
        snapshot.binding_ids.sort();
        snapshot.binding_ids.dedup();
        snapshot.browser_surface_ids.sort();
        snapshot.browser_surface_ids.dedup();
        snapshot.runtime_references.sort_by(|left, right| {
            left.environment_id
                .cmp(&right.environment_id)
                .then_with(|| left.binding_id.cmp(&right.binding_id))
        });
        snapshot.runtime_references.dedup();

        let _lock = self.acquire_lock()?;
        let path = self
            .root
            .join("restore")
            .join(format!("{}.json", snapshot.restore_id));
        self.write_json(&path, &snapshot)?;
        self.append_event(ComputeEvent::new(
            "restore.captured",
            actor,
            Some(snapshot.restore_id.clone()),
            Some(snapshot.workspace_id.clone()),
            json!({
                "window_id": snapshot.window_id,
                "remote_workspaces": snapshot.remote_workspace_ids.len(),
                "bindings": snapshot.binding_ids.len(),
                "browsers": snapshot.browser_surface_ids.len(),
                "runtime_references": snapshot.runtime_references.len(),
            }),
        ))?;
        Ok(snapshot)
    }

    pub fn get_restore_snapshot(&self, id: &str) -> Result<RuntimeRestoreSnapshot> {
        validate_id("restore id", id)?;
        self.read_json(&self.root.join("restore").join(format!("{id}.json")))
            .with_context(|| format!("unknown or invalid restore snapshot: {id}"))
    }

    pub fn list_restore_snapshots(&self) -> Result<Vec<RuntimeRestoreSnapshot>> {
        let mut snapshots = Vec::new();
        for entry in fs::read_dir(self.root.join("restore"))? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                snapshots.push(self.read_json(&path)?);
            }
        }
        snapshots.sort_by_key(|snapshot: &RuntimeRestoreSnapshot| {
            std::cmp::Reverse(snapshot.captured_at_ms)
        });
        Ok(snapshots)
    }

    pub fn latest_restore_snapshot(
        &self,
        window_id: &str,
        workspace_id: &str,
    ) -> Result<Option<RuntimeRestoreSnapshot>> {
        Ok(self.list_restore_snapshots()?.into_iter().find(|snapshot| {
            terminal_identity_eq(&snapshot.window_id, window_id)
                && terminal_identity_eq(&snapshot.workspace_id, workspace_id)
        }))
    }

    pub fn remove_restore_snapshot(&self, actor: &str, id: &str) -> Result<RuntimeRestoreSnapshot> {
        let snapshot = self.get_restore_snapshot(id)?;
        let _lock = self.acquire_lock()?;
        fs::remove_file(self.root.join("restore").join(format!("{id}.json")))?;
        self.append_event(ComputeEvent::new(
            "restore.deleted",
            actor,
            Some(id.to_string()),
            Some(snapshot.workspace_id.clone()),
            Value::Null,
        ))?;
        Ok(snapshot)
    }

    pub fn events(&self) -> Result<Vec<ComputeEvent>> {
        let path = self.events_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = fs::File::open(&path)?;
        BufReader::new(file)
            .lines()
            .enumerate()
            .filter_map(|(index, line)| match line {
                Ok(line) if line.trim().is_empty() => None,
                value => Some((index, value)),
            })
            .map(|(index, line)| {
                serde_json::from_str(&line?).with_context(|| {
                    format!("invalid compute event at {}:{}", path.display(), index + 1)
                })
            })
            .collect()
    }

    pub fn append_event(&self, event: ComputeEvent) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_path())?;
        serde_json::to_writer(&mut file, &event)?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }

    fn targets_path(&self) -> PathBuf {
        self.root.join("targets.json")
    }

    fn bindings_path(&self) -> PathBuf {
        self.root.join("bindings.json")
    }

    fn leases_path(&self) -> PathBuf {
        self.root.join("leases.json")
    }

    fn policies_path(&self) -> PathBuf {
        self.root.join("policies.json")
    }

    fn remote_workspaces_path(&self) -> PathBuf {
        self.root.join("remote-workspaces.json")
    }

    fn remote_surfaces_path(&self) -> PathBuf {
        self.root.join("remote-surfaces.json")
    }

    fn events_path(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }

    fn read_targets(&self) -> Result<TargetDocument> {
        self.read_document(&self.targets_path())
    }

    fn read_bindings(&self) -> Result<BindingDocument> {
        self.read_document(&self.bindings_path())
    }

    fn read_leases(&self) -> Result<LeaseDocument> {
        self.read_document(&self.leases_path())
    }

    fn read_policies(&self) -> Result<PolicyDocument> {
        self.read_document(&self.policies_path())
    }

    fn read_remote_workspaces(&self) -> Result<RemoteWorkspaceDocument> {
        self.read_document(&self.remote_workspaces_path())
    }

    fn read_remote_surfaces(&self) -> Result<RemoteSurfaceDocument> {
        self.read_document(&self.remote_surfaces_path())
    }

    fn read_document<T>(&self, path: &Path) -> Result<T>
    where
        T: DeserializeOwned + Default,
    {
        if !path.exists() {
            return Ok(T::default());
        }
        self.read_json(path)
    }

    fn read_json<T: DeserializeOwned>(&self, path: &Path) -> Result<T> {
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read compute state {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("compute state is corrupt: {}", path.display()))
    }

    fn read_json_directory<T: DeserializeOwned>(&self, name: &str) -> Result<Vec<T>> {
        let mut values = Vec::new();
        for entry in fs::read_dir(self.root.join(name))? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                values.push(self.read_json(&path)?);
            }
        }
        Ok(values)
    }

    fn write_json<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value)?;
        atomic_write(path, &bytes)
    }

    fn acquire_lock(&self) -> Result<ComputeLock> {
        fs::create_dir_all(&self.root)?;
        let path = self.root.join("state.lock");
        let started = std::time::Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "pid={} timestamp_ms={}", std::process::id(), now_ms())?;
                    file.flush()?;
                    return Ok(ComputeLock { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if started.elapsed() >= LOCK_WAIT {
                        bail!(
                            "timed out waiting for compute state lock {}",
                            path.display()
                        );
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl ComputeEvent {
    pub fn new(
        kind: &str,
        actor: &str,
        subject_id: Option<String>,
        workspace_id: Option<String>,
        payload: Value,
    ) -> Self {
        Self {
            schema_version: COMPUTE_SCHEMA_VERSION,
            id: Uuid::new_v4().to_string(),
            timestamp_ms: now_ms(),
            kind: kind.to_string(),
            actor: actor.to_string(),
            subject_id,
            workspace_id,
            payload,
        }
    }
}

struct ComputeLock {
    path: PathBuf,
}

impl Drop for ComputeLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn runtime_heartbeat_at_ms(binding: &SurfaceBinding) -> Option<u64> {
    binding
        .metadata
        .get("runtime_heartbeat_at_ms")
        .and_then(Value::as_u64)
}

fn ensure_object(value: &mut Value) {
    if !value.is_object() {
        *value = json!({});
    }
}

pub fn validate_id(label: &str, value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
    {
        bail!("{label} must use 1-128 ASCII letters, digits, '.', ':', '-' or '_'");
    }
    Ok(())
}

fn validate_target(target: &ComputeTarget) -> Result<()> {
    if target.display_name.trim().is_empty() {
        bail!("target display_name must not be empty");
    }
    if target.agent_slots == 0 && target.build_slots == 0 {
        bail!("target must expose at least one agent or build slot");
    }
    match target.provider {
        ProviderKind::Ssh
            if target
                .endpoint
                .ssh_alias
                .as_deref()
                .unwrap_or("")
                .is_empty() =>
        {
            bail!("SSH target requires endpoint.ssh_alias")
        }
        ProviderKind::Ssh => {
            super::ssh::validate_alias(
                target.endpoint.ssh_alias.as_deref().expect("guarded above"),
            )?;
        }
        ProviderKind::Wsl
            if target
                .endpoint
                .wsl_distro
                .as_deref()
                .unwrap_or("")
                .is_empty() =>
        {
            bail!("WSL target requires endpoint.wsl_distro")
        }
        ProviderKind::Azure
            if target
                .endpoint
                .azure_resource_id
                .as_deref()
                .unwrap_or("")
                .is_empty() =>
        {
            bail!("Azure target requires endpoint.azure_resource_id")
        }
        _ => {}
    }
    Ok(())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn lock_is_stale(path: &Path) -> bool {
    #[cfg(windows)]
    if lock_owner_is_gone(path) {
        return true;
    }

    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > STALE_LOCK_AGE)
}

#[cfg(windows)]
fn lock_owner_is_gone(path: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    let Some(pid) = contents
        .split_ascii_whitespace()
        .find_map(|field| field.strip_prefix("pid="))
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return false;
    };
    if pid == 0 || pid == std::process::id() {
        return false;
    }

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // The lock owner is always another WTA process running as the same user,
    // so PROCESS_QUERY_LIMITED_INFORMATION is sufficient. A null handle means
    // the recorded PID no longer exists; PID reuse remains conservative (the
    // age-based fallback will eventually recover that rare case).
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            true
        } else {
            CloseHandle(handle);
            false
        }
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("compute state path has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        Uuid::new_v4()
    ));
    {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
    }
    atomic_replace(&temp, path)
        .with_context(|| format!("failed to commit compute state {}", path.display()))
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source_path = source;
    let source_wide = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        let error = std::io::Error::last_os_error();
        let _ = fs::remove_file(source_path);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(label: &str) -> ComputeStore {
        let root = std::env::temp_dir().join(format!("wta-compute-{label}-{}", Uuid::new_v4()));
        ComputeStore::at(root).unwrap()
    }

    fn target(id: &str) -> ComputeTarget {
        ComputeTarget {
            schema_version: COMPUTE_SCHEMA_VERSION,
            id: id.to_string(),
            display_name: id.to_string(),
            provider: ProviderKind::Local,
            endpoint: TargetEndpoint::default(),
            os: "windows".to_string(),
            arch: "x86_64".to_string(),
            capabilities: vec!["codex".to_string()],
            toolchains: BTreeMap::new(),
            trust_tier: TrustTier::Personal,
            project_allowlist: Vec::new(),
            agent_slots: 2,
            build_slots: 2,
            memory_bytes: 16,
            cost_policy: Value::Null,
            power_policy: Value::Null,
            health: TargetHealth::Healthy,
            last_probe_at_ms: None,
            disabled: false,
            metadata: Value::Null,
        }
    }

    fn managed_binding(id: &str) -> SurfaceBinding {
        SurfaceBinding {
            schema_version: COMPUTE_SCHEMA_VERSION,
            binding_id: id.to_string(),
            window_id: format!("window-{id}"),
            workspace_id: "workspace".into(),
            pane_id: format!("pane-{id}"),
            surface_id: format!("surface-{id}"),
            focus_generation: 1,
            kind: BindingKind::ManagedAgent,
            agent_id: Some("codex".into()),
            adapter_kind: Some("acp".into()),
            acp_session_id: Some(format!("acp-{id}")),
            remote_session_id: Some(format!("runtime-{id}")),
            environment_id: None,
            preferred_endpoint_kind: None,
            home_target_id: Some("local".into()),
            worktree_id: Some(format!("worktree-{id}")),
            writer_lease_id: None,
            state: BindingState::Creating,
            created_at_ms: 1,
            updated_at_ms: 1,
            metadata: json!({"user_data": "preserve-me"}),
        }
    }

    fn age_binding(store: &ComputeStore, id: &str, timestamp_ms: u64) {
        let mut document = store.read_bindings().unwrap();
        let binding = document.bindings.get_mut(id).unwrap();
        binding.created_at_ms = timestamp_ms;
        binding.updated_at_ms = timestamp_ms;
        store.write_json(&store.bindings_path(), &document).unwrap();
    }

    #[test]
    fn target_round_trip_and_corruption_fail_closed() {
        let store = temp_store("roundtrip");
        store.upsert_target("test", target("local")).unwrap();
        assert_eq!(store.get_target("local").unwrap().id, "local");
        fs::write(store.targets_path(), b"{broken").unwrap();
        let error = store.list_targets().unwrap_err().to_string();
        assert!(error.contains("corrupt"), "{error}");
        assert!(store.targets_path().exists());
    }

    #[test]
    fn remote_file_roots_are_workspace_scoped_and_revocable() {
        let store = temp_store("file-root-policy");
        store.upsert_target("test", target("local")).unwrap();
        let policy = RemoteFileRootPolicy {
            schema_version: COMPUTE_SCHEMA_VERSION,
            root_id: "root-project".into(),
            workspace_id: "workspace-a".into(),
            target_id: "local".into(),
            binding_id: None,
            label: "Project".into(),
            canonical_path: r"C:\projects\a".into(),
            readable: true,
            writable: false,
            deletable: false,
            source: RemoteFileRootSource::Project,
            trust_tier: TrustTier::Personal,
            wide_scope_acknowledged: false,
            active: true,
            created_at_ms: 0,
            updated_at_ms: 0,
            revoked_at_ms: None,
        };
        store.save_file_root_policy("test", policy).unwrap();
        assert_eq!(
            store
                .list_file_root_policies(Some("workspace-a"), Some("local"), None, false,)
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .list_file_root_policies(Some("workspace-b"), Some("local"), None, false,)
            .unwrap()
            .is_empty());
        store
            .revoke_file_root_policy("test.revoke", "root-project")
            .unwrap();
        assert!(store
            .list_file_root_policies(Some("workspace-a"), Some("local"), None, false,)
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .list_file_root_policies(Some("workspace-a"), Some("local"), None, true,)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn broad_remote_file_root_requires_explicit_acknowledgement() {
        let store = temp_store("file-root-wide");
        store.upsert_target("test", target("local")).unwrap();
        let policy = RemoteFileRootPolicy {
            schema_version: COMPUTE_SCHEMA_VERSION,
            root_id: "root-home".into(),
            workspace_id: "workspace".into(),
            target_id: "local".into(),
            binding_id: None,
            label: "Home (broad access)".into(),
            canonical_path: r"C:\Users\owner".into(),
            readable: true,
            writable: false,
            deletable: false,
            source: RemoteFileRootSource::ExplicitHome,
            trust_tier: TrustTier::Personal,
            wide_scope_acknowledged: false,
            active: true,
            created_at_ms: 0,
            updated_at_ms: 0,
            revoked_at_ms: None,
        };
        assert!(store.save_file_root_policy("test", policy).is_err());
    }

    #[test]
    fn managed_binding_requires_target_and_unique_surface() {
        let store = temp_store("binding");
        store.upsert_target("test", target("local")).unwrap();
        let now = now_ms();
        let binding = SurfaceBinding {
            schema_version: COMPUTE_SCHEMA_VERSION,
            binding_id: "binding-a".into(),
            window_id: "window".into(),
            workspace_id: "workspace".into(),
            pane_id: "pane".into(),
            surface_id: "surface".into(),
            focus_generation: 1,
            kind: BindingKind::ManagedAgent,
            agent_id: Some("codex".into()),
            adapter_kind: Some("acp".into()),
            acp_session_id: None,
            remote_session_id: None,
            environment_id: None,
            preferred_endpoint_kind: None,
            home_target_id: Some("local".into()),
            worktree_id: Some("worktree".into()),
            writer_lease_id: None,
            state: BindingState::Creating,
            created_at_ms: now,
            updated_at_ms: now,
            metadata: Value::Null,
        };
        store.upsert_binding("test", binding.clone()).unwrap();
        let mut duplicate = binding;
        duplicate.binding_id = "binding-b".into();
        assert!(store.upsert_binding("test", duplicate).is_err());
    }

    #[test]
    fn managed_binding_rejects_missing_agent_or_adapter_identity() {
        let store = temp_store("binding-identity");
        store.upsert_target("test", target("local")).unwrap();
        let base = SurfaceBinding {
            schema_version: COMPUTE_SCHEMA_VERSION,
            binding_id: "binding-a".into(),
            window_id: "window".into(),
            workspace_id: "workspace".into(),
            pane_id: "pane".into(),
            surface_id: "surface".into(),
            focus_generation: 1,
            kind: BindingKind::ManagedAgent,
            agent_id: Some("codex".into()),
            adapter_kind: Some("acp".into()),
            acp_session_id: None,
            remote_session_id: Some("surface-a".into()),
            environment_id: None,
            preferred_endpoint_kind: None,
            home_target_id: Some("local".into()),
            worktree_id: Some("worktree".into()),
            writer_lease_id: None,
            state: BindingState::Creating,
            created_at_ms: 0,
            updated_at_ms: 0,
            metadata: Value::Null,
        };

        let mut missing_agent = base.clone();
        missing_agent.agent_id = None;
        assert!(store.upsert_binding("test", missing_agent).is_err());

        let mut missing_adapter = base;
        missing_adapter.adapter_kind = Some(" ".into());
        assert!(store.upsert_binding("test", missing_adapter).is_err());
    }

    #[test]
    fn removing_binding_revokes_all_owned_leases() {
        let store = temp_store("binding-remove-leases");
        store.upsert_target("test", target("local")).unwrap();

        let writer = store
            .acquire_lease(
                "test",
                LeaseKind::Writer,
                "binding-a",
                Some("local"),
                "workspace",
                "binding-a",
                60_000,
            )
            .unwrap();
        let slot = store
            .acquire_lease(
                "test",
                LeaseKind::AgentSlot,
                "binding-a",
                Some("local"),
                "workspace",
                "binding-a",
                60_000,
            )
            .unwrap();
        store
            .upsert_binding(
                "test",
                SurfaceBinding {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    binding_id: "binding-a".into(),
                    window_id: "window".into(),
                    workspace_id: "workspace".into(),
                    pane_id: "pane".into(),
                    surface_id: "surface".into(),
                    focus_generation: 1,
                    kind: BindingKind::ManagedAgent,
                    agent_id: Some("codex".into()),
                    adapter_kind: Some("acp".into()),
                    acp_session_id: None,
                    remote_session_id: Some("surface-a".into()),
                    environment_id: None,
                    preferred_endpoint_kind: None,
                    home_target_id: Some("local".into()),
                    worktree_id: None,
                    writer_lease_id: Some(writer.lease_id.clone()),
                    state: BindingState::Creating,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    metadata: Value::Null,
                },
            )
            .unwrap();

        store.remove_binding("test", "binding-a").unwrap();

        assert!(store.get_binding("binding-a").is_err());
        let leases = store.list_leases().unwrap();
        for expected in [writer.lease_id, slot.lease_id] {
            let lease = leases
                .iter()
                .find(|lease| lease.lease_id == expected)
                .expect("lease must remain as auditable history");
            assert_eq!(lease.state, LeaseState::Revoked);
        }
    }

    #[test]
    fn remove_surface_binding_normalizes_terminal_guids_and_is_idempotent() {
        let store = temp_store("binding-remove-surface-identity");
        store.upsert_target("test", target("local")).unwrap();
        let binding_id = "binding-guid";
        let writer = store
            .acquire_lease(
                "test",
                LeaseKind::Writer,
                binding_id,
                Some("local"),
                "{094b6bbb-6e5b-4aee-80ae-81c727a9292d}",
                binding_id,
                60_000,
            )
            .unwrap();
        store
            .upsert_binding(
                "test",
                SurfaceBinding {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    binding_id: binding_id.into(),
                    window_id: "1".into(),
                    workspace_id: "{094b6bbb-6e5b-4aee-80ae-81c727a9292d}".into(),
                    pane_id: "0".into(),
                    surface_id: "{c80296a2-e6b2-48b7-ae64-94299e7f4a31}".into(),
                    focus_generation: 0,
                    kind: BindingKind::ManagedAgent,
                    agent_id: Some("codex".into()),
                    adapter_kind: Some("acp".into()),
                    acp_session_id: None,
                    remote_session_id: Some("surface-runtime".into()),
                    environment_id: None,
                    preferred_endpoint_kind: None,
                    home_target_id: Some("local".into()),
                    worktree_id: None,
                    writer_lease_id: Some(writer.lease_id.clone()),
                    state: BindingState::Creating,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    metadata: Value::Null,
                },
            )
            .unwrap();

        let removed = store
            .remove_surface_binding(
                "terminal.surface_closed",
                "1",
                "094B6BBB-6E5B-4AEE-80AE-81C727A9292D",
                "C80296A2-E6B2-48B7-AE64-94299E7F4A31",
            )
            .unwrap()
            .expect("GUID-equivalent identity must resolve the binding");
        assert_eq!(removed.binding_id, binding_id);
        assert_eq!(
            store
                .list_leases()
                .unwrap()
                .into_iter()
                .find(|lease| lease.lease_id == writer.lease_id)
                .unwrap()
                .state,
            LeaseState::Revoked
        );
        assert!(store
            .remove_surface_binding(
                "terminal.surface_closed",
                "1",
                "{094b6bbb-6e5b-4aee-80ae-81c727a9292d}",
                "{c80296a2-e6b2-48b7-ae64-94299e7f4a31}",
            )
            .unwrap()
            .is_none());
    }

    #[test]
    fn writer_lease_is_exclusive_and_slot_capacity_is_enforced() {
        let store = temp_store("lease");
        let mut target = target("local");
        target.agent_slots = 1;
        store.upsert_target("test", target).unwrap();
        store
            .acquire_lease(
                "test",
                LeaseKind::Writer,
                "worktree-a",
                Some("local"),
                "workspace",
                "agent-a",
                60_000,
            )
            .unwrap();
        assert!(store
            .acquire_lease(
                "test",
                LeaseKind::Writer,
                "worktree-a",
                Some("local"),
                "workspace",
                "agent-b",
                60_000,
            )
            .is_err());
        store
            .acquire_lease(
                "test",
                LeaseKind::AgentSlot,
                "binding-a",
                Some("local"),
                "workspace",
                "agent-a",
                60_000,
            )
            .unwrap();
        assert!(store
            .acquire_lease(
                "test",
                LeaseKind::AgentSlot,
                "binding-b",
                Some("local"),
                "workspace",
                "agent-b",
                60_000,
            )
            .is_err());
    }

    #[test]
    fn reconcile_marks_only_unsupported_stale_creating_bindings_failed() {
        let store = temp_store("reconcile-stale");
        store.upsert_target("test", target("local")).unwrap();
        store
            .upsert_binding("test", managed_binding("orphan"))
            .unwrap();
        age_binding(&store, "orphan", 1_000);

        let report = store
            .reconcile_stale_managed_bindings_at("startup.reconcile", 10_000, 5_000)
            .unwrap();

        assert_eq!(report.examined, 1);
        assert_eq!(report.failed_binding_ids, vec!["orphan"]);
        let binding = store.get_binding("orphan").unwrap();
        assert_eq!(binding.state, BindingState::Failed);
        assert_eq!(binding.worktree_id.as_deref(), Some("worktree-orphan"));
        assert_eq!(binding.acp_session_id.as_deref(), Some("acp-orphan"));
        assert_eq!(
            binding.metadata.get("user_data").and_then(Value::as_str),
            Some("preserve-me")
        );
        assert_eq!(
            binding
                .metadata
                .pointer("/lifecycle_reconcile/reason")
                .and_then(Value::as_str),
            Some("creating_without_liveness")
        );

        let event_count = store.events().unwrap().len();
        let second = store
            .reconcile_stale_managed_bindings_at("startup.reconcile", 20_000, 5_000)
            .unwrap();
        assert!(second.failed_binding_ids.is_empty());
        assert_eq!(
            store.events().unwrap().len(),
            event_count,
            "an idempotent second pass must not emit another lifecycle event"
        );
    }

    #[test]
    fn reconcile_preserves_recent_and_non_creating_bindings() {
        let store = temp_store("reconcile-preserve-state");
        store.upsert_target("test", target("local")).unwrap();
        store
            .upsert_binding("test", managed_binding("recent"))
            .unwrap();
        age_binding(&store, "recent", 9_000);

        let mut ready = managed_binding("ready");
        ready.state = BindingState::Ready;
        store.upsert_binding("test", ready).unwrap();
        age_binding(&store, "ready", 1_000);

        let report = store
            .reconcile_stale_managed_bindings_at("startup.reconcile", 10_000, 5_000)
            .unwrap();

        assert!(report.failed_binding_ids.is_empty());
        assert_eq!(
            store.get_binding("recent").unwrap().state,
            BindingState::Creating
        );
        assert_eq!(
            store.get_binding("ready").unwrap().state,
            BindingState::Ready
        );
    }

    #[test]
    fn reconcile_preserves_creating_binding_with_live_lease() {
        let store = temp_store("reconcile-live-lease");
        store.upsert_target("test", target("local")).unwrap();
        store
            .upsert_binding("test", managed_binding("leased"))
            .unwrap();
        age_binding(&store, "leased", 1_000);
        store
            .acquire_lease(
                "test",
                LeaseKind::AgentSlot,
                "leased",
                Some("local"),
                "workspace",
                "leased",
                60_000,
            )
            .unwrap();

        let now = now_ms();
        let report = store
            .reconcile_stale_managed_bindings_at("startup.reconcile", now, 5_000)
            .unwrap();

        assert!(report.failed_binding_ids.is_empty());
        assert_eq!(
            report.preserved_live_binding_ids,
            vec!["leased"],
            "a non-expired lease is authoritative liveness"
        );
        assert_eq!(
            store.get_binding("leased").unwrap().state,
            BindingState::Creating
        );
    }

    #[test]
    fn reconcile_expires_dead_lease_then_fails_orphan() {
        let store = temp_store("reconcile-expired-lease");
        store.upsert_target("test", target("local")).unwrap();
        store
            .upsert_binding("test", managed_binding("expired"))
            .unwrap();
        age_binding(&store, "expired", 1_000);
        let lease = store
            .acquire_lease(
                "test",
                LeaseKind::AgentSlot,
                "expired",
                Some("local"),
                "workspace",
                "expired",
                60_000,
            )
            .unwrap();
        let mut leases = store.read_leases().unwrap();
        leases
            .leases
            .get_mut(&lease.lease_id)
            .unwrap()
            .expires_at_ms = 2_000;
        store.write_json(&store.leases_path(), &leases).unwrap();

        let report = store
            .reconcile_stale_managed_bindings_at("startup.reconcile", 10_000, 5_000)
            .unwrap();

        assert_eq!(report.failed_binding_ids, vec!["expired"]);
        assert_eq!(
            store
                .read_leases()
                .unwrap()
                .leases
                .get(&lease.lease_id)
                .unwrap()
                .state,
            LeaseState::Expired,
            "reconcile must persist lease expiry instead of only projecting it"
        );
    }

    #[test]
    fn reconcile_preserves_creating_binding_with_active_owned_job() {
        let store = temp_store("reconcile-live-job");
        store.upsert_target("test", target("local")).unwrap();
        store
            .upsert_binding("test", managed_binding("job-owner"))
            .unwrap();
        age_binding(&store, "job-owner", 1_000);
        store
            .save_job(
                "test",
                &ExecutionJob {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    job_id: "job-active".into(),
                    request: ExecutionRequest {
                        schema_version: COMPUTE_SCHEMA_VERSION,
                        request_id: "request-active".into(),
                        workspace_id: "workspace".into(),
                        class: WorkloadClass::Build,
                        argv: vec!["cargo".into(), "check".into()],
                        cwd_relative: ".".into(),
                        snapshot_id: None,
                        requirements: PlacementRequirements::default(),
                        target_policy: "local".into(),
                        environment_allowlist: Vec::new(),
                        declared_outputs: Vec::new(),
                        idempotency_key: None,
                        idempotent: true,
                        destructive: false,
                        timeout_ms: 60_000,
                        requested_by: "job-owner".into(),
                    },
                    target_id: "local".into(),
                    node_session_id: Some("job-process".into()),
                    lease_id: None,
                    state: JobState::Running,
                    attempt: 1,
                    started_at_ms: Some(1),
                    completed_at_ms: None,
                    exit_code: None,
                    termination_reason: None,
                    stdout_stream_id: "stdout".into(),
                    stderr_stream_id: "stderr".into(),
                    artifacts: Vec::new(),
                    decision_id: "decision".into(),
                },
            )
            .unwrap();

        let report = store
            .reconcile_stale_managed_bindings_at("startup.reconcile", 10_000, 5_000)
            .unwrap();

        assert!(report.failed_binding_ids.is_empty());
        assert_eq!(report.preserved_live_binding_ids, vec!["job-owner"]);
    }

    #[test]
    fn reconcile_preserves_recent_runtime_heartbeat_but_not_stale_one() {
        let store = temp_store("reconcile-runtime-heartbeat");
        store.upsert_target("test", target("local")).unwrap();
        for id in ["live-runtime", "dead-runtime"] {
            store.upsert_binding("test", managed_binding(id)).unwrap();
            age_binding(&store, id, 1_000);
        }
        store
            .heartbeat_binding_runtime_at("runtime", "live-runtime", 9_500)
            .unwrap();
        store
            .heartbeat_binding_runtime_at("runtime", "dead-runtime", 2_000)
            .unwrap();
        age_binding(&store, "live-runtime", 1_000);
        age_binding(&store, "dead-runtime", 1_000);

        let report = store
            .reconcile_stale_managed_bindings_at("startup.reconcile", 10_000, 5_000)
            .unwrap();

        assert_eq!(report.preserved_live_binding_ids, vec!["live-runtime"]);
        assert_eq!(report.failed_binding_ids, vec!["dead-runtime"]);
    }

    #[test]
    fn remote_workspace_owns_one_native_workspace_and_requires_ssh() {
        let store = temp_store("remote-workspace");
        let mut ssh = target("ssh-dev");
        ssh.provider = ProviderKind::Ssh;
        ssh.endpoint.ssh_alias = Some("ssh-dev".into());
        store.upsert_target("test", ssh).unwrap();
        let workspace = RemoteWorkspaceSession {
            schema_version: COMPUTE_SCHEMA_VERSION,
            remote_workspace_id: "remote-a".into(),
            window_id: "window-a".into(),
            workspace_id: "workspace-a".into(),
            target_id: "ssh-dev".into(),
            environment_id: None,
            preferred_endpoint_kind: None,
            state: RemoteWorkspaceState::Ready,
            reconnect_policy: ReconnectPolicy::default(),
            reconnect_attempt: 0,
            transport_session_id: None,
            node_id: None,
            last_error: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            metadata: Value::Null,
        };
        store
            .upsert_remote_workspace("test", workspace.clone())
            .unwrap();
        let mut duplicate = workspace.clone();
        duplicate.remote_workspace_id = "remote-b".into();
        assert!(store.upsert_remote_workspace("test", duplicate).is_err());

        let mut local = workspace;
        local.remote_workspace_id = "remote-local".into();
        local.workspace_id = "workspace-local".into();
        local.target_id = "local".into();
        store.upsert_target("test", target("local")).unwrap();
        assert!(store.upsert_remote_workspace("test", local).is_err());
    }

    #[test]
    fn browser_surface_requires_matching_workspace_proxy_and_owned_profile() {
        let store = temp_store("browser-scope");
        let mut ssh_target = target("ssh-dev");
        ssh_target.provider = ProviderKind::Ssh;
        ssh_target.endpoint.ssh_alias = Some("devbox".into());
        store.upsert_target("test", ssh_target).unwrap();
        let now = now_ms();
        store
            .upsert_remote_workspace(
                "test",
                RemoteWorkspaceSession {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    remote_workspace_id: "remote-browser".into(),
                    window_id: "window-browser".into(),
                    workspace_id: "workspace-browser".into(),
                    target_id: "ssh-dev".into(),
                    environment_id: None,
                    preferred_endpoint_kind: None,
                    state: RemoteWorkspaceState::Ready,
                    reconnect_policy: ReconnectPolicy::default(),
                    reconnect_attempt: 0,
                    transport_session_id: None,
                    node_id: None,
                    last_error: None,
                    created_at_ms: now,
                    updated_at_ms: now,
                    metadata: Value::Null,
                },
            )
            .unwrap();
        store
            .save_proxy(
                "test",
                &RemoteProxySession {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    proxy_id: "proxy-browser".into(),
                    target_id: "ssh-dev".into(),
                    environment_id: None,
                    endpoint_id: None,
                    workspace_id: "workspace-browser".into(),
                    surface_id: Some("surface-browser".into()),
                    local_address: "127.0.0.1".into(),
                    local_port: 43210,
                    state: RemoteProxyState::Ready,
                    worker_pid: None,
                    ssh_pid: None,
                    error: None,
                    created_at_ms: now,
                    updated_at_ms: now,
                },
            )
            .unwrap();
        let profile_id = "browser-profile";
        let browser = BrowserSurfaceSession {
            schema_version: COMPUTE_SCHEMA_VERSION,
            browser_surface_id: "browser-a".into(),
            remote_workspace_id: "remote-browser".into(),
            workspace_id: "workspace-browser".into(),
            surface_id: "surface-browser".into(),
            target_id: "ssh-dev".into(),
            environment_id: None,
            proxy_id: "proxy-browser".into(),
            profile_id: profile_id.into(),
            user_data_folder: store
                .browser_profile_path(profile_id)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            state: BrowserSurfaceState::Starting,
            current_url: "https://example.com".into(),
            navigation_history: vec!["https://example.com".into()],
            history_index: 0,
            persistent: true,
            last_error: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        store.save_browser("test", browser.clone()).unwrap();
        assert!(store.browser_profile_path(profile_id).unwrap().is_dir());

        let mut escaped = browser.clone();
        escaped.browser_surface_id = "browser-escaped".into();
        escaped.user_data_folder = std::env::temp_dir()
            .join("outside-profile")
            .to_string_lossy()
            .into_owned();
        assert!(store.save_browser("test", escaped).is_err());

        let mut duplicate = browser;
        duplicate.browser_surface_id = "browser-duplicate".into();
        duplicate.profile_id = "browser-profile-duplicate".into();
        duplicate.user_data_folder = store
            .browser_profile_path(&duplicate.profile_id)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(store.save_browser("test", duplicate).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn recent_lock_from_terminated_owner_is_stale_immediately() {
        let store = temp_store("orphan-lock");
        let lock_path = store.root.join("state.lock");
        fs::write(
            &lock_path,
            "pid=4294967295 timestamp_ms=18446744073709551615\n",
        )
        .unwrap();

        assert!(lock_is_stale(&lock_path));
    }
}
