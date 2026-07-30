//! Native multi-agent team control plane.
//!
//! The terminal panes remain the execution surface. This module is the durable,
//! agent-neutral coordination layer: workers and tasks have stable IDs,
//! ownership is fail-closed, heartbeats expose stale workers, retries are
//! explicit, and every transition is appended to an audit timeline.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

pub const TEAM_SCHEMA_VERSION: u8 = 1;
const LOCK_WAIT: Duration = Duration::from_secs(5);
const STALE_LOCK_AGE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamStatus {
    Active,
    ShuttingDown,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Starting,
    Idle,
    Working,
    Stale,
    Stopping,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Assigned,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamWorker {
    pub id: String,
    pub role: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_session_id: Option<String>,
    pub status: WorkerStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_task_id: Option<String>,
    pub last_heartbeat_ms: u64,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTask {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub owns: Vec<String>,
    pub attempts: u32,
    pub max_attempts: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamState {
    pub schema_version: u8,
    pub team_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub name: String,
    pub root: String,
    pub leader: String,
    pub status: TeamStatus,
    pub stale_after_ms: u64,
    pub default_max_attempts: u32,
    pub workers: BTreeMap<String, TeamWorker>,
    pub tasks: BTreeMap<String, TeamTask>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl TeamState {
    fn new(
        name: &str,
        root: &Path,
        leader: &str,
        workspace_id: Option<&str>,
        stale_after_ms: u64,
        default_max_attempts: u32,
    ) -> Result<Self> {
        validate_identifier("team name", name)?;
        validate_identifier("leader", leader)?;
        if stale_after_ms < 1_000 {
            bail!("stale_after_ms must be at least 1000");
        }
        if default_max_attempts == 0 {
            bail!("default_max_attempts must be greater than zero");
        }
        let now = now_ms();
        Ok(Self {
            schema_version: TEAM_SCHEMA_VERSION,
            team_id: Uuid::new_v4().to_string(),
            workspace_id: workspace_id
                .filter(|id| !id.trim().is_empty())
                .map(str::to_string),
            name: name.to_string(),
            root: root.to_string_lossy().into_owned(),
            leader: leader.to_string(),
            status: TeamStatus::Active,
            stale_after_ms,
            default_max_attempts,
            workers: BTreeMap::new(),
            tasks: BTreeMap::new(),
            created_at_ms: now,
            updated_at_ms: now,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamEvent {
    pub schema_version: u8,
    pub id: String,
    pub team_id: String,
    pub timestamp_ms: u64,
    pub kind: String,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default)]
    pub payload: Value,
}

impl TeamEvent {
    fn new(
        state: &TeamState,
        kind: &str,
        actor: &str,
        worker_id: Option<String>,
        task_id: Option<String>,
        payload: Value,
    ) -> Self {
        Self {
            schema_version: TEAM_SCHEMA_VERSION,
            id: Uuid::new_v4().to_string(),
            team_id: state.team_id.clone(),
            timestamp_ms: now_ms(),
            kind: kind.to_string(),
            actor: actor.to_string(),
            worker_id,
            task_id,
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TeamDoctorReport {
    pub state_path: String,
    pub schema_ok: bool,
    pub active_workers: usize,
    pub stale_workers: Vec<String>,
    pub runnable_tasks: Vec<String>,
    pub blocked_tasks: BTreeMap<String, Vec<String>>,
    pub ownership_conflicts: Vec<String>,
}

pub struct TeamStore {
    directory: PathBuf,
}

impl TeamStore {
    pub fn create(
        root: &Path,
        name: &str,
        leader: &str,
        workspace_id: Option<&str>,
        stale_after_ms: u64,
        default_max_attempts: u32,
    ) -> Result<(Self, TeamState)> {
        let store = Self::at(root, name)?;
        fs::create_dir_all(&store.directory).with_context(|| {
            format!("failed to create team store {}", store.directory.display())
        })?;
        let _lock = store.acquire_lock()?;
        if store.state_path().exists() {
            bail!(
                "team {name} already exists at {}",
                store.directory.display()
            );
        }
        let state = TeamState::new(
            name,
            root,
            leader,
            workspace_id,
            stale_after_ms,
            default_max_attempts,
        )?;
        store.save_state(&state)?;
        store.append_events(&[TeamEvent::new(
            &state,
            "team.created",
            leader,
            None,
            None,
            json!({
                "stale_after_ms": stale_after_ms,
                "default_max_attempts": default_max_attempts,
                "workspace_id": workspace_id,
            }),
        )])?;
        Ok((store, state))
    }

    pub fn open(root: &Path, name: &str) -> Result<Self> {
        let store = Self::at(root, name)?;
        if !store.state_path().is_file() {
            bail!(
                "team {name} does not exist at {}",
                store.directory.display()
            );
        }
        Ok(store)
    }

    fn at(root: &Path, name: &str) -> Result<Self> {
        let slug = slug(name)?;
        Ok(Self {
            directory: root.join(".intelligent-terminal").join("teams").join(slug),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn state_path(&self) -> PathBuf {
        self.directory.join("state.json")
    }

    pub fn load(&self) -> Result<TeamState> {
        self.load_unlocked()
    }

    pub fn events(&self) -> Result<Vec<TeamEvent>> {
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
                serde_json::from_str(&line?).with_context(|| {
                    format!("invalid team event at {}:{}", path.display(), index + 1)
                })
            })
            .collect()
    }

    pub fn add_worker(
        &self,
        actor: &str,
        id: &str,
        role: &str,
        agent: &str,
        model: Option<String>,
        cwd: &Path,
        capabilities: Vec<String>,
    ) -> Result<TeamWorker> {
        validate_identifier("worker id", id)?;
        if role.trim().is_empty() {
            bail!("worker role must not be empty");
        }
        if agent.trim().is_empty() {
            bail!("worker agent must not be empty");
        }
        if !cwd.is_dir() {
            bail!("worker cwd is not a directory: {}", cwd.display());
        }
        self.transact(|state, events| {
            require_active(state)?;
            if state.workers.contains_key(id) {
                bail!("worker {id} already exists");
            }
            let worker = TeamWorker {
                id: id.to_string(),
                role: role.to_string(),
                agent: agent.to_string(),
                model,
                cwd: cwd.to_string_lossy().into_owned(),
                pane_session_id: None,
                status: WorkerStatus::Starting,
                current_task_id: None,
                last_heartbeat_ms: now_ms(),
                capabilities,
            };
            state.workers.insert(id.to_string(), worker.clone());
            events.push(TeamEvent::new(
                state,
                "worker.added",
                actor,
                Some(id.to_string()),
                None,
                json!({"role": role, "agent": agent, "cwd": worker.cwd}),
            ));
            Ok(worker)
        })
    }

    pub fn set_worker_pane(
        &self,
        actor: &str,
        worker_id: &str,
        pane_session_id: &str,
    ) -> Result<TeamWorker> {
        if pane_session_id.trim().is_empty() {
            bail!("pane session id must not be empty");
        }
        self.transact(|state, events| {
            let worker = state
                .workers
                .get_mut(worker_id)
                .ok_or_else(|| anyhow!("unknown worker: {worker_id}"))?;
            worker.pane_session_id = Some(pane_session_id.to_string());
            worker.status = WorkerStatus::Idle;
            worker.last_heartbeat_ms = now_ms();
            let snapshot = worker.clone();
            events.push(TeamEvent::new(
                state,
                "worker.ready",
                actor,
                Some(worker_id.to_string()),
                None,
                json!({"pane_session_id": pane_session_id}),
            ));
            Ok(snapshot)
        })
    }

    pub fn add_task(
        &self,
        actor: &str,
        id: Option<&str>,
        title: &str,
        prompt: &str,
        dependencies: Vec<String>,
        owns: Vec<String>,
        max_attempts: Option<u32>,
    ) -> Result<TeamTask> {
        if title.trim().is_empty() {
            bail!("task title must not be empty");
        }
        if prompt.trim().is_empty() {
            bail!("task prompt must not be empty");
        }
        let id = id
            .map(str::to_string)
            .unwrap_or_else(|| format!("task-{}", &Uuid::new_v4().simple().to_string()[..8]));
        validate_identifier("task id", &id)?;
        let owns = normalize_ownership(owns)?;
        self.transact(|state, events| {
            require_active(state)?;
            if state.tasks.contains_key(&id) {
                bail!("task {id} already exists");
            }
            for dependency in &dependencies {
                if dependency == &id {
                    bail!("task {id} cannot depend on itself");
                }
                if !state.tasks.contains_key(dependency) {
                    bail!("unknown dependency {dependency} for task {id}");
                }
            }
            let max_attempts = max_attempts.unwrap_or(state.default_max_attempts);
            if max_attempts == 0 {
                bail!("max_attempts must be greater than zero");
            }
            let now = now_ms();
            let task = TeamTask {
                id: id.clone(),
                title: title.to_string(),
                prompt: prompt.to_string(),
                status: TaskStatus::Pending,
                owner: None,
                dependencies,
                owns,
                attempts: 0,
                max_attempts,
                created_at_ms: now,
                updated_at_ms: now,
                started_at_ms: None,
                completed_at_ms: None,
                result: None,
                error: None,
            };
            state.tasks.insert(id.clone(), task.clone());
            events.push(TeamEvent::new(
                state,
                "task.created",
                actor,
                None,
                Some(id.clone()),
                json!({
                    "title": title,
                    "dependencies": task.dependencies,
                    "owns": task.owns,
                    "max_attempts": max_attempts,
                }),
            ));
            Ok(task)
        })
    }

    pub fn assign_task(&self, actor: &str, task_id: &str, worker_id: &str) -> Result<TeamTask> {
        self.transact(|state, events| {
            let task = begin_task(state, task_id, worker_id, TaskStatus::Assigned)?;
            events.push(TeamEvent::new(
                state,
                "task.assigned",
                actor,
                Some(worker_id.to_string()),
                Some(task_id.to_string()),
                json!({"attempt": task.attempts}),
            ));
            Ok(task)
        })
    }

    pub fn claim_task(&self, worker_id: &str, requested_task_id: Option<&str>) -> Result<TeamTask> {
        self.transact(|state, events| {
            require_active(state)?;
            let task_id = match requested_task_id {
                Some(task_id) => task_id.to_string(),
                None => state
                    .tasks
                    .values()
                    .find(|task| {
                        task.status == TaskStatus::Pending
                            && dependencies_satisfied(state, task)
                            && ownership_available(state, task, None).is_ok()
                    })
                    .map(|task| task.id.clone())
                    .ok_or_else(|| anyhow!("no runnable task is available"))?,
            };
            let task = begin_task(state, &task_id, worker_id, TaskStatus::Running)?;
            events.push(TeamEvent::new(
                state,
                "task.claimed",
                worker_id,
                Some(worker_id.to_string()),
                Some(task_id),
                json!({"attempt": task.attempts}),
            ));
            Ok(task)
        })
    }

    pub fn start_assigned_task(&self, worker_id: &str, task_id: &str) -> Result<TeamTask> {
        self.transact(|state, events| {
            let task = state
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| anyhow!("unknown task: {task_id}"))?;
            if task.status != TaskStatus::Assigned || task.owner.as_deref() != Some(worker_id) {
                bail!("task {task_id} is not assigned to worker {worker_id}");
            }
            task.status = TaskStatus::Running;
            task.updated_at_ms = now_ms();
            let snapshot = task.clone();
            events.push(TeamEvent::new(
                state,
                "task.started",
                worker_id,
                Some(worker_id.to_string()),
                Some(task_id.to_string()),
                json!({"attempt": snapshot.attempts}),
            ));
            Ok(snapshot)
        })
    }

    pub fn heartbeat(&self, worker_id: &str, task_id: Option<&str>) -> Result<TeamWorker> {
        self.transact(|state, events| {
            let now = now_ms();
            let worker = state
                .workers
                .get_mut(worker_id)
                .ok_or_else(|| anyhow!("unknown worker: {worker_id}"))?;
            if matches!(
                worker.status,
                WorkerStatus::Stopped | WorkerStatus::Stopping
            ) {
                bail!("worker {worker_id} is not accepting heartbeats");
            }
            if let Some(task_id) = task_id {
                if worker.current_task_id.as_deref() != Some(task_id) {
                    bail!("worker {worker_id} does not own task {task_id}");
                }
            }
            worker.last_heartbeat_ms = now;
            worker.status = if worker.current_task_id.is_some() {
                WorkerStatus::Working
            } else {
                WorkerStatus::Idle
            };
            let snapshot = worker.clone();
            events.push(TeamEvent::new(
                state,
                "worker.heartbeat",
                worker_id,
                Some(worker_id.to_string()),
                task_id.map(str::to_string),
                Value::Null,
            ));
            Ok(snapshot)
        })
    }

    pub fn complete_task(&self, worker_id: &str, task_id: &str, result: &str) -> Result<TeamTask> {
        self.finish_task(
            worker_id,
            task_id,
            TaskStatus::Succeeded,
            Some(result),
            None,
            "task.completed",
        )
    }

    pub fn fail_task(&self, worker_id: &str, task_id: &str, error: &str) -> Result<TeamTask> {
        self.finish_task(
            worker_id,
            task_id,
            TaskStatus::Failed,
            None,
            Some(error),
            "task.failed",
        )
    }

    fn finish_task(
        &self,
        worker_id: &str,
        task_id: &str,
        status: TaskStatus,
        result: Option<&str>,
        error: Option<&str>,
        event_kind: &str,
    ) -> Result<TeamTask> {
        self.transact(|state, events| {
            let now = now_ms();
            let task = state
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| anyhow!("unknown task: {task_id}"))?;
            if !matches!(task.status, TaskStatus::Assigned | TaskStatus::Running)
                || task.owner.as_deref() != Some(worker_id)
            {
                bail!("task {task_id} is not owned by worker {worker_id}");
            }
            task.status = status;
            task.updated_at_ms = now;
            task.completed_at_ms = Some(now);
            task.result = result.map(str::to_string);
            task.error = error.map(str::to_string);
            let snapshot = task.clone();
            let worker = state
                .workers
                .get_mut(worker_id)
                .ok_or_else(|| anyhow!("unknown worker: {worker_id}"))?;
            worker.current_task_id = None;
            worker.status = WorkerStatus::Idle;
            worker.last_heartbeat_ms = now;
            events.push(TeamEvent::new(
                state,
                event_kind,
                worker_id,
                Some(worker_id.to_string()),
                Some(task_id.to_string()),
                json!({"result": result, "error": error, "attempt": snapshot.attempts}),
            ));
            Ok(snapshot)
        })
    }

    pub fn retry_task(&self, actor: &str, task_id: &str) -> Result<TeamTask> {
        self.transact(|state, events| {
            require_active(state)?;
            let task = state
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| anyhow!("unknown task: {task_id}"))?;
            if task.status != TaskStatus::Failed {
                bail!("only failed tasks can be retried");
            }
            if task.attempts >= task.max_attempts {
                bail!(
                    "task {task_id} exhausted its {} attempt(s)",
                    task.max_attempts
                );
            }
            task.status = TaskStatus::Pending;
            task.owner = None;
            task.updated_at_ms = now_ms();
            task.started_at_ms = None;
            task.completed_at_ms = None;
            task.result = None;
            task.error = None;
            let snapshot = task.clone();
            events.push(TeamEvent::new(
                state,
                "task.retried",
                actor,
                None,
                Some(task_id.to_string()),
                json!({"next_attempt": snapshot.attempts + 1}),
            ));
            Ok(snapshot)
        })
    }

    pub fn cancel_task(&self, actor: &str, task_id: &str, reason: &str) -> Result<TeamTask> {
        self.transact(|state, events| {
            let now = now_ms();
            let owner = {
                let task = state
                    .tasks
                    .get_mut(task_id)
                    .ok_or_else(|| anyhow!("unknown task: {task_id}"))?;
                if task.status.is_terminal() {
                    bail!("task {task_id} is already terminal");
                }
                task.status = TaskStatus::Cancelled;
                task.updated_at_ms = now;
                task.completed_at_ms = Some(now);
                task.error = Some(reason.to_string());
                task.owner.clone()
            };
            if let Some(worker_id) = owner.as_deref() {
                if let Some(worker) = state.workers.get_mut(worker_id) {
                    worker.current_task_id = None;
                    worker.status = WorkerStatus::Idle;
                }
            }
            let snapshot = state.tasks[task_id].clone();
            events.push(TeamEvent::new(
                state,
                "task.cancelled",
                actor,
                owner,
                Some(task_id.to_string()),
                json!({"reason": reason}),
            ));
            Ok(snapshot)
        })
    }

    pub fn reconcile(&self, actor: &str) -> Result<Vec<String>> {
        self.transact(|state, events| {
            let now = now_ms();
            let stale = state
                .workers
                .values()
                .filter(|worker| {
                    matches!(
                        worker.status,
                        WorkerStatus::Starting | WorkerStatus::Idle | WorkerStatus::Working
                    ) && now.saturating_sub(worker.last_heartbeat_ms) > state.stale_after_ms
                })
                .map(|worker| worker.id.clone())
                .collect::<Vec<_>>();
            for worker_id in &stale {
                if let Some(worker) = state.workers.get_mut(worker_id) {
                    worker.status = WorkerStatus::Stale;
                }
                events.push(TeamEvent::new(
                    state,
                    "worker.stale",
                    actor,
                    Some(worker_id.clone()),
                    state.workers[worker_id].current_task_id.clone(),
                    json!({"stale_after_ms": state.stale_after_ms}),
                ));
            }
            Ok(stale)
        })
    }

    pub fn shutdown(&self, actor: &str, force: bool) -> Result<TeamState> {
        self.transact(|state, events| {
            if state.status == TeamStatus::Stopped {
                return Ok(state.clone());
            }
            state.status = if force {
                TeamStatus::Stopped
            } else {
                TeamStatus::ShuttingDown
            };
            for worker in state.workers.values_mut() {
                worker.status = if force {
                    WorkerStatus::Stopped
                } else {
                    WorkerStatus::Stopping
                };
            }
            if force {
                let now = now_ms();
                for task in state
                    .tasks
                    .values_mut()
                    .filter(|task| !task.status.is_terminal())
                {
                    task.status = TaskStatus::Cancelled;
                    task.error = Some("team force shutdown".to_string());
                    task.completed_at_ms = Some(now);
                    task.updated_at_ms = now;
                }
            }
            events.push(TeamEvent::new(
                state,
                if force {
                    "team.stopped"
                } else {
                    "team.shutdown_requested"
                },
                actor,
                None,
                None,
                json!({"force": force}),
            ));
            Ok(state.clone())
        })
    }

    pub fn doctor(&self) -> Result<TeamDoctorReport> {
        let state = self.load()?;
        let now = now_ms();
        let stale_workers = state
            .workers
            .values()
            .filter(|worker| {
                worker.status == WorkerStatus::Stale
                    || (matches!(
                        worker.status,
                        WorkerStatus::Starting | WorkerStatus::Idle | WorkerStatus::Working
                    ) && now.saturating_sub(worker.last_heartbeat_ms) > state.stale_after_ms)
            })
            .map(|worker| worker.id.clone())
            .collect::<Vec<_>>();
        let mut runnable_tasks = Vec::new();
        let mut blocked_tasks = BTreeMap::new();
        for task in state
            .tasks
            .values()
            .filter(|task| task.status == TaskStatus::Pending)
        {
            let missing = task
                .dependencies
                .iter()
                .filter(|dependency| {
                    state.tasks.get(*dependency).map(|task| task.status)
                        != Some(TaskStatus::Succeeded)
                })
                .cloned()
                .collect::<Vec<_>>();
            if missing.is_empty() && ownership_available(&state, task, None).is_ok() {
                runnable_tasks.push(task.id.clone());
            } else {
                blocked_tasks.insert(task.id.clone(), missing);
            }
        }
        Ok(TeamDoctorReport {
            state_path: self.state_path().to_string_lossy().into_owned(),
            schema_ok: state.schema_version == TEAM_SCHEMA_VERSION,
            active_workers: state
                .workers
                .values()
                .filter(|worker| {
                    matches!(
                        worker.status,
                        WorkerStatus::Starting | WorkerStatus::Idle | WorkerStatus::Working
                    )
                })
                .count(),
            stale_workers,
            runnable_tasks,
            blocked_tasks,
            ownership_conflicts: ownership_conflicts(&state),
        })
    }

    pub fn worker(&self, worker_id: &str) -> Result<TeamWorker> {
        self.load()?
            .workers
            .remove(worker_id)
            .ok_or_else(|| anyhow!("unknown worker: {worker_id}"))
    }

    pub fn task(&self, task_id: &str) -> Result<TeamTask> {
        self.load()?
            .tasks
            .remove(task_id)
            .ok_or_else(|| anyhow!("unknown task: {task_id}"))
    }

    fn transact<T>(
        &self,
        operation: impl FnOnce(&mut TeamState, &mut Vec<TeamEvent>) -> Result<T>,
    ) -> Result<T> {
        let _lock = self.acquire_lock()?;
        let mut state = self.load_unlocked()?;
        let mut events = Vec::new();
        let result = operation(&mut state, &mut events)?;
        state.updated_at_ms = now_ms();
        self.save_state(&state)?;
        self.append_events(&events)?;
        Ok(result)
    }

    fn load_unlocked(&self) -> Result<TeamState> {
        let path = self.state_path();
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read team state {}", path.display()))?;
        let state: TeamState = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid team state {}", path.display()))?;
        if state.schema_version != TEAM_SCHEMA_VERSION {
            bail!(
                "unsupported team schema_version {} (expected {})",
                state.schema_version,
                TEAM_SCHEMA_VERSION
            );
        }
        Ok(state)
    }

    fn save_state(&self, state: &TeamState) -> Result<()> {
        atomic_write(&self.state_path(), &serde_json::to_vec_pretty(state)?)
    }

    fn append_events(&self, events: &[TeamEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let path = self.directory.join("events.jsonl");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open team event log {}", path.display()))?;
        for event in events {
            serde_json::to_writer(&mut file, event)?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        Ok(())
    }

    fn acquire_lock(&self) -> Result<TeamLock> {
        fs::create_dir_all(&self.directory)?;
        let path = self.directory.join("state.lock");
        let started = std::time::Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "pid={} timestamp_ms={}", std::process::id(), now_ms())?;
                    file.flush()?;
                    return Ok(TeamLock { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if started.elapsed() >= LOCK_WAIT {
                        bail!("timed out waiting for team state lock {}", path.display());
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

struct TeamLock {
    path: PathBuf,
}

impl Drop for TeamLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn begin_task(
    state: &mut TeamState,
    task_id: &str,
    worker_id: &str,
    target_status: TaskStatus,
) -> Result<TeamTask> {
    require_active(state)?;
    let worker = state
        .workers
        .get(worker_id)
        .ok_or_else(|| anyhow!("unknown worker: {worker_id}"))?;
    if worker.current_task_id.is_some()
        || !matches!(
            worker.status,
            WorkerStatus::Idle | WorkerStatus::Starting | WorkerStatus::Stale
        )
    {
        bail!("worker {worker_id} is not available");
    }
    let task = state
        .tasks
        .get(task_id)
        .ok_or_else(|| anyhow!("unknown task: {task_id}"))?;
    if task.status != TaskStatus::Pending {
        bail!("task {task_id} is not pending");
    }
    if !dependencies_satisfied(state, task) {
        bail!("task {task_id} has incomplete dependencies");
    }
    ownership_available(state, task, None)?;
    if task.attempts >= task.max_attempts {
        bail!("task {task_id} exhausted its attempts");
    }
    let now = now_ms();
    let task = state.tasks.get_mut(task_id).unwrap();
    task.status = target_status;
    task.owner = Some(worker_id.to_string());
    task.attempts += 1;
    task.started_at_ms = Some(now);
    task.updated_at_ms = now;
    let snapshot = task.clone();
    let worker = state.workers.get_mut(worker_id).unwrap();
    worker.current_task_id = Some(task_id.to_string());
    worker.status = if target_status == TaskStatus::Assigned {
        WorkerStatus::Idle
    } else {
        WorkerStatus::Working
    };
    worker.last_heartbeat_ms = now;
    Ok(snapshot)
}

fn dependencies_satisfied(state: &TeamState, task: &TeamTask) -> bool {
    task.dependencies.iter().all(|dependency| {
        state.tasks.get(dependency).map(|task| task.status) == Some(TaskStatus::Succeeded)
    })
}

fn ownership_available(
    state: &TeamState,
    candidate: &TeamTask,
    ignore_task_id: Option<&str>,
) -> Result<()> {
    for active in state.tasks.values().filter(|task| {
        Some(task.id.as_str()) != ignore_task_id
            && matches!(task.status, TaskStatus::Assigned | TaskStatus::Running)
    }) {
        for requested in &candidate.owns {
            for held in &active.owns {
                if paths_overlap(requested, held) {
                    bail!(
                        "ownership conflict: task {} requests {requested}, held by task {} as {held}",
                        candidate.id,
                        active.id
                    );
                }
            }
        }
    }
    Ok(())
}

fn ownership_conflicts(state: &TeamState) -> Vec<String> {
    let active = state
        .tasks
        .values()
        .filter(|task| matches!(task.status, TaskStatus::Assigned | TaskStatus::Running))
        .collect::<Vec<_>>();
    let mut conflicts = BTreeSet::new();
    for (index, left) in active.iter().enumerate() {
        for right in active.iter().skip(index + 1) {
            for left_path in &left.owns {
                for right_path in &right.owns {
                    if paths_overlap(left_path, right_path) {
                        conflicts.insert(format!(
                            "{}:{} overlaps {}:{}",
                            left.id, left_path, right.id, right_path
                        ));
                    }
                }
            }
        }
    }
    conflicts.into_iter().collect()
}

fn normalize_ownership(paths: Vec<String>) -> Result<Vec<String>> {
    let mut normalized = BTreeSet::new();
    for raw in paths {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("ownership path must not be empty");
        }
        let path = Path::new(raw);
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            bail!("ownership path must be project-relative and must not traverse parents: {raw}");
        }
        let value = raw
            .replace('\\', "/")
            .trim_matches('/')
            .to_ascii_lowercase();
        if value.is_empty() {
            bail!("ownership path must identify a project-relative target");
        }
        normalized.insert(value);
    }
    Ok(normalized.into_iter().collect())
}

fn paths_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|tail| tail.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|tail| tail.starts_with('/'))
}

fn require_active(state: &TeamState) -> Result<()> {
    if state.status != TeamStatus::Active {
        bail!("team {} is not active ({:?})", state.name, state.status);
    }
    Ok(())
}

fn validate_identifier(label: &str, value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 64
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("{label} must use 1-64 ASCII letters, digits, '.', '-' or '_'");
    }
    Ok(())
}

fn slug(value: &str) -> Result<String> {
    validate_identifier("team name", value)?;
    Ok(value.trim().to_ascii_lowercase())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn lock_is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > STALE_LOCK_AGE)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("team state path has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        Uuid::new_v4()
    ));
    fs::write(&temp, bytes)
        .with_context(|| format!("failed to write temporary state {}", temp.display()))?;
    atomic_replace(&temp, path)
        .with_context(|| format!("failed to commit team state {}", path.display()))
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source_path = source;
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
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

pub fn worker_bootstrap_prompt(root: &Path, team: &str, worker: &TeamWorker) -> String {
    let wta = quoted_wta_executable();
    format!(
        "You are worker `{worker_id}` in Intelligent Terminal team `{team}`.\n\
         Role: {role}\n\
         Team root: {root}\n\
         Use the native control plane through `wta team` commands. Do not edit \
         outside paths owned by your current task. Send periodic heartbeats, \
         report an explicit result, and never silently take another worker's task.\n\n\
         Core commands:\n\
         - Claim: {wta} team claim --root \"{root}\" --name \"{team}\" --worker \"{worker_id}\"\n\
         - Heartbeat: {wta} team heartbeat --root \"{root}\" --name \"{team}\" --worker \"{worker_id}\" --task <TASK_ID>\n\
         - Complete: {wta} team complete --root \"{root}\" --name \"{team}\" --worker \"{worker_id}\" --task <TASK_ID> --result <SUMMARY>\n\
         - Fail: {wta} team fail --root \"{root}\" --name \"{team}\" --worker \"{worker_id}\" --task <TASK_ID> --error <REASON>\n\
         Wait for a task prompt from the leader.",
        worker_id = worker.id,
        team = team,
        role = worker.role,
        root = root.to_string_lossy()
    )
}

pub fn task_dispatch_prompt(
    root: &Path,
    team: &str,
    worker: &TeamWorker,
    task: &TeamTask,
) -> String {
    let wta = quoted_wta_executable();
    format!(
        "TEAM TASK {task_id} (attempt {attempt}/{max_attempts})\n\
         Title: {title}\n\
         Ownership: {owns}\n\
         Dependencies: {dependencies}\n\n\
         {prompt}\n\n\
         First run:\n\
         {wta} team start --root \"{root}\" --name \"{team}\" --worker \"{worker_id}\" --task \"{task_id}\"\n\
         Then work only inside the declared ownership, heartbeat during long work, \
         and finish with `wta team complete` or `wta team fail`. Do not merely \
         describe the result; update the team control plane.",
        task_id = task.id,
        attempt = task.attempts,
        max_attempts = task.max_attempts,
        title = task.title,
        owns = if task.owns.is_empty() {
            "(read-only/unscoped)".to_string()
        } else {
            task.owns.join(", ")
        },
        dependencies = if task.dependencies.is_empty() {
            "(none)".to_string()
        } else {
            task.dependencies.join(", ")
        },
        prompt = task.prompt,
        root = root.to_string_lossy(),
        team = team,
        worker_id = worker.id,
    )
}

fn quoted_wta_executable() -> String {
    std::env::current_exe()
        .map(|path| format!("\"{}\"", path.to_string_lossy().replace('"', "\"\"")))
        .unwrap_or_else(|_| "wta".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("wta-team-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn two_workers_complete_dependent_tasks_and_emit_audit_events() {
        let root = temp_root("two-workers");
        let (store, _) = TeamStore::create(&root, "alpha", "leader", None, 60_000, 2).unwrap();
        for id in ["builder", "reviewer"] {
            store
                .add_worker("leader", id, id, "codex", None, &root, Vec::new())
                .unwrap();
            store
                .set_worker_pane("leader", id, &format!("pane-{id}"))
                .unwrap();
        }
        store
            .add_task(
                "leader",
                Some("build"),
                "Build",
                "Implement it",
                Vec::new(),
                vec!["src".into()],
                None,
            )
            .unwrap();
        store
            .add_task(
                "leader",
                Some("review"),
                "Review",
                "Review it",
                vec!["build".into()],
                vec!["tests".into()],
                None,
            )
            .unwrap();

        let build = store.claim_task("builder", None).unwrap();
        assert_eq!(build.id, "build");
        assert!(store.claim_task("reviewer", Some("review")).is_err());
        store.complete_task("builder", "build", "done").unwrap();
        let review = store.claim_task("reviewer", None).unwrap();
        assert_eq!(review.id, "review");
        store
            .complete_task("reviewer", "review", "approved")
            .unwrap();

        let state = store.load().unwrap();
        assert_eq!(state.tasks["build"].status, TaskStatus::Succeeded);
        assert_eq!(state.tasks["review"].status, TaskStatus::Succeeded);
        assert!(store.events().unwrap().len() >= 9);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn overlapping_ownership_is_rejected_until_owner_finishes() {
        let root = temp_root("ownership");
        let (store, _) = TeamStore::create(&root, "alpha", "leader", None, 60_000, 2).unwrap();
        for id in ["one", "two"] {
            store
                .add_worker("leader", id, id, "codex", None, &root, Vec::new())
                .unwrap();
            store
                .set_worker_pane("leader", id, &format!("pane-{id}"))
                .unwrap();
        }
        store
            .add_task(
                "leader",
                Some("parent"),
                "Parent",
                "One",
                Vec::new(),
                vec!["src".into()],
                None,
            )
            .unwrap();
        store
            .add_task(
                "leader",
                Some("child"),
                "Child",
                "Two",
                Vec::new(),
                vec!["src/team.rs".into()],
                None,
            )
            .unwrap();
        store.claim_task("one", Some("parent")).unwrap();
        let error = store
            .claim_task("two", Some("child"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("ownership conflict"));
        store.complete_task("one", "parent", "done").unwrap();
        store.claim_task("two", Some("child")).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failure_requires_explicit_retry_and_honors_attempt_limit() {
        let root = temp_root("retry");
        let (store, _) = TeamStore::create(&root, "alpha", "leader", None, 60_000, 2).unwrap();
        store
            .add_worker(
                "leader",
                "worker",
                "worker",
                "codex",
                None,
                &root,
                Vec::new(),
            )
            .unwrap();
        store
            .set_worker_pane("leader", "worker", "pane-worker")
            .unwrap();
        store
            .add_task(
                "leader",
                Some("flaky"),
                "Flaky",
                "Try",
                Vec::new(),
                vec!["src".into()],
                Some(2),
            )
            .unwrap();
        store.claim_task("worker", Some("flaky")).unwrap();
        store.fail_task("worker", "flaky", "first").unwrap();
        store.retry_task("leader", "flaky").unwrap();
        store.claim_task("worker", Some("flaky")).unwrap();
        store.fail_task("worker", "flaky", "second").unwrap();
        assert!(store.retry_task("leader", "flaky").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ownership_rejects_absolute_and_parent_paths() {
        assert!(normalize_ownership(vec!["../secret".into()]).is_err());
        assert!(normalize_ownership(vec![r"C:\secret".into()]).is_err());
        assert_eq!(
            normalize_ownership(vec![r"Src\Team.rs".into()]).unwrap(),
            vec!["src/team.rs"]
        );
    }
}
