//! Persistent generic PTY sessions for SSH-backed terminal surfaces.
//!
//! Each session is owned by a private `wta-node pty serve` runtime. SSH is
//! only an attachment transport: dropping it does not terminate the PTY.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use super::session::RemoteSessionState;

pub const PTY_SESSION_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtySessionRecord {
    pub schema_version: u16,
    pub session_id: String,
    pub program: String,
    pub pid: Option<u32>,
    pub state: RemoteSessionState,
    pub attachments: u32,
    pub effective_cols: u16,
    pub effective_rows: u16,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

pub fn validate_dimensions(cols: u16, rows: u16) -> Result<()> {
    if !(2..=1_000).contains(&cols) || !(2..=1_000).contains(&rows) {
        bail!("PTY dimensions must be within 2..=1000");
    }
    Ok(())
}

#[cfg(unix)]
mod unix {
    use std::collections::BTreeMap;
    use std::fs::OpenOptions;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt as _;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use anyhow::{bail, Context, Result};
    use nix::pty::{openpty, Winsize};
    use serde::{Deserialize, Serialize};
    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::process::Command;
    use tokio::sync::{broadcast, Mutex};
    use uuid::Uuid;

    use super::{validate_dimensions, PtySessionRecord, PTY_SESSION_SCHEMA_VERSION};
    use crate::compute::session::{validate_session_id, RemoteSessionState};

    const SOCKET_FILE: &str = "pty.sock";
    const RECORD_FILE: &str = "record.json";
    const BACKLOG_LIMIT: usize = 4 * 1024 * 1024;

    #[derive(Debug, Serialize, Deserialize)]
    struct PtyRequest {
        operation: String,
        #[serde(default)]
        attachment_id: Option<String>,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct PtyResponse {
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default)]
        attach: bool,
    }

    struct PtyRuntime {
        root: PathBuf,
        record: Mutex<PtySessionRecord>,
        writer: Mutex<tokio::fs::File>,
        resize_fd: OwnedFd,
        attachments: Mutex<BTreeMap<String, (u16, u16)>>,
        backlog: Mutex<Vec<u8>>,
        output: broadcast::Sender<Vec<u8>>,
    }

    impl PtyRuntime {
        async fn persist(&self) {
            let record = self.record.lock().await.clone();
            let _ = write_record(&self.root, &record);
        }

        async fn resize(&self, attachment_id: &str, cols: u16, rows: u16) -> Result<(u16, u16)> {
            validate_dimensions(cols, rows)?;
            let (effective_cols, effective_rows) = {
                let mut attachments = self.attachments.lock().await;
                attachments.insert(attachment_id.to_string(), (cols, rows));
                effective_dimensions(&attachments)
            };
            set_winsize(&self.resize_fd, effective_cols, effective_rows)?;
            {
                let mut record = self.record.lock().await;
                record.attachments = self.attachments.lock().await.len() as u32;
                record.effective_cols = effective_cols;
                record.effective_rows = effective_rows;
                record.updated_at_ms = now_ms();
            }
            self.persist().await;
            Ok((effective_cols, effective_rows))
        }

        async fn detach(&self, attachment_id: &str) {
            let (effective_cols, effective_rows, count) = {
                let mut attachments = self.attachments.lock().await;
                attachments.remove(attachment_id);
                let (cols, rows) = effective_dimensions(&attachments);
                (cols, rows, attachments.len() as u32)
            };
            if count > 0 {
                let _ = set_winsize(&self.resize_fd, effective_cols, effective_rows);
            }
            {
                let mut record = self.record.lock().await;
                record.attachments = count;
                record.effective_cols = effective_cols;
                record.effective_rows = effective_rows;
                record.state = if count == 0 {
                    RemoteSessionState::Detached
                } else {
                    RemoteSessionState::Attached
                };
                record.updated_at_ms = now_ms();
            }
            self.persist().await;
        }

        async fn push_backlog(&self, bytes: &[u8]) {
            let mut backlog = self.backlog.lock().await;
            backlog.extend_from_slice(bytes);
            if backlog.len() > BACKLOG_LIMIT {
                let overflow = backlog.len() - BACKLOG_LIMIT;
                backlog.drain(..overflow);
            }
        }
    }

    pub async fn serve(
        session_id: &str,
        argv: &[String],
        initial_cols: u16,
        initial_rows: u16,
    ) -> Result<()> {
        validate_session_id(session_id)?;
        validate_dimensions(initial_cols, initial_rows)?;
        if argv.is_empty() || argv[0].trim().is_empty() {
            bail!("PTY argv must contain an executable");
        }
        let root = session_root(session_id)?;
        secure_dir(&root)?;
        let socket_path = root.join(SOCKET_FILE);
        if socket_path.exists() {
            if UnixStream::connect(&socket_path).await.is_ok() {
                bail!("PTY session '{session_id}' is already running");
            }
            std::fs::remove_file(&socket_path)?;
        }

        let winsize = Winsize {
            ws_row: initial_rows,
            ws_col: initial_cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let opened = openpty(Some(&winsize), None).context("open PTY")?;
        let master_reader = duplicate_fd(&opened.master)?;
        let master_writer = duplicate_fd(&opened.master)?;
        let resize_fd = opened.master;
        let slave_stdout = duplicate_fd(&opened.slave)?;
        let slave_stderr = duplicate_fd(&opened.slave)?;
        let slave_stdin = opened.slave;

        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .env_remove("WT_COM_CLSID")
            .env_remove("WT_PROTOCOL_TOKEN")
            .stdin(Stdio::from(std::fs::File::from(slave_stdin)))
            .stdout(Stdio::from(std::fs::File::from(slave_stdout)))
            .stderr(Stdio::from(std::fs::File::from(slave_stderr)));
        unsafe {
            command.as_std_mut().pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::other)?;
                if nix::libc::ioctl(0, nix::libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("start PTY program '{}'", argv[0]))?;
        let pid = child.id().context("PTY child has no pid")?;
        let record = PtySessionRecord {
            schema_version: PTY_SESSION_SCHEMA_VERSION,
            session_id: session_id.to_string(),
            program: Path::new(&argv[0])
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or(&argv[0])
                .to_string(),
            pid: Some(pid),
            state: RemoteSessionState::Running,
            attachments: 0,
            effective_cols: initial_cols,
            effective_rows: initial_rows,
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
            exit_code: None,
            last_error: None,
        };
        write_record(&root, &record)?;
        let (output, _) = broadcast::channel(512);
        let runtime = Arc::new(PtyRuntime {
            root: root.clone(),
            record: Mutex::new(record),
            writer: Mutex::new(tokio::fs::File::from_std(std::fs::File::from(
                master_writer,
            ))),
            resize_fd,
            attachments: Mutex::new(BTreeMap::new()),
            backlog: Mutex::new(Vec::new()),
            output,
        });

        let reader_runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            let mut reader = tokio::fs::File::from_std(std::fs::File::from(master_reader));
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(count) => {
                        let bytes = buffer[..count].to_vec();
                        reader_runtime.push_backlog(&bytes).await;
                        let _ = reader_runtime.output.send(bytes);
                    }
                    Err(error) => {
                        let mut record = reader_runtime.record.lock().await;
                        record.last_error = Some(format!("PTY read failed: {error}"));
                        record.updated_at_ms = now_ms();
                        drop(record);
                        reader_runtime.persist().await;
                        break;
                    }
                }
            }
        });

        let listener = UnixListener::bind(&socket_path)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let runtime = Arc::clone(&runtime);
                    tokio::spawn(async move {
                        if let Err(error) = handle_connection(runtime, stream).await {
                            eprintln!("PTY attachment failed: {error:#}");
                        }
                    });
                }
                status = child.wait() => {
                    let status = status?;
                    {
                        let mut record = runtime.record.lock().await;
                        record.state = RemoteSessionState::Exited;
                        record.pid = None;
                        record.exit_code = status.code();
                        record.updated_at_ms = now_ms();
                    }
                    runtime.persist().await;
                    let _ = std::fs::remove_file(&socket_path);
                    return Ok(());
                }
            }
        }
    }

    async fn handle_connection(runtime: Arc<PtyRuntime>, mut stream: UnixStream) -> Result<()> {
        let request = read_request(&mut stream).await?;
        match request.operation.as_str() {
            "attach" => attach(runtime, stream, request).await,
            "resize" => {
                let attachment = request
                    .attachment_id
                    .as_deref()
                    .context("resize requires attachment_id")?;
                let (cols, rows) = runtime
                    .resize(
                        attachment,
                        request.cols.context("resize requires cols")?,
                        request.rows.context("resize requires rows")?,
                    )
                    .await?;
                write_response(
                    &mut stream,
                    PtyResponse {
                        ok: true,
                        result: Some(json!({"effective_cols": cols, "effective_rows": rows})),
                        error: None,
                        attach: false,
                    },
                )
                .await
            }
            "status" => {
                let record = runtime.record.lock().await.clone();
                let mut value = serde_json::to_value(&record)?;
                if let (Some(pid), Some(object)) = (record.pid, value.as_object_mut()) {
                    object.insert("metrics".into(), process_metrics(pid));
                }
                write_response(
                    &mut stream,
                    PtyResponse {
                        ok: true,
                        result: Some(value),
                        error: None,
                        attach: false,
                    },
                )
                .await
            }
            "stop" => {
                let pid = runtime
                    .record
                    .lock()
                    .await
                    .pid
                    .context("PTY is not running")?;
                signal_group(pid, nix::sys::signal::Signal::SIGTERM)?;
                write_response(
                    &mut stream,
                    PtyResponse {
                        ok: true,
                        result: Some(json!({"stopping": true, "pid": pid})),
                        error: None,
                        attach: false,
                    },
                )
                .await
            }
            other => bail!("unknown PTY operation: {other}"),
        }
    }

    fn process_metrics(pid: u32) -> Value {
        let mut rss_bytes = None;
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
            rss_bytes = status.lines().find_map(|line| {
                let value = line.strip_prefix("VmRSS:")?.trim();
                let kib = value.split_whitespace().next()?.parse::<u64>().ok()?;
                Some(kib.saturating_mul(1024))
            });
        }

        let mut user_cpu_ms = None;
        let mut system_cpu_ms = None;
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            if let Some(close) = stat.rfind(')') {
                let fields = stat[close + 1..].split_whitespace().collect::<Vec<_>>();
                if fields.len() > 12 {
                    let ticks = unsafe { nix::libc::sysconf(nix::libc::_SC_CLK_TCK) };
                    if ticks > 0 {
                        user_cpu_ms = fields[11]
                            .parse::<u64>()
                            .ok()
                            .map(|value| value.saturating_mul(1000) / ticks as u64);
                        system_cpu_ms = fields[12]
                            .parse::<u64>()
                            .ok()
                            .map(|value| value.saturating_mul(1000) / ticks as u64);
                    }
                }
            }
        }
        json!({
            "pid": pid,
            "rss_bytes": rss_bytes,
            "user_cpu_ms": user_cpu_ms,
            "system_cpu_ms": system_cpu_ms,
        })
    }

    async fn attach(
        runtime: Arc<PtyRuntime>,
        mut stream: UnixStream,
        request: PtyRequest,
    ) -> Result<()> {
        let attachment = request
            .attachment_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let (cols, rows) = runtime
            .resize(
                &attachment,
                request.cols.unwrap_or(120),
                request.rows.unwrap_or(30),
            )
            .await?;
        write_response(
            &mut stream,
            PtyResponse {
                ok: true,
                result: Some(json!({
                    "attachment_id": attachment,
                    "effective_cols": cols,
                    "effective_rows": rows,
                    "pid": runtime.record.lock().await.pid,
                })),
                error: None,
                attach: true,
            },
        )
        .await?;
        let backlog = runtime.backlog.lock().await.clone();
        if !backlog.is_empty() {
            stream.write_all(&backlog).await?;
        }
        let mut output = runtime.output.subscribe();
        let (mut read, mut write) = stream.into_split();
        let input_runtime = Arc::clone(&runtime);
        let input = async {
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let count = read.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                let mut writer = input_runtime.writer.lock().await;
                writer.write_all(&buffer[..count]).await?;
                writer.flush().await?;
            }
            Result::<()>::Ok(())
        };
        let outgoing = async {
            loop {
                match output.recv().await {
                    Ok(bytes) => {
                        write.write_all(&bytes).await?;
                        write.flush().await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // The bounded backlog is authoritative after a slow
                        // attachment reconnects; never silently claim lossless
                        // delivery on this live stream.
                        bail!("PTY attachment fell behind");
                    }
                }
            }
            Result::<()>::Ok(())
        };
        let result = tokio::select! {
            result = input => result,
            result = outgoing => result,
        };
        runtime.detach(&attachment).await;
        result
    }

    pub async fn start_and_attach(
        session_id: &str,
        argv: &[String],
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        validate_session_id(session_id)?;
        let (cols, rows) = terminal_dimensions().unwrap_or((cols, rows));
        validate_dimensions(cols, rows)?;
        if connect(session_id).await.is_err() {
            spawn_server(session_id, argv, cols, rows)?;
            wait_for_server(session_id).await?;
        }
        attach_client(session_id, cols, rows).await
    }

    pub async fn attach_client(session_id: &str, cols: u16, rows: u16) -> Result<()> {
        let (cols, rows) = terminal_dimensions().unwrap_or((cols, rows));
        validate_dimensions(cols, rows)?;
        let mut stream = connect(session_id).await?;
        let attachment_id = Uuid::new_v4().to_string();
        write_request(
            &mut stream,
            &PtyRequest {
                operation: "attach".into(),
                attachment_id: Some(attachment_id.clone()),
                cols: Some(cols),
                rows: Some(rows),
            },
        )
        .await?;
        let response = read_response(&mut stream).await?;
        if !response.ok || !response.attach {
            bail!(
                "{}",
                response
                    .error
                    .unwrap_or_else(|| "PTY server refused attachment".into())
            );
        }
        let (mut socket_read, mut socket_write) = stream.into_split();
        let to_node = async {
            let mut stdin = tokio::io::stdin();
            tokio::io::copy(&mut stdin, &mut socket_write).await?;
            Result::<()>::Ok(())
        };
        let from_node = async {
            let mut stdout = tokio::io::stdout();
            tokio::io::copy(&mut socket_read, &mut stdout).await?;
            stdout.flush().await?;
            Result::<()>::Ok(())
        };
        let resize_session = session_id.to_string();
        let resize_attachment = attachment_id.clone();
        let resize = async move {
            let mut signal =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
            while signal.recv().await.is_some() {
                if let Some((cols, rows)) = terminal_dimensions() {
                    let _ = resize_client(&resize_session, &resize_attachment, cols, rows).await;
                }
            }
            Result::<()>::Ok(())
        };
        tokio::select! {
            result = to_node => result,
            result = from_node => result,
            result = resize => result,
        }
    }

    pub async fn resize_client(
        session_id: &str,
        attachment_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<Value> {
        request(
            session_id,
            PtyRequest {
                operation: "resize".into(),
                attachment_id: Some(attachment_id.to_string()),
                cols: Some(cols),
                rows: Some(rows),
            },
        )
        .await
    }

    pub async fn status(session_id: &str) -> Result<Value> {
        request(
            session_id,
            PtyRequest {
                operation: "status".into(),
                attachment_id: None,
                cols: None,
                rows: None,
            },
        )
        .await
    }

    pub async fn stop(session_id: &str) -> Result<Value> {
        request(
            session_id,
            PtyRequest {
                operation: "stop".into(),
                attachment_id: None,
                cols: None,
                rows: None,
            },
        )
        .await
    }

    pub fn list() -> Result<Vec<PtySessionRecord>> {
        let root = crate::compute::node::node_state_root()?.join("pty");
        if !root.is_dir() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for entry in std::fs::read_dir(root)? {
            let path = entry?.path().join(RECORD_FILE);
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(record) = serde_json::from_slice(&bytes) {
                    records.push(record);
                }
            }
        }
        records.sort_by(|left: &PtySessionRecord, right: &PtySessionRecord| {
            left.session_id.cmp(&right.session_id)
        });
        Ok(records)
    }

    fn spawn_server(session_id: &str, argv: &[String], cols: u16, rows: u16) -> Result<()> {
        if argv.is_empty() {
            bail!("PTY start requires argv");
        }
        let root = session_root(session_id)?;
        secure_dir(&root)?;
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("server.log"))?;
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .arg("pty")
            .arg("serve")
            .arg("--session")
            .arg(session_id)
            .arg("--cols")
            .arg(cols.to_string())
            .arg("--rows")
            .arg(rows.to_string())
            .arg("--")
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::other)?;
                Ok(())
            });
        }
        command.spawn().context("start detached PTY server")?;
        Ok(())
    }

    fn terminal_dimensions() -> Option<(u16, u16)> {
        let mut winsize = nix::libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let result = unsafe {
            nix::libc::ioctl(
                std::io::stdin().as_raw_fd(),
                nix::libc::TIOCGWINSZ,
                &mut winsize,
            )
        };
        (result == 0)
            .then_some((winsize.ws_col, winsize.ws_row))
            .filter(|(cols, rows)| validate_dimensions(*cols, *rows).is_ok())
    }

    async fn wait_for_server(session_id: &str) -> Result<()> {
        for _ in 0..100 {
            if connect(session_id).await.is_ok() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        bail!("PTY server did not become ready")
    }

    async fn request(session_id: &str, request: PtyRequest) -> Result<Value> {
        let mut stream = connect(session_id).await?;
        write_request(&mut stream, &request).await?;
        let response = read_response(&mut stream).await?;
        if !response.ok {
            bail!(
                "{}",
                response
                    .error
                    .unwrap_or_else(|| "PTY request failed".into())
            );
        }
        Ok(response.result.unwrap_or(Value::Null))
    }

    async fn connect(session_id: &str) -> Result<UnixStream> {
        validate_session_id(session_id)?;
        Ok(UnixStream::connect(session_root(session_id)?.join(SOCKET_FILE)).await?)
    }

    fn session_root(session_id: &str) -> Result<PathBuf> {
        validate_session_id(session_id)?;
        Ok(crate::compute::node::node_state_root()?
            .join("pty")
            .join(session_id))
    }

    fn secure_dir(path: &Path) -> Result<()> {
        std::fs::create_dir_all(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    fn write_record(root: &Path, record: &PtySessionRecord) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(record)?;
        let temp = root.join(format!(".{RECORD_FILE}.{}.tmp", Uuid::new_v4()));
        std::fs::write(&temp, bytes)?;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(temp, root.join(RECORD_FILE))?;
        Ok(())
    }

    fn duplicate_fd(fd: &OwnedFd) -> Result<OwnedFd> {
        let duplicated = unsafe { nix::libc::dup(fd.as_raw_fd()) };
        if duplicated < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
    }

    fn set_winsize(fd: &OwnedFd, cols: u16, rows: u16) -> Result<()> {
        let winsize = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let result = unsafe { nix::libc::ioctl(fd.as_raw_fd(), nix::libc::TIOCSWINSZ, &winsize) };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn effective_dimensions(attachments: &BTreeMap<String, (u16, u16)>) -> (u16, u16) {
        attachments
            .values()
            .copied()
            .reduce(|left, right| (left.0.min(right.0), left.1.min(right.1)))
            .unwrap_or((120, 30))
    }

    fn signal_group(pid: u32, signal: nix::sys::signal::Signal) -> Result<()> {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(-(i32::try_from(pid)?)), signal)?;
        Ok(())
    }

    async fn read_request(stream: &mut UnixStream) -> Result<PtyRequest> {
        Ok(serde_json::from_slice(&read_line(stream).await?)?)
    }

    async fn read_response(stream: &mut UnixStream) -> Result<PtyResponse> {
        Ok(serde_json::from_slice(&read_line(stream).await?)?)
    }

    async fn read_line(stream: &mut UnixStream) -> Result<Vec<u8>> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while stream.read_exact(&mut byte).await.is_ok() {
            line.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
            if line.len() > 64 * 1024 {
                bail!("PTY control frame is too large");
            }
        }
        if line.last() != Some(&b'\n') {
            bail!("PTY server closed before completing control frame");
        }
        Ok(line)
    }

    async fn write_request(stream: &mut UnixStream, request: &PtyRequest) -> Result<()> {
        stream.write_all(&serde_json::to_vec(request)?).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;
        Ok(())
    }

    async fn write_response(stream: &mut UnixStream, response: PtyResponse) -> Result<()> {
        stream.write_all(&serde_json::to_vec(&response)?).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;
        Ok(())
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn smallest_screen_wins() {
            let attachments = BTreeMap::from([
                ("a".into(), (160, 50)),
                ("b".into(), (100, 70)),
                ("c".into(), (120, 30)),
            ]);
            assert_eq!(effective_dimensions(&attachments), (100, 30));
        }
    }
}

#[cfg(unix)]
pub use unix::*;

#[cfg(not(unix))]
mod unsupported {
    use anyhow::{bail, Result};
    use serde_json::Value;

    use super::PtySessionRecord;

    pub async fn serve(_: &str, _: &[String], _: u16, _: u16) -> Result<()> {
        bail!("persistent PTY sessions are currently supported on Unix wta-node targets")
    }
    pub async fn start_and_attach(_: &str, _: &[String], _: u16, _: u16) -> Result<()> {
        bail!("persistent PTY sessions are currently supported on Unix wta-node targets")
    }
    pub async fn attach_client(_: &str, _: u16, _: u16) -> Result<()> {
        bail!("persistent PTY sessions are currently supported on Unix wta-node targets")
    }
    pub async fn resize_client(_: &str, _: &str, _: u16, _: u16) -> Result<Value> {
        bail!("persistent PTY sessions are currently supported on Unix wta-node targets")
    }
    pub async fn status(_: &str) -> Result<Value> {
        bail!("persistent PTY sessions are currently supported on Unix wta-node targets")
    }
    pub async fn stop(_: &str) -> Result<Value> {
        bail!("persistent PTY sessions are currently supported on Unix wta-node targets")
    }
    pub fn list() -> Result<Vec<PtySessionRecord>> {
        Ok(Vec::new())
    }
}

#[cfg(not(unix))]
pub use unsupported::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_fail_closed() {
        assert!(validate_dimensions(1, 30).is_err());
        assert!(validate_dimensions(120, 1).is_err());
        assert!(validate_dimensions(120, 30).is_ok());
    }
}
