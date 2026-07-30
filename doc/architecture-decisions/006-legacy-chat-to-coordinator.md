# ADR-006: Legacy tab chat migrates to workspace coordinator

- Status: Accepted
- Date: 2026-07-26

## Context

An older chat belonged to the native tab. Assigning it to whichever surface
happens to be focused during upgrade would silently change its authority and
context.

## Decision

Treat legacy tab chat as the workspace coordinator binding. New surfaces start
with their own scope. Moving a legacy conversation to a surface must be an
explicit migration action. The normal Chat Dock never selects the legacy
coordinator binding and has no scope selector. Older hosts may temporarily open
the binding through `agent_scope_changed`, but the next focused terminal
surface becomes authoritative.

## Consequences

Migration never guesses ownership from focus. Legacy coordinator bindings stay
recoverable through session-management/migration tooling rather than competing
with terminal chat. Persisted bindings that cannot be revalidated become
`Needs reconnect`.
