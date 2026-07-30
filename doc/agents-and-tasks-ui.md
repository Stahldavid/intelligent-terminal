# Agents & Tasks: native workspace operations

## Product model

Intelligent Terminal keeps one canonical navigation and execution hierarchy:

```text
Window
  └─ Workspace (native Terminal tab, projected in the sidebar)
      └─ Pane (split region)
          └─ Surface (tab inside one pane)
              └─ Terminal content
```

The Chat Pane is contextual UI, not another hierarchy level. It follows the
focused surface and selects the conversation slot identified by the focused
window, workspace, pane, surface, terminal session and focus generation.
There is no manual `Surface / Workspace / Team` selector.

Native teams are a workspace-scoped control plane projected over this
hierarchy. The durable state, ownership transitions, heartbeat leases, task
dependencies, retries and audit events live in `wta team`. XAML never edits
`.intelligent-terminal/teams` state files directly.

## Persistent dashboard

The workspace toolbar and the bottom `Agent mesh` indicator open an in-page
`Agents & Tasks` overlay. It intentionally remains inside the page instead of
using a modal dialog:

- the left sidebar remains usable to change the focused workspace;
- agent and task counts always describe the focused workspace;
- selecting an agent focuses its real terminal session;
- the terminal layout is not reparented or replaced;
- closing the dashboard returns focus to the active terminal.

The dashboard has two projections of one WTA snapshot:

1. **Agent mesh** lists surface-bound agents and native team workers, including
   role, model, team, current task and status. The team leader/coordinator is an
   optional role marker, not a privileged mandatory AI.
2. **Task board** groups durable tasks as queued, in progress and completed.
   Focus, retry and cancel actions delegate to `wta team`.

The bottom status bar summarizes managed agents, running agents and open tasks
for the focused workspace. It is a navigation shortcut, not a second store.

## Creating teams, workers and tasks

The dashboard `Add` flow supports:

- creating a native team bound to the focused workspace stable ID;
- launching an agent worker (Codex by default) in its own terminal pane;
- adding a durable task with explicit ID, title and instructions.

Every operation executes a normal `wta team` command. Worker launch therefore
uses the same scoped bootstrap instructions, pane session registration and
terminal protocol boundary as the CLI.

## Creation semantics

The two plus buttons have different destinations:

- **Sidebar plus:** creates a new workspace using the native Windows Terminal
  profile/new-tab path.
- **Pane plus:** creates a new surface inside the focused pane. Primary click
  clones the active surface's profile and working directory; its dropdown
  projects the canonical `newTabMenu`, including profile folders, remaining
  profiles, matching profiles and actions.

Selecting PowerShell, Command Prompt, WSL, SSH or another profile in the pane
dropdown creates that profile as a surface. A workspace is not permanently
typed by the profile used for its first terminal.

## Capability boundary

An ordinary agent terminal does not inherit the host-wide Terminal COM
capability. Workspace-level terminal mutations remain in the trusted WTA
control process. Worker agents receive team-scoped instructions and update
their own task/heartbeat state through the native team protocol.

This preserves the intended operating model:

- any focused surface may have its own ACP conversation;
- workers may inspect and coordinate through declared workspace/team
  primitives;
- an optional coordinator may assign work, but the deterministic control plane
  remains functional without it;
- broad terminal automation is explicit, auditable and revocable.

## Verification

Run:

```powershell
build\scripts\Verify-WorkspaceNavigation.ps1
build\scripts\Verify-NativeChatDock.ps1
cargo test --manifest-path tools\wta\Cargo.toml
```

The workspace verification script rejects regressions to a modal fleet,
missing task columns, direct team-state file access, duplicated workspace
stores, or a non-canonical surface profile menu.

## Remote environments and files

The dashboard projects remote state from the same workspace context:

- Environment cards show target, lifecycle, connection state, runtime and
  protocol versions, launch method and the last supervised error.
- Remote files expose only an opaque root label and read/write/delete
  capabilities. Canonical remote paths are not serialized into the UI.
- Process metrics use the persistent PTY runtime's PID, RSS and user/system CPU
  counters; the UI does not start a second process monitor.

Browser, File Explorer, PTY and managed agent activity share the environment's
single connection supervisor. A consumer can attach to an already-connected
environment without resetting or flapping its state.
