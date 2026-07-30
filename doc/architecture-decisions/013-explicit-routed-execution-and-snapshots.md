# ADR-013: Route only declared jobs and immutable inputs

## Status

Accepted — 2026-07-27

## Decision

Remote execution is explicit through `wta compute exec`, a declared task,
Command Palette action or agent tool. The Terminal never rewrites arbitrary
commands typed in a PTY.

Jobs receive a Git replica or immutable snapshot identified by digest.
Environment variables and outputs are allowlisted. Destructive jobs never
receive automatic retry.

## Consequences

- Shell semantics, pipelines and interactive tools remain predictable.
- Every job records target, placement decision, input snapshot and artifacts.
- Cancellation, timeout and retries are policy decisions, not transport side
  effects.
