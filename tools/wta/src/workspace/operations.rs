use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::json;

use crate::shell::wt_channel::WtChannel;
use crate::win32;

use super::{
    DeclarativeOperation, DeclarativeWorkspacePlan, PaneActivity, PaneSpec, RuntimePane,
    SurfaceSpec, WorkspaceEvent, WorkspaceManifest, WorkspaceRuntime, WorkspaceStore,
};

#[derive(Debug, Clone, Serialize)]
pub struct ApplyResult {
    pub workspace: WorkspaceRuntime,
    pub store: String,
    pub opened_external_surfaces: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationResult {
    pub candidate: String,
    pub command: String,
    pub cwd: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u128,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationReport {
    pub winner: Option<String>,
    pub all_passed: bool,
    pub results: Vec<VerificationResult>,
}

pub async fn apply_declarative_plan(
    channel: &dyn WtChannel,
    manifest: &WorkspaceManifest,
    manifest_path: &Path,
    plan: &DeclarativeWorkspacePlan,
) -> Result<ApplyResult> {
    preflight_apply(plan)?;
    prepare_worktrees(plan)?;
    let root = PathBuf::from(&plan.root);
    let store = WorkspaceStore::open(&root, &plan.name)?;
    let mut runtime = WorkspaceRuntime::new(&plan.name, manifest_path, &root);
    let mut session_ids = BTreeMap::<String, String>::new();
    let mut opened_external_surfaces = Vec::new();

    for operation in &plan.operations {
        let pane = operation.pane();
        let commandline = prepare_command(&store, pane)?;
        let result = match operation {
            DeclarativeOperation::CreateTab { title, .. } => channel
                .request(
                    "create_tab",
                    compact_params(json!({
                        "commandline": commandline,
                        "cwd": pane.cwd,
                        "profile": pane.profile,
                        "title": title,
                    })),
                )
                .await
                .with_context(|| format!("failed to create workspace pane {}", pane.id))?,
            DeclarativeOperation::SplitPane {
                target,
                direction,
                ratio,
                ..
            } => {
                let target_session = session_ids.get(target).ok_or_else(|| {
                    anyhow!("workspace target pane {target} has not been created")
                })?;
                channel
                    .request(
                        "split_pane",
                        compact_params(json!({
                            "session_id": target_session,
                            "direction": direction.as_protocol_value(),
                            "size": ratio,
                            "commandline": commandline,
                            "cwd": pane.cwd,
                            "profile": pane.profile,
                        })),
                    )
                    .await
                    .with_context(|| format!("failed to split workspace pane {}", pane.id))?
            }
        };

        let session_id = response_id(&result, "session_id")?;
        if runtime.tab_id.is_none() {
            runtime.tab_id = optional_response_id(&result, "tab_id");
        }
        session_ids.insert(pane.id.clone(), session_id.clone());
        runtime.panes.insert(
            pane.id.clone(),
            RuntimePane {
                logical_id: pane.id.clone(),
                session_id,
                pid: result
                    .get("pid")
                    .and_then(|value| value.as_u64())
                    .and_then(|value| u32::try_from(value).ok()),
                role: pane.role.clone(),
                cwd: pane.cwd.clone(),
                command: pane.command.clone(),
                profile: pane.profile.clone(),
                model: pane.model.clone(),
                activity: PaneActivity::Starting,
                last_notification: None,
            },
        );

        if let Some(surface) = &pane.surface {
            match surface {
                SurfaceSpec::Browser { url, embedded } => {
                    if *embedded {
                        // Embedded surfaces are an explicit capability gate.
                        // The manifest can opt in, but this release refuses to
                        // silently downgrade it to a browser with shared
                        // cookies or an unmanaged WebView profile.
                        bail!(
                            "embedded browser surface for pane {} requires the native WebView2 host",
                            pane.id
                        );
                    }
                    win32::open_url_in_default_browser(url)
                        .with_context(|| format!("failed to open browser surface {url}"))?;
                    opened_external_surfaces.push(url.clone());
                }
                SurfaceSpec::File { path } => {
                    let absolute = resolve_from(&root, path);
                    Command::new("explorer.exe")
                        .arg(&absolute)
                        .spawn()
                        .with_context(|| {
                            format!("failed to open file surface {}", absolute.display())
                        })?;
                    opened_external_surfaces.push(absolute.to_string_lossy().into_owned());
                }
                SurfaceSpec::Terminal => {}
            }
        }
    }

    store.save_runtime(&mut runtime)?;
    let event = WorkspaceEvent::new(
        &runtime.workspace_id,
        "workspace.ready",
        "wta",
        Some("*".to_string()),
        json!({
            "name": runtime.name,
            "tab_id": runtime.tab_id,
            "pane_count": runtime.panes.len(),
        }),
        manifest.messaging.max_hops,
    )?;
    store.append_event(&event)?;
    channel
        .request("send_event", event.protocol_envelope()?)
        .await
        .context("workspace was created but its ready event could not be published")?;

    Ok(ApplyResult {
        workspace: runtime,
        store: store.directory().to_string_lossy().into_owned(),
        opened_external_surfaces,
    })
}

pub async fn close_workspace(channel: &dyn WtChannel, runtime: &WorkspaceRuntime) -> Result<()> {
    let mut failures = Vec::new();
    for pane in runtime.panes.values() {
        if let Err(error) = channel
            .request("close_pane", json!({ "session_id": pane.session_id }))
            .await
        {
            failures.push(format!("{}: {error:#}", pane.logical_id));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("failed to close workspace panes: {}", failures.join("; "))
    }
}

pub async fn send_to_workspace_pane(
    channel: &dyn WtChannel,
    runtime: &WorkspaceRuntime,
    target: &str,
    text: &str,
) -> Result<()> {
    let pane = runtime
        .panes
        .get(target)
        .ok_or_else(|| anyhow!("unknown logical pane id: {target}"))?;
    channel
        .request(
            "send_input",
            json!({ "session_id": pane.session_id, "text": text }),
        )
        .await?;
    Ok(())
}

pub async fn refresh_workspace_runtime(
    channel: &dyn WtChannel,
    runtime: &mut WorkspaceRuntime,
) -> usize {
    let mut refreshed = 0;
    for pane in runtime.panes.values_mut() {
        let Ok(status) = channel
            .request(
                "get_process_status",
                json!({ "session_id": pane.session_id }),
            )
            .await
        else {
            continue;
        };
        if let Some(pid) = status
            .get("pid")
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok())
        {
            pane.pid = Some(pid);
        }
        pane.activity = match status
            .get("state")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
        {
            "starting" => PaneActivity::Starting,
            "running" => PaneActivity::Working,
            "idle" => PaneActivity::Idle,
            "failed" => PaneActivity::Error,
            "exited" | "closed" => PaneActivity::Ended,
            _ => pane.activity.clone(),
        };
        refreshed += 1;
    }
    refreshed
}

pub async fn wait_for_workspace_pane(
    channel: &dyn WtChannel,
    runtime: &WorkspaceRuntime,
    target: &str,
    interval_ms: u64,
    timeout_seconds: u64,
) -> Result<serde_json::Value> {
    let pane = runtime
        .panes
        .get(target)
        .ok_or_else(|| anyhow!("unknown logical pane id: {target}"))?;
    let started = tokio::time::Instant::now();
    loop {
        let status = channel
            .request(
                "get_process_status",
                json!({ "session_id": pane.session_id }),
            )
            .await?;
        let state = status
            .get("state")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        if matches!(state, "exited" | "failed" | "closed") {
            return Ok(status);
        }
        if timeout_seconds > 0 && started.elapsed() >= Duration::from_secs(timeout_seconds) {
            bail!("timed out waiting for workspace pane {target}");
        }
        tokio::time::sleep(Duration::from_millis(interval_ms.max(50))).await;
    }
}

pub async fn run_verifier(plan: &DeclarativeWorkspacePlan) -> Result<VerificationReport> {
    let verifier = plan
        .verifier
        .as_ref()
        .ok_or_else(|| anyhow!("workspace manifest has no verifier"))?;
    let root = PathBuf::from(&plan.root);
    let candidates = if verifier.candidates.is_empty() {
        vec![(
            "workspace".to_string(),
            verifier
                .cwd
                .as_deref()
                .map(|value| resolve_from(&root, value))
                .unwrap_or_else(|| root.clone()),
        )]
    } else {
        verifier
            .candidates
            .iter()
            .map(|candidate| {
                let operation = plan
                    .operations
                    .iter()
                    .find(|operation| operation.pane().id == *candidate)
                    .ok_or_else(|| anyhow!("unknown verifier candidate pane: {candidate}"))?;
                Ok((candidate.clone(), PathBuf::from(&operation.pane().cwd)))
            })
            .collect::<Result<Vec<_>>>()?
    };
    let futures = candidates
        .into_iter()
        .map(|(candidate, cwd)| run_candidate(candidate, cwd, verifier));
    let mut results = futures::future::join_all(futures)
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    results.sort_by_key(|result| (result.duration_ms, result.candidate.clone()));
    let winner = results
        .iter()
        .find(|result| result.success)
        .map(|result| result.candidate.clone());
    let all_passed = results.iter().all(|result| result.success);
    Ok(VerificationReport {
        winner,
        all_passed,
        results,
    })
}

async fn run_candidate(
    candidate: String,
    cwd: PathBuf,
    verifier: &super::VerifierSpec,
) -> Result<VerificationResult> {
    if !cwd.is_dir() {
        bail!("verifier cwd is not a directory: {}", cwd.display());
    }

    let mut child = tokio::process::Command::new("cmd.exe");
    child
        .args(["/d", "/s", "/c", &verifier.command])
        .current_dir(&cwd)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = child
        .spawn()
        .context("failed to spawn workspace verifier")?;
    let timeout = Duration::from_secs(verifier.timeout_seconds.unwrap_or(300).max(1));
    let started = tokio::time::Instant::now();
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(output) => {
            let output = output?;
            Ok(VerificationResult {
                candidate,
                command: verifier.command.clone(),
                cwd: cwd.to_string_lossy().into_owned(),
                success: output.status.success(),
                exit_code: output.status.code(),
                timed_out: false,
                duration_ms: started.elapsed().as_millis(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
        Err(_) => Ok(VerificationResult {
            candidate,
            command: verifier.command.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            success: false,
            exit_code: None,
            timed_out: true,
            duration_ms: started.elapsed().as_millis(),
            stdout: String::new(),
            stderr: format!("verifier exceeded {} seconds", timeout.as_secs()),
        }),
    }
}

fn prepare_worktrees(plan: &DeclarativeWorkspacePlan) -> Result<()> {
    let root = Path::new(&plan.root);
    let mut claimed_paths = BTreeMap::<PathBuf, String>::new();
    for operation in &plan.operations {
        let pane = operation.pane();
        let Some(worktree) = &pane.worktree else {
            continue;
        };
        let path = Path::new(&pane.cwd);
        let normalized = path.to_path_buf();
        if let Some(owner) = claimed_paths.insert(normalized, pane.id.clone()) {
            bail!(
                "workspace panes {owner} and {} resolve to the same worktree path {}",
                pane.id,
                path.display()
            );
        }
        if path.is_dir() {
            let current_branch =
                git_output(path, &["branch", "--show-current"]).with_context(|| {
                    format!(
                        "existing worktree for pane {} is not a usable Git checkout: {}",
                        pane.id,
                        path.display()
                    )
                })?;
            if current_branch.trim() != worktree.branch {
                bail!(
                    "existing worktree for pane {} is on branch {:?}, expected {:?}",
                    pane.id,
                    current_branch.trim(),
                    worktree.branch
                );
            }
            continue;
        }
        if path.exists() {
            bail!(
                "worktree path for pane {} exists but is not a directory: {}",
                pane.id,
                path.display()
            );
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let reference = format!("refs/heads/{}", worktree.branch);
        let branch_exists = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["show-ref", "--verify", "--quiet", &reference])
            .status()
            .with_context(|| format!("failed to inspect Git branch {}", worktree.branch))?
            .success();
        if !branch_exists && !worktree.create_branch {
            bail!(
                "worktree branch {} does not exist; set create_branch: true to create it",
                worktree.branch
            );
        }
        let mut command = Command::new("git");
        command.arg("-C").arg(root).args(["worktree", "add"]);
        if !branch_exists && worktree.create_branch {
            command.args(["-b", &worktree.branch]);
        }
        let output = command
            .arg(path)
            .arg(&worktree.branch)
            .output()
            .with_context(|| format!("failed to create git worktree for pane {}", pane.id))?;
        if !output.status.success() {
            bail!(
                "git worktree add failed for pane {}: {}",
                pane.id,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(())
}

fn preflight_apply(plan: &DeclarativeWorkspacePlan) -> Result<()> {
    if plan.operations.len() != plan.pane_count {
        bail!(
            "workspace plan operation count {} does not match pane count {}",
            plan.operations.len(),
            plan.pane_count
        );
    }
    for operation in &plan.operations {
        let pane = operation.pane();
        let cwd = Path::new(&pane.cwd);
        if pane.worktree.is_none() && !cwd.is_dir() {
            bail!(
                "workspace pane {} cwd is not a directory: {}",
                pane.id,
                cwd.display()
            );
        }
        match &pane.surface {
            Some(SurfaceSpec::Browser { embedded: true, .. }) => {
                bail!(
                    "embedded browser surface for pane {} requires the native WebView2 host",
                    pane.id
                );
            }
            Some(SurfaceSpec::File { path }) => {
                let absolute = resolve_from(Path::new(&plan.root), path);
                if !absolute.exists() {
                    bail!(
                        "file surface for pane {} does not exist: {}",
                        pane.id,
                        absolute.display()
                    );
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git in {}", cwd.display()))?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn prepare_command(store: &WorkspaceStore, pane: &PaneSpec) -> Result<String> {
    let mut command = pane.command.trim().to_string();
    if command.is_empty() {
        command = "pwsh.exe -NoLogo".to_string();
    }
    if let Some(model) = pane
        .model
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let executable = command
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches('"')
            .to_ascii_lowercase();
        if ["codex", "claude", "copilot", "gemini", "opencode"]
            .iter()
            .any(|known| {
                executable.ends_with(known) || executable.ends_with(&format!("{known}.exe"))
            })
        {
            command.push_str(" --model ");
            command.push_str(&quote_cmd_argument(model));
        }
    }
    if pane.environment.is_empty() {
        return Ok(command);
    }

    let launcher_dir = store.directory().join("launchers");
    fs::create_dir_all(&launcher_dir)?;
    let launcher = launcher_dir.join(format!("{}.cmd", safe_filename(&pane.id)));
    let mut contents = String::from("@echo off\r\n");
    for (name, value) in &pane.environment {
        validate_environment_name(name)?;
        if value.contains(['\r', '\n', '"']) {
            bail!("environment value for {name} contains unsupported control or quote characters");
        }
        contents.push_str(&format!(
            "set \"{}={}\"\r\n",
            name,
            value.replace('%', "%%")
        ));
    }
    contents.push_str(&command);
    contents.push_str("\r\n");
    fs::write(&launcher, contents)
        .with_context(|| format!("failed to write pane launcher {}", launcher.display()))?;
    Ok(format!(
        "cmd.exe /d /k {}",
        quote_cmd_argument(&launcher.to_string_lossy())
    ))
}

fn validate_environment_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        || name.as_bytes()[0].is_ascii_digit()
    {
        bail!("invalid environment variable name: {name}");
    }
    Ok(())
}

fn compact_params(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(object) = value.as_object_mut() {
        object.retain(|_, value| {
            !value.is_null() && !matches!(value, serde_json::Value::String(text) if text.is_empty())
        });
    }
    value
}

fn response_id(value: &serde_json::Value, key: &str) -> Result<String> {
    optional_response_id(value, key)
        .ok_or_else(|| anyhow!("terminal response is missing {key}: {value}"))
}

fn optional_response_id(value: &serde_json::Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(serde_json::Value::String(value)) if !value.is_empty() => Some(value.clone()),
        Some(serde_json::Value::Number(value)) => Some(value.to_string()),
        _ => None,
    }
}

fn quote_cmd_argument(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn safe_filename(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn resolve_from(root: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{build_declarative_plan, WorkspaceManifest};

    #[test]
    fn embedded_browser_is_rejected_before_apply_mutations() {
        let root = std::env::temp_dir();
        let yaml = format!(
            r#"
name: browser
root: "{}"
layout:
  type: pane
  id: docs
  command: ""
  surface:
    kind: browser
    url: https://example.com/
    embedded: true
browser:
  allow_embedded: true
  allowed_hosts: [example.com]
"#,
            root.to_string_lossy().replace('\\', "/")
        );
        let manifest = WorkspaceManifest::parse_yaml(&yaml).unwrap();
        let plan = build_declarative_plan(&manifest, &root.join(".agent-workspace.yaml")).unwrap();
        let error = preflight_apply(&plan).unwrap_err().to_string();
        assert!(error.contains("native WebView2 host"));
    }
}
