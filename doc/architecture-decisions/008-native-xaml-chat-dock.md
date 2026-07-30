# ADR-008: Native XAML Chat Dock, not a WebView2 chat

- Status: Implemented; installed visual validation complete
- Date: 2026-07-26

## Context

The chat needs Windows accessibility, theming, localization and a clear scope
relationship without introducing a browser security/profile boundary.

## Decision

Use XAML/WinUI for the complete chat dock: contextual chrome, passive
`Following` indicator, conversation messages, streaming projection, tool and
permission cards, composer and sidebar integration. The dock always follows
the focused terminal surface and does not expose a manual scope selector.
Agent/team coordination is an explicit sidebar experience. WebView2 is absent
from the Chat Pane.

WTA publishes immutable, monotonically sequenced snapshots through the
authenticated Terminal Protocol. The dock emits structured actions containing
the canonical workspace and exact active scope key. WTA rejects missing,
foreign or stale identities before reusing its existing prompt, cancel,
permission and restart pipelines.

## Consequences

The chat view is rendered by native XAML controls. The helper's `TermControl`
is retained only as a compatibility surface for setup, authentication and the
session-management view; it is collapsed while the native chat view is active.

This decision applies to chat rendering. A later, separate Browser Surface may
host WebView2 for remote web content when it can establish an isolated
per-surface profile, scoped proxy, and fail-closed policy. Browser content does
not replace or share state with the native Chat Pane.

The snapshot publisher keeps one authenticated `wtcli publish-stdin` process,
requires an acknowledgement for each event, restarts on failure and falls back
to ordered one-shot publishing for older binaries. This avoids spawning a
process per streaming update without weakening server-side authorization.

Build and deterministic tests validate the contract. The installed x64 release
was visually verified with the dock following the focused PowerShell surface
and without a scope selector. Narrator inspection, high-contrast review and a
real adapter streaming/permission round trip remain manual/E2E release gates
and must not be inferred from compilation or the visual smoke test.
