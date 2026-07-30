use anyhow::{bail, Result};

pub const TEMPLATE_NAMES: &[&str] = &[
    "pair",
    "feature",
    "hotfix-race",
    "research",
    "release",
    "remote",
];

pub fn render_template(name: &str, workspace_name: &str) -> Result<String> {
    let name = name.trim().to_ascii_lowercase();
    let workspace_name = yaml_scalar(workspace_name);
    let layout = match name.as_str() {
        "pair" => {
            r#"layout:
  type: split
  direction: right
  ratio: 0.5
  first:
    type: pane
    id: builder
    role: implementation
    command: codex
    environment: { AGENT_ROLE: builder }
  second:
    type: pane
    id: reviewer
    role: review
    command: codex
    environment: { AGENT_ROLE: reviewer }
"#
        }
        "feature" => {
            r#"layout:
  type: split
  direction: right
  ratio: 0.62
  first:
    type: pane
    id: builder
    role: implementation
    command: codex
  second:
    type: split
    direction: down
    ratio: 0.5
    first:
      type: pane
      id: tests
      role: verifier
      command: pwsh
    second:
      type: pane
      id: app
      role: runtime
      command: pwsh
"#
        }
        "hotfix-race" => {
            r#"layout:
  type: split
  direction: right
  ratio: 0.5
  first:
    type: pane
    id: candidate-a
    role: implementation
    command: codex
    worktree: { branch: hotfix/candidate-a, create_branch: true }
  second:
    type: split
    direction: down
    ratio: 0.5
    first:
      type: pane
      id: candidate-b
      role: implementation
      command: codex
      worktree: { branch: hotfix/candidate-b, create_branch: true }
    second:
      type: pane
      id: judge
      role: verifier
      command: pwsh
"#
        }
        "research" => {
            r#"layout:
  type: split
  direction: right
  ratio: 0.55
  first:
    type: pane
    id: researcher
    role: research
    command: codex
  second:
    type: pane
    id: sources
    role: browser
    command: pwsh
    surface:
      kind: browser
      url: https://www.google.com/
      embedded: false
"#
        }
        "release" => {
            r#"layout:
  type: split
  direction: right
  ratio: 0.6
  first:
    type: pane
    id: release
    role: release-manager
    command: codex
  second:
    type: split
    direction: down
    ratio: 0.5
    first:
      type: pane
      id: tests
      role: verifier
      command: pwsh
    second:
      type: pane
      id: package
      role: packaging
      command: pwsh
"#
        }
        "remote" => {
            r#"layout:
  type: split
  direction: right
  ratio: 0.62
  first:
    type: pane
    id: local
    role: orchestrator
    command: codex
  second:
    type: split
    direction: down
    ratio: 0.5
    first:
      type: pane
      id: remote
      role: remote-shell
      command: ssh user@host
    second:
      type: pane
      id: observer
      role: diagnostics
      command: pwsh
"#
        }
        _ => bail!(
            "unknown workspace template {name}; expected one of: {}",
            TEMPLATE_NAMES.join(", ")
        ),
    };
    let candidates = if name == "hotfix-race" {
        "  candidates: [candidate-a, candidate-b]\n"
    } else {
        ""
    };
    Ok(format!(
        "schema_version: 1\nname: {workspace_name}\nroot: .\n{layout}messaging:\n  max_hops: 8\n  allow_broadcast: true\nbrowser:\n  external_by_default: true\n  allow_embedded: false\n  allow_cookie_import: false\nverifier:\n  command: git status --short\n  timeout_seconds: 60\n{candidates}"
    ))
}

fn yaml_scalar(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::WorkspaceManifest;

    #[test]
    fn every_built_in_template_is_valid() {
        for name in TEMPLATE_NAMES {
            let yaml = render_template(name, "Demo").unwrap();
            WorkspaceManifest::parse_yaml(&yaml)
                .unwrap_or_else(|error| panic!("invalid template {name}: {error:#}"));
        }
    }
}
