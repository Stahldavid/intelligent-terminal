# ADR-011: Use OpenSSH as transport and a verified `wta-node` as runtime

## Status

Accepted — 2026-07-27

## Decision

Remote control uses the user's OpenSSH configuration and concrete aliases.
Effective configuration is resolved with `ssh -G`; `Include`, `ProxyJump` and
OpenSSH precedence remain OpenSSH responsibilities.

The versioned `wta-node` artifact is uploaded to a per-user directory, checked
by SHA-256 and only then activated. Control uses JSON-RPC/stdio. No ACP or
app-server endpoint is exposed publicly.

## Consequences

- Alias/argument validation prevents option injection.
- Host-key changes fail closed.
- Large snapshots and artifacts do not travel as Base64 JSON-RPC payloads.
- Windows does not depend on `ControlMaster`/`ControlPersist` correctness.
