# ADR-007: Authorization is enforced at execution boundaries

- Status: Accepted incrementally
- Date: 2026-07-26

> **Current state:** Protocol 3.0 introduced authenticated execution
> boundaries. Protocol 3.1 completes the scoped-capability follow-up described
> below for ordinary surfaces and trusted workspace helpers, including
> operation masks, topology filtering, and scoped events. The host master
> remains in the trusted computing base.

## Context

A disabled button is not a security boundary. Agent and pane processes run as
the user and can call protocol surfaces directly if they possess ambient
credentials.

## Decision

Terminal Protocol 3.0 requires a random per-launch token and successful
per-COM-instance `Authenticate` before every method. `wtcli` fails closed
without it. ACP Agent CLI and native-team worker children have the token and
COM CLSID removed. ACP terminal create/read/terminate operations enforce
`auto`, `prompt` or `deny` policy in the client boundary.

## Consequences

This decision began as a host-scoped enforcement layer. Protocol 3.1 now gives
ordinary ConPTY children a signed surface capability and trusted WTA helpers a
workspace capability. Subject, issuer, scope, operation mask, expiry, nonce,
topology filtering, and scoped event delivery are implemented. The nonce is
integrity-protected but is not backed by a server-side one-use revocation
ledger, and confirmation policy is not yet universal across every direct
protocol/team/settings operation.
