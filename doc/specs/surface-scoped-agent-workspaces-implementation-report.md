# Surface-scoped agent workspaces: implementation report

> Update: the workspace-scoped operational projection is now implemented as a
> persistent native `Agents & Tasks` dashboard. It consumes the existing WTA
> team snapshots, exposes focus/retry/cancel through canonical `wta team`
> commands, adds team/worker/task creation, and publishes a focused-workspace
> Agent Mesh summary in the bottom bar. See
> [Agents & Tasks: native workspace operations](../agents-and-tasks-ui.md).

Date: 2026-07-27

Branch: `feature/agent-workspace-launcher`

Observed base: `6635b61a9`

This report separates implemented capability, deterministic verification,
observed build results and gates that still require real/manual validation.
The detailed acceptance contract remains
`surface-scoped-agent-workspaces-plan.md`.

## P0: adapter startup and heterogeneous creation

Implemented:

- Windows launcher normalization recognizes literal executables, `.cmd`
  wrappers, basenames and absolute paths;
- `npx` and Codex ACP use a cold-start-aware startup class;
- transient startup failures receive one bounded, cancelable retry;
- authentication, missing executable and access-denied failures do not retry;
- native/WSL ACP launch removes terminal-control credentials;
- each pane owns a SurfaceStack;
- primary surface `+` duplicates current profile/CWD;
- chevron projects the complete canonical `newTabMenu`: profiles,
  `remainingProfiles`, `matchProfiles`, nested/inline folders, separators and
  configured actions;
- profile and `newTab` entries selected in that surface-local menu pass
  `INewContentArgs` through SurfaceStack → Pane → Tab → TerminalPage;
- non-`newTab` entries delegate to the existing native
  `ShortcutActionDispatch`, preserving action IDs, keybindings and semantics;
- TerminalPage remains the only terminal-content factory;
- native tab context split actions already support direction and profile.

Verified:

- launcher/spawn and retry classifier unit tests pass;
- TerminalAppLib and WindowsTerminal Release compile;
- PowerShell version/navigation invariant scripts pass, including canonical
  menu-tree and destination-dispatch assertions.

Still open:

- an empty npm-cache real Codex ACP run;
- a single destination dispatcher UI combining profile and split direction.

## P1: identity and focus

Implemented:

- focus payload includes protocol version, window ID, stable native workspace
  ID, pane ID, terminal session ID, surface ID, surface-local ordinal/count and
  monotonically increasing focus generation;
- PaneInfo/TabInfo IDL and COM JSON carry workspace/surface/focus identity;
- WTA PaneContext and ACP `_meta.wta` carry the same scope;
- different-window, different-owner and stale-generation focus events are
  rejected;
- `surface_created`, `surface_activated`, `surface_closed`, `surface_moved`
  and `surface_detached` carry immutable identity snapshots through
  SurfaceStack → Tab → TerminalPage → WTA;
- closing one surface releases only that surface's ACP session and routing;
  detach preserves the binding for a subsequent reattach;
- tab/session close/reset paths rekey or remove scope state.

Verified:

- deterministic tests cover surface isolation, workspace isolation and stale
  focus rejection;
- TerminalProtocol, TerminalAppLib, TerminalConnection, wtcli and
  WindowsTerminal compile.

Still open:

- real drag-to-window, restore and rapid-switch-during-streaming E2E.

## P2: ACP scope isolation

Implemented:

- WTA uses a scope key preferring surface and falling back to legacy workspace;
- active scope is retained per native workspace;
- the user-facing Chat Dock has no manual scope selector;
- each valid focus event activates the exact focused surface conversation;
- legacy `agent_scope_changed` requests remain one-shot compatibility events
  and cannot override a later focus;
- workspace coordination and teams are exposed through the sidebar's explicit
  Agents and Teams view;
- session routing and close/reset operations are scoped;
- native surface creation materializes only its own conversation slot;
- native surface close removes the matching session-to-scope routing and
  cannot leave the composer targeting a dead terminal;
- lifecycle events are filtered by window, workspace owner and protocol
  version before mutation;
- process/connection sharing remains in the existing WTA master rather than
  starting a heavyweight process per surface.

Verified:

- two-surface state isolation and focus-after-legacy-destination tests pass;
- complete Rust suite passes.

Still open:

- persisted restore/reconnect and configurable detach grace period;
- real two-session adapter E2E, WSL backend E2E and truthful SSH/local-companion
  labeling validation;
- restore/reconnect and orphan-process validation.

## P3: native teams

Implemented:

- `wta team` is the only team control plane; no tmux/Claude Teams shim;
- persisted team model includes stable IDs, optional native workspace binding,
  workers, roles, model, pane session, tasks, dependencies, ownership paths,
  attempts, heartbeat/lease, result/error and lifecycle timestamps;
- state mutation uses a bounded lock plus atomic replacement;
- conflicting ownership is rejected;
- dispatch, claim/start, heartbeat, complete/fail, retry, cancel, message,
  inspect, focus, peek and graceful/forced shutdown are available;
- audit events are append-only;
- `team create` infers the active native workspace inside Intelligent Terminal
  or accepts explicit `--workspace-id`;
- workspace context only loads teams whose workspace ID exactly matches;
- sidebar projects worker status/task and focuses the worker's native terminal
  session.

Verified:

- deterministic team tests cover two workers, dependency/ownership conflict,
  failure, retry limits and lifecycle;
- strict workspace-to-team projection test passes.

Still open:

- opt-in E2E with two installed, trusted and authenticated real agents.

## P4: security and confirmation

Implemented:

- Terminal Protocol 3.0 uses a random per-launch host token;
- every COM object starts unauthenticated;
- `Authenticate` accepts either the exact host-admin token or an
  HMAC-SHA256-signed scoped capability; all other 18 methods fail with
  `E_ACCESSDENIED` until authenticated;
- ordinary ConPTY children receive a surface capability bound to their
  `WT_SESSION`, while trusted WTA `--connect-master --owner-tab-id` helpers
  receive a native-workspace capability;
- signed claims contain issuer, subject, workspace/surface, explicit operation
  mask, seven-day expiry and a unique nonce;
- no scoped capability contains host-level `CreateTab`; only workspace/admin
  callers may split, and every session-target operation resolves and verifies
  the requested surface/workspace;
- topology reads are filtered by scope and `GetCapabilities` advertises only
  operations present in the authenticated claim;
- event subscribers are filtered before queueing, incoming scoped events
  require an explicit matching identity, and unscoped events fail closed;
- `wtcli` refuses a missing token and protocol mismatch;
- built-in native/WSL ACP adapters and native-team Agent CLI workers do not
  inherit `WT_PROTOCOL_TOKEN` or `WT_COM_CLSID`;
- fresh confirmation defaults are `prompt`;
- ACP create-terminal, terminal-output and kill-terminal operations enforce
  `auto`, `prompt` or `deny` before executing;
- unknown policy values become `prompt`;
- explicit ACP approval creates a one-shot, 30-second grant;
- an allowed read is cached only for the same session/terminal resource;
- authorization decisions are emitted on a security audit target without
  transcript content.

Verified:

- tests prove parse/fail-closed, deny/no-event and prompt/allow/resource-cache;
- six native TAEF capability tests prove surface/workspace claim round-trips,
  delimiter rejection, tamper/wrong-secret rejection, expiry and unique
  nonces; two additional tests prove native-chat direct routing preserves the
  complete schema for fail-closed page validation;
- `Verify-TerminalProtocolSecurity.ps1` proves all COM methods remain guarded,
  scoped token derivation replaces raw host-token inheritance, event filtering
  occurs before enqueue, scoped `CreateTab` stays denied, defaults remain
  prompt and child credential scrubbing remains present;
- TerminalProtocol, TerminalConnection, wtcli, WindowsTerminal and
  TerminalApp.UnitTests compile.

Residual security gap:

- the trusted WTA master remains a host administrator and must be treated as
  part of the trusted computing base;
- scoped tokens expire, but ConPTY has no refresh/revocation channel and nonce
  uniqueness is not a server-side one-use replay ledger;
- same-user process inspection can copy a scoped bearer until it expires;
- confirmation enforcement is not universal across direct protocol operations,
  cross-surface context attachment, team mutation or settings relaxation.

The per-binding capability and subscriber-filtering subphases are implemented;
the remaining confirmation/revocation gates prevent declaring all of P4
complete.

## P5: native chat experience without WebView2

Implemented:

- native XAML chat header;
- localized passive `Following/Seguindo` context with profile and CWD;
- full workspace/pane/surface/profile/CWD context retained for UI Automation;
- no Surface/Workspace/Team selector in the conversation flow;
- native XAML conversation body for completed turns, user/agent/system/error
  messages, plans, active tools and streaming text;
- native permission card whose buttons resolve only option IDs advertised by
  the active ACP request;
- native multiline composer with Enter to send, Shift+Enter for a newline,
  cancel and retry;
- immutable WTA snapshots with monotonic sequence rejection in the UI;
- structured actions carrying the canonical workspace and exact scope key;
  missing, foreign and stale scopes fail closed before reaching ACP;
- a persistent authenticated `wtcli publish-stdin` bridge with per-event
  acknowledgement, a five-second timeout, restart and ordered one-shot
  compatibility fallback;
- native sidebar remains a projection of native tabs and their complete
  context menu;
- Agents and Teams is the explicit operational entry for workers and team
  coordination, and focusing an agent returns chat routing to its surface;
- workspace color fills the card, not only its left accent;
- team worker/task metadata appears in the workspace projection;
- the Chat Pane creates no WebView2 control or embedded browser profile.

Compatibility boundary:

- setup, authentication and session-management views still use the WTA
  `TermControl`;
- `TermControl` is collapsed while the native chat view is active and is not
  the chat renderer.
- a later Browser Surface can use an isolated WebView2 host for remote web
  content; it is a separate surface type and does not change the native-chat
  decision recorded here.

The deterministic/build criterion for the native conversation body is
satisfied. The installed x64 release was visually checked with the Chat Dock
following `PowerShell` and no manual scope selector. Broader visual polish,
high contrast, Narrator/keyboard inspection and a real authenticated adapter
streaming/permission round trip remain explicit manual/E2E gates.

## Validation ledger

Observed successful commands:

```powershell
cargo test --manifest-path tools/wta/Cargo.toml -q
# 1212 passed; 0 failed

cargo build --manifest-path tools/wta/Cargo.toml `
  --target x86_64-pc-windows-msvc --release

pwsh -File build/scripts/Verify-IntelligentTerminalVersion.ps1
pwsh -File build/scripts/Verify-WorkspaceNavigation.ps1
pwsh -File build/scripts/Verify-TerminalProtocolSecurity.ps1
pwsh -File build/scripts/Verify-NativeChatDock.ps1

pwsh -File build/scripts/Run-Tests.ps1 `
  Terminal.App.Unit.Tests.dll x64 Release
# 123 passed; 0 failed; 0 skipped
# includes all 8 TerminalProtocolCapabilityTests
```

Observed successful native projects, built with one compiler process to avoid
Windows page-file exhaustion:

- `TerminalProtocol.vcxproj`
- `TerminalAppLib.vcxproj`
- `TerminalConnection.vcxproj`
- `wtcli.vcxproj`
- `WindowsTerminal.vcxproj`
- `TerminalApp.UnitTests.vcxproj`

The structural native-chat verifier reported native XAML rendering, no
WebView2 chat renderer, exact workspace/scope routing, persistent acknowledged
delivery with ordered fallback, `en-US`/`pt-BR` localization and all four
focused Rust tests present.

Release packaging completed after the final native-chat and
scoped-capability changes:

```text
artifacts/local-installer/intelligent-terminal-0.9.4.5-x64-release-setup.exe
SHA-256: F7A35DF3051A7ABAFF848A0C28C08F9D52978F3301EFC3B2B9AFCFA08B1C37C0
```

The package build completed with 147 warnings and 0 errors:

```text
src/cascadia/CascadiaPackage/AppPackages/
  CascadiaPackage_0.9.4.5_x64_Test/CascadiaPackage_0.9.4.5_x64.msix
SHA-256: 94000871DC930B469462B42F8312EC63BCC27C88EF777D88D77C18549419D147
```

The WTA binary embedded in the installer staging payload is byte-for-byte
identical to the tested release binary:

```text
SHA-256: 8367E23AB17B6EA98F4702D16EF36F0DD9F33FDFF268174D51524062916C7E2F
```

The packaged/unpackaged resource index contains the native
`AgentPaneContent` resources, `TerminalApp.dll`, `wtcli.exe` and
`resources.pri` are present, and the self-extracting setup footer contains the
three bounded entries expected by the bootstrap: `install.cmd`,
`install-local-terminal.ps1` and `payload.zip`.

The bootstrap invokes the extracted `install-local-terminal.ps1` and
`payload.zip` through their validated absolute paths. It translates only the
documented `/quiet`, `/nopath` and `/noshortcuts` setup switches. The bootstrap
regression test executes a real self-extracting installer from a temporary
directory, which prevents a recurrence of the former `C:\install-local-terminal.ps1`
working-directory failure.

The release installer completed with exit code 0 and installed version
`0.9.4.5` under `%LOCALAPPDATA%\Programs\IntelligentTerminal`. The installed
`wta.exe` remained byte-for-byte identical to the tested release binary, and
the local `WindowsTerminal.exe` was observed running with a visible top-level
window.

The local MSIX and self-extracting setup are intentionally not
Authenticode-signed. They must not be presented as production-signed release
artifacts.

`cargo fmt --check` is not a useful release gate for the current dirty
checkout: it reports broad pre-existing formatting drift across unrelated Rust
files. No bulk reformat was applied because it would mix unrelated user work
into this implementation.

## Release rule

Do not describe this initiative as fully shipped until the unchecked
`C230`–`C245` gates in `doc/release-check-list.md` are closed. In particular,
contract tests do not substitute for real adapters, credentials, network,
multi-window restore, accessibility inspection or the remaining per-binding
capability boundary.

## Verification addendum — 2026-07-28

The current installed release supersedes the older `0.9.4.5` packaging
evidence above:

```text
Product: 0.9.4
Package: 0.9.4.12
Terminal protocol: 3.1
Setup SHA-256:
  3BA7DE92C16464F359A011139FCE7C5CA0A6995A6928159993384E101B0A16FA
Installed/release wta.exe SHA-256:
  E2B040B3E41E4449CF4499D71F4B5EC73FD8E29D05A31ADEE952823227D58352
```

The installed protocol probe returned `connected=true`. The installed surface
E2E created an explicit Command Prompt surface, duplicated the active surface,
created a Codex Managed Agent Surface and returned the collection count from
`1 → 2 → 3 → 4 → 1`. Closing the managed surface removed its canonical binding
and released both writer/slot leases.

Lifecycle cleanup no longer depends on a live Chat Pane or `wta-master`.
Terminal dispatches an idempotent delete-by-window/workspace/surface command;
the store normalizes WinRT/COM GUID formatting and revokes every lease owned by
the removed binding. A terminated WTA process can no longer block cleanup for
the stale-lock grace period: a lock whose recorded same-user WTA PID is gone is
recovered immediately, while live owners and the age fallback remain protected.

The final Rust suite passed:

```text
52 library tests passed
1226 binary tests passed
1278 total passed, 0 failed
```

The physical `do-codex` gate also passed with two authenticated, isolated Codex
ACP sessions, persistent PTY reattach, verified upload/download SHA-256 and
redacted evidence. This closes the specific transport/authentication gates but
does not replace the remaining interactive UI, accessibility, confirmation and
revocation gates in the release checklist.
