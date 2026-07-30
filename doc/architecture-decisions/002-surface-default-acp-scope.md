# ADR-002: Focused surface is the user-facing ACP chat

- Status: Accepted incrementally
- Date: 2026-07-26

## Context

A workspace can contain several shells, profiles and agents. Sharing one chat
implicitly across them mixes context and makes focus changes unsafe.

## Decision

The user-facing chat always follows the active surface, identified by its
terminal session ID. The Chat Dock does not expose a Surface/Workspace/Team
selector.

Workspace coordination and teams are separate operational experiences:

- agent/team discovery and focus live in the sidebar's Agents and Teams view;
- workers remain native terminal panes/surfaces;
- coordinator/team state is never selected implicitly as the target of a
  normal terminal chat;
- the old `agent_scope_changed` protocol event remains a one-shot
  compatibility path only. The next valid focus event always returns routing
  to its exact surface.

## Consequences

Every focus event carries window, workspace, pane, surface, terminal session
and generation. The Rust state is keyed by scope and rejects stale focus
generations. Per-surface drafts, history, streaming and permissions remain
isolated. Full attach/detach/reconnect lifecycle and real multi-backend E2E
remain required before this ADR is considered fully operational.
