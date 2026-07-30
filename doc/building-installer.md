# Building Installers

There are two installer types for distributing Intelligent Terminal.

## 1. MSIX ZIP Installer (Packaged)

A ZIP containing a dev certificate, signed MSIX package, XAML dependency, install script, and FRE reset helper. Recipients run `Install-Msix.ps1` to sideload the packaged app.

### Output structure

```
intelligent-terminal-<version>-<arch>-msix.zip
├── IntelligentTerminalDev.cer                    # Dev signing certificate
├── CascadiaPackage_<version>_<arch>.msix         # Signed Terminal MSIX
├── Dependencies/
│   └── Microsoft.UI.Xaml.2.8.appx                # XAML framework dependency
├── Install-Msix.ps1                              # Imports cert + installs packages
└── fre-test-reset.ps1                            # Resets First Run Experience for repeat testing
```

### Prerequisites

- Visual Studio 18 / Build Tools 2026 with the v145 C++ desktop & UWP
  workloads (matching the repository overlay triplets)
- Windows SDK 10.0.26100.0
- Rust toolchain (`cargo`, `rustup`) with both targets:
  ```
  rustup target add x86_64-pc-windows-msvc
  rustup target add aarch64-pc-windows-msvc
  ```

---

### TL;DR (typical version bump + ship)

Five lines, in order. Step details below.

```powershell
# 0. Bump the four package manifests and tools/wta/Cargo.toml
# 1. Provision the local signing certificate when producing a signed build
# 2. cargo build --release --target {x86_64,aarch64}-pc-windows-msvc --manifest-path tools/wta/Cargo.toml
# 3. .\_build_msix_x64.cmd   AND THEN   .\_build_msix_arm64.cmd      # serial — see note
# 4. .\_sign_msix.cmd
# 5. powershell -File build\scripts\assemble-msix-zip.ps1 -Version 0.9.4.12 -Arch x64
#    powershell -File build\scripts\assemble-msix-zip.ps1 -Version 0.9.4.12 -Arch ARM64
```

The driver scripts ([`_build_msix_x64.cmd`](../_build_msix_x64.cmd), [`_build_msix_arm64.cmd`](../_build_msix_arm64.cmd), [`_sign_msix.cmd`](../_sign_msix.cmd)) live at the repo root and encode workarounds the bare MSBuild invocation doesn't handle — see Step 3 for what they actually do.

---

### Step 0: Bump the version

Keep the product version coherent across `tools/wta/Cargo.toml` and all four
package manifests (`Package.appxmanifest`, `Package-Pre.appxmanifest`,
`Package-Can.appxmanifest`, and `Package-Dev.appxmanifest`). Then run:

```powershell
pwsh -File build\scripts\Verify-IntelligentTerminalVersion.ps1
```

[`_sign_msix.cmd`](../_sign_msix.cmd) reads the Dev manifest version
dynamically; it has no hardcoded package version.

### Step 1: Dev signing certificate

Signed local releases expect
`cert\IntelligentTerminalDev.pfx`. The private key is a local credential and
must be provisioned explicitly; do not commit a newly generated PFX.

To regenerate from scratch (e.g., cert expired — they're valid 3 years):

```powershell
powershell -ExecutionPolicy Bypass -File build\scripts\New-DevSigningCert.ps1
```

The script can generate a local certificate when authorized. Place the
resulting PFX at `cert\IntelligentTerminalDev.pfx` and distribute only the
public CER to installation targets.

### Step 2: Build `wta.exe`

```powershell
# x64
cargo build --release -j 2 --target x86_64-pc-windows-msvc --manifest-path tools/wta/Cargo.toml

# ARM64 (cross-compile)
cargo build --release -j 2 --target aarch64-pc-windows-msvc --manifest-path tools/wta/Cargo.toml
```

These two are independent, but run them serially on a normal developer
workstation. The release profile is partitioned to bound optimizer memory and
`-j 2` also prevents dependency compilation from starving the desktop.

MSBuild picks up `wta.exe` automatically from the Cargo output via a `<Content>` rule in `CascadiaPackage.wapproj`:
- x64: `tools\wta\target\x86_64-pc-windows-msvc\release\wta.exe`
- ARM64: `tools\wta\target\aarch64-pc-windows-msvc\release\wta.exe`

> **Always pass `--target` explicitly.** When
> `GenerateAppxPackageOnBuild=true`, the wapproj fails closed unless the exact
> target/profile executable exists. Host-architecture and opposite-profile
> fallbacks remain useful for local F5 builds, but are rejected for an MSIX.

> **Always re-run cargo even if you think source didn't change.** Cargo's incremental check is fast (~seconds for a no-op) and serves as cheap insurance against the "wta source did change but I forgot" footgun that bit 0.7.0.12.

### Step 2a: Build the Linux `wta-node`

Managed SSH surfaces require the Linux x64 node helper. Build it through the
WSL staging wrapper:

```powershell
pwsh -File build\scripts\Build-WtaNodeLinux.ps1 `
  -Distro Ubuntu-22.04 `
  -Configuration Release
```

Do not run `cargo` directly against the checkout through `/mnt/c`. Cargo and
rustc perform many small metadata operations, and DrvFS makes that path much
slower. The wrapper keeps the Windows checkout authoritative, stages only
`tools/wta` plus the telemetry header into
`$HOME/.cache/intelligent-terminal/linux-build` on the distro's ext4
filesystem, reuses an ext4 `CARGO_TARGET_DIR`, and copies only the final ELF
back to:

```text
tools\wta\remote\linux-x64\wta-node
```

The script excludes `tools/wta/target` and prior remote artifacts from the
stage, validates all cleanup paths before recursive deletion, sets the final
file executable, and prints its SHA-256. There is no need to move the full
repository into WSL or change `.wslconfig`.

### Step 3: Build the Terminal MSIX

Use the wrapper scripts — **not** the bare MSBuild command:

```powershell
.\_build_msix_x64.cmd        # x64 must finish first
.\_build_msix_arm64.cmd      # then ARM64
```

> **Run them serially.** x64 and ARM64 share `Generated Files\Profiles_Advanced.xaml` (and other generated XAML files) under `src\cascadia\TerminalSettingsEditor\`. Parallel builds race on those files and one of them dies with `WMC9999 — being used by another process`.

[`_build_msix_x64.cmd`](../_build_msix_x64.cmd) and [`_build_msix_arm64.cmd`](../_build_msix_arm64.cmd) do six things the bare MSBuild invocation doesn't:

1. **Wipe `obj\<Platform>\Release\` and `bin\<Platform>\Release\AppX\`** before building. The wapproj has a glob-based `<Content Include="...\wt-agent-hooks\**">` rule (for the agent hook bundle); incremental MSBuild caches the resolved file list and silently drops freshly-added files. 0.7.0.5 and 0.7.0.6 shipped without `wt-agent-hooks\` because of this — every "Install hooks" click failed until we figured it out.
2. **Pre-build `Host.Proxy.vcxproj` (`OpenConsoleProxy`)** so MIDL generates `ITerminalHandoff.h` and `ITerminalProtocol.h` before `TerminalConnection`, `wtcli` or another parallel consumer can start.
3. **Pin vcpkg to the same Visual Studio installation as MSBuild** with
   `VCPKG_VISUAL_STUDIO_PATH`. Without it, vcpkg auto-selects the newest VS
   installed and can create `.lib` files that reference a newer STL than the
   selected linker provides.
4. **Pre-build `Microsoft.Terminal.Settings.ModelLib.vcxproj`** so its `Microsoft.Terminal.Settings.Model.winmd` is the source of truth before any consumer (`TerminalSettingsAppAdapterLib`, etc.) calls `cppwinrt` to regenerate its WinRT projection headers. Without this, `cppwinrt` can scan a stale winmd from `bin\<Platform>\Release\<OtherProject>\` and emit projections missing newer members (e.g. `DragDropDelimiter` → `C2039` in `TerminalSettings.cpp`).
5. **Pre-build `Microsoft.Terminal.Settings.Editor.vcxproj`** to generate XBF files. Otherwise, `TerminalAppLib` starts before `AIAgents.xaml.g.h` exists and fails with `MSB3030: file not found`.
6. **`exit /b %BUILD_EXIT%`** at the end. The previous `echo Exit code: %ERRORLEVEL%` made the shell return 0 even when MSBuild failed, masking real errors as silent "successful" runs. 0.7.0.10 wasted a round on this.
7. **Keep the x64 C++ build serial** (`/m:1`, `CL_MPCount=1`). The
   C++/WinRT projects use large precompiled headers; unrestricted `/MP` can
   exhaust the default Windows page file and fail with `C3859`/system error
   1455 even when source code is valid.

#### ARM64 quirks

- **`ITerminalHandoff.h` not found**: this was a parallel generation race.
  Current drivers pre-build `OpenConsoleProxy`; treat any recurrence as a
  driver regression instead of relying on a second pass.
- **`APPX1204: SignTool Error: The file is being used by another process`**: MSBuild's built-in auto-sign (kicked off by `<AppxPackageSigningEnabled>true</AppxPackageSigningEnabled>` inferred from PFX presence) sometimes loses a race with AV/indexer locking the freshly-produced MSIX. The MSIX is still written to disk; just run [`_sign_msix.cmd`](../_sign_msix.cmd) in Step 4 to sign it explicitly. 0.7.0.14 ARM64 hit this.
- **Missing `Dependencies\` folder**: when MSBuild's auto-sign fails as above, it also skips staging the XAML dependency. Copy it manually from a prior successful build:
  ```powershell
  Copy-Item -Recurse src\cascadia\CascadiaPackage\AppPackages\CascadiaPackage_<PRIOR>_ARM64_Test\Dependencies `
                     src\cascadia\CascadiaPackage\AppPackages\CascadiaPackage_<NEW>_ARM64_Test\Dependencies
  ```
  The XAML appx is identical across our builds.

MSBuild outputs to:
```
src\cascadia\CascadiaPackage\AppPackages\CascadiaPackage_<version>_<arch>_Test\
├── CascadiaPackage_<version>_<arch>.msix      # may be unsigned if auto-sign raced
└── Dependencies\<arch>\Microsoft.UI.Xaml.2.8.appx
```

#### Bare MSBuild commands (for reference)

If you ever need to skip the wrapper:

```powershell
$env:MSBUILD = "C:\path\to\MSBuild.exe"
$env:REPO = (Get-Location).Path

& $env:MSBUILD src\cascadia\CascadiaPackage\CascadiaPackage.wapproj `
    /p:Platform=x64 /p:Configuration=Release /p:WindowsTerminalBranding=Dev `
    /p:GenerateAppxPackageOnBuild=true /p:AppxBundle=Never `
    /p:SolutionDir="$env:REPO\" /m:2 /p:CL_MPCount=2 /nodeReuse:false /nologo
```

You will hit the issues listed above. Use the wrappers.

### Step 4: Sign the MSIXs

```powershell
.\_sign_msix.cmd
```

[`_sign_msix.cmd`](../_sign_msix.cmd) signs both x64 and ARM64 with `cert\IntelligentTerminalDev.pfx` (SHA256, empty password). Use this even if MSBuild's auto-sign succeeded — it's idempotent and ensures both arches end up with our cert specifically.

### Step 5: Assemble the ZIPs

```powershell
powershell -File build\scripts\assemble-msix-zip.ps1 -Version 0.9.4.12 -Arch x64
powershell -File build\scripts\assemble-msix-zip.ps1 -Version 0.9.4.12 -Arch ARM64
```

Output: `artifacts\local-installer\intelligent-terminal-<version>-<arch>-msix.zip`

The script ([`build\scripts\assemble-msix-zip.ps1`](../build/scripts/assemble-msix-zip.ps1)) copies five things into each ZIP:
- Signed MSIX from `src\cascadia\CascadiaPackage\AppPackages\CascadiaPackage_<version>_<arch>_Test\`
- `Dependencies\<arch>\Microsoft.UI.Xaml.2.8.appx`
- `artifacts\local-installer\IntelligentTerminalDev.cer`
- [`installer\Install-Msix.ps1`](../installer/Install-Msix.ps1)
- [`tools\fre-test-reset.ps1`](../tools/fre-test-reset.ps1) — FRE reset helper for repeat testing

### Install on target machine

```powershell
# Extract the ZIP, then run (no admin needed if cert is already trusted):
powershell -ExecutionPolicy Bypass -File .\Install-Msix.ps1
```

`Install-Msix.ps1` does three things:
1. Removes any old unpackaged install (`%LOCALAPPDATA%\Programs\IntelligentTerminal`)
2. Imports `IntelligentTerminalDev.cer` into the Trusted People store — **only if not already trusted** (this step requires admin; subsequent installs skip it)
3. Installs the XAML dependency and the Terminal MSIX via `Add-AppxPackage` (per-user, no elevation needed)

To repeat-test the FRE, run [`fre-test-reset.ps1`](../tools/fre-test-reset.ps1) from the same extracted ZIP and pick `[1]` (just the FRE flag) or `[A]` (full reset including Copilot CLI uninstall).

### Certificate notes

- `cert\IntelligentTerminalDev.pfx` is a local signing credential. Do not
  commit or rotate it casually; installed builds trust its public certificate.
- The same cert signs both x64 and ARM64 MSIXs; no need to regenerate per-arch.
- Regenerate with
  [`build\scripts\New-DevSigningCert.ps1`](../build/scripts/New-DevSigningCert.ps1)
  only when explicitly authorized, then redistribute the public CER.

---

## 2. Self-Extracting EXE Installer (Unpackaged / Portable)

Built by [`build\scripts\New-WtaLocalInstaller.ps1`](../build/scripts/New-WtaLocalInstaller.ps1). Creates a portable distribution with `WindowsTerminal.exe`, `wta.exe`, `wtcli.exe`, and prompt templates — no MSIX, no package identity.

### Prerequisites

- Everything from the MSIX build above
- Rust toolchain (`cargo`, `rustup`) with the target platform installed
- A pre-built Terminal MSIX is only needed when deliberately using
  `-TerminalMsix -AllowPrebuiltTerminal`. The normal path rebuilds Terminal.

### Build command

```powershell
# Safe default: builds both Terminal MSIX and WTA from the same source tree:
.\build\scripts\New-WtaLocalInstaller.ps1 -Platform x64 -Configuration Release

# Explicit prebuilt package. The packaged protocol manifest is still required
# to match the current source before assembly is allowed:
.\build\scripts\New-WtaLocalInstaller.ps1 -Platform x64 -Configuration Release `
    -TerminalMsix C:\artifacts\IntelligentTerminal.msix -AllowPrebuiltTerminal

# Skip only the WTA rebuild while Terminal is rebuilt safely:
.\build\scripts\New-WtaLocalInstaller.ps1 -Platform x64 -Configuration Release `
    -SkipWtaBuild -WtaExePath tools\wta\target\x86_64-pc-windows-msvc\release\wta.exe
```

### Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `-Platform` | `ARM64` | Target arch: `x64`, `ARM64`, `x86` |
| `-Configuration` | `Debug` | `Debug` or `Release` |
| `-Destination` | `artifacts\local-installer` | Output directory |
| `-BuildTerminal` | automatic | Build Terminal MSIX from source. Automatically enabled unless `-TerminalMsix` is supplied |
| `-AllowPrebuiltTerminal` | (off) | Required with `-TerminalMsix`; does not bypass protocol-manifest validation |
| `-SkipWtaBuild` | (off) | Skip Rust build; requires `-WtaExePath` |
| `-WtaExePath` | (auto) | Path to pre-built `wta.exe` |
| `-TerminalMsix` | (none) | Explicit prebuilt Terminal MSIX; requires `-AllowPrebuiltTerminal` |
| `-XamlAppx` | (auto-detect) | Override path to XAML dependency |

### What it does

1. Builds the Terminal MSIX by default through the canonical, memory-bounded
   platform driver, or accepts an explicitly authorized prebuilt MSIX
2. Runs [`New-UnpackagedTerminalDistribution.ps1`](../build/scripts/New-UnpackagedTerminalDistribution.ps1) to extract the MSIX into a portable layout
3. Verifies the packaged `protocol-version.json`, `WindowsTerminal.exe`, and `wtcli.exe`
4. Builds `wta.exe` and the native `wta-node.exe` (Rust, release, static CRT)
   for the target platform with two Cargo jobs at most
5. Injects `wta.exe`, `wta-node.exe`, the verified Linux x64 helper when
   available, and prompt templates; the MSIX-built `wtcli.exe` is never
   replaced independently
6. Records protocol/component versions and SHA-256 hashes for every injected
   executable
7. Creates `payload.zip`, builds the Rust bootstrap, and assembles the self-extracting EXE

### Output

```
artifacts\local-installer\intelligent-terminal-<version>-<arch>-<config>-setup.exe
```

### Install on target machine

Just run the `.exe`. It self-extracts and launches `install.cmd`, which calls `install-local-terminal.ps1`.

Install location: `%LOCALAPPDATA%\Programs\IntelligentTerminal`

Options (pass to `install.cmd`): `/quiet`, `/nopath`, `/noshortcuts`

After installation, verify the authenticated protocol from a real terminal
surface:

```powershell
pwsh -NoProfile -File build\scripts\Test-InstalledTerminalProtocol.ps1
```

The script intentionally creates a short-lived pane before invoking `wtcli`.
Running `wtcli` from an unrelated shell has no `WT_COM_CLSID` and must fail
closed; that failure is not a server authentication regression.

---

## Quick reference

| Goal | Command |
|------|---------|
| Generate dev cert (one-time / expired) | [`powershell -File build\scripts\New-DevSigningCert.ps1`](../build/scripts/New-DevSigningCert.ps1) |
| Build wta (x64) | `cargo build --release --target x86_64-pc-windows-msvc --manifest-path tools/wta/Cargo.toml` |
| Build wta (ARM64) | `cargo build --release --target aarch64-pc-windows-msvc --manifest-path tools/wta/Cargo.toml` |
| Build MSIX (x64) | [`.\_build_msix_x64.cmd`](../_build_msix_x64.cmd) |
| Build MSIX (ARM64) | [`.\_build_msix_arm64.cmd`](../_build_msix_arm64.cmd) |
| Sign both MSIXs | [`.\_sign_msix.cmd`](../_sign_msix.cmd) |
| Assemble MSIX ZIP | [`powershell -File build\scripts\assemble-msix-zip.ps1 -Version X.X.X.X -Arch x64`](../build/scripts/assemble-msix-zip.ps1) |
| Build self-extracting EXE | [`.\build\scripts\New-WtaLocalInstaller.ps1 -Platform x64 -Configuration Release`](../build/scripts/New-WtaLocalInstaller.ps1) |
| Verify installed protocol | [`pwsh -File build\scripts\Test-InstalledTerminalProtocol.ps1`](../build/scripts/Test-InstalledTerminalProtocol.ps1) |

## Key files

| File | Purpose |
|------|---------|
| [`_build_msix_x64.cmd`](../_build_msix_x64.cmd) | Wrapper around MSBuild for x64 with all the workarounds |
| [`_build_msix_arm64.cmd`](../_build_msix_arm64.cmd) | Same for ARM64 |
| [`_sign_msix.cmd`](../_sign_msix.cmd) | Signs both arches with the locally provisioned `cert\IntelligentTerminalDev.pfx` |
| `cert\IntelligentTerminalDev.pfx` | Local, uncommitted development signing credential |
| [`build\scripts\New-DevSigningCert.ps1`](../build/scripts/New-DevSigningCert.ps1) | Generates PFX + CER for dev signing (only when expired) |
| [`build\scripts\assemble-msix-zip.ps1`](../build/scripts/assemble-msix-zip.ps1) | Assembles the MSIX ZIP from build outputs |
| [`installer\Install-Msix.ps1`](../installer/Install-Msix.ps1) | Install script included in the MSIX ZIP |
| [`tools\fre-test-reset.ps1`](../tools/fre-test-reset.ps1) | FRE reset helper bundled in the ZIP |
| [`build\scripts\New-WtaLocalInstaller.ps1`](../build/scripts/New-WtaLocalInstaller.ps1) | Self-extracting EXE builder |
| [`build\scripts\Test-InstalledTerminalProtocol.ps1`](../build/scripts/Test-InstalledTerminalProtocol.ps1) | In-pane authentication and protocol-version probe |
| [`build\scripts\New-UnpackagedTerminalDistribution.ps1`](../build/scripts/New-UnpackagedTerminalDistribution.ps1) | Extracts MSIX into portable layout |
| [`installer\bootstrap\`](../installer/bootstrap/) | Rust self-extracting bootstrap |
| [`installer\install-local-terminal.ps1`](../installer/install-local-terminal.ps1) | Unpackaged installer script |
| [`installer\install.cmd`](../installer/install.cmd) | CMD wrapper for the unpackaged installer |
| `artifacts\local-installer\` | Build output (gitignored) |
