# ADR-004: Canonical profiles with destination dispatch

- Status: Accepted incrementally
- Date: 2026-07-26

## Context

Profile creation must support PowerShell, WSL, SSH, Azure and custom/dynamic
profiles without a fork-owned registry.

## Decision

Creation always carries a native `INewContentArgs`. The destination determines
whether it becomes a workspace, a surface or a split. Primary surface `+`
duplicates the current profile/CWD; its flyout selects a profile and uses the
profile default CWD.

## Consequences

TerminalPage remains the only terminal-content factory. The current surface
flyout consumes canonical `ActiveProfiles`; preserving the complete customized
`newTabMenu` folder/action hierarchy for surface destinations is still pending.
