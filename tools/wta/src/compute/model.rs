//! Versioned contracts shared by the compute broker, CLI and remote node.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const COMPUTE_SCHEMA_VERSION: u16 = 1;
pub const COMPUTE_PROTOCOL_VERSION: u16 = 1;
pub const PLACEMENT_POLICY_VERSION: &str = "wta-placement-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Local,
    Wsl,
    Ssh,
    Azure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    Personal,
    Development,
    Restricted,
    Production,
}

impl TrustTier {
    pub fn permits(self, required: Self) -> bool {
        rank_trust(self) <= rank_trust(required)
    }
}

fn rank_trust(value: TrustTier) -> u8 {
    match value {
        TrustTier::Personal => 0,
        TrustTier::Development => 1,
        TrustTier::Restricted => 2,
        TrustTier::Production => 3,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetHealth {
    Unknown,
    Healthy,
    Degraded,
    Unreachable,
    TrustRequired,
    HostKeyChanged,
    Incompatible,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TargetEndpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wsl_distro: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub azure_resource_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputeTarget {
    pub schema_version: u16,
    pub id: String,
    pub display_name: String,
    pub provider: ProviderKind,
    pub endpoint: TargetEndpoint,
    pub os: String,
    pub arch: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub toolchains: BTreeMap<String, String>,
    pub trust_tier: TrustTier,
    #[serde(default)]
    pub project_allowlist: Vec<String>,
    pub agent_slots: u32,
    pub build_slots: u32,
    pub memory_bytes: u64,
    #[serde(default)]
    pub cost_policy: Value,
    #[serde(default)]
    pub power_policy: Value,
    pub health: TargetHealth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probe_at_ms: Option<u64>,
    pub disabled: bool,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMethod {
    Existing,
    SshManaged,
    BackgroundService,
    FutureAzureVm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentLifecycleState {
    Discovered,
    Provisioning,
    Ready,
    Degraded,
    Offline,
    VersionBlocked,
    Failed,
    Retired,
}

/// Stable runtime identity hosted by a compute target.
///
/// Hostnames, SSH aliases, ports and process IDs are deliberately excluded
/// from identity. They may change while the same versioned node environment
/// and its persistent sessions remain valid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionEnvironment {
    pub schema_version: u16,
    pub environment_id: String,
    pub target_id: String,
    pub runtime_version: String,
    pub protocol_version: u16,
    pub os: String,
    pub arch: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub lifecycle_state: EnvironmentLifecycleState,
    pub launch_method: LaunchMethod,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessEndpointKind {
    SshForward,
    PrivateNetwork,
    Tailscale,
    AuthenticatedWss,
    Relay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointReachability {
    Unknown,
    Loopback,
    Private,
    SshRequired,
    PublicAuthenticated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointHealth {
    Unknown,
    Healthy,
    Degraded,
    Unreachable,
    AuthBlocked,
    VersionBlocked,
}

/// One possible access path to an execution environment.
///
/// This record describes routing policy only. Authentication material,
/// forwarded ports and tunnel process IDs are intentionally not persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccessEndpoint {
    pub schema_version: u16,
    pub endpoint_id: String,
    pub environment_id: String,
    pub kind: AccessEndpointKind,
    pub reachability: EndpointReachability,
    pub health: EndpointHealth,
    pub priority: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentConnectionState {
    Disconnected,
    Connecting,
    Authenticating,
    Synchronizing,
    Connected,
    Offline,
    Reconnecting,
    AuthBlocked,
    VersionBlocked,
    Failed,
}

/// Canonical state for the single connection supervisor of an environment.
///
/// Consumers acquire a connection attempt through this record. They never
/// manufacture a second supervisor identity or persist transport internals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentConnectionSupervisor {
    pub schema_version: u16,
    pub environment_id: String,
    pub state: EnvironmentConnectionState,
    pub preferred_endpoint_kind: AccessEndpointKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_endpoint_id: Option<String>,
    pub retry_attempt: u32,
    pub backoff_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connected_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub generation: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementPolicy {
    LocalFirst,
    Balanced,
    CostFirst,
    Performance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceComputePolicy {
    pub schema_version: u16,
    pub workspace_id: String,
    pub project_root_identity: String,
    #[serde(default)]
    pub eligible_target_ids: Vec<String>,
    pub placement_policy: PlacementPolicy,
    pub default_agent_target: String,
    pub default_job_target: String,
    pub required_trust_tier: TrustTier,
    #[serde(default)]
    pub allowed_network_classes: Vec<String>,
    #[serde(default)]
    pub secret_allowlist: Vec<String>,
    pub production_targets_allowed: bool,
}

impl Default for WorkspaceComputePolicy {
    fn default() -> Self {
        Self {
            schema_version: COMPUTE_SCHEMA_VERSION,
            workspace_id: String::new(),
            project_root_identity: String::new(),
            eligible_target_ids: Vec::new(),
            placement_policy: PlacementPolicy::Balanced,
            default_agent_target: "sticky_auto".to_string(),
            default_job_target: "auto".to_string(),
            required_trust_tier: TrustTier::Development,
            allowed_network_classes: Vec::new(),
            secret_allowlist: Vec::new(),
            production_targets_allowed: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingKind {
    PlainTerminal,
    ManagedAgent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingState {
    Creating,
    Ready,
    Disconnected,
    Reconnecting,
    Detached,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteWorkspaceState {
    Creating,
    Probing,
    Bootstrapping,
    Ready,
    Reconnecting,
    Offline,
    Failed,
    Closing,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconnectPolicy {
    #[serde(default = "default_reconnect_delays")]
    pub delays_seconds: Vec<u64>,
    #[serde(default = "default_reconnect_ceiling")]
    pub ceiling_seconds: u64,
    #[serde(default = "default_true")]
    pub manual_reconnect: bool,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            delays_seconds: default_reconnect_delays(),
            ceiling_seconds: default_reconnect_ceiling(),
            manual_reconnect: true,
        }
    }
}

fn default_reconnect_delays() -> Vec<u64> {
    vec![3, 6, 12, 24, 48, 60]
}

fn default_reconnect_ceiling() -> u64 {
    60
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteWorkspaceSession {
    pub schema_version: u16,
    pub remote_workspace_id: String,
    pub window_id: String,
    pub workspace_id: String,
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_endpoint_kind: Option<AccessEndpointKind>,
    pub state: RemoteWorkspaceState,
    pub reconnect_policy: ReconnectPolicy,
    pub reconnect_attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSurfaceKind {
    Terminal,
    ManagedAgent,
    Browser,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteSurfaceSession {
    pub schema_version: u16,
    pub remote_surface_id: String,
    pub remote_workspace_id: String,
    pub binding_id: String,
    pub pty_session_id: String,
    pub kind: RemoteSurfaceKind,
    pub state: BindingState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfaceBinding {
    pub schema_version: u16,
    pub binding_id: String,
    pub window_id: String,
    pub workspace_id: String,
    pub pane_id: String,
    pub surface_id: String,
    pub focus_generation: u64,
    pub kind: BindingKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_endpoint_kind: Option<AccessEndpointKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_lease_id: Option<String>,
    pub state: BindingState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadClass {
    InteractiveAgent,
    Build,
    Test,
    Lint,
    Browser,
    Gpu,
}

impl WorkloadClass {
    pub fn slot_class(self) -> SlotClass {
        match self {
            Self::InteractiveAgent => SlotClass::Agent,
            _ => SlotClass::Build,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotClass {
    Agent,
    Build,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PlacementRequirements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub toolchains: BTreeMap<String, String>,
    #[serde(default)]
    pub minimum_memory_bytes: u64,
    #[serde(default)]
    pub project_identity: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacementRequest {
    pub schema_version: u16,
    pub request_id: String,
    pub workspace_id: String,
    pub workload: WorkloadClass,
    pub requirements: PlacementRequirements,
    pub candidate_policy: PlacementPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_target_id: Option<String>,
    #[serde(default)]
    pub excluded_target_ids: Vec<String>,
    pub production_targets_allowed: bool,
    pub required_trust_tier: TrustTier,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacementCandidate {
    pub target_id: String,
    pub eligible: bool,
    #[serde(default)]
    pub exclusion_reasons: Vec<String>,
    #[serde(default)]
    pub score_components: BTreeMap<String, f64>,
    pub total_score: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacementDecision {
    pub schema_version: u16,
    pub decision_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_target_id: Option<String>,
    pub candidates: Vec<PlacementCandidate>,
    pub policy_version: String,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionRequest {
    pub schema_version: u16,
    pub request_id: String,
    pub workspace_id: String,
    pub class: WorkloadClass,
    pub argv: Vec<String>,
    pub cwd_relative: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    pub requirements: PlacementRequirements,
    pub target_policy: String,
    #[serde(default)]
    pub environment_allowlist: Vec<String>,
    #[serde(default)]
    pub declared_outputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub idempotent: bool,
    pub destructive: bool,
    pub timeout_ms: u64,
    pub requested_by: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Staging,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Lost,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Lost
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactManifest {
    pub relative_path: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Upload,
    Download,
}

impl Default for TransferDirection {
    fn default() -> Self {
        Self::Upload
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Preparing,
    Uploading,
    Downloading,
    Cancelling,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileTransfer {
    pub schema_version: u16,
    pub transfer_id: String,
    #[serde(default)]
    pub direction: TransferDirection,
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface_id: Option<String>,
    pub source_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(default)]
    pub overwrite: bool,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_path: Option<String>,
    pub size_bytes: u64,
    #[serde(default)]
    pub bytes_transferred: u64,
    pub sha256: String,
    pub state: TransferState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteFileRootSource {
    Project,
    Worktree,
    ExplicitHome,
    Admin,
}

/// Broker-owned authorization for one remote file explorer root.
///
/// `canonical_path` is persisted only in the package-private Compute Store and
/// is never returned by `compute file roots`. UI and agents use the opaque
/// `root_id`; the broker resolves the path and grants the remote node only for
/// the lifetime of one authenticated SSH/stdio operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteFileRootPolicy {
    pub schema_version: u16,
    pub root_id: String,
    pub workspace_id: String,
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_id: Option<String>,
    pub label: String,
    pub canonical_path: String,
    pub readable: bool,
    pub writable: bool,
    pub deletable: bool,
    pub source: RemoteFileRootSource,
    pub trust_tier: TrustTier,
    /// Explicit acknowledgement for broad roots such as HOME or an
    /// administrator-configured path. This is never inferred from filesystem
    /// permissions.
    #[serde(default)]
    pub wide_scope_acknowledged: bool,
    #[serde(default = "default_true")]
    pub active: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProxyState {
    Starting,
    Ready,
    Stopping,
    Stopped,
    Failed,
}

impl RemoteProxyState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Failed)
    }
}

/// Host-local SOCKS endpoint whose traffic exits through one SSH target.
///
/// The proxy is scoped to a workspace and optionally to a surface. Browser
/// hosts consume `local_address`; they never receive SSH credentials or a
/// route to another workspace's proxy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteProxySession {
    pub schema_version: u16,
    pub proxy_id: String,
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_id: Option<String>,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface_id: Option<String>,
    pub local_address: String,
    pub local_port: u16,
    pub state: RemoteProxyState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserSurfaceState {
    Starting,
    Ready,
    Navigating,
    Reconnecting,
    Closing,
    Closed,
    Failed,
}

impl BrowserSurfaceState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Closed | Self::Failed)
    }
}

/// A native browser surface with one isolated WebView2 profile and one
/// surface-scoped remote proxy.
///
/// The browser renderer is intentionally outside the compute store. This
/// record is the canonical lifecycle/security contract consumed by the native
/// host: neither cookies nor SSH credentials are serialized here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserSurfaceSession {
    pub schema_version: u16,
    pub browser_surface_id: String,
    pub remote_workspace_id: String,
    pub workspace_id: String,
    pub surface_id: String,
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    pub proxy_id: String,
    pub profile_id: String,
    pub user_data_folder: String,
    pub state: BrowserSurfaceState,
    pub current_url: String,
    #[serde(default)]
    pub navigation_history: Vec<String>,
    #[serde(default)]
    pub history_index: usize,
    #[serde(default = "default_true")]
    pub persistent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeRestoreSnapshot {
    pub schema_version: u16,
    pub restore_id: String,
    pub window_id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_surface_id: Option<String>,
    #[serde(default)]
    pub remote_workspace_ids: Vec<String>,
    #[serde(default)]
    pub binding_ids: Vec<String>,
    #[serde(default)]
    pub browser_surface_ids: Vec<String>,
    #[serde(default)]
    pub runtime_references: Vec<RuntimeRestoreReference>,
    pub captured_at_ms: u64,
}

/// Stable identities required to reconstruct a remote runtime attachment.
/// Ephemeral ports, PIDs, tunnel paths and authentication details never enter
/// a restore snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRestoreReference {
    pub environment_id: String,
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_id: Option<String>,
    pub preferred_endpoint_kind: AccessEndpointKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreAction {
    RestoreNativeLayout,
    ReconnectEnvironment,
    ReconnectRemoteWorkspace,
    ReattachManagedAgent,
    RecreateBrowserController,
    RestartBrowserProxy,
    RestoreFocus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreItemState {
    Ready,
    Planned,
    Applied,
    RequiresNativeUi,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestorePlanItem {
    pub action: RestoreAction,
    pub entity_id: String,
    pub state: RestoreItemState,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeRestorePlan {
    pub schema_version: u16,
    pub restore_id: String,
    pub window_id: String,
    pub workspace_id: String,
    pub items: Vec<RestorePlanItem>,
    pub generated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionJob {
    pub schema_version: u16,
    pub job_id: String,
    pub request: ExecutionRequest,
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    pub state: JobState,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<String>,
    pub stdout_stream_id: String,
    pub stderr_stream_id: String,
    #[serde(default)]
    pub artifacts: Vec<ArtifactManifest>,
    pub decision_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    pub relative_path: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub schema_version: u16,
    pub snapshot_id: String,
    pub format_version: u16,
    pub repository_identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    pub tracked_patch_digest: String,
    #[serde(default)]
    pub untracked_entries: Vec<SnapshotEntry>,
    #[serde(default)]
    pub deleted_entries: Vec<String>,
    #[serde(default)]
    pub mode_entries: BTreeMap<String, String>,
    pub symlink_policy: String,
    #[serde(default)]
    pub ignored_includes: Vec<String>,
    #[serde(default)]
    pub excluded_secret_candidates: Vec<String>,
    pub overall_digest: String,
    pub created_by: String,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseKind {
    AgentSlot,
    BuildSlot,
    Writer,
    TargetLock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    Active,
    Released,
    Revoked,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    pub schema_version: u16,
    pub lease_id: String,
    pub kind: LeaseKind,
    pub subject_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    pub workspace_id: String,
    pub owner: String,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub heartbeat_at_ms: u64,
    pub state: LeaseState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputeEvent {
    pub schema_version: u16,
    pub id: String,
    pub timestamp_ms: u64,
    pub kind: String,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeHandshake {
    pub protocol_version: u16,
    pub node_version: String,
    pub node_id: String,
    pub os: String,
    pub arch: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub rpc_methods: Vec<String>,
    pub state_root: String,
}
