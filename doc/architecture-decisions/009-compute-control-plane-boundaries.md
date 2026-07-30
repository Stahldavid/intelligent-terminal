# ADR-009: Separate ACP, team coordination and compute placement

## Status

Accepted — 2026-07-27

## Decision

`wta-master` owns ACP connections and routing for the focused surface.
`wta team` owns tasks, ownership, heartbeat and worker lifecycle.
`wta compute` owns targets, policies, placement, leases, jobs and snapshots.
`wta-node` is the portable execution/runtime boundary.

ACP is not a scheduler and SSH is not a state store. No component may create a
second workspace/pane/surface hierarchy.

## Consequences

- The Chat Pane follows the selected surface automatically.
- Team and compute context are available as tools/context, not chat scopes.
- A failure in placement or a remote target cannot take down unrelated local
  ACP sessions.
- UI actions must map to the same CLI/store contracts.
