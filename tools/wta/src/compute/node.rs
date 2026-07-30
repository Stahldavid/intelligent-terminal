//! Portable line-delimited JSON-RPC bridge used over `ssh ... wta node bridge`.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use uuid::Uuid;

use super::model::*;
use super::snapshot;

pub const NODE_RPC_METHODS: &[&str] = &[
    "node.doctor",
    "node.exec",
    "node.handshake",
    "node.hash",
    "node.status",
    "pty.status",
    "file.abort_upload",
    "file.commit_upload",
    "file.close_root",
    "file.create_directory",
    "file.list",
    "file.list_directory",
    "file.prepare_upload",
    "file.read_text",
    "file.remove",
    "file.rename",
    "file.open_root",
    "file.prepare_download",
    "file.stat",
    "relay.capability.issue",
    "relay.capability.revoke",
    "relay.focus",
    "relay.list",
    "relay.notify",
    "relay.progress",
    "relay.status",
];

pub const NODE_CAPABILITIES: &[&str] = &[
    "exec",
    "resource_probe",
    "sha256",
    "verified_file_download_v1",
    "verified_file_upload_v1",
    "remote_file_explorer_v1",
    "files.read",
    "files.write",
    "files.delete",
    "workspace_surface_relay_v1",
];

#[cfg(unix)]
pub const UNIX_NODE_CAPABILITIES: &[&str] = &[
    "acp_reattach_v1",
    "persistent_pty_v1",
    "pty_multi_attach_v1",
    "pty_resize_v1",
    "session_registry",
];

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(default)]
    jsonrpc: String,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

pub async fn bridge_stdio() -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    // Windows/tests use this bridge-local fallback. On a Unix remote node,
    // relay RPCs are forwarded into the private per-user daemon so a transport
    // reconnect can resume the same scoped journal.
    let mut relay = super::relay::RelayService::new();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<RpcRequest>(&line) {
            Ok(request) => handle(request, &mut relay).await,
            Err(error) => RpcResponse {
                jsonrpc: "2.0",
                id: Value::Null,
                result: None,
                error: Some(RpcError {
                    code: -32700,
                    message: format!("parse error: {error}"),
                }),
            },
        };
        stdout
            .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
            .await?;
        stdout.flush().await?;
    }
    Ok(())
}

pub fn handshake() -> Result<NodeHandshake> {
    let root = node_state_root()?;
    std::fs::create_dir_all(&root)?;
    let node_id_path = root.join("node-id");
    let node_id = if node_id_path.is_file() {
        std::fs::read_to_string(&node_id_path)?.trim().to_string()
    } else {
        let id = Uuid::new_v4().to_string();
        let temp = root.join(format!(".node-id-{}.tmp", Uuid::new_v4()));
        {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(id.as_bytes())?;
            file.sync_all()?;
        }
        if std::fs::rename(&temp, &node_id_path).is_err() && !node_id_path.exists() {
            bail!("failed to persist node identity");
        }
        id
    };
    #[allow(unused_mut)]
    let mut capabilities = NODE_CAPABILITIES
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    #[cfg(unix)]
    {
        capabilities.extend(
            UNIX_NODE_CAPABILITIES
                .iter()
                .map(|value| (*value).to_string()),
        );
    }
    capabilities.sort();
    Ok(NodeHandshake {
        protocol_version: COMPUTE_PROTOCOL_VERSION,
        node_version: env!("CARGO_PKG_VERSION").to_string(),
        node_id,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        capabilities,
        rpc_methods: NODE_RPC_METHODS
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        state_root: root.to_string_lossy().into_owned(),
    })
}

pub fn node_state_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("WTA_NODE_STATE_ROOT") {
        return Ok(PathBuf::from(root));
    }
    #[cfg(windows)]
    {
        let root =
            PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?);
        return Ok(root.join("IntelligentTerminalNode"));
    }
    #[cfg(not(windows))]
    {
        if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(value).join("intelligent-terminal-node"));
        }
        let home = std::env::var_os("HOME").context("HOME is unavailable")?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("intelligent-terminal-node"))
    }
}

async fn handle(request: RpcRequest, _relay: &mut super::relay::RelayService) -> RpcResponse {
    if request.jsonrpc != "2.0" {
        return rpc_error(request.id, -32600, "jsonrpc must be 2.0");
    }
    let result = match request.method.as_str() {
        "node.handshake" | "node.status" => handshake()
            .and_then(|value| serde_json::to_value(value).context("serialize node handshake")),
        "node.hash" => hash_file(&request.params),
        "node.exec" => execute(&request.params).await,
        "node.doctor" => doctor(),
        "pty.status" => {
            async {
                let session_id = request
                    .params
                    .get("session_id")
                    .and_then(Value::as_str)
                    .context("pty.status requires session_id")?;
                super::pty::status(session_id).await
            }
            .await
        }
        "file.prepare_upload" => prepare_upload(&request.params),
        "file.commit_upload" => transfer_id(&request.params)
            .and_then(|id| super::transfer::commit_upload(&id))
            .and_then(|value| serde_json::to_value(value).context("serialize upload")),
        "file.abort_upload" => {
            transfer_id(&request.params).and_then(|id| super::transfer::abort_upload(&id))
        }
        "file.list" => super::transfer::list_node_uploads()
            .and_then(|value| serde_json::to_value(value).context("serialize uploads")),
        "file.open_root" => super::files::open_root(&request.params),
        "file.close_root" => super::files::close_root(&request.params),
        "file.list_directory" => super::files::list_directory(&request.params),
        "file.stat" => super::files::stat(&request.params),
        "file.read_text" => super::files::read_text(&request.params),
        "file.prepare_download" => super::files::prepare_download(&request.params),
        "file.create_directory" => super::files::create_directory(&request.params),
        "file.rename" => super::files::rename(&request.params),
        "file.remove" => super::files::remove(&request.params),
        method if super::relay::RELAY_RPC_METHODS.contains(&method) => {
            #[cfg(all(unix, not(test)))]
            {
                super::session::relay_dispatch(method, &request.params).await
            }
            #[cfg(any(not(unix), test))]
            {
                _relay.dispatch(method, &request.params)
            }
        }
        _ => Err(anyhow!("unknown method: {}", request.method)),
    };
    match result {
        Ok(result) => RpcResponse {
            jsonrpc: "2.0",
            id: request.id,
            result: Some(result),
            error: None,
        },
        Err(error) => rpc_error(request.id, -32000, &format!("{error:#}")),
    }
}

fn prepare_upload(params: &Value) -> Result<Value> {
    let transfer_id = transfer_id(params)?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("file.prepare_upload requires name")?;
    let size = params
        .get("size")
        .and_then(Value::as_u64)
        .context("file.prepare_upload requires size")?;
    let sha256 = params
        .get("sha256")
        .and_then(Value::as_str)
        .context("file.prepare_upload requires sha256")?;
    serde_json::to_value(super::transfer::prepare_upload(
        &transfer_id,
        name,
        size,
        sha256,
    )?)
    .context("serialize upload")
}

fn transfer_id(params: &Value) -> Result<String> {
    params
        .get("transfer_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("file operation requires transfer_id")
}

fn hash_file(params: &Value) -> Result<Value> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .context("node.hash requires path")?;
    Ok(json!({"sha256": snapshot::sha256_file(PathBuf::from(path).as_path())?}))
}

async fn execute(params: &Value) -> Result<Value> {
    let argv = params
        .get("argv")
        .and_then(Value::as_array)
        .context("node.exec requires argv")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .context("argv entries must be strings")
        })
        .collect::<Result<Vec<_>>>()?;
    if argv.is_empty() {
        bail!("node.exec argv is empty");
    }
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
        command.current_dir(cwd);
    }
    command.kill_on_drop(true);
    let output = command.output().await?;
    Ok(json!({
        "exit_code": output.status.code(),
        "success": output.status.success(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}

pub fn doctor() -> Result<Value> {
    let handshake = handshake()?;
    Ok(json!({
        "ok": true,
        "handshake": handshake,
        "tools": {
            "git": which::which("git").ok().map(|path| path.to_string_lossy().into_owned()),
            "ssh": which::which("ssh").ok().map(|path| path.to_string_lossy().into_owned()),
            "codex": which::which("codex").ok().map(|path| path.to_string_lossy().into_owned()),
        }
    }))
}

fn rpc_error(id: Value, code: i32, message: &str) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcError {
            code,
            message: message.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handshake_has_stable_protocol_and_capabilities() {
        let value = handshake().unwrap();
        assert_eq!(value.protocol_version, COMPUTE_PROTOCOL_VERSION);
        assert!(value.capabilities.contains(&"exec".to_string()));
        assert!(value
            .capabilities
            .contains(&"remote_file_explorer_v1".to_string()));
        assert!(value.capabilities.contains(&"files.read".to_string()));
        assert!(value.capabilities.contains(&"files.write".to_string()));
        assert!(value.capabilities.contains(&"files.delete".to_string()));
        assert!(value.rpc_methods.contains(&"file.open_root".to_string()));
        assert!(value.rpc_methods.contains(&"file.close_root".to_string()));
        assert!(!value.rpc_methods.contains(&"file.roots".to_string()));
        assert!(value.rpc_methods.contains(&"node.handshake".to_string()));
        assert!(value.rpc_methods.contains(&"node.exec".to_string()));
        assert!(!value.node_id.is_empty());
    }

    #[tokio::test]
    async fn file_explorer_methods_are_advertised() {
        let value = handshake().unwrap();
        for method in [
            "file.list_directory",
            "file.stat",
            "file.read_text",
            "file.prepare_download",
            "file.create_directory",
            "file.rename",
            "file.remove",
        ] {
            assert!(value.rpc_methods.iter().any(|value| value == method));
        }
    }

    #[tokio::test]
    async fn unknown_rpc_method_is_rejected() {
        let mut relay = crate::compute::relay::RelayService::new();
        let response = handle(
            RpcRequest {
                jsonrpc: "2.0".into(),
                id: json!(1),
                method: "unknown".into(),
                params: Value::Null,
            },
            &mut relay,
        )
        .await;
        assert!(response.error.is_some());
    }

    #[tokio::test]
    async fn relay_methods_are_advertised_and_reached_over_node_rpc() {
        let handshake = handshake().unwrap();
        assert!(handshake
            .capabilities
            .contains(&"workspace_surface_relay_v1".to_string()));
        assert!(handshake
            .rpc_methods
            .contains(&"relay.capability.issue".to_string()));
        for method in crate::compute::relay::RELAY_RPC_METHODS {
            assert!(
                handshake.rpc_methods.iter().any(|value| value == method),
                "relay method {method} is not advertised by the node handshake"
            );
        }

        let mut relay = crate::compute::relay::RelayService::new();
        let response = handle(
            RpcRequest {
                jsonrpc: "2.0".into(),
                id: json!(2),
                method: "relay.capability.issue".into(),
                params: json!({
                    "scope": {
                        "workspace_id": "workspace-rpc",
                        "surface_id": "surface-rpc"
                    },
                    "operations": ["notify"],
                    "ttl_ms": 30000
                }),
            },
            &mut relay,
        )
        .await;
        assert!(response.error.is_none());
        assert!(response
            .result
            .as_ref()
            .and_then(|value| value.get("token"))
            .and_then(Value::as_str)
            .is_some());
    }
}
