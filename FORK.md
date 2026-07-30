# Stahldavid Intelligent Terminal fork

This fork keeps `main` close to `microsoft/intelligent-terminal` and develops
cmux/wmux-inspired capabilities on focused feature branches.

## Remotes and branch policy

- `upstream`: `https://github.com/microsoft/intelligent-terminal.git`
- `origin`: `https://github.com/Stahldavid/intelligent-terminal.git`
- `main`: upstream-compatible integration branch
- `feature/*`: independently reviewable fork features

Sync `main` with a fast-forward whenever possible. Rebase active feature
branches onto the refreshed `main`; do not merge stale upstream experiment
branches wholesale.

## Current feature branch

The `feature/agent-workspace-launcher` branch started with a preview-first
workspace launcher:

```powershell
# Preview only: prints the exact JSON plan and does not mutate Terminal.
wta workspace `
  --cwd C:\work\project `
  --title Project `
  --pane "codex" `
  --pane "npm test" `
  --pane "npm run dev"

# Apply the reviewed plan to the current Intelligent Terminal instance.
wta workspace `
  --cwd C:\work\project `
  --title Project `
  --pane "codex" `
  --pane "npm test" `
  --pane "npm run dev" `
  --apply
```

The branch now also contains:

- a native sidebar that projects existing Terminal tabs as workspaces;
- pane-local surfaces with heterogeneous Terminal profiles;
- a native contextual Chat Pane that follows the focused surface;
- surface-scoped ACP sessions and a lazy multi-adapter pool;
- native agent teams and an Agents & Tasks dashboard;
- Terminal Protocol 3.1 authentication and scoped capabilities;
- compute targets, sticky placement, worktrees, snapshots, jobs, and leases;
- a versioned `wta-node` remote runtime with persistent PTY and ACP sessions;
- scoped files, transfer, proxy, relay, restore, and Browser Surfaces;
- reproducible build, installer transaction, and version-verification tooling.

The complete architectural and validation status is maintained in
[Fork architecture and current status](doc/fork-architecture-and-status.md).
The [cmux SSH parity plan](doc/specs/cmux-ssh-full-parity-plan.md) defines the
remaining physical and release gates.

## Upstream integration status

As of 2026-07-30, this branch is based on merge base `5b2356490236`. The
integration commit containing this snapshot brings it to two branch-only
commits, while the fetched `upstream/main` has 20 commits not yet integrated.

Do not describe the branch as current with upstream until those commits are
reviewed and integrated. The overlap includes Terminal and WTA files, so a
blind merge or rebase is not an acceptable validation strategy.

## Remaining release work

The principal open gates are physical host-key rotation, degraded-network and
suspend recovery, multi-adapter physical coverage, installed relay/notification
UX, application-level restore, WebView2 cross-workspace isolation and cleanup,
accessibility, and a clean signed release build. Unit tests, source verifiers,
WSL, or historical screenshots do not replace those gates.

## Safety boundaries

- Never edit agent or shell configuration silently.
- Show a preview before creating panes, worktrees, or persistent hooks.
- Keep command execution behind the existing Terminal permission model.
- Restrict control IPC to the current user; do not expose unauthenticated CDP
  or terminal-control endpoints on a network listener.
- Persist layout and resume identifiers, not credentials or false claims that
  a process survived a reboot.
- Keep restricted and production targets outside automatic placement.
- Keep Browser Surfaces separate from the native Chat Pane and fail closed if
  proxy, profile, or policy isolation cannot be established.
