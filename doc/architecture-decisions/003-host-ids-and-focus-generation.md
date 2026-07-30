# ADR-003: Host-issued identities and focus generations

- Status: Accepted
- Date: 2026-07-26

## Context

Visual indices change when panes or surfaces close, move or restore. Async ACP
updates can arrive after the user has selected another surface.

## Decision

Use stable host identities for window/workspace and terminal session identity
for the current live surface. Emit a monotonically increasing
`focus_generation` with the complete focus snapshot. Receivers reject missing,
zero, cross-window and stale generations.

## Consequences

Pane-local ordinals are display metadata only. A restored process must
revalidate its live terminal session instead of assuming an old handle or
session still exists. Dedicated surface lifecycle events remain follow-up work.
