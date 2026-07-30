//! Runtime restoration companion to Windows Terminal's native layout store.
//!
//! Windows Terminal remains authoritative for tabs, panes and commandlines.
//! WTA persists only the runtime identities required to reconnect remote
//! workspaces, managed ACP sessions and isolated browser controllers.

use anyhow::{bail, Result};
use uuid::Uuid;

use super::browser;
use super::diagnostics;
use super::model::*;
use super::store::{now_ms, terminal_identity_eq, ComputeStore};

pub fn capture(
    store: &ComputeStore,
    window_id: &str,
    workspace_id: &str,
    focused_surface_id: Option<String>,
) -> Result<RuntimeRestoreSnapshot> {
    let remote_workspaces = store
        .list_remote_workspaces()?
        .into_iter()
        .filter(|workspace| {
            terminal_identity_eq(&workspace.window_id, window_id)
                && terminal_identity_eq(&workspace.workspace_id, workspace_id)
                && workspace.state != RemoteWorkspaceState::Closed
        })
        .collect::<Vec<_>>();
    let remote_workspace_ids = remote_workspaces
        .iter()
        .map(|workspace| workspace.remote_workspace_id.clone())
        .collect();
    let bindings = store
        .list_bindings()?
        .into_iter()
        .filter(|binding| {
            terminal_identity_eq(&binding.window_id, window_id)
                && terminal_identity_eq(&binding.workspace_id, workspace_id)
                && binding.state != BindingState::Stopped
        })
        .collect::<Vec<_>>();
    let binding_ids = bindings
        .iter()
        .map(|binding| binding.binding_id.clone())
        .collect();
    let browser_surface_ids = store
        .list_browsers()?
        .into_iter()
        .filter(|browser| {
            terminal_identity_eq(&browser.workspace_id, workspace_id)
                && browser.state != BrowserSurfaceState::Closed
        })
        .map(|browser| browser.browser_surface_id)
        .collect();
    let mut runtime_references = remote_workspaces
        .iter()
        .filter_map(|workspace| {
            Some(RuntimeRestoreReference {
                environment_id: workspace.environment_id.clone()?,
                target_id: workspace.target_id.clone(),
                binding_id: None,
                preferred_endpoint_kind: workspace
                    .preferred_endpoint_kind
                    .unwrap_or(AccessEndpointKind::SshForward),
                runtime_session_id: workspace.transport_session_id.clone(),
            })
        })
        .chain(bindings.iter().filter_map(|binding| {
            Some(RuntimeRestoreReference {
                environment_id: binding.environment_id.clone()?,
                target_id: binding.home_target_id.clone()?,
                binding_id: Some(binding.binding_id.clone()),
                preferred_endpoint_kind: binding
                    .preferred_endpoint_kind
                    .unwrap_or(AccessEndpointKind::SshForward),
                runtime_session_id: binding.remote_session_id.clone(),
            })
        }))
        .collect::<Vec<_>>();
    runtime_references.sort_by(|left, right| {
        left.environment_id
            .cmp(&right.environment_id)
            .then_with(|| left.binding_id.cmp(&right.binding_id))
    });
    runtime_references.dedup();

    store.save_restore_snapshot(
        "restore.capture",
        RuntimeRestoreSnapshot {
            schema_version: COMPUTE_SCHEMA_VERSION,
            restore_id: format!("restore-{}", Uuid::new_v4()),
            window_id: window_id.to_string(),
            workspace_id: workspace_id.to_string(),
            focused_surface_id,
            remote_workspace_ids,
            binding_ids,
            browser_surface_ids,
            runtime_references,
            captured_at_ms: now_ms(),
        },
    )
}

pub fn plan(store: &ComputeStore, restore_id: &str) -> Result<RuntimeRestorePlan> {
    let snapshot = store.get_restore_snapshot(restore_id)?;
    let mut items = vec![RestorePlanItem {
        action: RestoreAction::RestoreNativeLayout,
        entity_id: snapshot.window_id.clone(),
        state: RestoreItemState::RequiresNativeUi,
        detail: "Windows Terminal ApplicationState restores tabs, panes and commandlines".into(),
    }];

    for reference in &snapshot.runtime_references {
        match store.get_environment(&reference.environment_id) {
            Ok(environment) if environment.target_id != reference.target_id => {
                items.push(failed(
                    RestoreAction::ReconnectEnvironment,
                    &reference.environment_id,
                    "restore target does not match the execution environment",
                ));
            }
            Ok(environment)
                if matches!(
                    environment.lifecycle_state,
                    EnvironmentLifecycleState::Retired
                        | EnvironmentLifecycleState::VersionBlocked
                        | EnvironmentLifecycleState::Failed
                ) =>
            {
                items.push(failed(
                    RestoreAction::ReconnectEnvironment,
                    &reference.environment_id,
                    &format!(
                        "execution environment is not restorable: {:?}",
                        environment.lifecycle_state
                    ),
                ));
            }
            Ok(_) => {
                let endpoint = store
                    .list_endpoints(Some(&reference.environment_id))?
                    .into_iter()
                    .find(|endpoint| {
                        endpoint.enabled && endpoint.kind == reference.preferred_endpoint_kind
                    });
                if endpoint.is_none() {
                    items.push(failed(
                        RestoreAction::ReconnectEnvironment,
                        &reference.environment_id,
                        "preferred endpoint kind is unavailable",
                    ));
                    continue;
                }
                match store.get_connection_supervisor(&reference.environment_id) {
                    Ok(supervisor) if supervisor.state == EnvironmentConnectionState::Connected => {
                        items.push(RestorePlanItem {
                            action: RestoreAction::ReconnectEnvironment,
                            entity_id: reference.environment_id.clone(),
                            state: RestoreItemState::Ready,
                            detail: "execution environment connection is ready".into(),
                        });
                    }
                    _ => items.push(RestorePlanItem {
                        action: RestoreAction::ReconnectEnvironment,
                        entity_id: reference.environment_id.clone(),
                        state: RestoreItemState::Planned,
                        detail:
                            "prepare the canonical environment supervisor before runtime reattach"
                                .into(),
                    }),
                }
            }
            Err(error) => items.push(failed(
                RestoreAction::ReconnectEnvironment,
                &reference.environment_id,
                &format!("execution environment record is unavailable: {error:#}"),
            )),
        }
    }

    for id in &snapshot.remote_workspace_ids {
        match store.get_remote_workspace(id) {
            Ok(workspace) if workspace.state == RemoteWorkspaceState::Ready => {
                items.push(RestorePlanItem {
                    action: RestoreAction::ReconnectRemoteWorkspace,
                    entity_id: id.clone(),
                    state: RestoreItemState::Ready,
                    detail: "remote workspace runtime is ready".into(),
                });
            }
            Ok(workspace)
                if !matches!(
                    workspace.state,
                    RemoteWorkspaceState::Closing | RemoteWorkspaceState::Closed
                ) =>
            {
                items.push(RestorePlanItem {
                    action: RestoreAction::ReconnectRemoteWorkspace,
                    entity_id: id.clone(),
                    state: RestoreItemState::Planned,
                    detail: format!(
                        "remote workspace requires reconnect from {:?}",
                        workspace.state
                    ),
                });
            }
            Ok(_) => items.push(skipped(
                RestoreAction::ReconnectRemoteWorkspace,
                id,
                "remote workspace is closing or closed",
            )),
            Err(error) => items.push(failed(
                RestoreAction::ReconnectRemoteWorkspace,
                id,
                &format!("remote workspace record is unavailable: {error:#}"),
            )),
        }
    }

    for id in &snapshot.binding_ids {
        match store.get_binding(id) {
            Ok(binding) if binding.kind == BindingKind::PlainTerminal => {
                items.push(skipped(
                    RestoreAction::ReattachManagedAgent,
                    id,
                    "plain terminal lifecycle belongs to the native layout",
                ));
            }
            Ok(binding)
                if matches!(binding.state, BindingState::Stopped | BindingState::Failed) =>
            {
                items.push(skipped(
                    RestoreAction::ReattachManagedAgent,
                    id,
                    "managed runtime is terminal and requires an explicit restart",
                ));
            }
            Ok(binding) => items.push(RestorePlanItem {
                action: RestoreAction::ReattachManagedAgent,
                entity_id: id.clone(),
                state: RestoreItemState::Planned,
                detail: if binding.remote_session_id.is_some() {
                    "reattach the existing surface-owned ACP runtime".into()
                } else {
                    "reconcile binding before native surface reconnect".into()
                },
            }),
            Err(error) => items.push(failed(
                RestoreAction::ReattachManagedAgent,
                id,
                &format!("binding record is unavailable: {error:#}"),
            )),
        }
    }

    for id in &snapshot.browser_surface_ids {
        match store.get_browser(id) {
            Ok(browser) => match store.get_proxy(&browser.proxy_id) {
                Ok(proxy) if proxy.state == RemoteProxyState::Ready => {
                    items.push(RestorePlanItem {
                        action: RestoreAction::RecreateBrowserController,
                        entity_id: id.clone(),
                        state: RestoreItemState::RequiresNativeUi,
                        detail: "reuse the isolated profile and ready surface proxy".into(),
                    });
                }
                Ok(_) => items.push(RestorePlanItem {
                    action: RestoreAction::RestartBrowserProxy,
                    entity_id: id.clone(),
                    state: RestoreItemState::Planned,
                    detail: "restart the surface-scoped proxy before recreating WebView2".into(),
                }),
                Err(error) => items.push(failed(
                    RestoreAction::RestartBrowserProxy,
                    id,
                    &format!("browser proxy record is unavailable: {error:#}"),
                )),
            },
            Err(error) => items.push(failed(
                RestoreAction::RecreateBrowserController,
                id,
                &format!("browser record is unavailable: {error:#}"),
            )),
        }
    }

    if let Some(surface_id) = snapshot.focused_surface_id {
        items.push(RestorePlanItem {
            action: RestoreAction::RestoreFocus,
            entity_id: surface_id,
            state: RestoreItemState::RequiresNativeUi,
            detail: "focus is applied after native layout and runtimes exist".into(),
        });
    }

    Ok(RuntimeRestorePlan {
        schema_version: COMPUTE_SCHEMA_VERSION,
        restore_id: snapshot.restore_id,
        window_id: snapshot.window_id,
        workspace_id: snapshot.workspace_id,
        items,
        generated_at_ms: now_ms(),
    })
}

pub fn apply(
    store: &ComputeStore,
    restore_id: &str,
    allow_production: bool,
) -> Result<RuntimeRestorePlan> {
    let mut restore_plan = plan(store, restore_id)?;
    for item in &mut restore_plan.items {
        match item.action {
            RestoreAction::ReconnectEnvironment if item.state == RestoreItemState::Planned => {
                let snapshot = store.get_restore_snapshot(restore_id)?;
                let reference = snapshot
                    .runtime_references
                    .iter()
                    .find(|reference| reference.environment_id == item.entity_id);
                match reference {
                    Some(reference) => match super::connection::ensure_supervisor(
                        store,
                        &reference.environment_id,
                        reference.preferred_endpoint_kind,
                    ) {
                        Ok(_) => {
                            item.state = RestoreItemState::RequiresNativeUi;
                            item.detail =
                                "supervisor prepared; native transport must connect the preferred endpoint"
                                    .into();
                        }
                        Err(error) => {
                            item.state = RestoreItemState::Failed;
                            item.detail =
                                format!("environment supervisor prepare failed: {error:#}");
                        }
                    },
                    None => {
                        item.state = RestoreItemState::Failed;
                        item.detail = "restore reference disappeared".into();
                    }
                }
            }
            RestoreAction::ReattachManagedAgent if item.state == RestoreItemState::Planned => {
                match diagnostics::reconcile_surface(store, &item.entity_id) {
                    Ok(binding)
                        if matches!(
                            binding.state,
                            BindingState::Ready
                                | BindingState::Detached
                                | BindingState::Reconnecting
                        ) =>
                    {
                        item.state = RestoreItemState::RequiresNativeUi;
                        item.detail =
                            "runtime reconciled; native surface must attach its transport".into();
                    }
                    Ok(binding) => {
                        item.state = RestoreItemState::Failed;
                        item.detail = format!("binding reconciled to {:?}", binding.state);
                    }
                    Err(error) => {
                        item.state = RestoreItemState::Failed;
                        item.detail = format!("binding reconcile failed: {error:#}");
                    }
                }
            }
            RestoreAction::RestartBrowserProxy if item.state == RestoreItemState::Planned => {
                match browser::recover(store, &item.entity_id, allow_production) {
                    Ok(_) => {
                        item.action = RestoreAction::RecreateBrowserController;
                        item.state = RestoreItemState::RequiresNativeUi;
                        item.detail =
                            "proxy restarted; native host must recreate the WebView2 controller"
                                .into();
                    }
                    Err(error) => {
                        item.state = RestoreItemState::Failed;
                        item.detail = format!("browser proxy recovery failed: {error:#}");
                    }
                }
            }
            RestoreAction::ReconnectRemoteWorkspace if item.state == RestoreItemState::Planned => {
                // Reconnect remains transport-owned: silently declaring it
                // applied here would duplicate the SSH supervisor.
                item.state = RestoreItemState::RequiresNativeUi;
                item.detail =
                    "native workspace transport must invoke the verified reconnect supervisor"
                        .into();
            }
            _ => {}
        }
    }
    restore_plan.generated_at_ms = now_ms();
    Ok(restore_plan)
}

pub fn latest_plan(
    store: &ComputeStore,
    window_id: &str,
    workspace_id: &str,
) -> Result<Option<RuntimeRestorePlan>> {
    store
        .latest_restore_snapshot(window_id, workspace_id)?
        .map(|snapshot| plan(store, &snapshot.restore_id))
        .transpose()
}

fn skipped(action: RestoreAction, id: &str, detail: &str) -> RestorePlanItem {
    RestorePlanItem {
        action,
        entity_id: id.to_string(),
        state: RestoreItemState::Skipped,
        detail: detail.to_string(),
    }
}

fn failed(action: RestoreAction, id: &str, detail: &str) -> RestorePlanItem {
    RestorePlanItem {
        action,
        entity_id: id.to_string(),
        state: RestoreItemState::Failed,
        detail: detail.to_string(),
    }
}

pub fn require_restorable(plan: &RuntimeRestorePlan) -> Result<()> {
    if plan
        .items
        .iter()
        .any(|item| item.state == RestoreItemState::Failed)
    {
        bail!("restore plan contains failed items");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_runtime_snapshot_keeps_native_layout_and_focus_authoritative() {
        let root = std::env::temp_dir().join(format!("wta-restore-test-{}", Uuid::new_v4()));
        let store = ComputeStore::at(root).unwrap();
        let snapshot =
            capture(&store, "window-1", "workspace-1", Some("surface-1".into())).unwrap();
        let plan = plan(&store, &snapshot.restore_id).unwrap();
        assert_eq!(plan.items.len(), 2);
        assert_eq!(plan.items[0].action, RestoreAction::RestoreNativeLayout);
        assert_eq!(plan.items[0].state, RestoreItemState::RequiresNativeUi);
        assert_eq!(plan.items[1].action, RestoreAction::RestoreFocus);
        require_restorable(&plan).unwrap();
    }

    #[test]
    fn restore_persists_stable_environment_and_runtime_ids_only() {
        let root =
            std::env::temp_dir().join(format!("wta-restore-runtime-test-{}", Uuid::new_v4()));
        let store = ComputeStore::at(root).unwrap();
        store
            .upsert_target(
                "test",
                ComputeTarget {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    id: "ssh-dev".into(),
                    display_name: "SSH dev".into(),
                    provider: ProviderKind::Ssh,
                    endpoint: TargetEndpoint {
                        ssh_alias: Some("dev".into()),
                        ..Default::default()
                    },
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    capabilities: vec![],
                    toolchains: Default::default(),
                    trust_tier: TrustTier::Development,
                    project_allowlist: vec![],
                    agent_slots: 1,
                    build_slots: 1,
                    memory_bytes: 0,
                    cost_policy: json!({}),
                    power_policy: json!({}),
                    health: TargetHealth::Healthy,
                    last_probe_at_ms: Some(now_ms()),
                    disabled: false,
                    metadata: json!({}),
                },
            )
            .unwrap();
        store
            .save_environment(
                "test",
                ExecutionEnvironment {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    environment_id: "environment-node-1".into(),
                    target_id: "ssh-dev".into(),
                    runtime_version: "1.0.0".into(),
                    protocol_version: COMPUTE_PROTOCOL_VERSION,
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    capabilities: vec![],
                    lifecycle_state: EnvironmentLifecycleState::Ready,
                    launch_method: LaunchMethod::SshManaged,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    metadata: json!({}),
                },
            )
            .unwrap();
        store
            .save_endpoint(
                "test",
                AccessEndpoint {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    endpoint_id: "endpoint-node-1-ssh".into(),
                    environment_id: "environment-node-1".into(),
                    kind: AccessEndpointKind::SshForward,
                    reachability: EndpointReachability::SshRequired,
                    health: EndpointHealth::Healthy,
                    priority: 10,
                    enabled: true,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    metadata: json!({}),
                },
            )
            .unwrap();
        store
            .upsert_remote_workspace(
                "test",
                RemoteWorkspaceSession {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    remote_workspace_id: "remote-1".into(),
                    window_id: "window-1".into(),
                    workspace_id: "workspace-1".into(),
                    target_id: "ssh-dev".into(),
                    environment_id: Some("environment-node-1".into()),
                    preferred_endpoint_kind: Some(AccessEndpointKind::SshForward),
                    state: RemoteWorkspaceState::Ready,
                    reconnect_policy: ReconnectPolicy::default(),
                    reconnect_attempt: 0,
                    transport_session_id: Some("transport-session-1".into()),
                    node_id: Some("node-1".into()),
                    last_error: None,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    metadata: json!({}),
                },
            )
            .unwrap();

        let snapshot = capture(&store, "window-1", "workspace-1", None).unwrap();
        assert_eq!(snapshot.runtime_references.len(), 1);
        assert_eq!(
            snapshot.runtime_references[0].environment_id,
            "environment-node-1"
        );
        assert_eq!(
            snapshot.runtime_references[0].runtime_session_id.as_deref(),
            Some("transport-session-1")
        );
        let serialized = serde_json::to_value(snapshot).unwrap();
        let text = serde_json::to_string(&serialized).unwrap();
        for forbidden in [
            "forwarded_port",
            "worker_pid",
            "ssh_pid",
            "tunnel_path",
            "auth",
        ] {
            assert!(!text.contains(forbidden), "{forbidden} leaked into restore");
        }
    }
}
