# ADR-001: Native tab is the workspace

- Status: Accepted
- Date: 2026-07-26

## Context

Windows Terminal already owns tab identity, ordering, color, title, restore,
move-to-window, close history and context actions. A second workspace
collection duplicated lifecycle and caused competing navigation.

## Decision

A native `Tab` is the only workspace object. The left sidebar is a projection
of native tabs plus optional metadata (pin, group, agents and teams). A split
region is a pane; sessions stacked inside it are surfaces.

## Consequences

Horizontal tabs may be hidden while the sidebar is expanded, but they remain
the source of truth. Sidebar actions must dispatch through native tab actions.
Persisted WTA metadata references the stable native workspace ID and never
owns tab destruction or restoration independently.
