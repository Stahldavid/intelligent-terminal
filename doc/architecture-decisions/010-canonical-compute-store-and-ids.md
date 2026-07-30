# ADR-010: Use a canonical versioned compute store and Terminal-owned IDs

## Status

Accepted — 2026-07-27

## Decision

The compute state is stored below
`%LOCALAPPDATA%\IntelligentTerminal\compute\v1`. Documents are versioned,
validated and replaced atomically. Corruption fails closed and does not delete
the original document.

Bindings reference the Terminal's stable window, workspace, pane and surface
IDs. The compute layer does not invent competing navigation entities.

## Consequences

- Terminal, Chat Pane, Agents & Tasks and CLI observe one source of truth.
- A future JSON-to-SQLite migration can remain behind `ComputeStore`.
- IDs and schemas are suitable for CLI/RPC automation.
- Secrets and transcript content are not part of compute metadata.
