//! Canonical environment connection supervision.
//!
//! There is exactly one persisted supervisor record per execution
//! environment. Browser, file, PTY and agent transports request an attempt
//! through this module instead of inventing their own retry identity. SSH
//! stdio remains the bootstrap/fallback transport in this milestone.

use anyhow::{bail, Context, Result};

use super::model::*;
use super::store::{now_ms, ComputeStore};

const RETRY_DELAYS_SECONDS: &[u64] = &[3, 6, 12, 24, 48, 60];

#[derive(Debug, Clone)]
pub struct ConnectionPermit {
    pub environment: ExecutionEnvironment,
    pub endpoint: AccessEndpoint,
    pub supervisor: EnvironmentConnectionSupervisor,
}

pub fn environment_for_target(
    store: &ComputeStore,
    target_id: &str,
) -> Result<ExecutionEnvironment> {
    let mut environments = store
        .list_environments()?
        .into_iter()
        .filter(|environment| {
            environment.target_id == target_id
                && environment.lifecycle_state != EnvironmentLifecycleState::Retired
        })
        .collect::<Vec<_>>();
    environments.sort_by_key(|environment| std::cmp::Reverse(environment.updated_at_ms));
    environments
        .into_iter()
        .next()
        .with_context(|| format!("target {target_id} has no execution environment"))
}

pub fn ensure_supervisor(
    store: &ComputeStore,
    environment_id: &str,
    preferred_endpoint_kind: AccessEndpointKind,
) -> Result<EnvironmentConnectionSupervisor> {
    match store.get_connection_supervisor(environment_id) {
        Ok(supervisor) => Ok(supervisor),
        Err(_) => store.save_connection_supervisor(
            "connection.ensure",
            EnvironmentConnectionSupervisor {
                schema_version: COMPUTE_SCHEMA_VERSION,
                environment_id: environment_id.to_string(),
                state: EnvironmentConnectionState::Disconnected,
                preferred_endpoint_kind,
                current_endpoint_id: None,
                retry_attempt: 0,
                backoff_seconds: 0,
                next_retry_at_ms: None,
                connected_at_ms: None,
                last_error: None,
                generation: 0,
                created_at_ms: 0,
                updated_at_ms: 0,
            },
        ),
    }
}

pub fn begin_for_target(
    store: &ComputeStore,
    target_id: &str,
    preferred_endpoint_kind: Option<AccessEndpointKind>,
) -> Result<ConnectionPermit> {
    let environment = environment_for_target(store, target_id)?;
    if !matches!(
        environment.lifecycle_state,
        EnvironmentLifecycleState::Ready | EnvironmentLifecycleState::Degraded
    ) {
        bail!(
            "execution environment {} is not connectable: {:?}",
            environment.environment_id,
            environment.lifecycle_state
        );
    }
    let preferred = preferred_endpoint_kind.unwrap_or(AccessEndpointKind::SshForward);
    let mut supervisor = ensure_supervisor(store, &environment.environment_id, preferred)?;
    if matches!(
        supervisor.state,
        EnvironmentConnectionState::AuthBlocked
            | EnvironmentConnectionState::VersionBlocked
            | EnvironmentConnectionState::Failed
    ) {
        bail!(
            "environment connection {} is blocked: {:?}",
            environment.environment_id,
            supervisor.state
        );
    }
    if supervisor
        .next_retry_at_ms
        .is_some_and(|deadline| deadline > now_ms())
    {
        bail!(
            "environment connection {} is backing off until {}",
            environment.environment_id,
            supervisor.next_retry_at_ms.unwrap_or_default()
        );
    }

    let endpoint = select_endpoint(store, &environment.environment_id, preferred)?;
    if supervisor.state == EnvironmentConnectionState::Connected
        && supervisor.current_endpoint_id.as_deref() == Some(&endpoint.endpoint_id)
    {
        return Ok(ConnectionPermit {
            environment,
            endpoint,
            supervisor,
        });
    }
    supervisor.preferred_endpoint_kind = preferred;
    supervisor.current_endpoint_id = Some(endpoint.endpoint_id.clone());
    supervisor.state = if supervisor.retry_attempt == 0 {
        EnvironmentConnectionState::Connecting
    } else {
        EnvironmentConnectionState::Reconnecting
    };
    supervisor.generation = supervisor.generation.saturating_add(1);
    supervisor.next_retry_at_ms = None;
    supervisor.last_error = None;
    let supervisor = store.save_connection_supervisor("connection.begin", supervisor)?;
    Ok(ConnectionPermit {
        environment,
        endpoint,
        supervisor,
    })
}

pub fn mark_authenticating(
    store: &ComputeStore,
    environment_id: &str,
) -> Result<EnvironmentConnectionSupervisor> {
    transition(
        store,
        environment_id,
        EnvironmentConnectionState::Authenticating,
        None,
    )
}

pub fn mark_synchronizing(
    store: &ComputeStore,
    environment_id: &str,
) -> Result<EnvironmentConnectionSupervisor> {
    transition(
        store,
        environment_id,
        EnvironmentConnectionState::Synchronizing,
        None,
    )
}

pub fn mark_connected(
    store: &ComputeStore,
    environment_id: &str,
) -> Result<EnvironmentConnectionSupervisor> {
    let mut supervisor = store.get_connection_supervisor(environment_id)?;
    supervisor.state = EnvironmentConnectionState::Connected;
    supervisor.retry_attempt = 0;
    supervisor.backoff_seconds = 0;
    supervisor.next_retry_at_ms = None;
    supervisor.connected_at_ms = Some(now_ms());
    supervisor.last_error = None;
    store.save_connection_supervisor("connection.connected", supervisor)
}

pub fn mark_failure(
    store: &ComputeStore,
    environment_id: &str,
    state: EnvironmentConnectionState,
    error: impl Into<String>,
) -> Result<EnvironmentConnectionSupervisor> {
    if !matches!(
        state,
        EnvironmentConnectionState::Offline
            | EnvironmentConnectionState::AuthBlocked
            | EnvironmentConnectionState::VersionBlocked
            | EnvironmentConnectionState::Failed
    ) {
        bail!("invalid terminal connection failure state: {state:?}");
    }
    let mut supervisor = store.get_connection_supervisor(environment_id)?;
    supervisor.state = state;
    supervisor.last_error = Some(error.into());
    supervisor.connected_at_ms = None;
    if matches!(
        state,
        EnvironmentConnectionState::AuthBlocked
            | EnvironmentConnectionState::VersionBlocked
            | EnvironmentConnectionState::Failed
    ) {
        supervisor.next_retry_at_ms = None;
        supervisor.backoff_seconds = 0;
    } else {
        supervisor.retry_attempt = supervisor.retry_attempt.saturating_add(1);
        let index = supervisor.retry_attempt.saturating_sub(1) as usize;
        supervisor.backoff_seconds = RETRY_DELAYS_SECONDS
            .get(index)
            .copied()
            .unwrap_or(*RETRY_DELAYS_SECONDS.last().unwrap_or(&60));
        supervisor.next_retry_at_ms =
            Some(now_ms().saturating_add(supervisor.backoff_seconds * 1000));
    }
    store.save_connection_supervisor("connection.failed", supervisor)
}

pub fn disconnect(
    store: &ComputeStore,
    environment_id: &str,
) -> Result<EnvironmentConnectionSupervisor> {
    let mut supervisor = store.get_connection_supervisor(environment_id)?;
    supervisor.state = EnvironmentConnectionState::Disconnected;
    supervisor.current_endpoint_id = None;
    supervisor.retry_attempt = 0;
    supervisor.backoff_seconds = 0;
    supervisor.next_retry_at_ms = None;
    supervisor.connected_at_ms = None;
    supervisor.last_error = None;
    store.save_connection_supervisor("connection.disconnect", supervisor)
}

fn transition(
    store: &ComputeStore,
    environment_id: &str,
    state: EnvironmentConnectionState,
    error: Option<String>,
) -> Result<EnvironmentConnectionSupervisor> {
    let mut supervisor = store.get_connection_supervisor(environment_id)?;
    if !transition_allowed(supervisor.state, state) {
        bail!(
            "invalid environment connection transition {:?} -> {:?}",
            supervisor.state,
            state
        );
    }
    supervisor.state = state;
    supervisor.last_error = error;
    store.save_connection_supervisor("connection.transition", supervisor)
}

fn select_endpoint(
    store: &ComputeStore,
    environment_id: &str,
    preferred: AccessEndpointKind,
) -> Result<AccessEndpoint> {
    let mut endpoints = store
        .list_endpoints(Some(environment_id))?
        .into_iter()
        .filter(|endpoint| {
            endpoint.enabled
                && matches!(
                    endpoint.kind,
                    AccessEndpointKind::SshForward | AccessEndpointKind::PrivateNetwork
                )
                && matches!(
                    endpoint.health,
                    EndpointHealth::Healthy | EndpointHealth::Degraded
                )
        })
        .collect::<Vec<_>>();
    endpoints.sort_by_key(|endpoint| {
        (
            endpoint.kind != preferred,
            endpoint.priority,
            endpoint.endpoint_id.clone(),
        )
    });
    endpoints.into_iter().next().with_context(|| {
        format!("environment {environment_id} has no healthy supported access endpoint")
    })
}

fn transition_allowed(from: EnvironmentConnectionState, to: EnvironmentConnectionState) -> bool {
    use EnvironmentConnectionState::*;
    matches!(
        (from, to),
        (Connecting | Reconnecting, Authenticating)
            | (Authenticating, Synchronizing)
            | (Synchronizing, Connected)
            | (Connected, Synchronizing)
    ) || from == to
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    fn fixture() -> (ComputeStore, String) {
        let store = ComputeStore::at(
            std::env::temp_dir().join(format!("wta-connection-{}", Uuid::new_v4())),
        )
        .unwrap();
        let target_id = "ssh-dev".to_string();
        store
            .upsert_target(
                "test",
                ComputeTarget {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    id: target_id.clone(),
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
        let environment_id = "environment-node-1".to_string();
        store
            .save_environment(
                "test",
                ExecutionEnvironment {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    environment_id: environment_id.clone(),
                    target_id,
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
                    environment_id: environment_id.clone(),
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
        (store, environment_id)
    }

    #[test]
    fn one_supervisor_owns_environment_state_and_backoff() {
        let (store, environment_id) = fixture();
        let permit = begin_for_target(&store, "ssh-dev", None).unwrap();
        assert_eq!(permit.environment.environment_id, environment_id);
        assert_eq!(permit.supervisor.generation, 1);
        mark_authenticating(&store, &environment_id).unwrap();
        mark_synchronizing(&store, &environment_id).unwrap();
        mark_connected(&store, &environment_id).unwrap();
        let supervisor = mark_failure(
            &store,
            &environment_id,
            EnvironmentConnectionState::Offline,
            "network unavailable",
        )
        .unwrap();
        assert_eq!(supervisor.retry_attempt, 1);
        assert_eq!(supervisor.backoff_seconds, 3);
        assert!(supervisor.next_retry_at_ms.is_some());
        assert_eq!(store.list_connection_supervisors().unwrap().len(), 1);
    }

    #[test]
    fn future_public_endpoint_contracts_are_disabled_fail_closed() {
        let (store, environment_id) = fixture();
        let error = store
            .save_endpoint(
                "test",
                AccessEndpoint {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    endpoint_id: "endpoint-public".into(),
                    environment_id,
                    kind: AccessEndpointKind::AuthenticatedWss,
                    reachability: EndpointReachability::PublicAuthenticated,
                    health: EndpointHealth::Healthy,
                    priority: 1,
                    enabled: true,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    metadata: json!({}),
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("future contract"));
    }
}
