//! Workspace-scoped browser traffic proxy over OpenSSH dynamic forwarding.
//!
//! The browser-facing endpoint is loopback-only. A small detached WTA worker
//! owns the exact `ssh` child and observes a store-backed stop marker, avoiding
//! a second daemon or an unauthenticated TCP control service.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use uuid::Uuid;

use super::model::{
    AccessEndpointKind, ProviderKind, RemoteProxySession, RemoteProxyState, TargetHealth,
    TrustTier, COMPUTE_SCHEMA_VERSION,
};
use super::ssh;
use super::store::{now_ms, ComputeStore};
use super::{connection, EnvironmentConnectionState};

const START_TIMEOUT: Duration = Duration::from_secs(20);
const SSH_READY_TIMEOUT: Duration = Duration::from_secs(12);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const RECONCILE_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

pub fn open(
    store: &ComputeStore,
    target_id: &str,
    workspace_id: &str,
    surface_id: Option<String>,
    requested_port: Option<u16>,
    allow_production: bool,
) -> Result<RemoteProxySession> {
    let target = store.get_target(target_id)?;
    if target.disabled || target.health != TargetHealth::Healthy {
        bail!("remote proxy target {target_id} must be enabled and healthy");
    }
    if !matches!(target.provider, ProviderKind::Ssh | ProviderKind::Azure) {
        bail!("remote browser proxy requires an SSH-backed target");
    }
    if target.trust_tier == TrustTier::Production && !allow_production {
        bail!("production proxy targets require explicit --allow-production");
    }
    let alias = target
        .endpoint
        .ssh_alias
        .as_deref()
        .context("remote proxy target has no SSH alias")?;
    ssh::validate_alias(alias)?;
    reject_configured_forwardings(alias)?;
    let permit =
        connection::begin_for_target(store, target_id, Some(AccessEndpointKind::SshForward))?;

    let local_port = reserve_loopback_port(requested_port)?;
    let proxy_id = format!("proxy-{}", Uuid::new_v4());
    // No stop marker can legitimately exist before the first proxy record is
    // published. Clearing it first makes the create/stop ordering monotonic.
    store.clear_proxy_stop(&proxy_id)?;
    let now = now_ms();
    let session = RemoteProxySession {
        schema_version: COMPUTE_SCHEMA_VERSION,
        proxy_id: proxy_id.clone(),
        target_id: target_id.to_string(),
        environment_id: Some(permit.environment.environment_id),
        endpoint_id: Some(permit.endpoint.endpoint_id),
        workspace_id: workspace_id.to_string(),
        surface_id,
        local_address: Ipv4Addr::LOCALHOST.to_string(),
        local_port,
        state: RemoteProxyState::Starting,
        worker_pid: None,
        ssh_pid: None,
        error: None,
        created_at_ms: now,
        updated_at_ms: now,
    };
    store.save_proxy("proxy.open", &session)?;

    let executable = std::env::current_exe().context("current WTA executable is unavailable")?;
    let mut worker = Command::new(executable);
    worker
        .arg("compute")
        .arg("proxy")
        .arg("worker")
        .arg("--id")
        .arg(&proxy_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_worker(&mut worker);
    if let Err(error) = worker.spawn() {
        if let Some(environment_id) = session.environment_id.as_deref() {
            let _ = connection::mark_failure(
                store,
                environment_id,
                EnvironmentConnectionState::Offline,
                format!("proxy worker launch failed: {error}"),
            );
        }
        let mut failed = session;
        failed.state = RemoteProxyState::Failed;
        failed.error = Some(format!("could not start proxy supervisor: {error}"));
        failed.updated_at_ms = now_ms();
        store.save_proxy("proxy.open", &failed)?;
        bail!(
            "{}",
            failed.error.as_deref().unwrap_or("proxy start failed")
        );
    }

    wait_for_state(store, &proxy_id, START_TIMEOUT, |state| {
        matches!(state, RemoteProxyState::Ready | RemoteProxyState::Failed)
    })
    .and_then(|ready| {
        if ready.state == RemoteProxyState::Ready {
            Ok(ready)
        } else {
            bail!(
                "{}",
                ready
                    .error
                    .as_deref()
                    .unwrap_or("remote proxy worker failed")
            )
        }
    })
}

pub fn close(store: &ComputeStore, id: &str) -> Result<RemoteProxySession> {
    let requested = store.request_proxy_stop("proxy.close", id)?;
    if requested.state.is_terminal() {
        return Ok(requested);
    }
    wait_for_state(store, id, STOP_TIMEOUT, RemoteProxyState::is_terminal)
}

/// Reconcile persisted proxy state after an unclean client or supervisor exit.
///
/// This deliberately never kills a PID read from disk: a stale PID may have
/// been reused by an unrelated process. On Windows the worker owns the SSH
/// child through a kill-on-close Job Object, so a crashed worker closes the
/// listener. Reconciliation only observes that loopback endpoint and commits a
/// fail-closed state transition.
pub fn reconcile(store: &ComputeStore, stale_after: Duration) -> Result<Vec<RemoteProxySession>> {
    let now = now_ms();
    let stale_after_ms = stale_after.as_millis().try_into().unwrap_or(u64::MAX);
    let mut changed = Vec::new();

    for mut session in store.list_proxies()? {
        let age_ms = now.saturating_sub(session.updated_at_ms);
        let listening = proxy_endpoint_available(&session);
        let transition = match session.state {
            RemoteProxyState::Ready if !listening => Some((
                RemoteProxyState::Failed,
                "proxy endpoint is unavailable after supervisor reconciliation",
            )),
            RemoteProxyState::Starting if age_ms >= stale_after_ms => Some((
                RemoteProxyState::Failed,
                if listening {
                    "proxy startup became stale while its endpoint remained open; manual process inspection is required"
                } else {
                    "proxy supervisor did not complete startup before the reconciliation deadline"
                },
            )),
            RemoteProxyState::Stopping if !listening => {
                Some((RemoteProxyState::Stopped, "proxy stop reconciled"))
            }
            RemoteProxyState::Stopping if age_ms >= stale_after_ms => Some((
                RemoteProxyState::Failed,
                "proxy stop became stale while its endpoint remained open; manual process inspection is required",
            )),
            _ => None,
        };

        if let Some((state, reason)) = transition {
            session.state = state;
            session.error = (state == RemoteProxyState::Failed).then(|| reason.to_string());
            session.updated_at_ms = now_ms();
            store.save_proxy("proxy.reconcile", &session)?;
            if state.is_terminal() {
                store.clear_proxy_stop(&session.proxy_id)?;
            }
            changed.push(session);
        } else if session.state.is_terminal() {
            store.clear_proxy_stop(&session.proxy_id)?;
        }
    }

    Ok(changed)
}

pub fn worker(store: &ComputeStore, id: &str) -> Result<()> {
    let mut session = store.get_proxy(id)?;
    if session.state != RemoteProxyState::Starting {
        bail!("proxy worker requires a starting session");
    }
    let target = store.get_target(&session.target_id)?;
    let alias = target
        .endpoint
        .ssh_alias
        .as_deref()
        .context("remote proxy target has no SSH alias")?;
    ssh::validate_alias(alias)?;
    reject_configured_forwardings(alias)?;
    let owns_supervisor_attempt = session
        .environment_id
        .as_deref()
        .and_then(|environment_id| store.get_connection_supervisor(environment_id).ok())
        .is_some_and(|supervisor| supervisor.state != EnvironmentConnectionState::Connected);
    if owns_supervisor_attempt {
        if let Some(environment_id) = session.environment_id.as_deref() {
            let _ = connection::mark_authenticating(store, environment_id);
        }
    }

    session.worker_pid = Some(std::process::id());
    session.updated_at_ms = now_ms();
    store.save_proxy("proxy.worker", &session)?;

    let mut child = spawn_ssh(alias, session.local_port).with_context(|| {
        format!(
            "could not start SSH dynamic forwarding for {}",
            session.target_id
        )
    })?;
    let _child_lifetime = bind_child_lifetime(&child).inspect_err(|_| {
        stop_child(&mut child);
    })?;
    session.ssh_pid = Some(child.id());
    session.updated_at_ms = now_ms();
    store.save_proxy("proxy.worker", &session)?;

    match wait_until_ready(&mut child, session.local_port) {
        Ok(()) => {
            if owns_supervisor_attempt {
                if let Some(environment_id) = session.environment_id.as_deref() {
                    let _ = connection::mark_synchronizing(store, environment_id);
                    connection::mark_connected(store, environment_id)?;
                }
            }
            session.state = RemoteProxyState::Ready;
            session.error = None;
            session.updated_at_ms = now_ms();
            store.save_proxy("proxy.worker", &session)?;
        }
        Err(error) => {
            if owns_supervisor_attempt {
                if let Some(environment_id) = session.environment_id.as_deref() {
                    let _ = connection::mark_failure(
                        store,
                        environment_id,
                        EnvironmentConnectionState::Offline,
                        format!("{error:#}"),
                    );
                }
            }
            stop_child(&mut child);
            session.state = RemoteProxyState::Failed;
            session.error = Some(format!("{error:#}"));
            session.updated_at_ms = now_ms();
            store.save_proxy("proxy.worker", &session)?;
            store.clear_proxy_stop(id)?;
            return Err(error);
        }
    }

    loop {
        if store.proxy_stop_requested(id)? {
            stop_child(&mut child);
            session.state = RemoteProxyState::Stopped;
            session.error = None;
            session.updated_at_ms = now_ms();
            store.save_proxy("proxy.worker", &session)?;
            store.clear_proxy_stop(id)?;
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            if owns_supervisor_attempt {
                if let Some(environment_id) = session.environment_id.as_deref() {
                    let _ = connection::mark_failure(
                        store,
                        environment_id,
                        EnvironmentConnectionState::Offline,
                        format!("SSH proxy exited unexpectedly with {status}"),
                    );
                }
            }
            session.state = RemoteProxyState::Failed;
            session.error = Some(format!("SSH proxy exited unexpectedly with {status}"));
            session.updated_at_ms = now_ms();
            store.save_proxy("proxy.worker", &session)?;
            store.clear_proxy_stop(id)?;
            bail!("{}", session.error.as_deref().unwrap());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn reserve_loopback_port(requested: Option<u16>) -> Result<u16> {
    let port = requested.unwrap_or(0);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .with_context(|| format!("loopback proxy port {port} is unavailable"))?;
    let selected = listener.local_addr()?.port();
    drop(listener);
    Ok(selected)
}

fn spawn_ssh(alias: &str, port: u16) -> Result<Child> {
    let ssh_executable = ssh::find_ssh_executable()?;
    let mut command = Command::new(ssh_executable);
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=yes")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("ServerAliveInterval=20")
        .arg("-o")
        .arg("ServerAliveCountMax=2")
        .arg("-N")
        .arg("-D")
        .arg(format!("127.0.0.1:{port}"))
        .arg(alias)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_window(&mut command);
    command.spawn().context("failed to spawn ssh proxy")
}

fn wait_until_ready(child: &mut Child, port: u16) -> Result<()> {
    let deadline = Instant::now() + SSH_READY_TIMEOUT;
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("SSH proxy exited before becoming ready: {status}");
        }
        if TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("SSH proxy did not bind 127.0.0.1:{port} within 12 seconds");
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn proxy_endpoint_available(session: &RemoteProxySession) -> bool {
    let Ok(address) = format!("{}:{}", session.local_address, session.local_port).parse() else {
        return false;
    };
    TcpStream::connect_timeout(&address, RECONCILE_CONNECT_TIMEOUT).is_ok()
}

fn wait_for_state<F>(
    store: &ComputeStore,
    id: &str,
    timeout: Duration,
    complete: F,
) -> Result<RemoteProxySession>
where
    F: Fn(RemoteProxyState) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let session = store.get_proxy(id)?;
        if complete(session.state) {
            return Ok(session);
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for remote proxy {id}");
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn reject_configured_forwardings(alias: &str) -> Result<()> {
    let resolved = ssh::resolve_alias(alias)?;
    for option in ["localforward", "remoteforward", "dynamicforward"] {
        if resolved
            .effective_options
            .get(option)
            .is_some_and(|values| !values.is_empty())
        {
            bail!(
                "SSH target {alias} config contains {option}; remove configured forwards before opening a workspace proxy"
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
fn hide_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_window(_command: &mut Command) {}

#[cfg(windows)]
fn detach_worker(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP. `CREATE_NO_WINDOW` is
    // deliberately not combined with DETACHED_PROCESS (Windows documents
    // those flags as mutually exclusive). stdio is already bound to NUL.
    command.creation_flags(0x0000_0008 | 0x0000_0200);
}

#[cfg(not(windows))]
fn detach_worker(_command: &mut Command) {}

#[cfg(windows)]
struct ChildLifetimeGuard(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for ChildLifetimeGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn bind_child_lifetime(child: &Child) -> Result<ChildLifetimeGuard> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("could not create proxy child Job Object");
        }

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            let error = std::io::Error::last_os_error();
            CloseHandle(job);
            return Err(error).context("could not configure proxy child Job Object");
        }

        let process = child.as_raw_handle() as HANDLE;
        if AssignProcessToJobObject(job, process) == 0 {
            let error = std::io::Error::last_os_error();
            CloseHandle(job);
            return Err(error).context("could not bind SSH proxy to its supervisor Job Object");
        }
        Ok(ChildLifetimeGuard(job))
    }
}

#[cfg(not(windows))]
struct ChildLifetimeGuard;

#[cfg(not(windows))]
fn bind_child_lifetime(_child: &Child) -> Result<ChildLifetimeGuard> {
    Ok(ChildLifetimeGuard)
}

#[cfg(test)]
mod tests {
    use super::super::model::{ComputeTarget, TargetEndpoint};
    use super::*;
    use serde_json::Value;
    use std::collections::BTreeMap;

    fn test_store(label: &str) -> ComputeStore {
        let root = std::env::temp_dir().join(format!("wta-proxy-{label}-{}", Uuid::new_v4()));
        let store = ComputeStore::at(root).unwrap();
        store
            .upsert_target(
                "test",
                ComputeTarget {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    id: "ssh:test".into(),
                    display_name: "test".into(),
                    provider: ProviderKind::Ssh,
                    endpoint: TargetEndpoint {
                        ssh_alias: Some("test".into()),
                        ..TargetEndpoint::default()
                    },
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    capabilities: Vec::new(),
                    toolchains: BTreeMap::new(),
                    trust_tier: TrustTier::Personal,
                    project_allowlist: Vec::new(),
                    agent_slots: 1,
                    build_slots: 1,
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
    }

    fn proxy(port: u16, state: RemoteProxyState, updated_at_ms: u64) -> RemoteProxySession {
        RemoteProxySession {
            schema_version: COMPUTE_SCHEMA_VERSION,
            proxy_id: format!("proxy-{}", Uuid::new_v4()),
            target_id: "ssh:test".into(),
            environment_id: None,
            endpoint_id: None,
            workspace_id: "workspace".into(),
            surface_id: Some("surface".into()),
            local_address: "127.0.0.1".into(),
            local_port: port,
            state,
            worker_pid: None,
            ssh_pid: None,
            error: None,
            created_at_ms: updated_at_ms,
            updated_at_ms,
        }
    }

    #[test]
    fn allocated_proxy_ports_are_loopback_and_nonzero() {
        assert_ne!(reserve_loopback_port(None).unwrap(), 0);
    }

    #[test]
    fn explicit_busy_proxy_port_is_rejected() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(reserve_loopback_port(Some(port)).is_err());
    }

    #[test]
    fn reconcile_fails_a_ready_proxy_whose_endpoint_disappeared() {
        let store = test_store("ready-missing");
        let session = proxy(
            reserve_loopback_port(None).unwrap(),
            RemoteProxyState::Ready,
            now_ms(),
        );
        store.save_proxy("test", &session).unwrap();

        let changed = reconcile(&store, Duration::from_secs(30)).unwrap();

        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].state, RemoteProxyState::Failed);
        assert!(changed[0].error.as_deref().unwrap().contains("unavailable"));
    }

    #[test]
    fn reconcile_completes_a_stopping_proxy_after_endpoint_closes() {
        let store = test_store("stopping-closed");
        let session = proxy(
            reserve_loopback_port(None).unwrap(),
            RemoteProxyState::Stopping,
            now_ms(),
        );
        store.save_proxy("test", &session).unwrap();

        let changed = reconcile(&store, Duration::from_secs(30)).unwrap();

        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].state, RemoteProxyState::Stopped);
        assert_eq!(changed[0].error, None);
    }

    #[test]
    fn reconcile_preserves_a_live_ready_proxy() {
        let store = test_store("ready-live");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let session = proxy(
            listener.local_addr().unwrap().port(),
            RemoteProxyState::Ready,
            now_ms(),
        );
        store.save_proxy("test", &session).unwrap();

        let changed = reconcile(&store, Duration::from_secs(30)).unwrap();

        assert!(changed.is_empty());
        assert_eq!(
            store.get_proxy(&session.proxy_id).unwrap().state,
            RemoteProxyState::Ready
        );
    }
}
