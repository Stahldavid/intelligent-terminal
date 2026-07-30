//! Shared host-side JSON-RPC client for an SSH-backed `wta-node`.
//!
//! Relay, file explorer and future remote services all use this transport.
//! Capability checks remain per consumer, while SSH spawning, framing, size
//! limits and shutdown semantics live in one place.

use std::process::Stdio;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::connection;
use super::installation;
use super::model::{AccessEndpointKind, EnvironmentConnectionState, ProviderKind, TargetHealth};
use super::store::ComputeStore;

const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub struct RemoteNodeClient {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    handshake: Value,
    environment_id: String,
    endpoint_id: String,
}

impl RemoteNodeClient {
    pub async fn connect(
        store: &ComputeStore,
        target_id: &str,
        required_capability: &str,
    ) -> Result<Self> {
        let permit =
            connection::begin_for_target(store, target_id, Some(AccessEndpointKind::SshForward))?;
        let environment_id = permit.environment.environment_id.clone();
        let endpoint_id = permit.endpoint.endpoint_id.clone();
        let owns_supervisor_attempt =
            permit.supervisor.state != EnvironmentConnectionState::Connected;
        let result: Result<Self> = async {
            let target = store.get_target(target_id)?;
            if target.disabled {
                bail!("remote node target {target_id} is disabled");
            }
            if matches!(
                target.health,
                TargetHealth::Unreachable
                    | TargetHealth::TrustRequired
                    | TargetHealth::HostKeyChanged
                    | TargetHealth::Incompatible
            ) {
                bail!(
                    "remote node target {target_id} is not connectable: {:?}",
                    target.health
                );
            }
            if !matches!(target.provider, ProviderKind::Ssh | ProviderKind::Azure) {
                bail!("remote node target {target_id} must be SSH-backed");
            }
            let alias = target
                .endpoint
                .ssh_alias
                .as_deref()
                .context("remote node target has no SSH alias")?;
            super::ssh::validate_alias(alias)?;
            let resolved = super::ssh::resolve_alias(alias)?;
            let installation = installation::from_target(&target)?;
            if owns_supervisor_attempt {
                connection::mark_authenticating(store, &environment_id)?;
            }

            let mut command = Command::new(super::ssh::find_ssh_executable()?);
            command
                .kill_on_drop(true)
                .arg("-o")
                .arg("BatchMode=yes")
                .arg("-o")
                .arg("ConnectTimeout=10")
                .arg("-o")
                .arg("StrictHostKeyChecking=yes");
            command.args(super::transport::default_keepalive_args(&resolved));
            command
                .arg(alias)
                .arg("--")
                .arg(format!("$HOME/{}", installation.active_path))
                .arg("bridge")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            let mut child = command
                .spawn()
                .with_context(|| format!("start SSH node bridge for {target_id}"))?;
            let stdin = child
                .stdin
                .take()
                .context("remote node bridge stdin is unavailable")?;
            let stdout = child
                .stdout
                .take()
                .context("remote node bridge stdout is unavailable")?;
            let mut client = Self {
                child,
                stdin,
                stdout: BufReader::new(stdout).lines(),
                next_id: 1,
                handshake: Value::Null,
                environment_id: environment_id.clone(),
                endpoint_id: endpoint_id.clone(),
            };
            let handshake = client.request("node.handshake", json!({})).await?;
            let supports_required = handshake
                .get("capabilities")
                .and_then(Value::as_array)
                .is_some_and(|values| {
                    values
                        .iter()
                        .any(|value| value.as_str() == Some(required_capability))
                });
            if !supports_required {
                bail!("remote node does not advertise {required_capability}");
            }
            client.handshake = handshake;
            if owns_supervisor_attempt {
                connection::mark_synchronizing(store, &environment_id)?;
                connection::mark_connected(store, &environment_id)?;
            }
            Ok(client)
        }
        .await;
        if owns_supervisor_attempt {
            if let Err(error) = &result {
                let message = format!("{error:#}");
                let lower = message.to_ascii_lowercase();
                let state =
                    if lower.contains("authentication") || lower.contains("permission denied") {
                        EnvironmentConnectionState::AuthBlocked
                    } else if lower.contains("does not advertise") || lower.contains("protocol") {
                        EnvironmentConnectionState::VersionBlocked
                    } else {
                        EnvironmentConnectionState::Offline
                    };
                let _ = connection::mark_failure(store, &environment_id, state, message);
            }
        }
        result
    }

    pub fn handshake(&self) -> &Value {
        &self.handshake
    }

    pub fn environment_id(&self) -> &str {
        &self.environment_id
    }

    pub fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        if !matches!(
            method.split_once('.').map(|(prefix, _)| prefix),
            Some("node" | "relay" | "file" | "pty")
        ) {
            bail!("unsupported remote node method: {method}");
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let frame = serde_json::to_vec(&request)?;
        if frame.len() > MAX_FRAME_BYTES {
            bail!("remote node request exceeds {MAX_FRAME_BYTES} bytes");
        }
        self.stdin.write_all(&frame).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;

        loop {
            let line = self
                .stdout
                .next_line()
                .await?
                .context("remote node bridge closed before responding")?;
            if line.len() > MAX_FRAME_BYTES {
                bail!("remote node response exceeds {MAX_FRAME_BYTES} bytes");
            }
            let response: Value =
                serde_json::from_str(&line).context("invalid remote node JSON-RPC response")?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error").filter(|value| !value.is_null()) {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("remote node request failed");
                bail!("{message}");
            }
            return Ok(response.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    pub async fn close(mut self) -> Result<()> {
        self.stdin.shutdown().await?;
        let status = match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.child.wait(),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                self.child.kill().await?;
                let _ = self.child.wait().await;
                return Ok(());
            }
        };
        if !status.success() {
            bail!("remote node SSH bridge exited with {status}");
        }
        Ok(())
    }
}
