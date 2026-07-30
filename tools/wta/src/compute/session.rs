//! Persistent ACP adapter sessions owned by `wta-node`.
//!
//! The SSH process is intentionally only a transport attachment. A per-user
//! daemon owns the adapter child and its stdio, so closing the SSH channel does
//! not terminate the agent runtime. Linux/Unix is the supported remote-node
//! platform today; Windows continues to run ACP adapters in `wta-master`.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub const REMOTE_SESSION_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSessionState {
    Starting,
    Running,
    Attached,
    Detached,
    Stopping,
    Exited,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSessionRecord {
    pub schema_version: u16,
    pub session_id: String,
    /// Executable display name only. Full argv is deliberately not persisted
    /// because custom adapter arguments may contain credentials.
    #[serde(default)]
    pub program: String,
    /// Stable digest used to reject a conflicting start request without
    /// exposing arguments through the registry or `acp list`.
    #[serde(default)]
    pub argv_sha256: String,
    pub pid: Option<u32>,
    pub state: RemoteSessionState,
    pub attached: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub exit_code: Option<i32>,
    pub last_error: Option<String>,
}

pub fn validate_session_id(value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 {
        bail!("remote session id must contain 1..=128 characters");
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("remote session id may contain only ASCII letters, digits, '.', '_' and '-'");
    }
    Ok(())
}

#[cfg(unix)]
mod unix {
    use std::collections::{BTreeMap, HashMap};
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Weak};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use anyhow::{anyhow, bail, Context, Result};
    use serde::{Deserialize, Serialize};
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::process::{ChildStdin, Command};
    use tokio::sync::{broadcast, mpsc, watch, Mutex};

    use super::{
        validate_session_id, RemoteSessionRecord, RemoteSessionState, REMOTE_SESSION_SCHEMA_VERSION,
    };

    const SOCKET_FILE: &str = "daemon.sock";
    const REGISTRY_FILE: &str = "sessions.json";
    const BACKLOG_LIMIT: usize = 1024 * 1024;

    #[derive(Debug, Deserialize)]
    struct DaemonRequest {
        operation: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        argv: Vec<String>,
        #[serde(default)]
        params: Value,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct DaemonResponse {
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default)]
        attach: bool,
    }

    #[derive(Debug, Default, Serialize, Deserialize)]
    struct RegistryDocument {
        schema_version: u16,
        #[serde(default)]
        sessions: BTreeMap<String, RemoteSessionRecord>,
    }

    struct DaemonState {
        root: PathBuf,
        executable_sha256: String,
        sessions: Mutex<HashMap<String, Arc<SessionHandle>>>,
        registry: Mutex<RegistryDocument>,
        relay: Mutex<super::super::relay::RelayService>,
    }

    struct SessionHandle {
        record: Mutex<RemoteSessionRecord>,
        /// Kept only in daemon memory for idempotent start comparison.
        argv: Vec<String>,
        stdin: Mutex<ChildStdin>,
        attached: AtomicBool,
        backlog: Mutex<Vec<u8>>,
        output_tx: broadcast::Sender<Vec<u8>>,
        detach_tx: watch::Sender<u64>,
        /// The daemon owns ACP initialization for the lifetime of the adapter.
        /// Reattached transports receive this response with their request id
        /// instead of sending a second initialize to an already initialized
        /// agent.
        initialize_response: Mutex<Option<Value>>,
        pending_initialize_id: Mutex<Option<Value>>,
        next_upstream_request_id: AtomicU64,
        downstream_request_ids: Mutex<HashMap<String, Value>>,
        state: Weak<DaemonState>,
    }

    impl DaemonState {
        async fn load(root: PathBuf) -> Result<Arc<Self>> {
            secure_dir(&root)?;
            let registry_path = root.join(REGISTRY_FILE);
            let mut registry = if registry_path.is_file() {
                serde_json::from_slice::<RegistryDocument>(&std::fs::read(&registry_path)?)
                    .with_context(|| {
                        format!("invalid node registry: {}", registry_path.display())
                    })?
            } else {
                RegistryDocument {
                    schema_version: REMOTE_SESSION_SCHEMA_VERSION,
                    sessions: BTreeMap::new(),
                }
            };
            // A daemon cannot adopt arbitrary inherited pipes after its own
            // restart. Mark previous live records fail-closed instead of
            // claiming they are reattachable.
            for record in registry.sessions.values_mut() {
                if matches!(
                    record.state,
                    RemoteSessionState::Starting
                        | RemoteSessionState::Running
                        | RemoteSessionState::Attached
                        | RemoteSessionState::Detached
                        | RemoteSessionState::Stopping
                ) {
                    record.state = RemoteSessionState::Failed;
                    record.attached = false;
                    record.pid = None;
                    record.last_error = Some("wta-node daemon restarted".to_string());
                    record.updated_at_ms = now_ms();
                }
            }
            let state = Arc::new(Self {
                root,
                executable_sha256: current_executable_sha256()?,
                sessions: Mutex::new(HashMap::new()),
                registry: Mutex::new(registry),
                relay: Mutex::new(super::super::relay::RelayService::new()),
            });
            state.persist_registry().await?;
            Ok(state)
        }

        async fn persist_record(&self, record: RemoteSessionRecord) -> Result<()> {
            {
                let mut registry = self.registry.lock().await;
                registry.sessions.insert(record.session_id.clone(), record);
            }
            self.persist_registry().await
        }

        async fn persist_registry(&self) -> Result<()> {
            let bytes = {
                let registry = self.registry.lock().await;
                serde_json::to_vec_pretty(&*registry)?
            };
            let path = self.root.join(REGISTRY_FILE);
            let temp = self
                .root
                .join(format!(".{REGISTRY_FILE}.{}.tmp", uuid::Uuid::new_v4()));
            {
                let mut file = std::fs::File::create(&temp)?;
                file.write_all(&bytes)?;
                file.write_all(b"\n")?;
                file.sync_all()?;
            }
            std::fs::rename(&temp, &path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            Ok(())
        }

        async fn list_records(&self) -> Vec<RemoteSessionRecord> {
            self.registry
                .lock()
                .await
                .sessions
                .values()
                .cloned()
                .collect()
        }

        async fn stop_all(&self) {
            let sessions = self
                .sessions
                .lock()
                .await
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for session in sessions {
                let state = session.record.lock().await.state;
                if !matches!(
                    state,
                    RemoteSessionState::Exited | RemoteSessionState::Failed
                ) {
                    let _ = session.stop().await;
                }
            }
        }

        async fn start_session(
            self: &Arc<Self>,
            session_id: &str,
            argv: &[String],
        ) -> Result<Arc<SessionHandle>> {
            validate_session_id(session_id)?;
            if argv.is_empty() {
                bail!("ACP session argv is empty");
            }
            if let Some(existing) = self.sessions.lock().await.get(session_id).cloned() {
                let record = existing.record.lock().await;
                if existing.argv != argv {
                    bail!(
                        "remote session '{session_id}' already exists with a different adapter command"
                    );
                }
                if matches!(
                    record.state,
                    RemoteSessionState::Exited | RemoteSessionState::Failed
                ) {
                    bail!("remote session '{session_id}' is not running");
                }
                drop(record);
                return Ok(existing);
            }

            let session_dir = self.root.join("sessions").join(session_id);
            secure_dir(&session_dir)?;
            let mut command = Command::new(&argv[0]);
            command
                .args(&argv[1..])
                .env_remove("CLAUDECODE")
                .env_remove("WT_COM_CLSID")
                .env_remove("WT_PROTOCOL_TOKEN")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // A dedicated process group lets stop terminate adapter children
            // without shell interpolation or PID guessing.
            command.process_group(0);
            let mut child = command
                .spawn()
                .with_context(|| format!("failed to start ACP adapter '{}'", argv[0]))?;
            let pid = child.id().context("spawned ACP adapter has no pid")?;
            let stdin = child.stdin.take().context("ACP adapter has no stdin")?;
            let mut stdout = child.stdout.take().context("ACP adapter has no stdout")?;
            let stderr = child.stderr.take().context("ACP adapter has no stderr")?;
            let (output_tx, _) = broadcast::channel(256);
            let (detach_tx, _) = watch::channel(0u64);
            let record = RemoteSessionRecord {
                schema_version: REMOTE_SESSION_SCHEMA_VERSION,
                session_id: session_id.to_string(),
                program: Path::new(&argv[0])
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(&argv[0])
                    .to_string(),
                argv_sha256: argv_digest(argv),
                pid: Some(pid),
                state: RemoteSessionState::Running,
                attached: false,
                created_at_ms: now_ms(),
                updated_at_ms: now_ms(),
                exit_code: None,
                last_error: None,
            };
            let session = Arc::new(SessionHandle {
                record: Mutex::new(record.clone()),
                argv: argv.to_vec(),
                stdin: Mutex::new(stdin),
                attached: AtomicBool::new(false),
                backlog: Mutex::new(Vec::new()),
                output_tx,
                detach_tx,
                initialize_response: Mutex::new(None),
                pending_initialize_id: Mutex::new(None),
                next_upstream_request_id: AtomicU64::new(1),
                downstream_request_ids: Mutex::new(HashMap::new()),
                state: Arc::downgrade(self),
            });
            self.sessions
                .lock()
                .await
                .insert(session_id.to_string(), Arc::clone(&session));
            self.persist_record(record).await?;

            {
                let session = Arc::clone(&session);
                tokio::spawn(async move {
                    let mut stdout = BufReader::new(&mut stdout);
                    loop {
                        let mut frame = Vec::new();
                        match stdout.read_until(b'\n', &mut frame).await {
                            Ok(0) => break,
                            Ok(_) => {
                                let frame = session.route_adapter_frame(&frame).await;
                                if session.attached.load(Ordering::Acquire) {
                                    if session.output_tx.send(frame.clone()).is_err() {
                                        session.push_backlog(&frame).await;
                                    }
                                } else {
                                    session.push_backlog(&frame).await;
                                }
                            }
                            Err(error) => {
                                session
                                    .set_error(format!("adapter stdout failed: {error}"))
                                    .await;
                                break;
                            }
                        }
                    }
                });
            }
            {
                let stderr_path = session_dir.join("stderr.log");
                tokio::spawn(async move {
                    if let Ok(mut file) = tokio::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(stderr_path)
                        .await
                    {
                        let mut stderr = stderr;
                        let _ = tokio::io::copy(&mut stderr, &mut file).await;
                        let _ = file.flush().await;
                    }
                });
            }
            {
                let session = Arc::clone(&session);
                tokio::spawn(async move {
                    match child.wait().await {
                        Ok(status) => {
                            session
                                .set_terminal_state(RemoteSessionState::Exited, status.code(), None)
                                .await;
                        }
                        Err(error) => {
                            session
                                .set_terminal_state(
                                    RemoteSessionState::Failed,
                                    None,
                                    Some(format!("adapter wait failed: {error}")),
                                )
                                .await;
                        }
                    }
                });
            }
            Ok(session)
        }

        async fn lookup(&self, session_id: &str) -> Result<Arc<SessionHandle>> {
            validate_session_id(session_id)?;
            self.sessions
                .lock()
                .await
                .get(session_id)
                .cloned()
                .with_context(|| format!("remote session '{session_id}' is not running"))
        }
    }

    impl SessionHandle {
        async fn route_adapter_frame(&self, frame: &[u8]) -> Vec<u8> {
            let Ok(mut value) = serde_json::from_slice::<Value>(frame) else {
                return frame.to_vec();
            };
            let Some(response_id) = value.get("id").cloned() else {
                return frame.to_vec();
            };

            let pending_id = self.pending_initialize_id.lock().await.clone();
            if pending_id.as_ref() == Some(&response_id) && value.get("result").is_some() {
                *self.initialize_response.lock().await = Some(value.clone());
                *self.pending_initialize_id.lock().await = None;
            } else if value.get("method").is_none() {
                let key = serde_json::to_string(&response_id).unwrap_or_default();
                if let Some(downstream_id) = self.downstream_request_ids.lock().await.remove(&key) {
                    value["id"] = downstream_id;
                }
            }

            let mut routed = serde_json::to_vec(&value).unwrap_or_else(|_| frame.to_vec());
            if frame.ends_with(b"\n") {
                routed.push(b'\n');
            }
            routed
        }

        async fn route_client_frame(
            &self,
            frame: &[u8],
            direct_output: &mpsc::UnboundedSender<Vec<u8>>,
        ) -> Result<()> {
            if let Ok(mut value) = serde_json::from_slice::<Value>(frame) {
                if value.get("method").and_then(Value::as_str) == Some("initialize") {
                    let request_id = value.get("id").cloned().unwrap_or(Value::Null);
                    if let Some(mut cached) = self.initialize_response.lock().await.clone() {
                        cached["id"] = request_id;
                        let mut replay = serde_json::to_vec(&cached)?;
                        replay.push(b'\n');
                        direct_output
                            .send(replay)
                            .map_err(|_| anyhow!("attached client output channel closed"))?;
                        return Ok(());
                    }
                    *self.pending_initialize_id.lock().await = Some(request_id);
                } else if value.get("method").is_some() {
                    if let Some(downstream_id) = value.get("id").cloned() {
                        let sequence = self
                            .next_upstream_request_id
                            .fetch_add(1, Ordering::Relaxed);
                        let upstream_id = Value::String(format!("wta-{sequence}"));
                        let key = serde_json::to_string(&upstream_id)?;
                        self.downstream_request_ids
                            .lock()
                            .await
                            .insert(key, downstream_id);
                        value["id"] = upstream_id;
                        let mut routed = serde_json::to_vec(&value)?;
                        routed.push(b'\n');
                        let mut stdin = self.stdin.lock().await;
                        stdin.write_all(&routed).await?;
                        stdin.flush().await?;
                        return Ok(());
                    }
                }
            }

            let mut stdin = self.stdin.lock().await;
            stdin.write_all(frame).await?;
            stdin.flush().await?;
            Ok(())
        }

        async fn push_backlog(&self, bytes: &[u8]) {
            let mut backlog = self.backlog.lock().await;
            backlog.extend_from_slice(bytes);
            if backlog.len() > BACKLOG_LIMIT {
                let overflow = backlog.len() - BACKLOG_LIMIT;
                backlog.drain(..overflow);
            }
        }

        async fn persist(&self) {
            let record = self.record.lock().await.clone();
            if let Some(state) = self.state.upgrade() {
                let _ = state.persist_record(record).await;
            }
        }

        async fn set_error(&self, error: String) {
            {
                let mut record = self.record.lock().await;
                record.last_error = Some(error);
                record.updated_at_ms = now_ms();
            }
            self.persist().await;
        }

        async fn set_terminal_state(
            &self,
            state: RemoteSessionState,
            exit_code: Option<i32>,
            error: Option<String>,
        ) {
            self.attached.store(false, Ordering::Release);
            {
                let mut record = self.record.lock().await;
                record.state = state;
                record.attached = false;
                record.pid = None;
                record.exit_code = exit_code;
                record.last_error = error;
                record.updated_at_ms = now_ms();
            }
            let _ = self.detach_tx.send(*self.detach_tx.borrow() + 1);
            self.persist().await;
        }

        async fn request_detach(&self) {
            let _ = self.detach_tx.send(*self.detach_tx.borrow() + 1);
        }

        async fn stop(&self) -> Result<()> {
            let pid = {
                let mut record = self.record.lock().await;
                if matches!(
                    record.state,
                    RemoteSessionState::Exited | RemoteSessionState::Failed
                ) {
                    return Ok(());
                }
                record.state = RemoteSessionState::Stopping;
                record.updated_at_ms = now_ms();
                record.pid
            };
            self.persist().await;
            self.request_detach().await;
            if let Some(pid) = pid {
                signal_process_group(pid, "TERM").await?;
                for _ in 0..50 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let state = self.record.lock().await.state;
                    if matches!(
                        state,
                        RemoteSessionState::Exited | RemoteSessionState::Failed
                    ) {
                        return Ok(());
                    }
                }
                signal_process_group(pid, "KILL").await?;
            }
            Ok(())
        }
    }

    async fn signal_process_group(pid: u32, signal: &str) -> Result<()> {
        let status = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg("--")
            .arg(format!("-{pid}"))
            .status()
            .await?;
        if !status.success() {
            bail!("failed to send SIG{signal} to adapter process group {pid}");
        }
        Ok(())
    }

    pub async fn daemon_serve() -> Result<()> {
        let root = super::super::node::node_state_root()?;
        secure_dir(&root)?;
        let socket = root.join(SOCKET_FILE);
        if socket.exists() {
            if UnixStream::connect(&socket).await.is_ok() {
                bail!("wta-node daemon is already running");
            }
            std::fs::remove_file(&socket)
                .with_context(|| format!("remove stale socket {}", socket.display()))?;
        }
        let listener = UnixListener::bind(&socket)
            .with_context(|| format!("bind node socket {}", socket.display()))?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let state = DaemonState::load(root).await?;
        loop {
            let (stream, _) = listener.accept().await?;
            if daemon_binary_changed(&state.executable_sha256, &current_executable_sha256()?) {
                state.stop_all().await;
                drop(stream);
                drop(listener);
                let _ = std::fs::remove_file(&socket);
                return Ok(());
            }
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                if let Err(error) = handle_connection(state, stream).await {
                    eprintln!("wta-node daemon connection failed: {error:#}");
                }
            });
        }
    }

    async fn handle_connection(state: Arc<DaemonState>, mut stream: UnixStream) -> Result<()> {
        let mut request_line = String::new();
        {
            let mut reader = BufReader::new(&mut stream);
            reader.read_line(&mut request_line).await?;
        }
        let request: DaemonRequest =
            serde_json::from_str(&request_line).context("invalid daemon request")?;
        match request.operation.as_str() {
            "start" => {
                let session_id = required_session_id(&request)?;
                let session = state.start_session(session_id, &request.argv).await?;
                attach_stream(session, stream).await
            }
            "attach" => {
                let session = state.lookup(required_session_id(&request)?).await?;
                attach_stream(session, stream).await
            }
            "detach" => {
                let session = state.lookup(required_session_id(&request)?).await?;
                session.request_detach().await;
                let record = session.record.lock().await.clone();
                write_response(
                    &mut stream,
                    DaemonResponse {
                        ok: true,
                        result: Some(serde_json::to_value(record)?),
                        error: None,
                        attach: false,
                    },
                )
                .await
            }
            "stop" => {
                let session = state.lookup(required_session_id(&request)?).await?;
                session.stop().await?;
                let record = session.record.lock().await.clone();
                write_response(
                    &mut stream,
                    DaemonResponse {
                        ok: true,
                        result: Some(serde_json::to_value(record)?),
                        error: None,
                        attach: false,
                    },
                )
                .await
            }
            "list" => {
                write_response(
                    &mut stream,
                    DaemonResponse {
                        ok: true,
                        result: Some(serde_json::to_value(state.list_records().await)?),
                        error: None,
                        attach: false,
                    },
                )
                .await
            }
            other if other.starts_with("relay.") => {
                let result = state.relay.lock().await.dispatch(other, &request.params);
                match result {
                    Ok(value) => {
                        write_response(
                            &mut stream,
                            DaemonResponse {
                                ok: true,
                                result: Some(value),
                                error: None,
                                attach: false,
                            },
                        )
                        .await
                    }
                    Err(error) => {
                        write_response(
                            &mut stream,
                            DaemonResponse {
                                ok: false,
                                result: None,
                                error: Some(format!("{error:#}")),
                                attach: false,
                            },
                        )
                        .await
                    }
                }
            }
            other => {
                write_response(
                    &mut stream,
                    DaemonResponse {
                        ok: false,
                        result: None,
                        error: Some(format!("unknown daemon operation: {other}")),
                        attach: false,
                    },
                )
                .await
            }
        }
    }

    fn required_session_id(request: &DaemonRequest) -> Result<&str> {
        request
            .session_id
            .as_deref()
            .context("daemon operation requires session_id")
    }

    async fn attach_stream(session: Arc<SessionHandle>, mut stream: UnixStream) -> Result<()> {
        if session
            .attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return write_response(
                &mut stream,
                DaemonResponse {
                    ok: false,
                    result: None,
                    error: Some("remote session already has an attached client".to_string()),
                    attach: false,
                },
            )
            .await;
        }

        let mut output_rx = session.output_tx.subscribe();
        let mut detach_rx = session.detach_tx.subscribe();
        let backlog = {
            let mut backlog = session.backlog.lock().await;
            std::mem::take(&mut *backlog)
        };
        {
            let mut record = session.record.lock().await;
            if matches!(
                record.state,
                RemoteSessionState::Exited | RemoteSessionState::Failed
            ) {
                session.attached.store(false, Ordering::Release);
                bail!("remote session is not running");
            }
            record.state = RemoteSessionState::Attached;
            record.attached = true;
            record.updated_at_ms = now_ms();
        }
        session.persist().await;
        write_response(
            &mut stream,
            DaemonResponse {
                ok: true,
                result: Some(serde_json::to_value(session.record.lock().await.clone())?),
                error: None,
                attach: true,
            },
        )
        .await?;
        if !backlog.is_empty() {
            stream.write_all(&backlog).await?;
            stream.flush().await?;
        }

        let (socket_read, mut socket_write) = stream.into_split();
        let (direct_tx, mut direct_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let input = async {
            let mut socket_read = BufReader::new(socket_read);
            loop {
                let mut frame = Vec::new();
                let count = socket_read.read_until(b'\n', &mut frame).await?;
                if count == 0 {
                    break;
                }
                session.route_client_frame(&frame, &direct_tx).await?;
            }
            Result::<()>::Ok(())
        };
        let output = async {
            loop {
                let bytes = tokio::select! {
                    direct = direct_rx.recv() => {
                        let Some(bytes) = direct else { break };
                        bytes
                    }
                    broadcast = output_rx.recv() => match broadcast {
                        Ok(bytes) => bytes,
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(count)) => {
                            return Err(anyhow!(
                                "attached client fell behind by {count} adapter output chunks"
                            ));
                        }
                    }
                };
                {
                    if let Err(error) = socket_write.write_all(&bytes).await {
                        // Preserve at-least-once delivery across a transport
                        // break. A partial socket write can duplicate a JSON
                        // chunk after reattach, which is safer than silently
                        // losing an ACP response.
                        session.push_backlog(&bytes).await;
                        return Err(error.into());
                    }
                    if let Err(error) = socket_write.flush().await {
                        session.push_backlog(&bytes).await;
                        return Err(error.into());
                    }
                }
            }
            Result::<()>::Ok(())
        };
        let detached = async {
            detach_rx.changed().await?;
            Result::<()>::Ok(())
        };
        let result = tokio::select! {
            result = input => result,
            result = output => result,
            result = detached => result,
        };

        session.attached.store(false, Ordering::Release);
        {
            let mut record = session.record.lock().await;
            if !matches!(
                record.state,
                RemoteSessionState::Exited
                    | RemoteSessionState::Failed
                    | RemoteSessionState::Stopping
            ) {
                record.state = RemoteSessionState::Detached;
            }
            record.attached = false;
            record.updated_at_ms = now_ms();
        }
        session.persist().await;
        result
    }

    async fn write_response(stream: &mut UnixStream, response: DaemonResponse) -> Result<()> {
        stream
            .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
            .await?;
        stream.flush().await?;
        Ok(())
    }

    async fn ensure_daemon() -> Result<PathBuf> {
        let root = super::super::node::node_state_root()?;
        secure_dir(&root)?;
        let socket = root.join(SOCKET_FILE);
        if UnixStream::connect(&socket).await.is_ok() {
            return Ok(socket);
        }

        let executable = std::env::current_exe().context("resolve current wta-node executable")?;
        let mut command = std::process::Command::new("nohup");
        command
            .arg(&executable)
            .arg("daemon")
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if command.spawn().is_err() {
            std::process::Command::new(&executable)
                .arg("daemon")
                .arg("serve")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("start wta-node daemon")?;
        }
        for _ in 0..100 {
            if UnixStream::connect(&socket).await.is_ok() {
                return Ok(socket);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        bail!("wta-node daemon did not create {}", socket.display())
    }

    async fn request(operation: &str, session_id: Option<&str>, argv: &[String]) -> Result<Value> {
        for attempt in 0..2 {
            let socket = ensure_daemon().await?;
            let mut stream = UnixStream::connect(&socket).await?;
            let request = json!({
                "operation": operation,
                "session_id": session_id,
                "argv": argv,
                "params": {},
            });
            stream
                .write_all(format!("{}\n", serde_json::to_string(&request)?).as_bytes())
                .await?;
            stream.flush().await?;
            let response = match read_response(&mut stream).await {
                Ok(response) => response,
                Err(_) if attempt == 0 => {
                    // A freshly bootstrapped binary asks the old daemon to
                    // retire at its next accepted connection. That deliberate
                    // rollover closes this first stream without a response.
                    // Retry once so callers do not see a transient refusal.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if !response.ok {
                bail!(
                    "{}",
                    response
                        .error
                        .unwrap_or_else(|| "wta-node daemon request failed".to_string())
                );
            }
            return Ok(response.result.unwrap_or(Value::Null));
        }
        unreachable!("daemon request loop returns or errors")
    }

    async fn attach(operation: &str, session_id: &str, argv: &[String]) -> Result<()> {
        validate_session_id(session_id)?;
        let mut attached_stream = None;
        for attempt in 0..2 {
            let socket = ensure_daemon().await?;
            let mut stream = UnixStream::connect(&socket).await?;
            let request = json!({
                "operation": operation,
                "session_id": session_id,
                "argv": argv,
                "params": {},
            });
            stream
                .write_all(format!("{}\n", serde_json::to_string(&request)?).as_bytes())
                .await?;
            stream.flush().await?;
            let response = match read_response(&mut stream).await {
                Ok(response) => response,
                Err(_) if attempt == 0 => {
                    // See request(): one bounded retry crosses an intentional
                    // old-binary daemon shutdown after verified bootstrap.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if !response.ok || !response.attach {
                bail!(
                    "{}",
                    response
                        .error
                        .unwrap_or_else(|| "wta-node daemon refused attachment".to_string())
                );
            }
            attached_stream = Some(stream);
            break;
        }
        let stream = attached_stream.context("wta-node daemon attachment retry exhausted")?;

        let (mut socket_read, mut socket_write) = stream.into_split();
        let to_node = async {
            let mut stdin = tokio::io::stdin();
            tokio::io::copy(&mut stdin, &mut socket_write).await?;
            socket_write.shutdown().await?;
            Result::<()>::Ok(())
        };
        let from_node = async {
            let mut stdout = tokio::io::stdout();
            tokio::io::copy(&mut socket_read, &mut stdout).await?;
            stdout.flush().await?;
            Result::<()>::Ok(())
        };
        tokio::select! {
            result = to_node => result,
            result = from_node => result,
        }
    }

    async fn read_response(stream: &mut UnixStream) -> Result<DaemonResponse> {
        // Read exactly through the first newline. BufReader is intentionally
        // avoided here because the daemon may immediately append ACP backlog
        // bytes after the response line.
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while stream.read_exact(&mut byte).await.is_ok() {
            line.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
            if line.len() > 64 * 1024 {
                bail!("wta-node daemon response is too large");
            }
        }
        if line.is_empty() || *line.last().unwrap_or(&0) != b'\n' {
            bail!("wta-node daemon closed before sending a response");
        }
        Ok(serde_json::from_slice(&line)?)
    }

    pub async fn relay_dispatch(method: &str, params: &Value) -> Result<Value> {
        if !method.starts_with("relay.") {
            bail!("remote relay method must start with relay.");
        }
        for attempt in 0..2 {
            let request = json!({
                "operation": method,
                "session_id": null,
                "argv": [],
                "params": params,
            });
            let response = match async {
                let socket = ensure_daemon().await?;
                let mut stream = UnixStream::connect(&socket).await?;
                stream
                    .write_all(format!("{}\n", serde_json::to_string(&request)?).as_bytes())
                    .await?;
                stream.flush().await?;
                read_response(&mut stream).await
            }
            .await
            {
                Ok(response) => response,
                Err(_) if attempt == 0 => {
                    // A verified helper upgrade retires the old daemon. The
                    // connection may close before, during, or after the first
                    // write, so the one rollover retry covers the complete
                    // request transaction rather than only response parsing.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if !response.ok {
                bail!(
                    "{}",
                    response
                        .error
                        .unwrap_or_else(|| "wta-node relay request failed".to_string())
                );
            }
            return Ok(response.result.unwrap_or(Value::Null));
        }
        unreachable!("relay request loop returns or errors")
    }

    fn secure_dir(path: &Path) -> Result<()> {
        std::fs::create_dir_all(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    fn current_executable_sha256() -> Result<String> {
        let executable = std::env::current_exe().context("resolve current wta-node executable")?;
        super::super::snapshot::sha256_file(&executable)
            .with_context(|| format!("hash current wta-node executable {}", executable.display()))
    }

    fn daemon_binary_changed(started_sha256: &str, current_sha256: &str) -> bool {
        started_sha256 != current_sha256
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    fn argv_digest(argv: &[String]) -> String {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        for argument in argv {
            digest.update((argument.len() as u64).to_le_bytes());
            digest.update(argument.as_bytes());
        }
        format!("{:x}", digest.finalize())
    }

    pub async fn acp_start(session_id: &str, argv: &[String]) -> Result<()> {
        attach("start", session_id, argv).await
    }

    pub async fn acp_attach(session_id: &str) -> Result<()> {
        attach("attach", session_id, &[]).await
    }

    pub async fn acp_detach(session_id: &str) -> Result<Value> {
        request("detach", Some(session_id), &[]).await
    }

    pub async fn acp_stop(session_id: &str) -> Result<Value> {
        request("stop", Some(session_id), &[]).await
    }

    pub async fn acp_list() -> Result<Value> {
        request("list", None, &[]).await
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn detached_transport_reattaches_to_the_same_adapter_process() {
            let root = std::env::temp_dir()
                .join(format!("wta-node-session-test-{}", uuid::Uuid::new_v4()));
            let state = DaemonState::load(root.clone()).await.unwrap();
            let secret_marker = "not-persisted-secret";
            let argv = vec!["/bin/cat".to_string(), format!("--ignored-{secret_marker}")];

            // `cat` rejects arbitrary arguments, so use a shell-free executable
            // invocation with no secret for the actual process and separately
            // assert the registry's sanitized schema below.
            let process_argv = vec!["/bin/cat".to_string()];
            let session = state
                .start_session("surface-test", &process_argv)
                .await
                .unwrap();
            let first_pid = session.record.lock().await.pid;

            let (server, mut client) = UnixStream::pair().unwrap();
            let attached_session = Arc::clone(&session);
            let first_attach =
                tokio::spawn(async move { attach_stream(attached_session, server).await });
            let response = read_response(&mut client).await.unwrap();
            assert!(response.ok && response.attach);
            client.write_all(b"one\n").await.unwrap();
            let mut echoed = [0u8; 4];
            tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut echoed))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&echoed, b"one\n");
            drop(client);
            tokio::time::timeout(Duration::from_secs(2), first_attach)
                .await
                .unwrap()
                .unwrap()
                .unwrap();

            *session.initialize_response.lock().await = Some(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"protocolVersion": 1}
            }));
            let (server, mut client) = UnixStream::pair().unwrap();
            let attached_session = Arc::clone(&session);
            let second_attach =
                tokio::spawn(async move { attach_stream(attached_session, server).await });
            let response = read_response(&mut client).await.unwrap();
            assert!(response.ok && response.attach);
            assert_eq!(session.record.lock().await.pid, first_pid);

            client
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"initialize\",\"params\":{}}\n",
                )
                .await
                .unwrap();
            let mut replay_value = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"protocolVersion": 1}
            });
            replay_value["id"] = json!(42);
            let mut expected_replay = serde_json::to_vec(&replay_value).unwrap();
            expected_replay.push(b'\n');
            let mut actual_replay = vec![0; expected_replay.len()];
            tokio::time::timeout(
                Duration::from_secs(2),
                client.read_exact(&mut actual_replay),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(actual_replay, expected_replay);

            session
                .downstream_request_ids
                .lock()
                .await
                .insert("\"wta-99\"".to_string(), json!(7));
            let routed_response = session
                .route_adapter_frame(
                    b"{\"jsonrpc\":\"2.0\",\"id\":\"wta-99\",\"result\":{\"ok\":true}}\n",
                )
                .await;
            let routed_value: Value = serde_json::from_slice(&routed_response).unwrap();
            assert_eq!(routed_value["id"], json!(7));

            client.write_all(b"two\n").await.unwrap();
            let mut echoed = [0u8; 4];
            tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut echoed))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&echoed, b"two\n");
            drop(client);
            tokio::time::timeout(Duration::from_secs(2), second_attach)
                .await
                .unwrap()
                .unwrap()
                .unwrap();

            // Persisted metadata exposes neither complete argv nor secrets.
            let sanitized = RemoteSessionRecord {
                schema_version: REMOTE_SESSION_SCHEMA_VERSION,
                session_id: "sanitized".to_string(),
                program: "adapter".to_string(),
                argv_sha256: argv_digest(&argv),
                pid: None,
                state: RemoteSessionState::Exited,
                attached: false,
                created_at_ms: 0,
                updated_at_ms: 0,
                exit_code: Some(0),
                last_error: None,
            };
            state.persist_record(sanitized).await.unwrap();
            let registry = std::fs::read_to_string(root.join(REGISTRY_FILE)).unwrap();
            assert!(!registry.contains(secret_marker));
            assert!(!registry.contains("\"argv\":"));

            session.stop().await.unwrap();
            let _ = std::fs::remove_dir_all(root);
        }

        #[tokio::test]
        async fn two_surface_sessions_have_distinct_processes_and_isolated_streams() {
            let root = std::env::temp_dir().join(format!(
                "wta-node-session-isolation-test-{}",
                uuid::Uuid::new_v4()
            ));
            let state = DaemonState::load(root.clone()).await.unwrap();
            let process_argv = vec!["/bin/cat".to_string()];
            let session_a = state
                .start_session("surface-a", &process_argv)
                .await
                .unwrap();
            let session_b = state
                .start_session("surface-b", &process_argv)
                .await
                .unwrap();

            let pid_a = session_a.record.lock().await.pid.unwrap();
            let pid_b = session_b.record.lock().await.pid.unwrap();
            assert_ne!(pid_a, pid_b, "surfaces must not share an adapter process");

            let (server_a, mut client_a) = UnixStream::pair().unwrap();
            let (server_b, mut client_b) = UnixStream::pair().unwrap();
            let attach_a = {
                let session = Arc::clone(&session_a);
                tokio::spawn(async move { attach_stream(session, server_a).await })
            };
            let attach_b = {
                let session = Arc::clone(&session_b);
                tokio::spawn(async move { attach_stream(session, server_b).await })
            };
            assert!(read_response(&mut client_a).await.unwrap().attach);
            assert!(read_response(&mut client_b).await.unwrap().attach);

            client_a.write_all(b"surface-a\n").await.unwrap();
            let mut echoed_a = [0u8; 10];
            tokio::time::timeout(Duration::from_secs(2), client_a.read_exact(&mut echoed_a))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&echoed_a, b"surface-a\n");
            assert!(
                tokio::time::timeout(Duration::from_millis(100), client_b.read_u8())
                    .await
                    .is_err(),
                "surface-a output leaked into surface-b"
            );

            client_b.write_all(b"surface-b\n").await.unwrap();
            let mut echoed_b = [0u8; 10];
            tokio::time::timeout(Duration::from_secs(2), client_b.read_exact(&mut echoed_b))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&echoed_b, b"surface-b\n");

            drop(client_a);
            drop(client_b);
            tokio::time::timeout(Duration::from_secs(2), attach_a)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), attach_b)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            session_a.stop().await.unwrap();
            session_b.stop().await.unwrap();
            let _ = std::fs::remove_dir_all(root);
        }

        #[test]
        fn daemon_upgrade_is_detected_by_executable_digest() {
            assert!(!daemon_binary_changed("same", "same"));
            assert!(daemon_binary_changed("old", "new"));
        }
    }
}

#[cfg(unix)]
pub use unix::{
    acp_attach, acp_detach, acp_list, acp_start, acp_stop, daemon_serve, relay_dispatch,
};

#[cfg(not(unix))]
mod unsupported {
    use anyhow::{bail, Result};
    use serde_json::Value;

    pub async fn daemon_serve() -> Result<()> {
        bail!("persistent wta-node ACP sessions currently require a Unix remote node")
    }
    pub async fn acp_start(_session_id: &str, _argv: &[String]) -> Result<()> {
        bail!("persistent wta-node ACP sessions currently require a Unix remote node")
    }
    pub async fn acp_attach(_session_id: &str) -> Result<()> {
        bail!("persistent wta-node ACP sessions currently require a Unix remote node")
    }
    pub async fn acp_detach(_session_id: &str) -> Result<Value> {
        bail!("persistent wta-node ACP sessions currently require a Unix remote node")
    }
    pub async fn acp_stop(_session_id: &str) -> Result<Value> {
        bail!("persistent wta-node ACP sessions currently require a Unix remote node")
    }
    pub async fn acp_list() -> Result<Value> {
        bail!("persistent wta-node ACP sessions currently require a Unix remote node")
    }
    pub async fn relay_dispatch(_method: &str, _params: &Value) -> Result<Value> {
        bail!("persistent wta-node relay currently requires a Unix remote node")
    }
}

#[cfg(not(unix))]
pub use unsupported::{
    acp_attach, acp_detach, acp_list, acp_start, acp_stop, daemon_serve, relay_dispatch,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_session_ids_are_shell_independent_and_bounded() {
        for valid in ["surface-42", "adapter.codex_1", "A"] {
            validate_session_id(valid).unwrap();
        }
        for invalid in ["", "has space", "../escape", "a/b", "{guid}"] {
            assert!(validate_session_id(invalid).is_err(), "{invalid}");
        }
        assert!(validate_session_id(&"x".repeat(129)).is_err());
    }
}
