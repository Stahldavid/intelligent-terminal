use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use super::WorkspaceRuntime;
use crate::team::TeamState;
use wta::compute::model::{
    AccessEndpoint, BrowserSurfaceSession, ComputeEvent, ComputeTarget,
    EnvironmentConnectionSupervisor, ExecutionEnvironment, ExecutionJob, FileTransfer,
    RemoteFileRootSource, RemoteProxySession, RemoteWorkspaceSession, RuntimeRestoreSnapshot,
    SurfaceBinding, WorkspaceComputePolicy,
};
use wta::compute::store::ComputeStore;

const DEFAULT_GIT_INSPECTION_BYTES: usize = 256 * 1024;
const MAX_TEAM_STATE_BYTES: u64 = 1024 * 1024;
const MAX_TEAMS_PER_WORKSPACE: usize = 32;

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceContext {
    pub project: ProjectContext,
    pub git: Option<GitContext>,
    pub pull_request: Option<Value>,
    pub listening_ports: Vec<PortContext>,
    pub agents: Vec<AgentContext>,
    /// Native team control-plane snapshots bound to this exact workspace.
    /// Legacy teams without a workspace_id are intentionally omitted: showing
    /// them in an arbitrary tab would violate the canonical scope boundary.
    pub teams: Vec<TeamState>,
    /// Read-only projection of the compute control plane for this workspace.
    /// The Terminal consumes this alongside agents/tasks so UI and CLI never
    /// grow competing stores.
    pub compute: ComputeContext,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ComputeContext {
    pub targets: Vec<ComputeTarget>,
    pub bindings: Vec<SurfaceBinding>,
    pub jobs: Vec<ExecutionJob>,
    pub transfers: Vec<FileTransfer>,
    pub events: Vec<ComputeEvent>,
    pub remote_workspaces: Vec<RemoteWorkspaceSession>,
    pub browsers: Vec<BrowserSurfaceSession>,
    pub proxies: Vec<RemoteProxySession>,
    pub restore_snapshots: Vec<RuntimeRestoreSnapshot>,
    pub environments: Vec<ExecutionEnvironment>,
    pub endpoints: Vec<AccessEndpoint>,
    pub connections: Vec<EnvironmentConnectionSupervisor>,
    pub file_roots: Vec<RemoteFileRootSummary>,
    pub policy: Option<WorkspaceComputePolicy>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteFileRootSummary {
    pub id: String,
    pub target_id: String,
    pub binding_id: Option<String>,
    pub label: String,
    pub readable: bool,
    pub writable: bool,
    pub deletable: bool,
    pub source: RemoteFileRootSource,
    pub broad_access: bool,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectContext {
    pub name: String,
    pub root: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitContext {
    pub branch: String,
    pub upstream: Option<String>,
    pub changed_files: usize,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct PortContext {
    pub address: String,
    pub port: u16,
    pub pid: u32,
    pub pane: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentContext {
    pub id: String,
    pub role: String,
    pub model: Option<String>,
    pub activity: String,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitInspection {
    pub root: String,
    pub summary: Option<GitContext>,
    pub status: String,
    pub diff: String,
    pub truncated: bool,
}

pub async fn collect_context(runtime: &WorkspaceRuntime) -> WorkspaceContext {
    let root = Path::new(&runtime.root);
    let compute = collect_compute_context(&runtime.workspace_id);
    WorkspaceContext {
        project: ProjectContext {
            name: root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            root: runtime.root.clone(),
        },
        git: collect_git(root).await,
        pull_request: collect_pull_request(root).await,
        listening_ports: collect_ports(runtime).await,
        agents: runtime
            .panes
            .values()
            .map(|pane| AgentContext {
                id: pane.logical_id.clone(),
                role: pane.role.clone(),
                model: pane.model.clone(),
                activity: format!("{:?}", pane.activity).to_ascii_lowercase(),
                pid: pane.pid,
            })
            .collect(),
        teams: collect_teams(root, &runtime.workspace_id),
        compute,
    }
}

fn collect_compute_context(workspace_id: &str) -> ComputeContext {
    let store = match ComputeStore::package_default() {
        Ok(store) => store,
        Err(error) => {
            return ComputeContext {
                error: Some(error.to_string()),
                ..Default::default()
            };
        }
    };
    let targets = match store.list_targets() {
        Ok(targets) => targets,
        Err(error) => {
            return ComputeContext {
                error: Some(error.to_string()),
                ..Default::default()
            };
        }
    };
    let bindings: Vec<SurfaceBinding> = store
        .list_bindings()
        .unwrap_or_default()
        .into_iter()
        .filter(|binding| binding.workspace_id == workspace_id)
        .collect();
    let jobs = store
        .list_jobs()
        .unwrap_or_default()
        .into_iter()
        .filter(|job| job.request.workspace_id == workspace_id)
        .collect();
    let transfers = store
        .list_transfers()
        .unwrap_or_default()
        .into_iter()
        .filter(|transfer| transfer.workspace_id.as_deref() == Some(workspace_id))
        .collect();
    let events = store
        .events()
        .unwrap_or_default()
        .into_iter()
        .filter(|event| event.workspace_id.as_deref() == Some(workspace_id))
        .collect();
    let remote_workspaces: Vec<RemoteWorkspaceSession> = store
        .list_remote_workspaces()
        .unwrap_or_default()
        .into_iter()
        .filter(|remote| remote.workspace_id == workspace_id)
        .collect();
    let browsers = store
        .list_browsers()
        .unwrap_or_default()
        .into_iter()
        .filter(|browser| browser.workspace_id == workspace_id)
        .collect();
    let proxies = store
        .list_proxies()
        .unwrap_or_default()
        .into_iter()
        .filter(|proxy| proxy.workspace_id == workspace_id)
        .collect();
    let restore_snapshots = store
        .list_restore_snapshots()
        .unwrap_or_default()
        .into_iter()
        .filter(|snapshot| snapshot.workspace_id == workspace_id)
        .collect();
    let environment_ids = remote_workspaces
        .iter()
        .filter_map(|workspace| workspace.environment_id.clone())
        .chain(
            bindings
                .iter()
                .filter_map(|binding| binding.environment_id.clone()),
        )
        .collect::<BTreeSet<_>>();
    let environments = store
        .list_environments()
        .unwrap_or_default()
        .into_iter()
        .filter(|environment| environment_ids.contains(&environment.environment_id))
        .collect::<Vec<_>>();
    let endpoints = store
        .list_endpoints(None)
        .unwrap_or_default()
        .into_iter()
        .filter(|endpoint| environment_ids.contains(&endpoint.environment_id))
        .collect::<Vec<_>>();
    let connections = store
        .list_connection_supervisors()
        .unwrap_or_default()
        .into_iter()
        .filter(|connection| environment_ids.contains(&connection.environment_id))
        .collect::<Vec<_>>();
    let file_roots = store
        .list_file_root_policies(Some(workspace_id), None, None, true)
        .unwrap_or_default()
        .into_iter()
        .map(|policy| RemoteFileRootSummary {
            id: policy.root_id,
            target_id: policy.target_id,
            binding_id: policy.binding_id,
            label: policy.label,
            readable: policy.readable,
            writable: policy.writable,
            deletable: policy.deletable,
            source: policy.source,
            broad_access: matches!(
                policy.source,
                RemoteFileRootSource::ExplicitHome | RemoteFileRootSource::Admin
            ),
            active: policy.active,
        })
        .collect();
    let policy = store.get_policy(workspace_id).ok();
    ComputeContext {
        targets,
        bindings,
        jobs,
        transfers,
        events,
        remote_workspaces,
        browsers,
        proxies,
        restore_snapshots,
        environments,
        endpoints,
        connections,
        file_roots,
        policy,
        error: None,
    }
}

fn collect_teams(root: &Path, workspace_id: &str) -> Vec<TeamState> {
    let teams_root = root.join(".intelligent-terminal").join("teams");
    let Ok(entries) = fs::read_dir(teams_root) else {
        return Vec::new();
    };
    let mut states = entries
        .filter_map(Result::ok)
        .take(MAX_TEAMS_PER_WORKSPACE * 2)
        .filter_map(|entry| {
            let state_path = entry.path().join("state.json");
            let metadata = fs::metadata(&state_path).ok()?;
            if !metadata.is_file() || metadata.len() > MAX_TEAM_STATE_BYTES {
                return None;
            }
            let bytes = fs::read(state_path).ok()?;
            let state = serde_json::from_slice::<TeamState>(&bytes).ok()?;
            (state.workspace_id.as_deref() == Some(workspace_id)).then_some(state)
        })
        .take(MAX_TEAMS_PER_WORKSPACE)
        .collect::<Vec<_>>();
    states.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
    states
}

/// Collect a bounded, read-only native Git view for the workspace.
///
/// `git status` and `git diff` are executed without a shell and with a hard
/// timeout. The byte cap keeps very large repositories from overwhelming the
/// UI or the WTA transport.
pub async fn inspect_git(root: &Path, max_bytes: Option<usize>) -> GitInspection {
    let max_bytes = max_bytes
        .unwrap_or(DEFAULT_GIT_INSPECTION_BYTES)
        .clamp(4 * 1024, 2 * 1024 * 1024);
    let summary = collect_git(root).await;
    let status_output = run_bounded(
        "git",
        &[
            "-C",
            &root.to_string_lossy(),
            "status",
            "--short",
            "--branch",
        ],
        root,
    )
    .await;
    let diff_output = run_bounded(
        "git",
        &[
            "-C",
            &root.to_string_lossy(),
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--unified=3",
        ],
        root,
    )
    .await;

    let (status, status_truncated) = bounded_output(status_output, max_bytes / 4);
    let (diff, diff_truncated) = bounded_output(diff_output, max_bytes);
    GitInspection {
        root: root.to_string_lossy().into_owned(),
        summary,
        status,
        diff,
        truncated: status_truncated || diff_truncated,
    }
}

async fn collect_git(root: &Path) -> Option<GitContext> {
    let output = run_bounded(
        "git",
        &[
            "-C",
            &root.to_string_lossy(),
            "status",
            "--porcelain=v2",
            "--branch",
        ],
        root,
    )
    .await?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut branch = String::new();
    let mut upstream = None;
    let mut ahead = 0;
    let mut behind = 0;
    let mut changed_files = 0;
    for line in stdout.lines() {
        if let Some(value) = line.strip_prefix("# branch.head ") {
            branch = value.to_string();
        } else if let Some(value) = line.strip_prefix("# branch.upstream ") {
            upstream = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("# branch.ab ") {
            for token in value.split_whitespace() {
                if let Some(value) = token.strip_prefix('+') {
                    ahead = value.parse().unwrap_or_default();
                } else if let Some(value) = token.strip_prefix('-') {
                    behind = value.parse().unwrap_or_default();
                }
            }
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            changed_files += 1;
        }
    }
    Some(GitContext {
        branch,
        upstream,
        changed_files,
        ahead,
        behind,
    })
}

async fn collect_pull_request(root: &Path) -> Option<Value> {
    let output = run_bounded(
        "gh",
        &[
            "pr",
            "view",
            "--json",
            "number,title,state,url,headRefName,reviewDecision,statusCheckRollup",
        ],
        root,
    )
    .await?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

async fn collect_ports(runtime: &WorkspaceRuntime) -> Vec<PortContext> {
    let root = Path::new(&runtime.root);
    let Some(output) = run_bounded("netstat.exe", &["-ano", "-p", "tcp"], root).await else {
        return Vec::new();
    };
    let pane_by_pid = runtime
        .panes
        .values()
        .filter_map(|pane| pane.pid.map(|pid| (pid, pane.logical_id.clone())))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut ports = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let columns = line.split_whitespace().collect::<Vec<_>>();
            if columns.len() < 5 || columns[0] != "TCP" || columns[3] != "LISTENING" {
                return None;
            }
            let pid = columns[4].parse::<u32>().ok()?;
            let (address, port) = split_endpoint(columns[1])?;
            Some(PortContext {
                address,
                port,
                pid,
                pane: pane_by_pid.get(&pid).cloned(),
            })
        })
        .filter(|port| port.pane.is_some())
        .collect::<Vec<_>>();
    ports.sort_by_key(|port| (port.port, port.pid));
    ports.dedup_by_key(|port| (port.port, port.pid));
    ports
}

fn split_endpoint(endpoint: &str) -> Option<(String, u16)> {
    let (address, port) = endpoint.rsplit_once(':')?;
    Some((
        address.trim_matches(['[', ']']).to_string(),
        port.parse().ok()?,
    ))
}

async fn run_bounded(program: &str, args: &[&str], cwd: &Path) -> Option<std::process::Output> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = command.spawn().ok()?;
    tokio::time::timeout(Duration::from_secs(3), child.wait_with_output())
        .await
        .ok()?
        .ok()
}

fn bounded_output(output: Option<std::process::Output>, max_bytes: usize) -> (String, bool) {
    let Some(output) = output.filter(|output| output.status.success()) else {
        return (String::new(), false);
    };
    if output.stdout.len() <= max_bytes {
        return (String::from_utf8_lossy(&output.stdout).into_owned(), false);
    }
    (
        String::from_utf8_lossy(&output.stdout[..max_bytes]).into_owned(),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::TeamStatus;
    use std::collections::BTreeMap;
    use uuid::Uuid;

    #[test]
    fn parses_ipv4_and_ipv6_endpoints() {
        assert_eq!(
            split_endpoint("127.0.0.1:3000"),
            Some(("127.0.0.1".to_string(), 3000))
        );
        assert_eq!(split_endpoint("[::]:8080"), Some(("::".to_string(), 8080)));
    }

    #[test]
    fn bounded_output_reports_truncation() {
        let output = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "echo", "123456789"])
            .output()
            .unwrap();
        let (value, truncated) = bounded_output(Some(output), 4);
        assert_eq!(value, "1234");
        assert!(truncated);
    }

    #[test]
    fn team_context_is_strictly_scoped_to_workspace_id() {
        let root = std::env::temp_dir().join(format!("wta-context-{}", Uuid::new_v4()));
        let teams = root.join(".intelligent-terminal").join("teams");
        for name in ["matching", "other", "legacy"] {
            fs::create_dir_all(teams.join(name)).unwrap();
        }
        let state = |name: &str, workspace_id: Option<&str>, updated_at_ms: u64| TeamState {
            schema_version: 1,
            team_id: format!("team-{name}"),
            workspace_id: workspace_id.map(str::to_string),
            name: name.to_string(),
            root: root.to_string_lossy().into_owned(),
            leader: "leader".into(),
            status: TeamStatus::Active,
            stale_after_ms: 60_000,
            default_max_attempts: 2,
            workers: BTreeMap::new(),
            tasks: BTreeMap::new(),
            created_at_ms: 1,
            updated_at_ms,
        };
        for team in [
            state("matching", Some("workspace-a"), 3),
            state("other", Some("workspace-b"), 2),
            state("legacy", None, 1),
        ] {
            fs::write(
                teams.join(&team.name).join("state.json"),
                serde_json::to_vec(&team).unwrap(),
            )
            .unwrap();
        }

        let collected = collect_teams(&root, "workspace-a");
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].name, "matching");

        fs::remove_dir_all(root).unwrap();
    }
}
