use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde::Serialize;

use super::{
    PaneNode, PaneSpec, SplitDirection, SurfaceSpec, VerifierSpec, WorkspaceManifest, WorktreeSpec,
};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeclarativeWorkspacePlan {
    pub schema_version: u8,
    pub name: String,
    pub manifest_path: String,
    pub root: String,
    pub pane_count: usize,
    pub operations: Vec<DeclarativeOperation>,
    pub verifier: Option<VerifierSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum DeclarativeOperation {
    CreateTab {
        pane: PaneSpec,
        title: String,
    },
    SplitPane {
        pane: PaneSpec,
        target: String,
        direction: SplitDirection,
        ratio: f64,
    },
}

impl DeclarativeOperation {
    pub fn pane(&self) -> &PaneSpec {
        match self {
            Self::CreateTab { pane, .. } | Self::SplitPane { pane, .. } => pane,
        }
    }
}

pub fn build_declarative_plan(
    manifest: &WorkspaceManifest,
    manifest_path: &Path,
) -> Result<DeclarativeWorkspacePlan> {
    manifest.validate()?;
    validate_browser_policy(manifest)?;

    let mut panes = manifest
        .panes(manifest_path)
        .into_iter()
        .map(|pane| (pane.id.clone(), pane))
        .collect::<BTreeMap<_, _>>();
    let root = manifest.resolved_root(manifest_path);
    if !root.is_dir() {
        bail!("workspace root is not a directory: {}", root.display());
    }

    for pane in panes.values_mut() {
        if let Some(worktree) = &pane.worktree {
            pane.cwd = planned_worktree_path(&root, &manifest.name, &pane.id, worktree)
                .to_string_lossy()
                .into_owned();
        }
    }

    let seed = first_leaf_id(&manifest.layout)?;
    let seed_pane = panes
        .get(seed)
        .cloned()
        .ok_or_else(|| anyhow!("workspace layout seed pane {seed} is missing"))?;
    let mut operations = vec![DeclarativeOperation::CreateTab {
        pane: seed_pane,
        title: manifest.name.clone(),
    }];
    expand_node(&manifest.layout, seed, &panes, &mut operations)?;

    Ok(DeclarativeWorkspacePlan {
        schema_version: manifest.schema_version,
        name: manifest.name.clone(),
        manifest_path: manifest_path.to_string_lossy().into_owned(),
        root: root.to_string_lossy().into_owned(),
        pane_count: panes.len(),
        operations,
        verifier: manifest.verifier.clone(),
    })
}

fn expand_node(
    node: &PaneNode,
    seed: &str,
    panes: &BTreeMap<String, PaneSpec>,
    operations: &mut Vec<DeclarativeOperation>,
) -> Result<()> {
    match node {
        PaneNode::Pane { id, .. } => {
            if id != seed {
                bail!("invalid recursive workspace seed: expected {id}, received {seed}");
            }
        }
        PaneNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let first_seed = first_leaf_id(first)?;
            if first_seed != seed {
                bail!("invalid recursive workspace seed: expected {first_seed}, received {seed}");
            }
            let second_seed = first_leaf_id(second)?;
            let second_pane = panes
                .get(second_seed)
                .cloned()
                .ok_or_else(|| anyhow!("workspace pane {second_seed} is missing"))?;
            operations.push(DeclarativeOperation::SplitPane {
                pane: second_pane,
                target: seed.to_string(),
                direction: *direction,
                ratio: *ratio,
            });
            expand_node(first, first_seed, panes, operations)?;
            expand_node(second, second_seed, panes, operations)?;
        }
    }
    Ok(())
}

fn first_leaf_id(node: &PaneNode) -> Result<&str> {
    match node {
        PaneNode::Pane { id, .. } => Ok(id),
        PaneNode::Split { first, .. } => first_leaf_id(first),
    }
}

pub fn planned_worktree_path(
    root: &Path,
    workspace_name: &str,
    pane_id: &str,
    worktree: &WorktreeSpec,
) -> PathBuf {
    if let Some(path) = &worktree.path {
        let path = Path::new(path);
        return if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
    }
    root.parent()
        .unwrap_or(root)
        .join(".intelligent-terminal-worktrees")
        .join(safe_segment(workspace_name))
        .join(safe_segment(pane_id))
}

fn safe_segment(value: &str) -> String {
    let value = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    value.trim_matches('-').to_string()
}

fn validate_browser_policy(manifest: &WorkspaceManifest) -> Result<()> {
    for pane in manifest.panes(Path::new(&manifest.root)) {
        if let Some(SurfaceSpec::Browser { url, embedded }) = pane.surface {
            if embedded && !manifest.browser.allow_embedded {
                bail!(
                    "pane {} requests an embedded browser but browser.allow_embedded is false",
                    pane.id
                );
            }
            if !manifest.browser.allowed_hosts.is_empty() {
                let host = http_host(&url)?;
                if !manifest
                    .browser
                    .allowed_hosts
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(host))
                {
                    bail!("browser host {host} is not in browser.allowed_hosts");
                }
            }
        }
    }
    Ok(())
}

fn http_host(url: &str) -> Result<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| anyhow!("browser URL must use http or https: {url}"))?;
    let host = rest.split(['/', ':', '?', '#']).next().unwrap_or_default();
    if host.is_empty() {
        bail!("browser URL has no host: {url}");
    }
    Ok(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_nested_tree_parent_before_children() {
        let root = std::env::temp_dir();
        let yaml = format!(
            r#"
name: nested
root: "{}"
layout:
  type: split
  direction: right
  ratio: 0.6
  first:
    type: split
    direction: down
    first: {{ type: pane, id: builder, command: codex }}
    second: {{ type: pane, id: tests, command: cargo test }}
  second:
    type: pane
    id: server
    command: cargo run
"#,
            root.to_string_lossy().replace('\\', "/")
        );
        let manifest = WorkspaceManifest::parse_yaml(&yaml).unwrap();
        let plan = build_declarative_plan(&manifest, &root.join(".agent-workspace.yaml")).unwrap();

        assert_eq!(plan.operations.len(), 3);
        assert_eq!(plan.operations[0].pane().id, "builder");
        match &plan.operations[1] {
            DeclarativeOperation::SplitPane { pane, target, .. } => {
                assert_eq!(pane.id, "server");
                assert_eq!(target, "builder");
            }
            _ => panic!("expected root split"),
        }
        match &plan.operations[2] {
            DeclarativeOperation::SplitPane { pane, target, .. } => {
                assert_eq!(pane.id, "tests");
                assert_eq!(target, "builder");
            }
            _ => panic!("expected nested split"),
        }
    }

    #[test]
    fn embedded_browser_requires_opt_in_and_allowlisted_host() {
        let root = std::env::temp_dir();
        let base = format!(
            r#"
name: browser
root: "{}"
layout:
  type: pane
  id: docs
  command: ""
  surface:
    kind: browser
    url: https://example.com/docs
    embedded: true
"#,
            root.to_string_lossy().replace('\\', "/")
        );
        let manifest = WorkspaceManifest::parse_yaml(&base).unwrap();
        assert!(build_declarative_plan(&manifest, &root.join("manifest.yaml")).is_err());

        let allowed =
            format!("{base}\nbrowser:\n  allow_embedded: true\n  allowed_hosts: [example.com]\n");
        let manifest = WorkspaceManifest::parse_yaml(&allowed).unwrap();
        assert!(build_declarative_plan(&manifest, &root.join("manifest.yaml")).is_ok());
    }
}
