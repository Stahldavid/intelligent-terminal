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

## First fork capability: multi-pane workspaces

The `feature/agent-workspace-launcher` branch adds a preview-first workspace
launcher to WTA:

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

The first release deliberately supports one to four panes. The layout is
deterministic, all mutations go through the existing Terminal protocol, and
execution stops if a creation response lacks a pane identity.

## Roadmap

1. Project-local workspace manifests and named presets.
2. Workspace sidebar with cwd, git branch, dirty state, ports, and agent status.
3. Explicit worktree allocation for concurrent write-capable agents.
4. Isolated WebView2 browser panes with visible, permissioned automation.
5. Durable daemon-backed terminal sessions with honest resume semantics.
6. Rich Codex App Server adapter for approvals, threads, and subagent trees,
   alongside the generic ACP provider.

## Safety boundaries

- Never edit agent or shell configuration silently.
- Show a preview before creating panes, worktrees, or persistent hooks.
- Keep command execution behind the existing Terminal permission model.
- Restrict control IPC to the current user; do not expose unauthenticated CDP
  or terminal-control endpoints on a network listener.
- Persist layout and resume identifiers, not credentials or false claims that
  a process survived a reboot.
