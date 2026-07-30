use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cwd_util::validate_starting_directory;
use crate::shell::wt_channel::WtChannel;

mod context;
mod model;
mod operations;
mod planner;
mod state;
mod templates;

pub use context::{collect_context, inspect_git};
pub use model::{PaneNode, PaneSpec, SurfaceSpec, VerifierSpec, WorkspaceManifest, WorktreeSpec};
pub use operations::{
    apply_declarative_plan, close_workspace, refresh_workspace_runtime, run_verifier,
    send_to_workspace_pane, wait_for_workspace_pane, ApplyResult,
};
pub use planner::{build_declarative_plan, DeclarativeOperation, DeclarativeWorkspacePlan};
pub use state::{PaneActivity, RuntimePane, WorkspaceEvent, WorkspaceRuntime, WorkspaceStore};
pub use templates::render_template;

pub const MAX_WORKSPACE_PANES: usize = 4;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkspacePlan {
    pub schema_version: u8,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub pane_count: usize,
    pub steps: Vec<WorkspaceStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum WorkspaceStep {
    CreateTab {
        pane_index: usize,
        commandline: String,
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    SplitPane {
        pane_index: usize,
        target_pane_index: usize,
        direction: SplitDirection,
        size: f64,
        commandline: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    Down,
}

impl SplitDirection {
    pub fn as_protocol_value(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Down => "down",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkspaceCreationResult {
    pub schema_version: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    pub pane_ids: Vec<String>,
}

pub fn build_plan(
    cwd: &str,
    title: Option<String>,
    pane_commands: Vec<String>,
) -> Result<WorkspacePlan> {
    let cwd = validate_starting_directory(cwd)
        .ok_or_else(|| anyhow!("workspace cwd is empty, missing, or not a directory: {cwd}"))?;

    if pane_commands.is_empty() {
        bail!("workspace requires at least one --pane command");
    }
    if pane_commands.len() > MAX_WORKSPACE_PANES {
        bail!(
            "workspace supports at most {MAX_WORKSPACE_PANES} panes in this release (received {})",
            pane_commands.len()
        );
    }

    let pane_commands = pane_commands
        .into_iter()
        .enumerate()
        .map(|(index, command)| {
            let trimmed = command.trim();
            if trimmed.is_empty() {
                bail!("workspace pane {} has an empty command", index + 1);
            }
            Ok(trimmed.to_string())
        })
        .collect::<Result<Vec<_>>>()?;

    let title = title.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    });

    let mut steps = vec![WorkspaceStep::CreateTab {
        pane_index: 0,
        commandline: pane_commands[0].clone(),
        cwd: cwd.clone(),
        title: title.clone(),
    }];

    let split_targets: &[(usize, SplitDirection)] = match pane_commands.len() {
        1 => &[],
        2 => &[(0, SplitDirection::Right)],
        3 => &[(0, SplitDirection::Right), (1, SplitDirection::Down)],
        4 => &[
            (0, SplitDirection::Right),
            (0, SplitDirection::Down),
            (1, SplitDirection::Down),
        ],
        _ => unreachable!("pane count validated above"),
    };

    for (offset, (target_pane_index, direction)) in split_targets.iter().enumerate() {
        let pane_index = offset + 1;
        steps.push(WorkspaceStep::SplitPane {
            pane_index,
            target_pane_index: *target_pane_index,
            direction: *direction,
            size: 0.5,
            commandline: pane_commands[pane_index].clone(),
        });
    }

    Ok(WorkspacePlan {
        schema_version: 1,
        cwd,
        title,
        pane_count: pane_commands.len(),
        steps,
    })
}

pub async fn apply_plan(
    channel: &dyn WtChannel,
    plan: &WorkspacePlan,
) -> Result<WorkspaceCreationResult> {
    let mut tab_id = None;
    let mut pane_ids = Vec::with_capacity(plan.pane_count);

    for step in &plan.steps {
        match step {
            WorkspaceStep::CreateTab {
                pane_index,
                commandline,
                cwd,
                title,
            } => {
                if *pane_index != pane_ids.len() {
                    bail!(
                        "invalid workspace plan: create_tab pane index {} is out of sequence",
                        pane_index
                    );
                }

                let mut params = json!({
                    "commandline": commandline,
                    "cwd": cwd,
                });
                if let Some(title) = title {
                    params["title"] = json!(title);
                }

                let result = channel
                    .request("create_tab", params)
                    .await
                    .context("workspace create_tab failed")?;
                pane_ids.push(required_id(&result, "session_id", "create_tab")?);
                tab_id = optional_id(&result, "tab_id");
            }
            WorkspaceStep::SplitPane {
                pane_index,
                target_pane_index,
                direction,
                size,
                commandline,
            } => {
                if *pane_index != pane_ids.len() {
                    bail!(
                        "invalid workspace plan: split_pane pane index {} is out of sequence",
                        pane_index
                    );
                }
                let target_id = pane_ids.get(*target_pane_index).ok_or_else(|| {
                    anyhow!(
                        "invalid workspace plan: target pane index {} does not exist",
                        target_pane_index
                    )
                })?;

                let result = channel
                    .request(
                        "split_pane",
                        json!({
                            "session_id": target_id,
                            "direction": direction.as_protocol_value(),
                            "size": size,
                            "commandline": commandline,
                        }),
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "workspace split_pane failed while creating pane {}",
                            pane_index
                        )
                    })?;
                pane_ids.push(required_id(&result, "session_id", "split_pane")?);
            }
        }
    }

    Ok(WorkspaceCreationResult {
        schema_version: plan.schema_version,
        tab_id,
        pane_ids,
    })
}

fn optional_id(value: &Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(Value::String(id)) if !id.trim().is_empty() => Some(id.clone()),
        Some(Value::Number(id)) => Some(id.to_string()),
        _ => None,
    }
}

fn required_id(value: &Value, key: &str, operation: &str) -> Result<String> {
    optional_id(value, key).ok_or_else(|| {
        anyhow!("workspace {operation} response did not include a non-empty {key}: {value}")
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    struct MockChannel {
        responses: Mutex<VecDeque<Value>>,
        requests: Mutex<Vec<(String, Value)>>,
    }

    impl MockChannel {
        fn new(responses: Vec<Value>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<(String, Value)> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WtChannel for MockChannel {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.requests
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow!("no mock response for {method}"))
        }

        fn is_available(&self) -> bool {
            true
        }
    }

    #[test]
    fn plan_requires_between_one_and_four_non_empty_panes() {
        assert!(build_plan(".", None, vec![]).is_err());
        assert!(build_plan(".", None, vec![" ".to_string()]).is_err());
        assert!(build_plan(
            ".",
            None,
            (0..=MAX_WORKSPACE_PANES)
                .map(|index| format!("pane-{index}"))
                .collect()
        )
        .is_err());
    }

    #[test]
    fn two_pane_plan_splits_the_initial_pane_to_the_right() {
        let plan = build_plan(
            ".",
            Some("  Agents  ".to_string()),
            vec![" codex ".to_string(), "pwsh.exe".to_string()],
        )
        .unwrap();

        assert_eq!(plan.title.as_deref(), Some("Agents"));
        assert_eq!(plan.pane_count, 2);
        assert_eq!(
            plan.steps[1],
            WorkspaceStep::SplitPane {
                pane_index: 1,
                target_pane_index: 0,
                direction: SplitDirection::Right,
                size: 0.5,
                commandline: "pwsh.exe".to_string(),
            }
        );
    }

    #[test]
    fn four_pane_plan_builds_a_balanced_grid() {
        let plan = build_plan(
            ".",
            None,
            vec![
                "codex".to_string(),
                "codex".to_string(),
                "npm test".to_string(),
                "npm run dev".to_string(),
            ],
        )
        .unwrap();

        let targets = plan
            .steps
            .iter()
            .skip(1)
            .map(|step| match step {
                WorkspaceStep::SplitPane {
                    target_pane_index,
                    direction,
                    ..
                } => (*target_pane_index, *direction),
                WorkspaceStep::CreateTab { .. } => unreachable!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            targets,
            vec![
                (0, SplitDirection::Right),
                (0, SplitDirection::Down),
                (1, SplitDirection::Down),
            ]
        );
    }

    #[tokio::test]
    async fn apply_plan_executes_requests_in_dependency_order() {
        let plan = build_plan(
            ".",
            Some("Agents".to_string()),
            vec![
                "codex".to_string(),
                "npm test".to_string(),
                "npm run dev".to_string(),
            ],
        )
        .unwrap();
        let channel = MockChannel::new(vec![
            json!({"tab_id": 7, "session_id": "pane-a"}),
            json!({"session_id": "pane-b"}),
            json!({"session_id": "pane-c"}),
        ]);

        let created = apply_plan(&channel, &plan).await.unwrap();

        assert_eq!(created.tab_id.as_deref(), Some("7"));
        assert_eq!(created.pane_ids, vec!["pane-a", "pane-b", "pane-c"]);
        assert_eq!(
            channel.requests(),
            vec![
                (
                    "create_tab".to_string(),
                    json!({
                        "commandline": "codex",
                        "cwd": ".",
                        "title": "Agents",
                    }),
                ),
                (
                    "split_pane".to_string(),
                    json!({
                        "session_id": "pane-a",
                        "direction": "right",
                        "size": 0.5,
                        "commandline": "npm test",
                    }),
                ),
                (
                    "split_pane".to_string(),
                    json!({
                        "session_id": "pane-b",
                        "direction": "down",
                        "size": 0.5,
                        "commandline": "npm run dev",
                    }),
                ),
            ]
        );
    }

    #[tokio::test]
    async fn apply_plan_fails_closed_when_creation_response_has_no_pane_id() {
        let plan = build_plan(".", None, vec!["codex".to_string()]).unwrap();
        let channel = MockChannel::new(vec![json!({"tab_id": 1})]);

        let error = apply_plan(&channel, &plan).await.unwrap_err();

        assert!(error.to_string().contains("session_id"));
        assert_eq!(channel.requests().len(), 1);
    }
}
