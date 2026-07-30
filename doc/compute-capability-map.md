# Compute capability map

This matrix is the canonical product-level map for the distributed compute
control plane. “External gate” means the implementation exists but validation
requires authorized infrastructure.

| Capability | Local | WSL | SSH dev host | Azure dev VM | Production |
|---|---:|---:|---:|---:|---:|
| Plain terminal surface | yes | yes | yes | after SSH enable | explicit only |
| Managed ACP surface | yes | yes | implemented; external gate | after SSH/auth gate | blocked by default |
| Sticky HomeTarget | yes | yes | yes | yes | no automatic placement |
| Explicit build/test job | yes | yes | implemented; external gate | after lifecycle gate | explicit policy only |
| Git replica/snapshot | yes | yes | implemented; transfer gate | transfer gate | denied by default |
| Target probe and doctor | yes | yes | yes | after start | read-only unless authorized |
| Node bootstrap/hash | n/a | packaged | implemented; external gate | external gate | explicit only |
| Persistent PTY/reattach | local ConPTY | verified | physical SSH observed | after SSH gate | explicit only |
| Root-scoped files | local filesystem | verified | physical SSH observed | after SSH gate | denied by default |
| Remote proxy/relay | loopback only | verified | backend physically observed | after SSH gate | denied by default |
| Browser Surface | local content | supported | source preview; UI gate | after proxy/UI gate | denied by default |
| Runtime restore metadata | yes | yes | implemented; app E2E gate | after lifecycle gate | explicit only |
| Auto placement | yes | yes | only when healthy/enabled | budget + allowlist | never |
| Start/deallocate | n/a | n/a | n/a | explicit CLI only | blocked by default |
| Agent/team context | yes | yes | yes | yes | policy-limited |

## Required capability labels

- `exec`: fixed-argv job execution without a shell-concatenated command.
- `sha256`: remote helper and artifact verification.
- `resource_probe`: sanitized CPU/memory/capacity status.
- `session_registry`: remote session identity and reconnect metadata.
- `codex`, `claude`, `gemini`, or another adapter label: the corresponding ACP
  adapter is installed and authenticated on that target.

## Trust rules

1. Newly discovered SSH targets are `restricted` and `disabled`.
2. A target becomes eligible only after explicit trust, probe and enable.
3. `production` is never selected automatically.
4. Project allowlists and required capabilities are constraints, not score
   hints.
5. A missing credential/capability excludes the target and produces an
   explainable reason.
6. Browser, file, proxy, and relay operations require the exact workspace,
   binding, and surface capability; a ready SSH target alone is insufficient.

“Implemented” in this matrix describes source capability. The physical and
installed UI gates are tracked in
[`specs/cmux-ssh-full-parity-plan.md`](specs/cmux-ssh-full-parity-plan.md).
