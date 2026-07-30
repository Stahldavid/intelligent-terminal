# ADR-014: Keep elastic compute explicit, allowlisted and budget-bound

## Status

Accepted — 2026-07-27

## Decision

Cloud lifecycle commands are opt-in. Azure resources require an explicit
resource ID, trust tier, allowlist and cost policy. Discovery, startup and
installation never start, stop, create or delete a VM.

Production and restricted targets are excluded from automatic placement.
Idle deallocation must first prove that no active binding, job or lease uses
the target.

## Consequences

- Production machines cannot become generic worker capacity accidentally.
- Budget/quota checks fail closed.
- Cloud mutation remains auditable and separable from normal Terminal use.
