use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const RUNTIME_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneActivity {
    Starting,
    Idle,
    Working,
    Attention,
    Error,
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimePane {
    pub logical_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub role: String,
    pub cwd: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub activity: PaneActivity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_notification: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRuntime {
    pub schema_version: u8,
    pub workspace_id: String,
    pub name: String,
    pub manifest_path: String,
    pub root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    pub panes: BTreeMap<String, RuntimePane>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl WorkspaceRuntime {
    pub fn new(name: &str, manifest_path: &Path, root: &Path) -> Self {
        let now = now_ms();
        Self {
            schema_version: RUNTIME_SCHEMA_VERSION,
            workspace_id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            manifest_path: manifest_path.to_string_lossy().into_owned(),
            root: root.to_string_lossy().into_owned(),
            tab_id: None,
            panes: BTreeMap::new(),
            created_at_ms: now,
            updated_at_ms: now,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceEvent {
    pub schema_version: u8,
    pub id: String,
    pub workspace_id: String,
    pub timestamp_ms: u64,
    pub kind: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub hop: u8,
    pub max_hops: u8,
    #[serde(default)]
    pub payload: Value,
}

impl WorkspaceEvent {
    pub fn new(
        workspace_id: &str,
        kind: &str,
        source: &str,
        target: Option<String>,
        payload: Value,
        max_hops: u8,
    ) -> Result<Self> {
        if workspace_id.trim().is_empty() {
            bail!("event workspace_id must not be empty");
        }
        if kind.trim().is_empty() {
            bail!("event kind must not be empty");
        }
        if source.trim().is_empty() {
            bail!("event source must not be empty");
        }
        if max_hops == 0 {
            bail!("event max_hops must be greater than zero");
        }
        Ok(Self {
            schema_version: 1,
            id: Uuid::new_v4().to_string(),
            workspace_id: workspace_id.to_string(),
            timestamp_ms: now_ms(),
            kind: kind.to_string(),
            source: source.to_string(),
            target,
            correlation_id: None,
            hop: 0,
            max_hops,
            payload,
        })
    }

    pub fn forwarded(&self, source: &str, target: Option<String>) -> Result<Self> {
        if source.trim().is_empty() {
            bail!("forwarded event source must not be empty");
        }
        if self.hop >= self.max_hops {
            bail!(
                "event {} reached its loop-safety hop limit {}",
                self.id,
                self.max_hops
            );
        }
        let mut forwarded = self.clone();
        forwarded.id = Uuid::new_v4().to_string();
        forwarded.source = source.to_string();
        forwarded.target = target;
        forwarded.timestamp_ms = now_ms();
        forwarded.hop += 1;
        forwarded.correlation_id = Some(
            self.correlation_id
                .clone()
                .unwrap_or_else(|| self.id.clone()),
        );
        Ok(forwarded)
    }

    /// Wrap the durable workspace event in the Terminal Protocol broadcast
    /// envelope. SendEvent requires `params.event`; publishing the durable
    /// record directly is rejected with E_INVALIDARG.
    pub fn protocol_envelope(&self) -> Result<Value> {
        let Value::Object(mut params) = serde_json::to_value(self)? else {
            bail!("workspace event must serialize as a JSON object");
        };
        params.insert("event".to_string(), Value::String(self.kind.clone()));
        Ok(serde_json::json!({
            "type": "event",
            "method": "agent_event",
            "params": params,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceMetrics {
    pub total_events: usize,
    pub unread_events: usize,
    pub by_kind: BTreeMap<String, usize>,
    pub by_source: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_event_at_ms: Option<u64>,
}

pub struct WorkspaceStore {
    directory: PathBuf,
}

impl WorkspaceStore {
    pub fn open(root: &Path, workspace_name: &str) -> Result<Self> {
        let slug = workspace_slug(workspace_name)?;
        let directory = root
            .join(".intelligent-terminal")
            .join("workspaces")
            .join(slug);
        fs::create_dir_all(&directory)
            .with_context(|| format!("failed to create workspace store {}", directory.display()))?;
        Ok(Self { directory })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Discover the declarative workspace associated with a live Terminal tab.
    ///
    /// The sidebar also represents ordinary tabs, so absence is not an error:
    /// callers simply fall back to an ad-hoc runtime. A tab-id match wins;
    /// otherwise the newest workspace rooted at `root` is returned. This
    /// makes restored tabs useful after their ephemeral Terminal StableId has
    /// changed while avoiding creation of any workspace directories during a
    /// read-only context refresh.
    pub fn discover(root: &Path, tab_id: Option<&str>) -> Result<Option<WorkspaceRuntime>> {
        let runtimes = Self::discover_all(root)?;
        if let Some(tab_id) = tab_id {
            if let Some(runtime) = runtimes
                .iter()
                .find(|runtime| runtime.tab_id.as_deref() == Some(tab_id))
            {
                return Ok(Some(runtime.clone()));
            }
        }
        Ok(runtimes.into_iter().next())
    }

    /// Discover every persisted workspace rooted at `root`, newest first.
    ///
    /// This operation is intentionally read-only. It powers Fleet and the
    /// workspace switcher without creating `.intelligent-terminal` in ordinary
    /// repositories merely because their tabs are visible.
    pub fn discover_all(root: &Path) -> Result<Vec<WorkspaceRuntime>> {
        let base = root.join(".intelligent-terminal").join("workspaces");
        if !base.is_dir() {
            return Ok(Vec::new());
        }

        let requested_root = normalize_path_for_match(root);
        let mut runtimes = Vec::new();
        for entry in fs::read_dir(&base)
            .with_context(|| format!("failed to read workspace store {}", base.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let state_path = entry.path().join("state.json");
            let Ok(bytes) = fs::read(&state_path) else {
                continue;
            };
            let Ok(runtime) = serde_json::from_slice::<WorkspaceRuntime>(&bytes) else {
                continue;
            };
            if runtime.schema_version != RUNTIME_SCHEMA_VERSION
                || normalize_path_for_match(Path::new(&runtime.root)) != requested_root
            {
                continue;
            }
            runtimes.push(runtime);
        }
        runtimes.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(runtimes)
    }

    pub fn save_runtime(&self, runtime: &mut WorkspaceRuntime) -> Result<()> {
        runtime.updated_at_ms = now_ms();
        let bytes = serde_json::to_vec_pretty(runtime)?;
        atomic_write(&self.directory.join("state.json"), &bytes)
    }

    pub fn load_runtime(&self) -> Result<WorkspaceRuntime> {
        let path = self.directory.join("state.json");
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read workspace state {}", path.display()))?;
        let state: WorkspaceRuntime = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid workspace state {}", path.display()))?;
        if state.schema_version != RUNTIME_SCHEMA_VERSION {
            bail!(
                "unsupported workspace runtime schema_version {}",
                state.schema_version
            );
        }
        Ok(state)
    }

    pub fn create_snapshot(&self, runtime: &WorkspaceRuntime) -> Result<PathBuf> {
        let snapshots = self.directory.join("snapshots");
        fs::create_dir_all(&snapshots)?;
        let path = snapshots.join(format!("{}.json", now_ms()));
        let bytes = serde_json::to_vec_pretty(runtime)?;
        atomic_write(&path, &bytes)?;
        Ok(path)
    }

    pub fn load_snapshot(&self, path: &Path) -> Result<WorkspaceRuntime> {
        let canonical_snapshots = self.directory.join("snapshots").canonicalize()?;
        let canonical_path = path
            .canonicalize()
            .with_context(|| format!("failed to resolve snapshot {}", path.display()))?;
        if !canonical_path.starts_with(&canonical_snapshots) {
            bail!("snapshot must be inside {}", canonical_snapshots.display());
        }
        let bytes = fs::read(&canonical_path)?;
        let runtime: WorkspaceRuntime = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid workspace snapshot {}", canonical_path.display()))?;
        if runtime.schema_version != RUNTIME_SCHEMA_VERSION {
            bail!(
                "unsupported workspace snapshot schema_version {}",
                runtime.schema_version
            );
        }
        Ok(runtime)
    }

    pub fn append_event(&self, event: &WorkspaceEvent) -> Result<()> {
        if event.hop > event.max_hops {
            bail!("event {} exceeds its hop limit", event.id);
        }
        let path = self.directory.join("events.jsonl");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open event log {}", path.display()))?;
        let mut record = serde_json::to_vec(event)?;
        record.push(b'\n');
        file.write_all(&record)?;
        file.flush()?;
        Ok(())
    }

    pub fn events(&self) -> Result<Vec<WorkspaceEvent>> {
        let path = self.directory.join("events.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = fs::File::open(&path)?;
        BufReader::new(file)
            .lines()
            .enumerate()
            .filter_map(|(index, line)| match line {
                Ok(line) if line.trim().is_empty() => None,
                value => Some((index, value)),
            })
            .map(|(index, line)| {
                let line = line?;
                serde_json::from_str(&line)
                    .with_context(|| format!("invalid event at {}:{}", path.display(), index + 1))
            })
            .collect()
    }

    /// Return the newest `limit` durable events in chronological order.
    ///
    /// Reading the complete JSONL log remains the source of truth; the bounded
    /// result prevents the native sidebar from retaining an unbounded history.
    pub fn events_tail(&self, limit: usize) -> Result<Vec<WorkspaceEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut events = self.events()?;
        if events.len() > limit {
            events.drain(..events.len() - limit);
        }
        Ok(events)
    }

    pub fn inbox(&self, recipient: &str, after_ms: Option<u64>) -> Result<Vec<WorkspaceEvent>> {
        let recipient = recipient.trim();
        if recipient.is_empty() {
            bail!("inbox recipient must not be empty");
        }
        Ok(self
            .events()?
            .into_iter()
            .filter(|event| {
                event.target.as_deref() == Some(recipient) || event.target.as_deref() == Some("*")
            })
            .filter(|event| after_ms.is_none_or(|after| event.timestamp_ms > after))
            .collect())
    }

    pub fn metrics(
        &self,
        recipient: Option<&str>,
        after_ms: Option<u64>,
    ) -> Result<WorkspaceMetrics> {
        let events = self.events()?;
        let mut by_kind = BTreeMap::new();
        let mut by_source = BTreeMap::new();
        let mut latest_event_at_ms: Option<u64> = None;
        let mut unread_events = 0;
        for event in &events {
            *by_kind.entry(event.kind.clone()).or_insert(0) += 1;
            *by_source.entry(event.source.clone()).or_insert(0) += 1;
            latest_event_at_ms = Some(
                latest_event_at_ms
                    .unwrap_or_default()
                    .max(event.timestamp_ms),
            );
            if recipient.is_some_and(|recipient| {
                (event.target.as_deref() == Some(recipient) || event.target.as_deref() == Some("*"))
                    && after_ms.is_none_or(|after| event.timestamp_ms > after)
            }) {
                unread_events += 1;
            }
        }
        Ok(WorkspaceMetrics {
            total_events: events.len(),
            unread_events,
            by_kind,
            by_source,
            latest_event_at_ms,
        })
    }
}

fn normalize_path_for_match(path: &Path) -> String {
    path.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .replace('/', "\\")
        .to_ascii_lowercase()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn workspace_slug(name: &str) -> Result<String> {
    let slug = name
        .trim()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if slug.is_empty() {
        bail!("workspace name does not contain a usable identifier");
    }
    Ok(slug)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("workspace state path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        Uuid::new_v4()
    ));
    fs::write(&temp, bytes)
        .with_context(|| format!("failed to write temporary state {}", temp.display()))?;
    atomic_replace(&temp, path)
        .with_context(|| format!("failed to commit state {}", path.display()))?;
    Ok(())
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source_path = source;
    let source = source_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replaced = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        let error = std::io::Error::last_os_error();
        let _ = fs::remove_file(source_path);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wta-workspace-{label}-{}", Uuid::new_v4()))
    }

    #[test]
    fn runtime_and_event_log_round_trip_with_inbox_and_metrics() {
        let root = temp_root("store");
        let store = WorkspaceStore::open(&root, "Feature Race").unwrap();
        let mut runtime =
            WorkspaceRuntime::new("Feature Race", &root.join(".agent-workspace.yaml"), &root);
        store.save_runtime(&mut runtime).unwrap();
        let first_update = runtime.updated_at_ms;
        runtime.tab_id = Some("tab-1".to_string());
        store.save_runtime(&mut runtime).unwrap();
        assert_eq!(
            store.load_runtime().unwrap().workspace_id,
            runtime.workspace_id
        );
        assert_eq!(
            store.load_runtime().unwrap().tab_id.as_deref(),
            Some("tab-1")
        );
        assert!(runtime.updated_at_ms >= first_update);

        let direct = WorkspaceEvent::new(
            &runtime.workspace_id,
            "agent.message",
            "builder",
            Some("reviewer".to_string()),
            serde_json::json!({"text": "ready"}),
            2,
        )
        .unwrap();
        let broadcast = WorkspaceEvent::new(
            &runtime.workspace_id,
            "workspace.notice",
            "orchestrator",
            Some("*".to_string()),
            serde_json::json!({"text": "tests green"}),
            2,
        )
        .unwrap();
        store.append_event(&direct).unwrap();
        store.append_event(&broadcast).unwrap();

        assert_eq!(store.inbox("reviewer", None).unwrap().len(), 2);
        let metrics = store.metrics(Some("reviewer"), Some(0)).unwrap();
        assert_eq!(metrics.total_events, 2);
        assert_eq!(metrics.unread_events, 2);
        assert_eq!(metrics.by_kind["agent.message"], 1);
        assert_eq!(store.events_tail(1).unwrap(), vec![broadcast]);
        assert!(store.events_tail(0).unwrap().is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discover_prefers_matching_tab_then_newest_workspace_at_root() {
        let root = temp_root("discover");
        let first_store = WorkspaceStore::open(&root, "First").unwrap();
        let mut first =
            WorkspaceRuntime::new("First", &root.join("first.agent-workspace.yaml"), &root);
        first.tab_id = Some("tab-first".to_string());
        first_store.save_runtime(&mut first).unwrap();

        let second_store = WorkspaceStore::open(&root, "Second").unwrap();
        let mut second =
            WorkspaceRuntime::new("Second", &root.join("second.agent-workspace.yaml"), &root);
        second.tab_id = Some("tab-second".to_string());
        second.updated_at_ms = first.updated_at_ms + 10;
        let bytes = serde_json::to_vec_pretty(&second).unwrap();
        atomic_write(&second_store.directory().join("state.json"), &bytes).unwrap();

        assert_eq!(
            WorkspaceStore::discover(&root, Some("tab-first"))
                .unwrap()
                .unwrap()
                .name,
            "First"
        );
        assert_eq!(
            WorkspaceStore::discover(&root, Some("unknown"))
                .unwrap()
                .unwrap()
                .name,
            "Second"
        );
        let discovered = WorkspaceStore::discover_all(&root).unwrap();
        assert_eq!(discovered.len(), 2);
        assert_eq!(discovered[0].name, "Second");
        assert_eq!(discovered[1].name, "First");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn forwarding_stops_at_configured_hop_limit() {
        let original = WorkspaceEvent::new(
            "workspace",
            "agent.message",
            "a",
            Some("b".to_string()),
            Value::Null,
            1,
        )
        .unwrap();
        let forwarded = original.forwarded("b", Some("a".to_string())).unwrap();
        assert_eq!(forwarded.hop, 1);
        assert!(forwarded.forwarded("a", Some("b".to_string())).is_err());
        assert!(original.forwarded(" ", Some("b".to_string())).is_err());
    }

    #[test]
    fn workspace_event_protocol_envelope_matches_send_event_contract() {
        let event = WorkspaceEvent::new(
            "workspace-1",
            "workspace.ready",
            "wta",
            Some("*".to_string()),
            serde_json::json!({"pane_count": 2}),
            3,
        )
        .unwrap();

        let envelope = event.protocol_envelope().unwrap();
        assert_eq!(envelope["type"], "event");
        assert_eq!(envelope["method"], "agent_event");
        assert_eq!(envelope["params"]["event"], "workspace.ready");
        assert_eq!(envelope["params"]["workspace_id"], "workspace-1");
        assert_eq!(envelope["params"]["payload"]["pane_count"], 2);
    }
}
