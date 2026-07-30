# ADR-015: Separate execution environments from targets and scope remote files

## Status

Accepted — 2026-07-29

## Context

`ComputeTarget` describes placement, capacity, trust and cost. It is not a
stable runtime identity. An SSH alias, forwarded port, process ID or tunnel
path can change while the remote agent or PTY session remains the same.

The first Remote File Explorer prototype used the remote user's home directory
as its effective trust boundary. Traversal and symlink checks prevented escape
from HOME, but did not prevent one workspace from reading credentials,
configuration or another project already inside HOME.

## Decision

The canonical hierarchy is:

```text
ComputeTarget
  └─ ExecutionEnvironment
       ├─ AccessEndpoint
       └─ EnvironmentConnectionSupervisor
            └─ RemoteWorkspace / SurfaceBinding / Browser / File operation
```

- `ComputeTarget` remains the placement and policy record.
- `ExecutionEnvironment` is the stable runtime identity and records runtime
  version, protocol version, OS, architecture, capabilities and lifecycle.
- `AccessEndpoint` is replaceable connectivity metadata. SSH is the active
  bootstrap/fallback. Public, overlay and relay endpoint kinds remain disabled
  fail-closed until their own threat models and E2Es exist.
- Exactly one `EnvironmentConnectionSupervisor` owns connection state,
  endpoint selection, retry and capped backoff. Browser, files, PTYs and agents
  consume it instead of creating competing retry identities.
- Restore persists environment, target, binding, preferred endpoint kind and
  runtime/session IDs. It never treats a port, PID, tunnel path or credential
  as identity.

Remote filesystem access is authorized by `RemoteFileRootPolicy`:

```text
root_id + workspace_id + optional binding_id + relative_path
```

The canonical path remains package-private. The UI and ordinary CLI receive an
opaque root ID, label and explicit read, write and delete capabilities. Project
and worktree roots are normal sources. HOME/admin roots require a visible
broad-access acknowledgement and the target's `files.admin_roots` capability.
Revocation invalidates subsequent operations.

The broker opens a root only on the active JSON-RPC bridge, performs one
operation and closes the grant. Download preparation snapshots the already
authorized resolved file and returns a manifest without canonical source or
snapshot paths. The legacy HOME/path download route fails closed.

## Consequences

- One workspace cannot use another workspace's root ID.
- Read-only roots cannot upload, rename or delete.
- UI, CLI, Browser Surface and restore use one Compute Store.
- SSH aliases, ports and processes can change without changing environment
  identity.
- WebSocket, relay and cloud launch providers can be added later behind these
  contracts, but are not enabled by this ADR.
- Plain SSH profiles remain available and distinct from managed workspaces.

## Verification

```powershell
build\scripts\Verify-RemoteRuntimeVerticalSlice.ps1
build\scripts\Verify-TerminalProtocolSecurity.ps1
cargo +stable test --manifest-path tools\wta\Cargo.toml --lib
cargo +stable check --manifest-path tools\wta\Cargo.toml --all-targets
```

Physical SSH reconnect, installed UI focus, cross-workspace cookie isolation
and app-restart restore remain acceptance gates. Deterministic source and unit
tests do not substitute for those observations.
