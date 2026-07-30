//! Redacted, reproducible diagnostics for remote workspaces and managed agents.
//!
//! Diagnostics read the canonical ComputeStore and the verified wta-node
//! installation. They never inspect credential files or print environment
//! values, private keys, command-line secrets, or raw transfer source paths.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use super::installation;
use super::model::{BindingKind, BindingState, ProviderKind, SurfaceBinding};
use super::ssh;
use super::store::{now_ms, ComputeStore};

pub fn doctor_ssh(store: &ComputeStore, target_id: &str) -> Result<Value> {
    let target = store.get_target(target_id)?;
    if !matches!(target.provider, ProviderKind::Ssh | ProviderKind::Azure) {
        bail!("target {target_id} is not SSH-backed");
    }
    let alias = target
        .endpoint
        .ssh_alias
        .as_deref()
        .context("SSH target has no alias")?;
    ssh::validate_alias(alias)?;
    let resolved = ssh::resolve_alias(alias)?;
    let preview = ssh::preview_trust(alias)?;
    let installation = installation::from_target(&target).ok();
    let remote = installation.as_ref().map(|installation| {
        json!({
            "doctor": run_node_json(alias, &installation.active_path, &["doctor"]),
            "pty_sessions": run_node_json(alias, &installation.active_path, &["pty", "list"]),
            "acp_sessions": run_node_json(alias, &installation.active_path, &["acp", "list"]),
        })
    });
    let workspaces = store
        .list_remote_workspaces()?
        .into_iter()
        .filter(|workspace| workspace.target_id == target_id)
        .collect::<Vec<_>>();
    let bindings = store
        .list_bindings()?
        .into_iter()
        .filter(|binding| binding.home_target_id.as_deref() == Some(target_id))
        .collect::<Vec<_>>();
    Ok(json!({
        "ok": target.health == super::model::TargetHealth::Healthy && !target.disabled,
        "checked_at_ms": now_ms(),
        "target": sanitize_target(&target),
        "resolved": {
            "alias": resolved.alias,
            "hostname": resolved.hostname,
            "user": resolved.user,
            "port": resolved.port,
            "proxy_jump": resolved.proxy_jump,
            "strict_host_key_checking": resolved.effective_options.get("stricthostkeychecking"),
            "server_alive_interval": resolved.effective_options.get("serveraliveinterval"),
            "server_alive_count_max": resolved.effective_options.get("serveralivecountmax"),
        },
        "trust": preview,
        "installation": installation,
        "remote": remote,
        "remote_workspaces": workspaces,
        "bindings": bindings,
    }))
}

pub fn doctor_surface(store: &ComputeStore, binding_id: &str) -> Result<Value> {
    let binding = store.get_binding(binding_id)?;
    let target = binding
        .home_target_id
        .as_deref()
        .map(|id| store.get_target(id))
        .transpose()?;
    let leases = store
        .list_leases()?
        .into_iter()
        .filter(|lease| lease.subject_id == binding.binding_id)
        .collect::<Vec<_>>();
    let remote_workspace = store
        .list_remote_workspaces()?
        .into_iter()
        .find(|workspace| workspace.workspace_id == binding.workspace_id);
    let remote_runtime = match (target.as_ref(), binding.remote_session_id.as_deref()) {
        (Some(target), Some(session_id))
            if matches!(target.provider, ProviderKind::Ssh | ProviderKind::Azure) =>
        {
            let alias = target.endpoint.ssh_alias.as_deref();
            let installation = installation::from_target(target).ok();
            match (alias, installation) {
                (Some(alias), Some(installation)) => Some(json!({
                    "pty": run_node_json(
                        alias,
                        &installation.active_path,
                        &["pty", "status", "--session", session_id],
                    ),
                    "acp_sessions": if binding.kind == BindingKind::ManagedAgent {
                        run_node_json(alias, &installation.active_path, &["acp", "list"])
                    } else {
                        Value::Null
                    },
                })),
                _ => None,
            }
        }
        _ => None,
    };
    Ok(json!({
        "ok": binding.state != super::model::BindingState::Failed,
        "checked_at_ms": now_ms(),
        "binding": binding,
        "target": target.as_ref().map(sanitize_target),
        "remote_workspace": remote_workspace,
        "leases": leases,
        "remote_runtime": remote_runtime,
    }))
}

pub fn doctor_agent(store: &ComputeStore, agent_id: &str) -> Result<Value> {
    let bindings = store
        .list_bindings()?
        .into_iter()
        .filter(|binding| {
            binding.kind == BindingKind::ManagedAgent
                && binding.agent_id.as_deref() == Some(agent_id)
        })
        .collect::<Vec<_>>();
    if bindings.is_empty() {
        bail!("no managed agent binding found for {agent_id}");
    }
    let surfaces = bindings
        .iter()
        .map(|binding| doctor_surface(store, &binding.binding_id))
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "ok": surfaces.iter().all(|surface| surface.get("ok").and_then(Value::as_bool) == Some(true)),
        "checked_at_ms": now_ms(),
        "agent_id": agent_id,
        "surfaces": surfaces,
    }))
}

/// Reconcile one managed surface against its exact persistent remote runtime.
///
/// The binding is only marked ready when the node confirms that the recorded
/// PTY still exists. Transport failures become `Disconnected`; a terminated
/// PTY becomes `Failed`. This prevents session restore from manufacturing
/// liveness by only editing local JSON.
pub fn reconcile_surface(store: &ComputeStore, binding_id: &str) -> Result<SurfaceBinding> {
    let mut binding = store.get_binding(binding_id)?;
    if binding.kind != BindingKind::ManagedAgent {
        bail!("binding {binding_id} is not a managed agent");
    }
    let Some(target_id) = binding.home_target_id.clone() else {
        binding.state = BindingState::Ready;
        binding.updated_at_ms = now_ms();
        return store.upsert_binding("session.reconcile", binding);
    };
    let target = store.get_target(&target_id)?;
    if !matches!(target.provider, ProviderKind::Ssh | ProviderKind::Azure) {
        binding.state = if target.disabled {
            BindingState::Disconnected
        } else {
            BindingState::Ready
        };
        binding.updated_at_ms = now_ms();
        return store.upsert_binding("session.reconcile", binding);
    }
    let alias = target
        .endpoint
        .ssh_alias
        .as_deref()
        .context("managed surface target has no SSH alias")?;
    let installation = installation::from_target(&target)?;
    let session_id = binding
        .remote_session_id
        .as_deref()
        .context("managed remote surface has no persistent PTY session")?;
    let status = run_node_json(
        alias,
        &installation.active_path,
        &["pty", "status", "--session", session_id],
    );
    let remote_state = status
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default();
    binding.state = match remote_state {
        "running" | "attached" => BindingState::Ready,
        "detached" => BindingState::Detached,
        "stopping" => BindingState::Stopping,
        "stopped" | "exited" | "failed" => BindingState::Failed,
        _ => BindingState::Disconnected,
    };
    binding.updated_at_ms = now_ms();
    binding.metadata["last_reconcile"] = json!({
        "at_ms": binding.updated_at_ms,
        "remote_state": remote_state,
        "reachable": status.get("ok").and_then(Value::as_bool).unwrap_or(false)
            || !remote_state.is_empty(),
    });
    let reconciled = store.upsert_binding("session.reconcile", binding)?;
    if matches!(
        reconciled.state,
        BindingState::Ready | BindingState::Detached
    ) {
        store.heartbeat_binding_runtime("session.reconcile", &reconciled.binding_id)
    } else {
        Ok(reconciled)
    }
}

/// Stop only the exact persistent runtimes recorded by a managed binding.
pub fn stop_surface(store: &ComputeStore, binding_id: &str) -> Result<SurfaceBinding> {
    let mut binding = store.get_binding(binding_id)?;
    if binding.kind != BindingKind::ManagedAgent {
        bail!("binding {binding_id} is not a managed agent");
    }
    binding.state = BindingState::Stopping;
    binding.updated_at_ms = now_ms();
    store.upsert_binding("session.stop_requested", binding.clone())?;

    if let Some(target_id) = binding.home_target_id.as_deref() {
        let target = store.get_target(target_id)?;
        if matches!(target.provider, ProviderKind::Ssh | ProviderKind::Azure) {
            let alias = target
                .endpoint
                .ssh_alias
                .as_deref()
                .context("managed surface target has no SSH alias")?;
            let installation = installation::from_target(&target)?;
            if let Some(session_id) = binding.remote_session_id.as_deref() {
                let result = run_node_json(
                    alias,
                    &installation.active_path,
                    &["pty", "stop", "--session", session_id],
                );
                if result.get("ok").and_then(Value::as_bool) == Some(false) {
                    binding.state = BindingState::Failed;
                    binding.updated_at_ms = now_ms();
                    binding.metadata["last_stop"] = result;
                    return store.upsert_binding("session.stop_failed", binding);
                }
            }
            if let Some(session_id) = binding.acp_session_id.as_deref() {
                let _ = run_node_json(
                    alias,
                    &installation.active_path,
                    &["acp", "stop", "--session", session_id],
                );
            }
        }
    }

    binding.state = BindingState::Stopped;
    binding.updated_at_ms = now_ms();
    store.upsert_binding("session.stopped", binding)
}

pub fn export_redacted(store: &ComputeStore, output: &Path) -> Result<Value> {
    let targets = store
        .list_targets()?
        .iter()
        .map(sanitize_target)
        .collect::<Vec<_>>();
    let mut transfers = serde_json::to_value(store.list_transfers()?)?;
    redact_value(&mut transfers);
    let mut events = serde_json::to_value(store.events()?)?;
    redact_value(&mut events);
    let mut bundle = json!({
        "schema_version": 1,
        "redacted": true,
        "created_at_ms": now_ms(),
        "targets": targets,
        "remote_workspaces": store.list_remote_workspaces()?,
        "bindings": store.list_bindings()?,
        "leases": store.list_leases()?,
        "jobs": store.list_jobs()?,
        "transfers": transfers,
        "events": events,
    });
    redact_value(&mut bundle);
    write_json_atomic(output, &bundle)?;
    Ok(json!({
        "ok": true,
        "redacted": true,
        "output": output,
        "bytes": fs::metadata(output)?.len(),
    }))
}

fn run_node_json(alias: &str, active_path: &str, args: &[&str]) -> Value {
    let ssh_exe = match ssh::find_ssh_executable() {
        Ok(path) => path,
        Err(error) => return json!({"ok": false, "error": error.to_string()}),
    };
    let active = format!("$HOME/{active_path}");
    let output = Command::new(ssh_exe)
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=yes",
            alias,
            "--",
            &active,
        ])
        .args(args)
        .output();
    match output {
        Ok(output) if output.status.success() => serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| {
                json!({
                    "ok": false,
                    "error": "remote node returned invalid JSON",
                })
            }),
        Ok(output) => json!({
            "ok": false,
            "exit_code": output.status.code(),
            "error": String::from_utf8_lossy(&output.stderr).trim(),
        }),
        Err(error) => json!({"ok": false, "error": error.to_string()}),
    }
}

fn sanitize_target(target: &super::model::ComputeTarget) -> Value {
    json!({
        "schema_version": target.schema_version,
        "id": target.id,
        "display_name": target.display_name,
        "provider": target.provider,
        "endpoint": {
            "ssh_alias": target.endpoint.ssh_alias,
            "wsl_distro": target.endpoint.wsl_distro,
            "azure_resource_id": target.endpoint.azure_resource_id,
        },
        "os": target.os,
        "arch": target.arch,
        "capabilities": target.capabilities,
        "trust_tier": target.trust_tier,
        "agent_slots": target.agent_slots,
        "build_slots": target.build_slots,
        "memory_bytes": target.memory_bytes,
        "health": target.health,
        "last_probe_at_ms": target.last_probe_at_ms,
        "disabled": target.disabled,
    })
}

fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let keys = map.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                let normalized = key.to_ascii_lowercase();
                if should_redact(&normalized) {
                    map.insert(key, Value::String("[redacted]".into()));
                } else if let Some(child) = map.get_mut(&key) {
                    redact_value(child);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_value(value);
            }
        }
        _ => {}
    }
}

fn should_redact(key: &str) -> bool {
    [
        "token",
        "password",
        "secret",
        "credential",
        "private_key",
        "identity_file",
        "source_path",
        "environment",
    ]
    .iter()
    .any(|candidate| key.contains(candidate))
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = PathBuf::from(format!("{}.{}.tmp", path.display(), Uuid::new_v4()));
    {
        let mut file = fs::File::create(&temp)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)
        .with_context(|| format!("could not atomically publish evidence {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::compute::model::{
        ComputeTarget, SurfaceBinding, TargetEndpoint, TargetHealth, TrustTier,
        COMPUTE_SCHEMA_VERSION,
    };

    #[test]
    fn redaction_removes_nested_credentials_and_source_paths() {
        let mut value = json!({
            "token": "abc",
            "nested": {
                "source_path": "C:\\secret\\file.txt",
                "safe": "value",
            }
        });
        redact_value(&mut value);
        assert_eq!(value["token"], "[redacted]");
        assert_eq!(value["nested"]["source_path"], "[redacted]");
        assert_eq!(value["nested"]["safe"], "value");
    }

    #[test]
    fn local_surface_reconcile_does_not_manufacture_a_remote_runtime() {
        let root = std::env::temp_dir().join(format!("wta-doctor-{}", Uuid::new_v4()));
        let store = ComputeStore::at(root.clone()).unwrap();
        store
            .upsert_target(
                "test",
                ComputeTarget {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    id: "local".into(),
                    display_name: "Local".into(),
                    provider: ProviderKind::Local,
                    endpoint: TargetEndpoint::default(),
                    os: "windows".into(),
                    arch: "x86_64".into(),
                    capabilities: vec!["codex".into()],
                    toolchains: BTreeMap::new(),
                    trust_tier: TrustTier::Personal,
                    project_allowlist: Vec::new(),
                    agent_slots: 2,
                    build_slots: 2,
                    memory_bytes: 1,
                    cost_policy: Value::Null,
                    power_policy: Value::Null,
                    health: TargetHealth::Healthy,
                    last_probe_at_ms: None,
                    disabled: false,
                    metadata: Value::Null,
                },
            )
            .unwrap();
        store
            .upsert_binding(
                "test",
                SurfaceBinding {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    binding_id: "binding-local".into(),
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
                    created_at_ms: 1,
                    updated_at_ms: 1,
                    metadata: Value::Null,
                },
            )
            .unwrap();

        let reconciled = reconcile_surface(&store, "binding-local").unwrap();
        assert_eq!(reconciled.state, BindingState::Ready);
        assert!(reconciled.remote_session_id.is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
