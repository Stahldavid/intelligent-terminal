use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::SplitDirection;

pub const WORKSPACE_SCHEMA_VERSION: u8 = 1;

fn default_schema_version() -> u8 {
    WORKSPACE_SCHEMA_VERSION
}

fn default_root() -> String {
    ".".to_string()
}

fn default_role() -> String {
    "shell".to_string()
}

fn default_ratio() -> f64 {
    0.5
}

/// `Path::canonicalize` uses the `\\?\` namespace on Windows. Win32 APIs
/// accept it, but cmd.exe treats `\\?\C:\...` as a UNC current directory and
/// silently falls back to another directory. Convert only that transport
/// prefix before a path enters a workspace plan or shell command.
fn shell_compatible_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        const VERBATIM: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
        const VERBATIM_UNC: &[u16] = &[
            b'\\' as u16,
            b'\\' as u16,
            b'?' as u16,
            b'\\' as u16,
            b'U' as u16,
            b'N' as u16,
            b'C' as u16,
            b'\\' as u16,
        ];
        let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if wide.starts_with(VERBATIM_UNC) {
            let normalized = [b'\\' as u16, b'\\' as u16]
                .into_iter()
                .chain(wide[VERBATIM_UNC.len()..].iter().copied())
                .collect::<Vec<_>>();
            return PathBuf::from(OsString::from_wide(&normalized));
        }
        if wide.starts_with(VERBATIM) {
            return PathBuf::from(OsString::from_wide(&wide[VERBATIM.len()..]));
        }
    }

    path.to_path_buf()
}

fn default_max_hops() -> u8 {
    8
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceManifest {
    #[serde(default = "default_schema_version")]
    pub schema_version: u8,
    pub name: String,
    #[serde(default = "default_root")]
    pub root: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    pub layout: PaneNode,
    #[serde(default)]
    pub verifier: Option<VerifierSpec>,
    #[serde(default)]
    pub browser: BrowserPolicy,
    #[serde(default)]
    pub messaging: MessagingPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PaneNode {
    Pane {
        id: String,
        #[serde(default = "default_role")]
        role: String,
        command: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        profile: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        environment: BTreeMap<String, String>,
        #[serde(default)]
        worktree: Option<WorktreeSpec>,
        #[serde(default)]
        surface: Option<SurfaceSpec>,
    },
    Split {
        direction: SplitDirection,
        #[serde(default = "default_ratio")]
        ratio: f64,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeSpec {
    pub branch: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub create_branch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SurfaceSpec {
    Terminal,
    Browser {
        url: String,
        #[serde(default)]
        embedded: bool,
    },
    File {
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifierSpec {
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    /// Logical pane ids evaluated by the same oracle. The fastest passing
    /// candidate wins; an empty list verifies the workspace root once.
    #[serde(default)]
    pub candidates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserPolicy {
    pub external_by_default: bool,
    pub allow_embedded: bool,
    pub allow_cookie_import: bool,
    pub allowed_hosts: Vec<String>,
}

impl Default for BrowserPolicy {
    fn default() -> Self {
        Self {
            external_by_default: true,
            allow_embedded: false,
            allow_cookie_import: false,
            allowed_hosts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MessagingPolicy {
    pub max_hops: u8,
    pub allow_broadcast: bool,
}

impl Default for MessagingPolicy {
    fn default() -> Self {
        Self {
            max_hops: default_max_hops(),
            allow_broadcast: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneSpec {
    pub id: String,
    pub role: String,
    pub command: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub environment: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<SurfaceSpec>,
}

impl WorkspaceManifest {
    pub fn parse_yaml(contents: &str) -> Result<Self> {
        let manifest: Self =
            serde_yaml::from_str(contents).context("invalid agent workspace YAML")?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read workspace manifest {}", path.display()))?;
        Self::parse_yaml(&contents)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != WORKSPACE_SCHEMA_VERSION {
            bail!(
                "unsupported workspace schema_version {} (expected {})",
                self.schema_version,
                WORKSPACE_SCHEMA_VERSION
            );
        }
        if self.name.trim().is_empty() {
            bail!("workspace name must not be empty");
        }
        if self.root.trim().is_empty() {
            bail!("workspace root must not be empty");
        }
        if self.messaging.max_hops == 0 {
            bail!("messaging.max_hops must be greater than zero");
        }
        if self.browser.allow_cookie_import {
            bail!("browser cookie import is intentionally unsupported");
        }

        let mut pane_ids = BTreeSet::new();
        self.layout.validate(&mut pane_ids)?;
        if let Some(verifier) = &self.verifier {
            if verifier.command.trim().is_empty() {
                bail!("verifier command must not be empty");
            }
            for candidate in &verifier.candidates {
                if !pane_ids.contains(candidate) {
                    bail!("verifier references unknown candidate pane: {candidate}");
                }
            }
        }
        Ok(())
    }

    pub fn resolved_root(&self, manifest_path: &Path) -> PathBuf {
        let root = Path::new(&self.root);
        let resolved = if root.is_absolute() {
            root.to_path_buf()
        } else {
            manifest_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(root)
        };
        shell_compatible_path(&resolved)
    }

    pub fn panes(&self, manifest_path: &Path) -> Vec<PaneSpec> {
        let root = self.resolved_root(manifest_path);
        let mut panes = Vec::new();
        self.layout
            .collect_panes(&root, &self.environment, &mut panes);
        panes
    }
}

impl PaneNode {
    fn validate(&self, pane_ids: &mut BTreeSet<String>) -> Result<()> {
        match self {
            Self::Pane {
                id,
                role,
                command,
                cwd,
                model,
                worktree,
                surface,
                ..
            } => {
                let id = id.trim();
                if id.is_empty() {
                    bail!("pane id must not be empty");
                }
                if !id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
                {
                    bail!("pane id {id:?} must contain only ASCII letters, digits, '-' or '_'");
                }
                if !pane_ids.insert(id.to_string()) {
                    bail!("duplicate pane id: {id}");
                }
                if role.trim().is_empty() {
                    bail!("pane {id} role must not be empty");
                }
                if command.trim().is_empty()
                    && !matches!(
                        surface,
                        Some(SurfaceSpec::Browser { .. } | SurfaceSpec::File { .. })
                    )
                {
                    bail!("pane {id} command must not be empty");
                }
                if cwd.as_deref().is_some_and(|value| value.trim().is_empty()) {
                    bail!("pane {id} cwd must not be empty when provided");
                }
                if model.as_deref().is_some_and(|value| {
                    value.trim().is_empty()
                        || !value.chars().all(|ch| {
                            ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':')
                        })
                }) {
                    bail!(
                        "pane {id} model must be a non-empty identifier using letters, digits, '-', '_', '.', '/' or ':'"
                    );
                }
                if let Some(worktree) = worktree {
                    if worktree.branch.trim().is_empty() {
                        bail!("pane {id} worktree branch must not be empty");
                    }
                    if worktree
                        .path
                        .as_deref()
                        .is_some_and(|value| value.trim().is_empty())
                    {
                        bail!("pane {id} worktree path must not be empty when provided");
                    }
                }
                if let Some(SurfaceSpec::Browser { url, embedded }) = surface {
                    if url.trim().is_empty() {
                        bail!("pane {id} browser URL must not be empty");
                    }
                    if *embedded {
                        // Policy validation happens at apply time because it
                        // belongs to the workspace, not the leaf in isolation.
                    }
                }
            }
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if !ratio.is_finite() || *ratio <= 0.0 || *ratio >= 1.0 {
                    bail!("split ratio must be between 0 and 1 (exclusive)");
                }
                first.validate(pane_ids)?;
                second.validate(pane_ids)?;
            }
        }
        Ok(())
    }

    fn collect_panes(
        &self,
        root: &Path,
        inherited_environment: &BTreeMap<String, String>,
        panes: &mut Vec<PaneSpec>,
    ) {
        match self {
            Self::Pane {
                id,
                role,
                command,
                cwd,
                profile,
                model,
                environment,
                worktree,
                surface,
            } => {
                let mut merged_environment = inherited_environment.clone();
                merged_environment.extend(environment.clone());
                let resolved_cwd = cwd
                    .as_deref()
                    .map(Path::new)
                    .map(|value| {
                        if value.is_absolute() {
                            value.to_path_buf()
                        } else {
                            root.join(value)
                        }
                    })
                    .unwrap_or_else(|| root.to_path_buf());
                panes.push(PaneSpec {
                    id: id.trim().to_string(),
                    role: role.trim().to_string(),
                    command: command.trim().to_string(),
                    cwd: resolved_cwd.to_string_lossy().into_owned(),
                    profile: profile.clone(),
                    model: model.clone(),
                    environment: merged_environment,
                    worktree: worktree.clone(),
                    surface: surface.clone(),
                });
            }
            Self::Split { first, second, .. } => {
                first.collect_panes(root, inherited_environment, panes);
                second.collect_panes(root, inherited_environment, panes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
schema_version: 1
name: feature-race
root: .
environment:
  RUST_BACKTRACE: "1"
layout:
  type: split
  direction: right
  ratio: 0.6
  first:
    type: pane
    id: builder
    role: implementation
    command: codex
    model: gpt-5
    environment:
      AGENT_ROLE: builder
  second:
    type: split
    direction: down
    first:
      type: pane
      id: tests
      role: verifier
      command: cargo test
    second:
      type: pane
      id: app
      command: cargo run
browser:
  allow_embedded: false
verifier:
  command: cargo test
"#;

    #[test]
    fn parses_arbitrary_split_tree_and_merges_environment() {
        let manifest = WorkspaceManifest::parse_yaml(SAMPLE).unwrap();
        let panes = manifest.panes(Path::new(r"C:\repo\.agent-workspace.yaml"));

        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].id, "builder");
        assert_eq!(panes[0].environment["RUST_BACKTRACE"], "1");
        assert_eq!(panes[0].environment["AGENT_ROLE"], "builder");
        assert_eq!(panes[1].role, "verifier");
    }

    #[test]
    fn rejects_duplicate_ids_invalid_ratios_and_unknown_fields() {
        let duplicate = SAMPLE.replace("id: tests", "id: builder");
        assert!(WorkspaceManifest::parse_yaml(&duplicate)
            .unwrap_err()
            .to_string()
            .contains("duplicate pane id"));

        let ratio = SAMPLE.replace("ratio: 0.6", "ratio: 1.0");
        assert!(WorkspaceManifest::parse_yaml(&ratio)
            .unwrap_err()
            .to_string()
            .contains("split ratio"));

        let unknown = SAMPLE.replace("name: feature-race", "name: feature-race\nmystery: true");
        assert!(WorkspaceManifest::parse_yaml(&unknown).is_err());
    }

    #[test]
    fn embedded_browser_is_explicit_and_cookie_import_is_rejected() {
        let with_cookie_import = SAMPLE.replace(
            "allow_embedded: false",
            "allow_embedded: true\n  allow_cookie_import: true",
        );
        assert!(WorkspaceManifest::parse_yaml(&with_cookie_import)
            .unwrap_err()
            .to_string()
            .contains("cookie import"));
    }

    #[test]
    fn rejects_unsafe_launcher_ids_and_model_arguments() {
        let invalid_id = SAMPLE.replace("id: builder", "id: \"builder & reviewer\"");
        assert!(WorkspaceManifest::parse_yaml(&invalid_id)
            .unwrap_err()
            .to_string()
            .contains("pane id"));

        let invalid_model = SAMPLE.replace("model: gpt-5", "model: \"gpt-5 & whoami\"");
        assert!(WorkspaceManifest::parse_yaml(&invalid_model)
            .unwrap_err()
            .to_string()
            .contains("model"));
    }

    #[test]
    fn resolved_root_removes_windows_verbatim_prefix_before_shell_launch() {
        let manifest = WorkspaceManifest::parse_yaml(SAMPLE).unwrap();
        let path =
            manifest.resolved_root(Path::new(r"\\?\C:\repo\workspace\.agent-workspace.yaml"));

        assert_eq!(path, PathBuf::from(r"C:\repo\workspace\."));
    }

    #[test]
    fn shell_compatible_path_preserves_unc_semantics() {
        assert_eq!(
            shell_compatible_path(Path::new(r"\\?\UNC\server\share\repo")),
            PathBuf::from(r"\\server\share\repo")
        );
    }
}
