//! Explicit routed execution. Normal terminal commands are never intercepted.

use std::fs::{self, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use serde_json::json;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

use super::model::*;
use super::placement;
use super::snapshot;
use super::store::{now_ms, ComputeStore};

pub async fn execute(
    store: &ComputeStore,
    project_root: &Path,
    request: ExecutionRequest,
) -> Result<ExecutionJob> {
    validate_request(&request)?;
    let decision = placement::decide(
        store,
        &PlacementRequest {
            schema_version: COMPUTE_SCHEMA_VERSION,
            request_id: Uuid::new_v4().to_string(),
            workspace_id: request.workspace_id.clone(),
            workload: request.class,
            requirements: request.requirements.clone(),
            candidate_policy: PlacementPolicy::Balanced,
            preferred_target_id: (request.target_policy != "auto")
                .then(|| request.target_policy.clone()),
            excluded_target_ids: Vec::new(),
            production_targets_allowed: false,
            required_trust_tier: TrustTier::Development,
        },
    )?;
    let target_id = decision
        .selected_target_id
        .clone()
        .context("no eligible compute target")?;
    let target = store.get_target(&target_id)?;
    if target.provider == ProviderKind::Ssh {
        bail!(
            "SSH routed execution is disabled until snapshot staging, remote cwd, artifact retrieval and remote process cancellation are all active; use a local/WSL target"
        );
    }
    let job_id = format!("job-{}", Uuid::new_v4());
    let lease = store.acquire_lease(
        &request.requested_by,
        LeaseKind::BuildSlot,
        &job_id,
        Some(&target_id),
        &request.workspace_id,
        &request.requested_by,
        request.timeout_ms.saturating_add(60_000),
    )?;
    let mut job = ExecutionJob {
        schema_version: COMPUTE_SCHEMA_VERSION,
        job_id: job_id.clone(),
        request,
        target_id,
        node_session_id: None,
        lease_id: Some(lease.lease_id.clone()),
        state: JobState::Staging,
        attempt: 1,
        started_at_ms: None,
        completed_at_ms: None,
        exit_code: None,
        termination_reason: None,
        stdout_stream_id: format!("{job_id}:stdout"),
        stderr_stream_id: format!("{job_id}:stderr"),
        artifacts: Vec::new(),
        decision_id: decision.decision_id.clone(),
    };
    store.save_job(&job.request.requested_by, &job)?;
    let job_root = store.job_path(&job.job_id);
    fs::create_dir_all(&job_root)?;
    fs::write(
        job_root.join("placement.json"),
        serde_json::to_vec_pretty(&decision)?,
    )?;
    let stdout_path = job_root.join("stdout.log");
    let stderr_path = job_root.join("stderr.log");
    let stdout = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&stdout_path)?;
    let stderr = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&stderr_path)?;

    let cwd = safe_cwd(project_root, &job.request.cwd_relative)?;
    let mut command = command_for_target(&target, &job.request.argv, &cwd)?;
    for key in &job.request.environment_allowlist {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    job.state = JobState::Running;
    job.started_at_ms = Some(now_ms());
    store.save_job(&job.request.requested_by, &job)?;
    let mut child = command
        .spawn()
        .context("failed to spawn routed execution")?;
    job.node_session_id = child.id().map(|pid| pid.to_string());
    store.save_job(&job.request.requested_by, &job)?;
    let result = timeout(Duration::from_millis(job.request.timeout_ms), child.wait()).await;
    job.completed_at_ms = Some(now_ms());
    match result {
        Ok(Ok(status)) => {
            job.exit_code = status.code();
            job.state = if status.success() {
                JobState::Succeeded
            } else {
                JobState::Failed
            };
            if !status.success() {
                job.termination_reason = Some("nonzero_exit".to_string());
            }
        }
        Ok(Err(error)) => {
            job.state = JobState::Failed;
            job.termination_reason = Some(format!("wait_failed:{error}"));
        }
        Err(_) => {
            // kill_on_drop terminates the child process tree owned by tokio.
            job.state = JobState::TimedOut;
            job.termination_reason = Some("timeout".to_string());
        }
    }
    if job.state == JobState::Succeeded {
        job.artifacts = collect_artifacts(&cwd, &job.request.declared_outputs)?;
    }
    store.save_job(&job.request.requested_by, &job)?;
    let _ = store.revoke_lease(&job.request.requested_by, &lease.lease_id, "job terminal");
    Ok(job)
}

pub fn logs(store: &ComputeStore, id: &str) -> Result<serde_json::Value> {
    let job = store.get_job(id)?;
    let root = store.job_path(id);
    Ok(json!({
        "job_id": id,
        "state": job.state,
        "stdout": fs::read_to_string(root.join("stdout.log")).unwrap_or_default(),
        "stderr": fs::read_to_string(root.join("stderr.log")).unwrap_or_default(),
    }))
}

pub fn cancel(store: &ComputeStore, actor: &str, id: &str) -> Result<ExecutionJob> {
    let mut job = store.get_job(id)?;
    if job.state.is_terminal() {
        bail!("job {id} is already terminal");
    }
    let pid = job
        .node_session_id
        .as_deref()
        .context("job has no process identity")?;
    #[cfg(windows)]
    let output = std::process::Command::new("taskkill")
        .args(["/PID", pid, "/T", "/F"])
        .output()?;
    #[cfg(not(windows))]
    let output = std::process::Command::new("kill")
        .args(["-TERM", &format!("-{pid}")])
        .output()?;
    if !output.status.success() {
        bail!(
            "failed to terminate job {id}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    job.state = JobState::Cancelled;
    job.completed_at_ms = Some(now_ms());
    job.termination_reason = Some("cancelled_by_user".to_string());
    store.save_job(actor, &job)?;
    if let Some(lease) = job.lease_id.as_deref() {
        let _ = store.revoke_lease(actor, lease, "job cancelled");
    }
    Ok(job)
}

fn command_for_target(target: &ComputeTarget, argv: &[String], cwd: &Path) -> Result<Command> {
    let command = match target.provider {
        ProviderKind::Local => {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]).current_dir(cwd);
            command
        }
        ProviderKind::Wsl => {
            let distro = target
                .endpoint
                .wsl_distro
                .as_deref()
                .context("WSL target is missing distro")?;
            let mut command = Command::new("wsl.exe");
            command.arg("-d").arg(distro).arg("--cd").arg(cwd).arg("--");
            command.args(argv);
            command
        }
        ProviderKind::Ssh => {
            let alias = target
                .endpoint
                .ssh_alias
                .as_deref()
                .context("SSH target is missing alias")?;
            super::ssh::resolve_alias(alias)?;
            let mut command = Command::new(super::ssh::find_ssh_executable()?);
            command.arg(alias).arg("--").args(argv);
            command
        }
        ProviderKind::Azure => {
            bail!("Azure target must be started and exposed through SSH before execution")
        }
    };
    Ok(command)
}

fn validate_request(request: &ExecutionRequest) -> Result<()> {
    if request.argv.is_empty() || request.argv[0].trim().is_empty() {
        bail!("execution argv must contain an executable");
    }
    if request.timeout_ms < 100 {
        bail!("execution timeout must be at least 100ms");
    }
    if request.destructive && request.idempotent {
        bail!("destructive execution cannot be declared idempotent");
    }
    for output in &request.declared_outputs {
        validate_relative(output)?;
    }
    validate_relative_allow_empty(&request.cwd_relative)?;
    Ok(())
}

fn safe_cwd(root: &Path, relative: &str) -> Result<PathBuf> {
    validate_relative_allow_empty(relative)?;
    let cwd = if relative.is_empty() || relative == "." {
        root.to_path_buf()
    } else {
        root.join(relative)
    };
    if !cwd.is_dir() {
        bail!("execution cwd does not exist: {}", cwd.display());
    }
    Ok(cwd)
}

fn validate_relative_allow_empty(value: &str) -> Result<()> {
    if value.is_empty() || value == "." {
        return Ok(());
    }
    validate_relative(value)
}

fn validate_relative(value: &str) -> Result<()> {
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("path must be project-relative: {value}");
    }
    Ok(())
}

fn collect_artifacts(root: &Path, outputs: &[String]) -> Result<Vec<ArtifactManifest>> {
    let mut artifacts = Vec::new();
    for relative in outputs {
        let path = root.join(relative);
        if path.is_file() {
            artifacts.push(ArtifactManifest {
                relative_path: relative.clone(),
                size_bytes: fs::metadata(&path)?.len(),
                sha256: snapshot::sha256_file(&path)?,
            });
        }
    }
    Ok(artifacts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_shellless_and_traversing_requests() {
        let request = ExecutionRequest {
            schema_version: COMPUTE_SCHEMA_VERSION,
            request_id: "r".into(),
            workspace_id: "w".into(),
            class: WorkloadClass::Build,
            argv: Vec::new(),
            cwd_relative: "../x".into(),
            snapshot_id: None,
            requirements: PlacementRequirements::default(),
            target_policy: "auto".into(),
            environment_allowlist: Vec::new(),
            declared_outputs: vec!["../secret".into()],
            idempotency_key: None,
            idempotent: false,
            destructive: false,
            timeout_ms: 1000,
            requested_by: "test".into(),
        };
        assert!(validate_request(&request).is_err());
    }
}
