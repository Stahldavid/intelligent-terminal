# Agent workspaces

Intelligent Terminal workspaces are declarative, inspectable Windows Terminal
layouts for running several AI agents and deterministic support processes in
one project. The source of truth is `.agent-workspace.yaml`; runtime state and
events are plain files under `.intelligent-terminal/workspaces/<name>/`.

## Quick start

```powershell
wta aw template feature --name my-feature
wta aw plan
wta aw apply
wta aw status --name my-feature
```

`plan` is read-only. `apply` creates any requested Git worktrees, then creates
the tab and recursively materializes the split tree. Each pane has an
independent id, role, working directory, command, profile, model, environment,
worktree and optional surface.

Pane ids are stable machine identifiers and accept ASCII letters, digits,
`-` and `_`. Model values are single identifiers (for example `gpt-5` or
`openai/gpt-5`), not shell fragments; unsafe values fail validation before any
pane or worktree is created.

```yaml
schema_version: 1
name: checkout-race
root: .
environment:
  RUST_BACKTRACE: "1"
layout:
  type: split
  direction: right
  ratio: 0.5
  first:
    type: pane
    id: candidate-a
    role: implementation
    command: codex
    model: gpt-5
    worktree: { branch: feature/a, create_branch: true }
  second:
    type: pane
    id: candidate-b
    role: implementation
    command: codex
    worktree: { branch: feature/b, create_branch: true }
verifier:
  command: cargo test
  timeout_seconds: 300
  candidates: [candidate-a, candidate-b]
messaging:
  max_hops: 8
  allow_broadcast: true
browser:
  external_by_default: true
  allow_embedded: false
  allow_cookie_import: false
```

## Commands

All commands produce JSON so agents can use the same primitives as a person.

| Command | Effect |
| --- | --- |
| `wta aw plan [manifest]` | Validate and compile a recursive layout without mutations |
| `wta aw apply/open [manifest]` | Create worktrees, tab, panes and runtime state |
| `wta aw list --root ROOT` | Discover every persisted workspace at a root, newest first |
| `wta aw tree --name NAME` | Read the logical-to-native pane map |
| `wta aw status --name NAME` | Read state, metrics and fresh project/Git/PR/port context |
| `wta aw context --root ROOT [--tab-id ID]` | Read sidebar context, recent events and stable workspace identity |
| `wta aw inspect-git --root ROOT` | Read bounded status and worktree diff without invoking a shell |
| `wta aw doctor --root ROOT` | Diagnose Git, GitHub CLI, Terminal Protocol, persistence and browser policy |
| `wta aw send --name NAME -t PANE TEXT` | Send literal input to a logical pane |
| `wta aw focus --name NAME -t PANE` | Navigate to a logical pane |
| `wta aw peek --name NAME -t PANE` | Read recent scrollback without focusing |
| `wta aw wait --name NAME -t PANE` | Wait for a pane process to finish |
| `wta aw close --name NAME` | Close every recorded pane |
| `wta aw notify ...` | Append a structured direct or broadcast event |
| `wta aw forward ...` | Forward an event with correlation and hop-limit checks |
| `wta aw read ...` | Read the timeline or a recipient inbox |
| `wta aw verify [manifest]` | Run the oracle; fastest passing candidate wins |
| `wta aw snapshot --name NAME` | Save a point-in-time runtime map |
| `wta aw restore --name NAME` | Recreate from the persisted manifest |

Built-in templates are `pair`, `feature`, `hotfix-race`, `research`, `release`
and `remote`. `template --stdout` previews without writing. Template creation
refuses to overwrite an existing manifest unless `--force` is explicit.

## Native workspace shell

The WinUI sidebar is a projection of Terminal tabs plus the WTA runtime, not a
second session database. A native `Tab` remains the canonical session object:
its title, icon, color, order, focus and close lifecycle are reused directly.
When the sidebar is open, native horizontal tab headers and their new-tab
button are hidden; collapsing the sidebar restores the original horizontal
navigation. This prevents two visible controls from claiming ownership of the
same tabs.

The sidebar provides:

- stable workspace identity, pinning, groups, filtering and a responsive
  compact mode, while reading color and icon from the native tab;
- workspace → pane/agent hierarchy with working, attention, error, idle and
  ended state;
- a durable Attention Center backed by `events.jsonl`, and a Fleet view that
  focuses the selected native pane;
- a Composer that previews a built-in manifest and writes it under
  `.intelligent-terminal/manifests/` before applying it;
- a creation split button that reuses the native new-tab flyout for profiles,
  settings and the command palette, extended with the agent Composer;
- native, read-only Git status/diff, trusted manifest verification and
  `wta doctor`;
- same-process undo through Terminal actions and cross-process declarative
  restore through the persisted manifest and pre-close snapshot.

Right-click navigation is also canonical. The horizontal tab header and the
vertical workspace card use independent WinUI `MenuFlyout` visual presenters
built by the same native `Tab` command factory. This preserves color, rename,
duplicate, split, move-to-window, export, find, restart and the complete close
submenu without parenting one flyout under two controls. Intelligent Terminal
appends one dynamically populated `Workspace` submenu to each presenter for
pinning, groups, Git/PR/ports, trusted verification and snapshots. Dynamic
enabled/visible state is recomputed when each presenter opens, and rename and
color use the navigation element that opened that specific menu as their flyout
anchor, so they remain usable while the horizontal header is collapsed.

The sidebar/content separator is composited in two layers. A narrow opaque
underlay overlaps both surfaces behind the terminal, while the visible
translucent rule and the wider transparent resize hit target sit above it.
This preserves terminal acrylic/opacity without ever alpha-blending the
separator directly against the transparent app root.

Terminal tabs and agent workspaces are therefore two levels of one model:

- **tab/session** is the native runtime and visual source of truth;
- **agent workspace metadata** adds pinning, grouping and WTA identity to that
  tab;
- **saved layout** is the upstream, named-window persistence feature and is
  deliberately not called a workspace in the UI.

Recently closed agent workspaces reference the native close-history entry by a
stable id rather than by vector position, so closing a pane or another tab
cannot make restore reopen the wrong session.

PRs and listening ports opened from workspace metadata continue to use the
system browser. This path is separate from an explicit Browser Surface and
does not create WebView2 content implicitly.

### Native agent chat dock

The active chat view is a native XAML projection, not a web view and not
terminal-drawn chat. WTA publishes immutable snapshots containing the active
workspace/surface scope, messages, streaming text, tools, connection state and
the current permission request. TerminalPage routes each snapshot to the
workspace identified by the native tab StableId; AgentPaneContent rejects
delayed sequence numbers.

Composer, permission, cancel and retry actions return through the authenticated
Terminal Protocol with both `workspace_id` and the exact `scope_key`. WTA
rejects missing, foreign and stale identities, then reuses the same ACP
prompt/cancel/permission/restart paths as the compatibility TUI. Setup,
authentication and session management keep the terminal helper UI as a
fallback; the helper control is collapsed whenever native chat is active.

The event bridge uses one persistent `wtcli publish-stdin` process and waits
for a delivery acknowledgement. On timeout, process failure or an older wtcli,
it restarts and falls back to ordered one-shot publishing.

### Native surface stacks

Each ordinary pane is a `SurfaceStackPaneContent`: one pane position can host
multiple live terminal sessions, with only the selected session attached to
the visual content host. The primary `+` duplicates the active profile and
working directory. Its chevron is a destination-aware projection of the same
`newTabMenu` configured for the window-level button, including nested and
auto-inline folders, profile collections, separators and action entries.
Selecting a profile or `newTab` action creates a surface in the current pane;
other actions continue through the native `ShortcutActionDispatch`.

Surface identity on the wire is the connection session GUID. The pane-local
ordinal is presentation metadata only. Create, activate, close, move and detach
publish immutable lifecycle snapshots carrying window, workspace, pane,
surface, index/count and focus generation. WTA filters them by window,
workspace owner and protocol version. Close releases only the matching
surface-scoped ACP conversation; detach deliberately preserves it for a
subsequent pane/window reattach. The generation-checked `focus_changed` event,
not the visual tab index, remains authoritative for active routing.

## Persistence and events

The runtime store contains:

- `state.json`: workspace id, manifest, tab, logical pane ids, native session
  ids, PIDs, roles, models and activity;
- `events.jsonl`: append-only events with UUID, timestamp, source, target,
  correlation id and hop count;
- `launchers/*.cmd`: generated environment launchers when a pane needs
  variables that are not part of the Terminal protocol;
- `snapshots/*.json`: point-in-time runtime maps.

Direct inbox reads include events addressed to the recipient and broadcasts.
Forwarding increments `hop`; forwarding at `max_hops` fails closed.

## Isolation and verification

Worktrees are never removed automatically. An omitted worktree path resolves
next to the repository under
`.intelligent-terminal-worktrees/<workspace>/<pane>`. Existing directories are
reused; new branches are created only when `create_branch: true` is explicit.

When `verifier.candidates` is present, the oracle runs concurrently in each
candidate worktree with an independent timeout. The report preserves stdout,
stderr, exit code and duration for every candidate. The winner is the fastest
successful candidate, with logical pane id as a stable tie-breaker.

## Browser and file surfaces

Browser URLs use the system browser by default. Hosts can be restricted with
`browser.allowed_hosts`, and cookie import is unsupported and rejected during
manifest validation.

A ready Remote Workspace can create a native Browser Surface through Terminal
Protocol 3.1. The host uses a per-surface WebView2 data directory and a
surface-scoped, loopback-only SSH SOCKS proxy. It accepts only HTTP/HTTPS
navigation and disables or denies DevTools, web messages, host objects,
password storage, autofill, default script dialogs, new windows, permissions,
and downloads. Proxy, profile, or policy setup failure closes the browser
instead of silently sharing a profile or bypassing the remote route.

Browser Surface source and contract verification are implemented, but
cross-workspace cookie isolation, cleanup, degraded-network behavior, and
installed UI recovery remain release gates. Do not infer those outcomes from a
build or source verifier.

File surfaces open through Explorer. Terminal panes remain the durable native
surface; `focus`, `peek`, snapshot and restore provide navigation and recovery.

## Build and compatibility

The product/WTA version is `0.9.4` (`0.9.4.12` in MSIX manifests) and Terminal
Protocol is `3.1`. `wtcli` requires the per-launch `WT_PROTOCOL_TOKEN` and
rejects a mismatched protocol before invoking the
changed COM surface. Verify coherence with:

```powershell
pwsh -File build/scripts/Verify-IntelligentTerminalVersion.ps1
pwsh -File build/scripts/Verify-WorkspaceNavigation.ps1
pwsh -File build/scripts/Verify-TerminalProtocolSecurity.ps1
```

`tools\razzle.cmd` and both MSIX drivers prefer the v145-capable Visual Studio
18 toolset with Windows SDK 26100. `CommonResources.xaml` is explicitly marked as a
small `ResourceDictionary` runtime class (as are the other loose dictionaries)
so SDK 26100 never passes a null `ClassFullName` into
`CheckIsHybridCppWinRTCx` and fails with `WMC9999`. The .NET SDK is pinned by
`global.json`. Development, Preview, Canary and release manifests retain
distinct package identities so they can coexist.
