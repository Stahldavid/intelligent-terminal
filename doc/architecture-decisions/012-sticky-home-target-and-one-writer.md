# ADR-012: Pin interactive agents to a sticky HomeTarget and one writer

## Status

Accepted — 2026-07-27

## Decision

A Managed Agent Surface receives one HomeTarget when it is created. Placement
is explainable and sticky. Target changes require an explicit handoff.

Every writable worktree has exactly one owner and one writer lease. Builds and
tests may run elsewhere only from immutable Git replicas or snapshots.

## Consequences

- A transient load change cannot silently move an interactive agent.
- Reconnect first targets the same runtime/session.
- Stale or duplicate writers fail closed.
- Generated artifacts return through a manifest with hashes.
