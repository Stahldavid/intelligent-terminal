# ADR-005: Share adapter processes, isolate ACP sessions

- Status: Accepted
- Date: 2026-07-26

## Context

Starting one heavyweight agent process for every visible surface scales poorly,
while sharing one ACP session would mix conversations.

## Decision

Process, ACP connection, ACP session and UI binding are separate concepts. A
WTA master may reuse an adapter process while maintaining independent
scope/session state for each surface, workspace coordinator or team.

## Consequences

Stopping one session must not stop unrelated sessions. Process failure can
affect every session multiplexed through it and must identify those bindings.
Resource/handle measurements and real adapter E2E determine future pool
boundaries.
