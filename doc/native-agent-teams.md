# Native agent teams

`wta team` is Intelligent Terminal's native, agent-neutral coordination
protocol. It uses the existing Terminal Protocol for panes and the existing
agent launchers for Codex, Gemini, Copilot, Claude, OpenCode, or a custom
command. It does not emulate tmux and does not depend on Claude Teams.

## Model

A team is persisted below:

```text
<project>/.intelligent-terminal/teams/<team>/
  state.json
  events.jsonl
  state.lock
```

`state.json` is the current source of truth. `events.jsonl` is an append-only
audit timeline. Mutations use a bounded cross-process lock and an atomic
replace, so two agents cannot both claim the same task through a read/modify/
write race.

The schema has:

- a stable team ID, name, leader, lifecycle, retry default, and stale timeout;
- stable worker IDs, roles, agent/model, cwd, pane ID, activity, current task,
  capabilities, and last heartbeat;
- stable task IDs, dependencies, exclusive project-relative ownership paths,
  owner, attempts, result/error, and lifecycle timestamps.

Team lifecycle:

```text
active -> shutting_down -> stopped
```

Worker lifecycle:

```text
starting -> idle -> working -> idle
                    |
                    +-> stale
starting/idle/working -> stopping -> stopped
```

Task lifecycle:

```text
pending -> assigned -> running -> succeeded
                      |       \-> failed -> pending (explicit retry)
                      \----------> cancelled
```

## Quick start

```powershell
wta team create --root . --name feature-x --leader david

wta team add-worker --root . --name feature-x `
  --worker builder --role implementation --agent codex

wta team add-worker --root . --name feature-x `
  --worker reviewer --role review --agent gemini

wta team add-task --root . --name feature-x `
  --id implementation --title "Implement feature X" `
  --prompt "Implement the agreed behavior and run the focused tests." `
  --owns src/feature-x --owns tests/feature-x

wta team add-task --root . --name feature-x `
  --id review --title "Review feature X" `
  --prompt "Review the implementation and report actionable findings." `
  --depends-on implementation --owns review-notes

wta team dispatch --root . --name feature-x `
  --worker builder --task implementation
```

`add-worker` opens a real terminal tab by default and records its pane ID.
Pass `--no-launch` for an externally launched worker, or `--split-target
<pane-id>` to create a split instead of a tab.

The dispatched prompt tells the worker to call:

```powershell
wta team start ...
wta team heartbeat ...
wta team complete ... --result "..."
# or
wta team fail ... --error "..."
```

The prompt uses the absolute path of the running `wta.exe`, so packaged builds
do not depend on `wta` being globally available on `PATH`.

When `team create` runs inside Intelligent Terminal, it binds the team to the
active native workspace automatically. Automation can make that identity
explicit with `--workspace-id <stable-tab-id>`. A team created outside the
Terminal remains deliberately unbound instead of guessing from a title or
directory.

## Leader controls

```powershell
wta team status --root . --name feature-x --reconcile
wta team events --root . --name feature-x
wta team doctor --root . --name feature-x
wta team peek --root . --name feature-x --worker builder
wta team focus --root . --name feature-x --worker builder
wta team send --root . --name feature-x --worker builder --enter "Please checkpoint now."
wta team retry --root . --name feature-x --task implementation
wta team cancel --root . --name feature-x --task review --reason "No longer needed"
wta team shutdown --root . --name feature-x
wta team shutdown --root . --name feature-x --force --close-panes
```

Graceful shutdown notifies workers and moves them to `stopping`; it does not
destroy panes. `--force` makes tasks terminal, and `--close-panes` explicitly
closes the recorded panes.

## Ownership and concurrency

`--owns` accepts only normalized, project-relative paths. Absolute paths and
`..` traversal are rejected. A task cannot be assigned or claimed while one of
its ownership paths equals, contains, or is contained by a path held by an
assigned/running task.

Ownership is a coordination invariant, not an operating-system sandbox. The
agent prompt tells workers to stay inside their claim, and the control plane
prevents conflicting claims; a malicious or defective process can still write
elsewhere if its own sandbox permits it. Use separate Git worktrees from
`agent-workspace` when hard filesystem separation is required.

Tasks with no `--owns` are treated as read-only/unscoped work. This is useful
for research and review, but it does not grant an implicit write claim.

## Heartbeats, recovery, and retries

Every claim/start/heartbeat updates a worker lease. `reconcile` marks an
expired worker `stale`; it does not silently reassign the worker's task,
because that could create two writers. The leader must cancel/fail the old
attempt and explicitly retry it.

Attempts increment when assignment/claim begins. `fail` records the failure
but never loops automatically. `retry` succeeds only for a failed task whose
`max_attempts` has not been exhausted.

## Real two-agent E2E

With Intelligent Terminal running:

```powershell
wta team e2e --root . --agent codex --agent-two gemini --wait-seconds 180
```

This creates a unique team, launches two real agent tabs, dispatches one
independent task to each, and waits until both agents report a result through
the native protocol. `--wait-seconds 0` returns immediately for manual visual
inspection. The deterministic Rust tests separately verify two-worker claims,
dependencies, ownership conflicts, audit events, failure, and retry limits.

### Transport isolation is a separate gate

Before running the opt-in task workflow, the installed WSL node and ACP adapter
can be checked without launching autonomous team tasks:

```powershell
.\build\scripts\Test-WtaNodePersistentAcp.ps1 `
  -WtaExe "$env:LOCALAPPDATA\Programs\IntelligentTerminal\wta.exe" `
  -WtaNodeLinux "$env:LOCALAPPDATA\Programs\IntelligentTerminal\wta-node-linux-x64" `
  -VerifyIsolation
```

This starts two real ACP processes, verifies distinct process IDs, detaches and
reattaches each session, and confirms that each process ID remains stable. It
also records the post-reattach authentication result for each adapter. Add
`-RequireAuthenticatedReattach` only after the WSL adapter is authenticated;
that switch fails closed if the authenticated operation cannot be completed.

Passing this transport test does not close the `wta team e2e` gate. The latter
additionally proves independent task dispatch, ownership, heartbeat and result
reporting by two authenticated agents.

If an agent has not yet trusted the project directory, it will correctly stop
at its interactive trust gate. Do not bypass that gate. Either complete it
manually or pass a previously trusted process directory while keeping team
state at `--root`:

```powershell
wta team e2e --root C:\repo --worker-cwd C:\trusted-agent-home `
  --agent codex --agent-two codex
```

## Codex ACP compatibility

The built-in Codex ACP command is pinned consistently in the runtime registry,
Settings UI, and Terminal host:

```text
npx -y @agentclientprotocol/codex-acp@1.1.7
```

The pin is intentional: it keeps the adapter and its bundled Codex app-server
compatible with current Codex configuration values, including
`default_tools_approval_mode = "writes"`. The terminal must not rewrite a
valid user `config.toml` to compensate for an obsolete adapter.

## Deliberate boundaries

- No Claude Teams/tmux shim: native teams are the only coordination source of
  truth.
- No MCP facade yet: a future MCP server should be a thin mapping onto the
  same `TeamStore` operations, never a parallel state model.
- No WebView2 dependency for teams: all team execution and control use native
  terminal panes and the Terminal Protocol. A separate Browser Surface may use
  an isolated WebView2 host without becoming a team or chat control plane.
- The native sidebar projects teams whose stored `workspace_id` exactly
  matches the native tab. It shows worker role, model, state and current task;
  selecting a worker focuses its recorded terminal session. Legacy/unbound
  team files are not leaked into an arbitrary workspace.
