[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Origin,

    [Parameter(Mandatory)]
    [string]$Commit,

    [Parameter(Mandatory)]
    [string]$ExpectedSourceFingerprint,

    [Parameter(Mandatory)]
    [string]$InputRoot,

    [Parameter(Mandatory)]
    [string]$OutputRoot,

    [Parameter(Mandatory)]
    [string]$LinuxNodeAttestationPath,

    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release',

    [ValidateSet('x64')]
    [string]$Platform = 'x64'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Resolve-RequiredFile {
    param([Parameter(Mandatory)][string]$Path)

    $item = Get-Item -LiteralPath $Path -ErrorAction Stop
    if ($item.PSIsContainer) {
        throw "Expected a file, got a directory: $Path"
    }
    return $item.FullName
}

function Add-BuildToolPath {
    $paths = [Collections.Generic.List[string]]::new()
    foreach ($candidate in @(
        'C:\Toolchains\PortableGit\cmd',
        'C:\Toolchains\Rust\cargo\bin',
        'C:\Program Files\PowerShell\7',
        'C:\Program Files\dotnet'
    )) {
        if (Test-Path -LiteralPath $candidate -PathType Container) {
            $paths.Add($candidate)
        }
    }

    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
        throw 'Visual Studio Installer vswhere.exe is unavailable.'
    }
    $installation = (& $vswhere -latest -products * -property installationPath).Trim()
    if ([string]::IsNullOrWhiteSpace($installation)) {
        throw 'Visual Studio Build Tools are unavailable.'
    }
    $msbuild = Join-Path $installation 'MSBuild\Current\Bin'
    if (-not (Test-Path -LiteralPath (Join-Path $msbuild 'MSBuild.exe') -PathType Leaf)) {
        throw "MSBuild.exe is unavailable under $installation."
    }
    $paths.Add($msbuild)

    # VS 2026/v145 discovery requires a recent vcpkg tool. The Azure builder
    # carries a release-pinned standalone checkout so a stale Visual Studio
    # bundled copy cannot make otherwise identical builds fail or select a
    # different toolset.
    $standaloneVcpkg = 'C:\Toolchains\vcpkg'
    if (-not (Test-Path -LiteralPath (Join-Path $standaloneVcpkg 'vcpkg.exe') -PathType Leaf)) {
        throw "The release-pinned Azure vcpkg tool is unavailable at $standaloneVcpkg."
    }
    $env:VCPKG_ROOT = $standaloneVcpkg
    $env:VCPKG_DISABLE_METRICS = '1'
    $paths.Add($standaloneVcpkg)

    $env:PATH = (($paths.ToArray() + @($env:PATH)) -join ';')
}

function Copy-ReproducibleEvidence {
    param(
        [Parameter(Mandatory)][string]$RepoRoot,
        [Parameter(Mandatory)][string]$BuildStartedUtc,
        [Parameter(Mandatory)][string]$EvidenceRoot
    )

    $started = [datetime]::Parse($BuildStartedUtc)
    $installer = Get-ChildItem (Join-Path $RepoRoot 'artifacts\local-installer') `
        -Filter 'intelligent-terminal-*-x64-release-setup.exe' -File |
        Where-Object LastWriteTimeUtc -ge $started.AddSeconds(-2) |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1
    if (-not $installer) {
        throw 'No installer produced by this build was found.'
    }

    $stage = Get-ChildItem (Join-Path $RepoRoot 'artifacts\local-installer') `
        -Filter 'stage-x64-release-*' -Directory |
        Where-Object CreationTimeUtc -ge $started.AddSeconds(-2) |
        Sort-Object CreationTimeUtc -Descending |
        Select-Object -First 1
    if (-not $stage) {
        throw 'No current-build installer stage directory was found.'
    }

    $payloadRoot = Join-Path $stage.FullName 'payload-extracted'
    $payloadEvidence = Join-Path $EvidenceRoot 'payload'
    [IO.Directory]::CreateDirectory($payloadEvidence) | Out-Null
    $roles = [ordered]@{
        'windows-terminal' = 'WindowsTerminal.exe'
        'wtcli' = 'wtcli.exe'
        'wta' = 'wta.exe'
        'wta-node-windows' = 'wta-node.exe'
        'wta-node-linux-x64' = 'wta-node-linux-x64'
        'protocol-manifest' = 'protocol-version.json'
    }
    $payloadArguments = [Collections.Generic.List[string]]::new()
    foreach ($entry in $roles.GetEnumerator()) {
        $matches = @(Get-ChildItem $payloadRoot -Recurse -File -Filter $entry.Value)
        if ($matches.Count -ne 1) {
            throw "Expected exactly one $($entry.Value) under $payloadRoot, found $($matches.Count)."
        }
        $destination = Join-Path $payloadEvidence $entry.Value
        Copy-Item -LiteralPath $matches[0].FullName -Destination $destination
        (Get-Item -LiteralPath $destination).LastWriteTimeUtc = $matches[0].LastWriteTimeUtc
        $payloadArguments.Add("$($entry.Key)=$destination")
    }

    $installerDestination = Join-Path $EvidenceRoot $installer.Name
    Copy-Item -LiteralPath $installer.FullName -Destination $installerDestination
    (Get-Item -LiteralPath $installerDestination).LastWriteTimeUtc = $installer.LastWriteTimeUtc
    return [pscustomobject]@{
        Installer = $installerDestination
        Payload = $payloadArguments.ToArray()
    }
}

Add-BuildToolPath

$patchPath = Resolve-RequiredFile (Join-Path $InputRoot 'tracked.patch')
$untrackedPath = Resolve-RequiredFile (Join-Path $InputRoot 'untracked.zip')
$runId = Split-Path $InputRoot -Leaf
$workspaceRoot = Join-Path $env:SystemDrive "IntelligentTerminalBuild\work\$runId"
$repoRoot = Join-Path $workspaceRoot 'source'
$stateRoot = Join-Path $workspaceRoot 'state'
$evidenceRoot = Join-Path $OutputRoot 'evidence'
$resultPath = Join-Path $OutputRoot 'result.json'

if (Test-Path -LiteralPath $workspaceRoot) {
    throw "Refusing to overwrite existing remote build workspace: $workspaceRoot"
}
[IO.Directory]::CreateDirectory($workspaceRoot) | Out-Null
[IO.Directory]::CreateDirectory($stateRoot) | Out-Null
[IO.Directory]::CreateDirectory($OutputRoot) | Out-Null
[IO.Directory]::CreateDirectory($evidenceRoot) | Out-Null

try {
    & git clone --filter=blob:none --no-checkout -- $Origin $repoRoot
    if ($LASTEXITCODE -ne 0) {
        throw 'git clone failed on the Azure builder.'
    }
    & git -C $repoRoot checkout --detach $Commit
    if ($LASTEXITCODE -ne 0) {
        throw "Could not check out source commit $Commit."
    }
    if ((Get-Item $patchPath).Length -gt 0) {
        & git -C $repoRoot apply --binary --whitespace=nowarn -- $patchPath
        if ($LASTEXITCODE -ne 0) {
            throw 'Could not apply the tracked dirty-worktree patch.'
        }
    }
    Expand-Archive -LiteralPath $untrackedPath -DestinationPath $repoRoot -Force
    $emptyOverlayMarker = Join-Path $repoRoot '.wta-empty-overlay'
    if (Test-Path -LiteralPath $emptyOverlayMarker -PathType Leaf) {
        Remove-Item -LiteralPath $emptyOverlayMarker -Force
    }

    $vcpkgManifest = Get-Content -LiteralPath (Join-Path $repoRoot 'vcpkg.json') -Raw |
        ConvertFrom-Json
    $expectedVcpkgCommit = [string]$vcpkgManifest.'builtin-baseline'
    $actualVcpkgCommit = (& git.exe -C $env:VCPKG_ROOT rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or
        $expectedVcpkgCommit -notmatch '^[0-9a-f]{40}$' -or
        $actualVcpkgCommit -ne $expectedVcpkgCommit) {
        throw "Azure vcpkg registry '$actualVcpkgCommit' does not match source builtin-baseline '$expectedVcpkgCommit'. Re-run Install-AzureBuilderVcpkg.ps1 for this source state."
    }

    $manifestTool = Join-Path $repoRoot 'build\scripts\New-ReproducibleBuildManifest.ps1'
    $statePath = Join-Path $stateRoot 'build-start.json'
    & $manifestTool -Mode Begin -RepoRoot $repoRoot -StatePath $statePath
    $sourceState = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
    if ($sourceState.source.fingerprint -ne $ExpectedSourceFingerprint) {
        throw "Remote source fingerprint '$($sourceState.source.fingerprint)' does not match local '$ExpectedSourceFingerprint'."
    }

    & (Join-Path $repoRoot 'build\scripts\Confirm-LinuxNodeBuildAttestation.ps1') `
        -AttestationPath $LinuxNodeAttestationPath `
        -ArtifactPath (Join-Path $repoRoot 'tools\wta\remote\linux-x64\wta-node') `
        -ExpectedSourceFingerprint $ExpectedSourceFingerprint `
        -NormalizeForPackaging

    & (Join-Path $repoRoot 'build\scripts\Test-ReproducibleBuildManifest.ps1')
    if ($LASTEXITCODE -ne 0) {
        throw 'The reproducible-build harness failed on the Azure builder.'
    }
    & (Join-Path $repoRoot 'build\scripts\Test-WebView2RuntimeContract.ps1')
    if ($LASTEXITCODE -ne 0) {
        throw 'The WebView2 runtime contract failed on the Azure builder.'
    }
    & (Join-Path $repoRoot 'build\scripts\Test-InstallerSettingsTransaction.ps1')
    if ($LASTEXITCODE -ne 0) {
        throw 'The installer settings transaction contract failed on the Azure builder.'
    }

    $rustup = Get-Command rustup.exe -ErrorAction Stop
    & $rustup.Source target add x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not install the x86_64-pc-windows-msvc Rust target.'
    }

    $env:CARGO_INCREMENTAL = '0'
    $env:CARGO_TERM_COLOR = 'always'
    & (Join-Path $repoRoot 'build\scripts\New-WtaLocalInstaller.ps1') `
        -Platform $Platform `
        -Configuration $Configuration `
        -BuildTerminal
    if ($LASTEXITCODE -ne 0) {
        throw 'The canonical installer build failed.'
    }

    $evidence = Copy-ReproducibleEvidence `
        -RepoRoot $repoRoot `
        -BuildStartedUtc $sourceState.buildStartedUtc `
        -EvidenceRoot $evidenceRoot
    $manifestPath = Join-Path $evidenceRoot 'build-manifest.json'
    & $manifestTool `
        -Mode Complete `
        -RepoRoot $repoRoot `
        -StatePath $statePath `
        -OutputPath $manifestPath `
        -InstallerPath $evidence.Installer `
        -Payload $evidence.Payload `
        -Platform $Platform `
        -Configuration $Configuration
    & $manifestTool `
        -Mode Verify `
        -RepoRoot $repoRoot `
        -ManifestPath $manifestPath `
        -VerifyCurrentSource

    $result = [ordered]@{
        schemaVersion = 1
        ok = $true
        runId = $runId
        machine = $env:COMPUTERNAME
        sourceCommit = $Commit
        sourceFingerprint = $sourceState.source.fingerprint
        installer = Split-Path $evidence.Installer -Leaf
        installerSha256 = (Get-FileHash -LiteralPath $evidence.Installer -Algorithm SHA256).Hash.ToLowerInvariant()
        completedAtUtc = [datetime]::UtcNow.ToString('o')
    }
    $result | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $resultPath -Encoding utf8NoBOM
}
catch {
    $buildLog = Join-Path $repoRoot '_build_msix_x64.log'
    if (Test-Path -LiteralPath $buildLog -PathType Leaf) {
        Copy-Item -LiteralPath $buildLog -Destination (Join-Path $evidenceRoot 'build.log') -Force
    }
    [ordered]@{
        schemaVersion = 1
        ok = $false
        runId = $runId
        machine = $env:COMPUTERNAME
        sourceCommit = $Commit
        sourceFingerprint = $ExpectedSourceFingerprint
        error = $_.Exception.Message
        failedAtUtc = [datetime]::UtcNow.ToString('o')
    } | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $resultPath -Encoding utf8NoBOM
    throw
}
finally {
    # Outputs live outside the disposable source workspace. Removing the exact
    # run directory prevents stale obj/bin files from influencing a later run.
    if (Test-Path -LiteralPath $workspaceRoot) {
        try {
            Remove-Item -LiteralPath $workspaceRoot -Recurse -Force
        }
        catch {
            # Build modules can keep their own DLL loaded until this PowerShell
            # process exits. The controller performs a second, exact-path
            # cleanup in a fresh process; cleanup must not replace the primary
            # build failure or a successful result.
            Write-Warning "Deferred cleanup of exact build workspace $workspaceRoot."
        }
    }
}
