# Reproducible Windows build and installer evidence

## Goal

Every installable Intelligent Terminal artifact must identify the exact source
state that produced it and the exact payload binaries it contains. A successful
MSBuild exit code or a versioned filename is not sufficient evidence.

The implementation is
[`New-ReproducibleBuildManifest.ps1`](../../build/scripts/New-ReproducibleBuildManifest.ps1).
It deliberately wraps the existing canonical installer builder instead of
creating a second build graph.

## Source identity

`Begin` records:

- Git commit and branch.
- The raw `git status` and binary tracked-diff digests as diagnostics.
- SHA-256 and length of every tracked or non-ignored untracked regular file,
  enumerated by Git and sorted ordinally.
- An aggregate digest of that canonical file manifest.
- One aggregate source fingerprint.
- Package, component and Terminal Protocol versions.
- UTC build start.

A dirty local build is traceable, but it is not reproducible from a commit
alone. Release/CI runs therefore pass `-RequireClean`. Generated state and
manifest paths must be outside the repository or matched by `.gitignore`; this
prevents the evidence file from changing its own source fingerprint.

```powershell
pwsh -File build/scripts/New-ReproducibleBuildManifest.ps1 `
  -Mode Begin `
  -StatePath artifacts/repro-build/build-start.json `
  -RequireClean
```

## Completion gates

`Complete` fails closed when:

1. Source changed after `Begin`.
2. A required payload role is missing or duplicated.
3. A core payload has the wrong filename.
4. Installer or payload is empty.
5. Installer or executable payload predates the build start.
6. The installer predates a payload it claims to contain.
7. Installer filename does not contain the source package version.
8. Payload `protocol-version.json` differs from the source capture.

The default required payload roles are:

| Role | Required filename |
|---|---|
| `windows-terminal` | `WindowsTerminal.exe` |
| `wtcli` | `wtcli.exe` |
| `wta` | `wta.exe` |
| `wta-node-windows` | `wta-node.exe` |
| `wta-node-linux-x64` | `wta-node-linux-x64` |
| `protocol-manifest` | `protocol-version.json` |

`protocol-manifest` is the sole default freshness exception: it is a source
metadata file, and its two version fields are compared with the captured source
instead. Use `-AllowOlderPayloadRole <role>` for any additional role only when
it is a deliberate immutable third-party input. Every exception is recorded in
the manifest. It must not be used for `windows-terminal`, `wtcli` or `wta`.

```powershell
pwsh -File build/scripts/New-ReproducibleBuildManifest.ps1 `
  -Mode Complete `
  -StatePath artifacts/repro-build/build-start.json `
  -OutputPath artifacts/repro-build/build-manifest.json `
  -InstallerPath artifacts/repro-build/intelligent-terminal-0.9.4.12-x64-release-setup.exe `
  -Payload `
    "windows-terminal=artifacts/repro-build/payload/WindowsTerminal.exe" `
    "wtcli=artifacts/repro-build/payload/wtcli.exe" `
    "wta=artifacts/repro-build/payload/wta.exe" `
    "wta-node-windows=artifacts/repro-build/payload/wta-node.exe" `
    "wta-node-linux-x64=artifacts/repro-build/payload/wta-node-linux-x64" `
    "protocol-manifest=artifacts/repro-build/payload/protocol-version.json"
```

The result links commit, dirty fingerprint, toolchain, installer and every
payload with SHA-256 and length. A separate `.sha256` file protects the JSON
manifest itself.

## Independent verification

Keep the manifest, sidecar and payload evidence together:

```powershell
pwsh -File build/scripts/New-ReproducibleBuildManifest.ps1 `
  -Mode Verify `
  -ManifestPath artifacts/repro-build/build-manifest.json
```

Add `-VerifyCurrentSource` on the build machine to prove that its checkout still
matches. Verification after download does not require a Git checkout; pass
`-RepoRoot` only because the script itself resolves its default relative to the
repository.

The self-check exercises successful completion plus four negative gates:
payload tampering, mismatched protocol, stale payload and source drift.

```powershell
pwsh -File build/scripts/Test-ReproducibleBuildManifest.ps1
```

## Ephemeral Azure Windows builder

The canonical offload path is
[`Invoke-AzureWindowsBuild.ps1`](../../build/scripts/Invoke-AzureWindowsBuild.ps1).
It intentionally does not register a persistent GitHub self-hosted runner.
That avoids giving an idle machine a durable control relationship with a
public repository and avoids the failure mode where a queued job never starts,
so its final deallocation job never runs.

The controller:

- verifies the exact VM and requires `production=false` plus
  `autoDeallocate=true`;
- builds the Linux helper in WSL/ext4 before source capture;
- captures the exact clean or dirty worktree fingerprint;
- transfers a binary tracked patch and every non-ignored untracked file;
- starts the VM only after the immutable build inputs exist;
- clones the exact commit, applies the overlay and verifies the same source
  fingerprint on the VM;
- invokes the existing `New-WtaLocalInstaller.ps1` canonical build graph;
- downloads the installer, payload hashes, manifest and result;
- independently verifies the downloaded evidence;
- removes only its exact remote run directories;
- always calls `az vm deallocate` in `finally`.

Do not cache `bin`, `obj`, `AppPackages`, `tools/wta/target` or installer stage
directories. Those are outputs, and restoring them would weaken the stale
payload gate.

The local machine needs Azure CLI authentication and the dedicated SSH private
key. The VM needs PowerShell 7, Git, Rust and Visual Studio Build Tools. VS
2026 also needs a standalone `vcpkg` whose executable understands toolset
`v145` and whose registry checkout equals the `builtin-baseline` in
`vcpkg.json`. Provision it with
[`Install-AzureBuilderVcpkg.ps1`](../../build/scripts/Install-AzureBuilderVcpkg.ps1).
The script validates the Microsoft origin, refuses paths outside
`C:\Toolchains`, replaces only its own validated partial-clone cache, checks
out the exact registry commit and bootstraps with metrics disabled.

The build preserves this explicit `VCPKG_ROOT` across `Enter-VsDevShell`;
otherwise VS silently selects its bundled copy. Both x64 and ARM64 drivers log
the exact MSBuild, Visual Studio and vcpkg roots before compilation. The VM
does not need a GitHub token because the repository is public and the dirty
overlay is transferred directly.

```powershell
pwsh -File build/scripts/Invoke-AzureWindowsBuild.ps1
```

The VM region currently does not expose the DevTestLab auto-shutdown schedule
resource. Therefore `autoDeallocate=true` is a policy tag, not an Azure timer;
the controller's `finally` block is the primary cost guard. Azure Policy or an
external Automation Account may be added as a second independent failsafe, but
is not represented as already configured.

### Observed Azure build

Run `run-20260729-012330-5f5588cc` exercised the complete x64 Release graph on
the dedicated VM:

- the exact `x86_64-pc-windows-msvc` Release `wta.exe` and `wta-node.exe` were
  built before `CascadiaPackage`;
- the four MSBuild stages completed with zero errors and produced package
  `0.9.4.12`;
- the final setup is
  `intelligent-terminal-0.9.4.12-x64-release-setup.exe`, 24,241,339 bytes,
  SHA-256
  `44b6025c9eb4ec22ad0e80eb2dc506652eec90b36c167a7f495f2a4094cfa447`;
- protocol `3.1`, Windows Terminal, `wtcli`, WTA, Windows node and Linux x64
  node were captured as separately hashed evidence;
- `Complete` and `Verify -VerifyCurrentSource` passed against reconstructed
  source fingerprint
  `4b1f4be4f0e92106cb1c2ba46a7c8c0c97b5e8e9b85deaec6e70bb8d208c2e1d`;
- the downloaded manifest SHA-256 is
  `d858414030bd8279c6e7d6c1b81b3956aaa1e6ed8a6925e965c0a490d3a3c067`;
- the VM was observed in `PowerState/deallocated` after evidence retrieval.

The run exposed two high-cost orchestration regressions. The installer used to
build WTA after `CascadiaPackage`, although the package correctly requires the
exact WTA pair before packaging. The batch drivers could also propagate a stale
`ERRORLEVEL` after MSBuild reported success. WTA resolution now precedes
Terminal packaging, successful drivers return explicit zero, and
`Test-WtaInstallerBuildOrder.ps1` enforces both invariants.

This was a dirty-worktree engineering build, not a signed release. Its
reconstructed source fingerprint is traceable, but it does not satisfy the
clean-source release gate below.

## Release gate

An installer is eligible for installation or release only when:

1. `Begin` used `-RequireClean`.
2. `Complete` succeeded with all executable core roles fresh and a matching
   protocol manifest.
3. `Verify -VerifyCurrentSource` succeeded on the builder.
4. The uploaded evidence artifact contains the installer, payload directory,
   manifest and sidecar.
5. A clean machine can run `Verify` after download.

This evidence answers “which source and binaries are installed?” It does not
replace signing, unit tests, physical SSH gates or installation smoke tests.
