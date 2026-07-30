# Fork architecture and current status

**Snapshot date:** 2026-07-30

**Branch:** `feature/agent-workspace-launcher`

**Status:** implemented development snapshot; full cmux SSH parity is not yet
declared

This document is the current architectural index for the Stahldavid
Intelligent Terminal fork. It distinguishes source capability, automated
verification, observed runtime behavior, and external release gates. Dated
plans and implementation reports remain evidence for their own snapshots; use
this page when their earlier statements conflict with the current source.

## Relationship to the original project

The fork retains the original Intelligent Terminal foundations:

- Windows Terminal profiles, tabs, splits, settings, renderer, ConPTY, and COM
  automation;
- native Agent Pane integration through Agent Client Protocol (ACP);
- one `wta-master` process, per-pane `wta-helper` processes, and the `wta` /
  `wtcli` control surface;
- agent discovery, authentication, history, slash commands, permissions,
  autofix, localization, and session hooks.

The fork extends those foundations instead of introducing a second terminal or
a second source of truth. Native Terminal tabs remain workspaces. Native pane
trees remain split regions. The sidebar and pane-local surface bars are
projections of those same objects.

The integration commit containing this snapshot brings the feature branch to
two commits beyond merge base `5b2356490236`. The fetched `upstream/main`
remains 20 commits ahead of that base. Upstream integration must therefore be
reviewed explicitly; this document does not claim that the fork already
contains those upstream commits.

## Canonical hierarchy

```text
Window
  └─ Workspace
      └─ Pane
          └─ Surface
              └─ Content
```

- **Window** is a Windows Terminal window.
- **Workspace** is a native Terminal tab represented in the left sidebar.
- **Pane** is a split region in the selected workspace.
- **Surface** is a pane-local tab. A pane can keep multiple live surfaces.
- **Content** is a terminal, managed ACP agent, or remote browser.

The sidebar does not own a duplicate workspace collection. It projects the
native tab collection and reuses native identity, title, icon, color, order,
focus, close behavior, and context actions. When the sidebar is visible, the
horizontal native tab strip is suppressed. Collapsing the sidebar restores it.

## Local user experience

### Workspaces and surfaces

`WorkspaceSidebar` renders native workspaces as information-rich cards. A
workspace color fills the card rather than appearing only as a narrow accent.
The sidebar divider has an opaque underlay so resizing cannot expose an
unpainted strip from a background application.

`SurfaceStackPaneContent` gives each leaf pane its own surface bar:

- the primary `+` duplicates the active profile and working directory;
- the adjacent dropdown projects the canonical `newTabMenu`, including
  profiles, dynamic profiles, folders, separators, and configured actions;
- a selected profile creates a surface inside the focused pane, not a new
  workspace;
- surfaces in one workspace may use different profiles, including PowerShell,
  Command Prompt, WSL, and SSH.

Terminal content creation remains centralized in `TerminalPage`. The surface
stack requests content; it does not duplicate profile resolution or connection
creation.

### Contextual Chat Pane

The native XAML Chat Pane follows the focused surface automatically. It does
not expose a manual `Surface / Workspace / Team` conversation selector.

- Focusing a managed agent surface selects its exact ACP session.
- Focusing a plain terminal keeps that surface context and shows that no
  managed agent is attached when appropriate.
- Workspace and team information are available as context and tools, not as a
  competing conversation scope.
- Structured actions include the canonical workspace and surface identity.
  WTA rejects missing, foreign, or stale identities.

The chat UI is native XAML. WebView2 is not used to render chat. The legacy
terminal TUI remains a compatibility path for setup, authentication, and
session management.

### Managed agents and teams

A Managed Agent Surface binds one agent session to one canonical surface.
`wta-master` keeps a lazy ACP adapter pool keyed by the trusted agent command.
Helpers selecting the same adapter reuse its process and connection; different
adapters can coexist without another master. The master reconstructs commands
from the registry and allowlist rather than executing command text received
from a pipe.

`wta team` provides the coordination plane:

- teams, workers, tasks, ownership, and dependencies;
- heartbeat, result, retry, cancel, and shutdown;
- durable events and diagnostics;
- an optional coordinator role rather than a separate chat mode.

The native **Agents & Tasks** dashboard reads team, workspace, and compute state
from the canonical WTA stores. It does not maintain a competing UI-only model.

## Terminal Protocol 3.1

Protocol 3.1 carries stable workspace, pane, and surface identity. It adds
surface creation and capability-scoped control without exposing an
unauthenticated network service.

- Each COM client authenticates before calling protected methods.
- Host clients use the per-launch host token.
- Ordinary clients use short-lived, HMAC-signed capabilities scoped to a
  surface or workspace.
- Server methods and events enforce scope and fail closed.
- Current structural verification covers 18 guarded protocol methods and the
  capability tests in `TerminalProtocolCapabilityTests.cpp`.

`CreateSurface`, `CreateManagedSurface`, and `CreateBrowserSurface` preserve the
existing COM ABI by using the established protocol transport and reserved split
direction markers. The implementation does not add an incompatible second COM
interface.

## Remote compute and SSH

The remote architecture separates four responsibilities:

```text
Intelligent Terminal
  ├─ wta-master  → ACP sessions and Chat Pane routing
  ├─ wta team    → coordination, ownership, and heartbeat
  ├─ wta compute → targets, placement, leases, jobs, snapshots, and policy
  └─ wta-node    → persistent remote PTY, ACP, files, proxy, and relay
```

`wta compute` is the control plane. `wta-node` is a versioned remote runtime
bootstrapped over SSH/stdio. ACP remains the agent conversation protocol; it is
not used as a compute scheduler.

Implemented source contracts include:

- recursive OpenSSH `Include` discovery, concrete aliases, `ssh -G`, trust
  classification, and option-injection rejection;
- versioned and hash-verified node bootstrap;
- persistent PTY sessions with attach, detach, resize, reconnect, and cleanup;
- Managed Agent Surfaces with sticky target, worktree, lease, and ACP binding;
- explicit build/test/lint jobs, immutable snapshots, logs, cancellation, and
  artifacts;
- root-scoped file transfer with opaque root identifiers and fail-closed raw
  paths;
- surface-scoped loopback proxy and relay capabilities;
- restore metadata and canonical environment/connection state.

Targets classified as restricted or production are excluded from automatic
placement. A writable worktree has one owner. Arbitrary terminal commands are
not silently redirected to another machine.

## Browser Surfaces

Browser Surface support now exists in source as a separate content type. It is
not the Chat Pane and does not weaken the native-chat decision.

For a ready Remote Workspace, the surface menu can create a native WebView2
browser bound to that workspace:

- a per-surface user-data folder isolates browser state;
- a surface-scoped, loopback-only SSH SOCKS proxy carries remote traffic;
- navigation is limited to HTTP and HTTPS;
- DevTools, web messages, host objects, password storage, autofill, default
  script dialogs, new windows, and browser downloads are disabled or denied;
- policy or proxy setup failure closes the surface instead of falling back to
  an unscoped profile.

This is an implemented preview, not a completed release claim. Physical UI
validation of cross-workspace cookie isolation, cleanup, reconnect, and
long-running browser traffic remains open. System-browser fallback does not
satisfy those gates.

## Current verification

The current source has deterministic verifiers for:

- Terminal Protocol authentication and capability scope;
- native workspace/sidebar and surface-menu invariants;
- native contextual Chat Pane wiring;
- remote runtime, file-root, proxy, relay, restore, and browser contracts;
- installer ordering, settings transaction, build manifest, and version
  alignment.

The full WTA suite observed for this snapshot passed 70 library tests and 1226
binary tests: 1296 total, with no failures. Compiler warnings remain and are
not represented as failures.

Historical physical SSH evidence on the non-production `do-codex` target
observed persistent PTY reattach, two isolated authenticated Codex ACP sessions,
hash-preserving file transfer, transfer cancellation, and
HTTP/HTTPS/WebSocket proxying. Those reports are evidence for the recorded
artifacts and dates, not proof that every current working-tree byte is installed
or that every release gate is closed.

## Open release gates

The fork must not claim full cmux SSH parity until these external gates pass on
the release build:

1. host-key rotation and fail-closed confirmation;
2. prolonged jitter, packet loss, suspend, and reconnect to the original
   runtime;
3. two Managed Agent Surfaces switched during active streaming in the installed
   UI;
4. authenticated physical Claude and Gemini adapter coverage;
5. relay notification, unread state, and exact jump behavior in the installed
   UI;
6. application restart restoring physical remote runtimes;
7. WebView2 cookie/profile isolation and cleanup across workspaces;
8. accessibility, high-contrast, localization, and security hardening;
9. a clean, signed, reproducible release build with recorded provenance.

See [cmux SSH full-parity plan](specs/cmux-ssh-full-parity-plan.md) for the
requirement matrix and
[distributed compute implementation report](specs/distributed-agent-compute-control-plane-implementation-report.md)
for dated verification evidence.

## Documentation authority

Use this order when documents disagree:

1. current source, tests, protocol schema, and build scripts;
2. this current-status document and the security model;
3. current user guides;
4. dated implementation reports;
5. plans, experiments, and historical ADR context.

Plans describe intent. Reports describe evidence from a specific snapshot.
Neither should be interpreted as proof of a later installed build.
