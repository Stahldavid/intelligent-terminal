[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Begin', 'Complete', 'Verify')]
    [string]$Mode,

    [string]$RepoRoot = (Join-Path $PSScriptRoot '..\..'),

    [string]$StatePath,

    [string]$ManifestPath,

    [string]$OutputPath,

    [string]$InstallerPath,

    [string[]]$Payload = @(),

    [string[]]$RequiredPayloadRole = @(
        'windows-terminal',
        'wtcli',
        'wta',
        'wta-node-windows',
        'wta-node-linux-x64',
        'protocol-manifest'
    ),

    [string[]]$AllowOlderPayloadRole = @('protocol-manifest'),

    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release',

    [ValidateSet('x64', 'ARM64', 'x86')]
    [string]$Platform = 'x64',

    [switch]$RequireClean,

    [switch]$VerifyCurrentSource,

    [Nullable[datetime]]$BuildStartedUtc,

    [ValidateRange(0, 300)]
    [int]$ClockSkewSeconds = 2
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:SchemaVersion = 1
$script:Utf8NoBom = New-Object System.Text.UTF8Encoding($false)

function Resolve-AbsolutePath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,

        [Parameter(Mandatory = $true)]
        [string]$BasePath
    )

    if ([System.IO.Path]::IsPathRooted($Path)) {
        return [System.IO.Path]::GetFullPath($Path)
    }

    return [System.IO.Path]::GetFullPath((Join-Path $BasePath $Path))
}

function Write-JsonAtomically {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,

        [Parameter(Mandatory = $true)]
        [object]$Value
    )

    $parent = Split-Path $Path -Parent
    if (-not [string]::IsNullOrWhiteSpace($parent) -and -not (Test-Path $parent -PathType Container)) {
        [System.IO.Directory]::CreateDirectory($parent) | Out-Null
    }

    $temporaryPath = "$Path.$([Guid]::NewGuid().ToString('N')).tmp"
    try {
        $json = $Value | ConvertTo-Json -Depth 20
        [System.IO.File]::WriteAllText($temporaryPath, "$json`n", $script:Utf8NoBom)
        Move-Item -LiteralPath $temporaryPath -Destination $Path -Force
    }
    finally {
        if (Test-Path $temporaryPath -PathType Leaf) {
            Remove-Item -LiteralPath $temporaryPath -Force
        }
    }
}

function Get-Sha256Bytes {
    param([byte[]]$Bytes)

    $algorithm = [System.Security.Cryptography.SHA256]::Create()
    try {
        return ([BitConverter]::ToString($algorithm.ComputeHash($Bytes))).Replace('-', '').ToLowerInvariant()
    }
    finally {
        $algorithm.Dispose()
    }
}

function Get-Sha256Text {
    param([string]$Text)

    return Get-Sha256Bytes -Bytes $script:Utf8NoBom.GetBytes($Text)
}

function Get-FileSha256 {
    param([string]$Path)

    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Invoke-GitBytes {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments,

        [switch]$AllowFailure
    )

    $git = Get-Command git.exe -ErrorAction SilentlyContinue
    if (-not $git) {
        $git = Get-Command git -ErrorAction SilentlyContinue
    }
    if (-not $git) {
        throw 'git is required to create a source identity.'
    }

    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $git.Source
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($argument in @('-C', $script:ResolvedRepoRoot) + $Arguments) {
        $startInfo.ArgumentList.Add($argument) | Out-Null
    }

    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $startInfo
    $output = New-Object System.IO.MemoryStream
    try {
        if (-not $process.Start()) {
            throw 'Failed to start git.'
        }

        $copyTask = $process.StandardOutput.BaseStream.CopyToAsync($output)
        $errorTask = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $copyTask.GetAwaiter().GetResult() | Out-Null
        $errorText = $errorTask.GetAwaiter().GetResult()

        if ($process.ExitCode -ne 0 -and -not $AllowFailure) {
            throw "git $($Arguments -join ' ') failed with exit code $($process.ExitCode): $errorText"
        }

        return [pscustomobject]@{
            ExitCode = $process.ExitCode
            Bytes = $output.ToArray()
            Error = $errorText
        }
    }
    finally {
        $output.Dispose()
        $process.Dispose()
    }
}

function Invoke-GitText {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments,

        [switch]$AllowFailure
    )

    $result = Invoke-GitBytes -Arguments $Arguments -AllowFailure:$AllowFailure
    return [pscustomobject]@{
        ExitCode = $result.ExitCode
        Text = $script:Utf8NoBom.GetString($result.Bytes).Trim()
        Error = $result.Error
    }
}

function Get-SourceProductMetadata {
    $protocolPath = Join-Path $script:ResolvedRepoRoot 'src\cascadia\TerminalProtocol\protocol-version.json'
    $packageManifestPath = Join-Path $script:ResolvedRepoRoot 'src\cascadia\CascadiaPackage\Package-Dev.appxmanifest'

    if (-not (Test-Path $protocolPath -PathType Leaf)) {
        throw "Source protocol manifest not found: $protocolPath"
    }
    if (-not (Test-Path $packageManifestPath -PathType Leaf)) {
        throw "Source package manifest not found: $packageManifestPath"
    }

    $protocol = Get-Content -LiteralPath $protocolPath -Raw | ConvertFrom-Json
    [xml]$packageXml = Get-Content -LiteralPath $packageManifestPath -Raw
    $namespace = New-Object System.Xml.XmlNamespaceManager($packageXml.NameTable)
    $namespace.AddNamespace('appx', 'http://schemas.microsoft.com/appx/manifest/foundation/windows10')
    $identity = $packageXml.SelectSingleNode('/appx:Package/appx:Identity', $namespace)
    if (-not $identity) {
        throw "Package identity was not found in $packageManifestPath."
    }

    return [ordered]@{
        packageVersion = [string]$identity.Version
        packageName = [string]$identity.Name
        protocolVersion = [string]$protocol.protocolVersion
        componentVersion = [string]$protocol.componentVersion
    }
}

function Get-SourceIdentity {
    $head = Invoke-GitText -Arguments @('rev-parse', 'HEAD')
    $branch = Invoke-GitText -Arguments @('branch', '--show-current')
    $status = Invoke-GitBytes -Arguments @('status', '--porcelain=v1', '-z', '--untracked-files=all')
    $trackedDiff = Invoke-GitBytes -Arguments @('diff', '--binary', '--no-ext-diff', 'HEAD', '--', '.')
    $untrackedResult = Invoke-GitBytes -Arguments @('ls-files', '--others', '--exclude-standard', '-z')
    $sourceFilesResult = Invoke-GitBytes -Arguments @(
        'ls-files', '--cached', '--others', '--exclude-standard', '-z'
    )

    $untracked = New-Object System.Collections.Generic.List[object]
    $untrackedText = $script:Utf8NoBom.GetString($untrackedResult.Bytes)
    foreach ($relativePath in $untrackedText.Split([char]0, [System.StringSplitOptions]::RemoveEmptyEntries)) {
        $absolutePath = Join-Path $script:ResolvedRepoRoot $relativePath
        if (-not (Test-Path $absolutePath -PathType Leaf)) {
            throw "Untracked source entry is not a regular file: $relativePath"
        }
        $untracked.Add([ordered]@{
            path = $relativePath.Replace('\', '/')
            sha256 = Get-FileSha256 -Path $absolutePath
            length = (Get-Item -LiteralPath $absolutePath).Length
        })
    }

    $statusHash = Get-Sha256Bytes -Bytes $status.Bytes
    $trackedDiffHash = Get-Sha256Bytes -Bytes $trackedDiff.Bytes
    # Raw `git status` and `git diff` bytes remain useful diagnostics, but they
    # are not a portable source identity: Git-for-Windows configuration and
    # checkout metadata can serialize the same file tree differently on a
    # remote builder. Fingerprint the actual build inputs instead.
    $sourcePaths = $script:Utf8NoBom.GetString($sourceFilesResult.Bytes).
        Split([char]0, [System.StringSplitOptions]::RemoveEmptyEntries)
    [Array]::Sort($sourcePaths, [StringComparer]::Ordinal)
    $sourceFiles = New-Object System.Collections.Generic.List[object]
    foreach ($relativePath in $sourcePaths) {
        $absolutePath = Join-Path $script:ResolvedRepoRoot $relativePath
        if (-not (Test-Path $absolutePath -PathType Leaf)) {
            throw "Source identity entry is not a regular file: $relativePath"
        }
        $sourceFiles.Add([ordered]@{
            path = $relativePath.Replace('\', '/')
            sha256 = Get-FileSha256 -Path $absolutePath
            length = (Get-Item -LiteralPath $absolutePath).Length
        })
    }

    $canonical = New-Object System.Collections.Generic.List[string]
    $canonical.Add("head=$($head.Text)")
    foreach ($entry in $sourceFiles) {
        $canonical.Add("file=$($entry.path)|$($entry.length)|$($entry.sha256)")
    }

    return [ordered]@{
        commit = $head.Text
        branch = $branch.Text
        dirty = ($status.Bytes.Length -gt 0)
        fingerprint = Get-Sha256Text -Text (($canonical -join "`n") + "`n")
        statusSha256 = $statusHash
        trackedDiffSha256 = $trackedDiffHash
        sourceFileCount = $sourceFiles.Count
        sourceFilesSha256 = Get-Sha256Text -Text (($canonical -join "`n") + "`n")
        untracked = $untracked.ToArray()
    }
}

function Assert-GeneratedPathIsOutsideSourceIdentity {
    param([string]$Path)

    $relative = [System.IO.Path]::GetRelativePath($script:ResolvedRepoRoot, $Path)
    if ($relative -eq '..' -or $relative.StartsWith("..$([System.IO.Path]::DirectorySeparatorChar)")) {
        return
    }

    $tracked = Invoke-GitText -Arguments @('ls-files', '--error-unmatch', '--', $relative) -AllowFailure
    if ($tracked.ExitCode -eq 0) {
        throw "Generated state/manifest path is tracked and would change the source identity: $Path"
    }

    $ignored = Invoke-GitText -Arguments @('check-ignore', '--quiet', '--', $relative) -AllowFailure
    if ($ignored.ExitCode -ne 0) {
        throw "Generated state/manifest path is inside the repository but is not ignored: $Path"
    }
}

function Get-ToolVersion {
    param(
        [string]$CommandName,
        [string[]]$Arguments = @('--version')
    )

    $command = Get-Command $CommandName -ErrorAction SilentlyContinue
    if (-not $command) {
        return $null
    }

    try {
        $text = (& $command.Source @Arguments 2>&1 | Select-Object -First 1).ToString().Trim()
        return [ordered]@{
            path = $command.Source
            version = $text
        }
    }
    catch {
        return [ordered]@{
            path = $command.Source
            version = $null
        }
    }
}

function Get-ToolchainIdentity {
    return [ordered]@{
        powershell = [ordered]@{
            edition = $PSVersionTable.PSEdition
            version = $PSVersionTable.PSVersion.ToString()
        }
        os = [System.Runtime.InteropServices.RuntimeInformation]::OSDescription
        architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        dotnet = Get-ToolVersion -CommandName 'dotnet'
        cargo = Get-ToolVersion -CommandName 'cargo'
        rustc = Get-ToolVersion -CommandName 'rustc'
        msbuild = Get-ToolVersion -CommandName 'msbuild' -Arguments @('-version', '-nologo')
    }
}

function ConvertFrom-PayloadSpec {
    param([string[]]$Specifications)

    $result = [ordered]@{}
    foreach ($specification in $Specifications) {
        $separator = $specification.IndexOf('=')
        if ($separator -lt 1 -or $separator -eq ($specification.Length - 1)) {
            throw "Payload must use role=path syntax: $specification"
        }

        $role = $specification.Substring(0, $separator).Trim().ToLowerInvariant()
        $path = $specification.Substring($separator + 1).Trim()
        if ($role -notmatch '^[a-z0-9][a-z0-9.-]*$') {
            throw "Invalid payload role '$role'."
        }
        if ($result.Contains($role)) {
            throw "Duplicate payload role '$role'."
        }

        $result[$role] = Resolve-AbsolutePath -Path $path -BasePath $script:ResolvedRepoRoot
    }
    return $result
}

function Assert-PayloadNames {
    param([System.Collections.IDictionary]$PayloadMap)

    $expectedNames = @{
        'windows-terminal' = 'WindowsTerminal.exe'
        'wtcli' = 'wtcli.exe'
        'wta' = 'wta.exe'
        'wta-node-windows' = 'wta-node.exe'
        'wta-node-linux-x64' = 'wta-node-linux-x64'
        'protocol-manifest' = 'protocol-version.json'
    }
    foreach ($role in $expectedNames.Keys) {
        if ($PayloadMap.Contains($role)) {
            $actualName = Split-Path $PayloadMap[$role] -Leaf
            if ($actualName -ine $expectedNames[$role]) {
                throw "Payload '$role' must be named '$($expectedNames[$role])', got '$actualName'."
            }
        }
    }
}

function Get-ArtifactRecord {
    param(
        [string]$Role,
        [string]$Path,
        [string]$ManifestDirectory,
        [datetime]$BuildStartedUtc,
        [bool]$RequireFresh
    )

    if (-not (Test-Path $Path -PathType Leaf)) {
        throw "Artifact '$Role' not found: $Path"
    }

    $item = Get-Item -LiteralPath $Path
    if ($item.Length -le 0) {
        throw "Artifact '$Role' is empty: $Path"
    }

    $oldestAllowed = $BuildStartedUtc.AddSeconds(-$ClockSkewSeconds)
    if ($RequireFresh -and $item.LastWriteTimeUtc -lt $oldestAllowed) {
        throw "Artifact '$Role' is stale. Last write $($item.LastWriteTimeUtc.ToString('o')) predates build start $($BuildStartedUtc.ToString('o'))."
    }

    return [ordered]@{
        role = $Role
        path = ([System.IO.Path]::GetRelativePath($ManifestDirectory, $item.FullName)).Replace('\', '/')
        fileName = $item.Name
        length = $item.Length
        lastWriteUtc = $item.LastWriteTimeUtc.ToString('o')
        sha256 = Get-FileSha256 -Path $item.FullName
        freshnessRequired = $RequireFresh
    }
}

function Write-ManifestSidecar {
    param([string]$Path)

    $hash = Get-FileSha256 -Path $Path
    $sidecarPath = "$Path.sha256"
    [System.IO.File]::WriteAllText($sidecarPath, "$hash *$(Split-Path $Path -Leaf)`n", $script:Utf8NoBom)
    return $sidecarPath
}

function Invoke-Begin {
    if ([string]::IsNullOrWhiteSpace($StatePath)) {
        throw '-StatePath is required in Begin mode.'
    }

    $resolvedStatePath = Resolve-AbsolutePath -Path $StatePath -BasePath $script:ResolvedRepoRoot
    Assert-GeneratedPathIsOutsideSourceIdentity -Path $resolvedStatePath
    $source = Get-SourceIdentity
    if ($RequireClean -and $source.dirty) {
        throw 'The source tree is dirty. CI/release builds require a clean checkout.'
    }

    $state = [ordered]@{
        schemaVersion = $script:SchemaVersion
        stateType = 'intelligent-terminal-build-start'
        buildStartedUtc = if ($null -ne $BuildStartedUtc) {
            $BuildStartedUtc.ToUniversalTime().ToString('o')
        } else {
            [datetime]::UtcNow.ToString('o')
        }
        requireClean = [bool]$RequireClean
        source = $source
        product = Get-SourceProductMetadata
    }
    Write-JsonAtomically -Path $resolvedStatePath -Value $state
    Write-Host "[repro-build] Captured source $($source.commit) / $($source.fingerprint)"
    Get-Item -LiteralPath $resolvedStatePath
}

function Invoke-Complete {
    if ([string]::IsNullOrWhiteSpace($StatePath)) {
        throw '-StatePath is required in Complete mode.'
    }
    if ([string]::IsNullOrWhiteSpace($OutputPath)) {
        throw '-OutputPath is required in Complete mode.'
    }
    if ([string]::IsNullOrWhiteSpace($InstallerPath)) {
        throw '-InstallerPath is required in Complete mode.'
    }

    $resolvedStatePath = Resolve-AbsolutePath -Path $StatePath -BasePath $script:ResolvedRepoRoot
    $resolvedOutputPath = Resolve-AbsolutePath -Path $OutputPath -BasePath $script:ResolvedRepoRoot
    $resolvedInstallerPath = Resolve-AbsolutePath -Path $InstallerPath -BasePath $script:ResolvedRepoRoot
    Assert-GeneratedPathIsOutsideSourceIdentity -Path $resolvedOutputPath

    $state = Get-Content -LiteralPath $resolvedStatePath -Raw | ConvertFrom-Json
    if ($state.schemaVersion -ne $script:SchemaVersion -or $state.stateType -ne 'intelligent-terminal-build-start') {
        throw "Unsupported or invalid build state: $resolvedStatePath"
    }

    $currentSource = Get-SourceIdentity
    if ($currentSource.fingerprint -ne $state.source.fingerprint) {
        throw "Source changed during the build. Started with $($state.source.fingerprint), now $($currentSource.fingerprint)."
    }
    if ($state.requireClean -and $currentSource.dirty) {
        throw 'The build began as a clean-only build but the source is now dirty.'
    }

    $payloadMap = ConvertFrom-PayloadSpec -Specifications $Payload
    foreach ($requiredRole in $RequiredPayloadRole) {
        if (-not $payloadMap.Contains($requiredRole.ToLowerInvariant())) {
            throw "Required payload role is missing: $requiredRole"
        }
    }
    Assert-PayloadNames -PayloadMap $payloadMap

    $installerExtension = [System.IO.Path]::GetExtension($resolvedInstallerPath).ToLowerInvariant()
    if ($installerExtension -notin @('.exe', '.msix', '.zip', '.msixbundle')) {
        throw "Unsupported installer extension '$installerExtension'."
    }
    if ((Split-Path $resolvedInstallerPath -Leaf) -notlike "*$($state.product.packageVersion)*") {
        throw "Installer name does not contain source package version $($state.product.packageVersion): $resolvedInstallerPath"
    }

    $protocolPayloadPath = $payloadMap['protocol-manifest']
    if ($protocolPayloadPath) {
        $payloadProtocol = Get-Content -LiteralPath $protocolPayloadPath -Raw | ConvertFrom-Json
        foreach ($field in @('protocolVersion', 'componentVersion')) {
            if ([string]$payloadProtocol.$field -ne [string]$state.product.$field) {
                throw "Payload $field '$($payloadProtocol.$field)' does not match source '$($state.product.$field)'."
            }
        }
    }

    $manifestDirectory = Split-Path $resolvedOutputPath -Parent
    if (-not (Test-Path $manifestDirectory -PathType Container)) {
        [System.IO.Directory]::CreateDirectory($manifestDirectory) | Out-Null
    }
    $buildStartedUtc = [datetime]::Parse(
        [string]$state.buildStartedUtc,
        [System.Globalization.CultureInfo]::InvariantCulture,
        [System.Globalization.DateTimeStyles]::RoundtripKind)

    $records = New-Object System.Collections.Generic.List[object]
    $installerRecord = Get-ArtifactRecord `
        -Role 'installer' `
        -Path $resolvedInstallerPath `
        -ManifestDirectory $manifestDirectory `
        -BuildStartedUtc $buildStartedUtc `
        -RequireFresh $true
    $records.Add($installerRecord)

    foreach ($entry in $payloadMap.GetEnumerator() | Sort-Object Key) {
        if ([System.IO.Path]::GetFullPath($entry.Value) -ieq [System.IO.Path]::GetFullPath($resolvedInstallerPath)) {
            throw "Payload '$($entry.Key)' resolves to the installer path."
        }
        $requireFresh = $AllowOlderPayloadRole -notcontains $entry.Key
        $records.Add((Get-ArtifactRecord `
            -Role $entry.Key `
            -Path $entry.Value `
            -ManifestDirectory $manifestDirectory `
            -BuildStartedUtc $buildStartedUtc `
            -RequireFresh $requireFresh))
    }

    $installerTime = [datetime]::Parse($installerRecord.lastWriteUtc)
    foreach ($record in $records | Where-Object { $_.role -ne 'installer' -and $_.freshnessRequired }) {
        $payloadTime = [datetime]::Parse($record.lastWriteUtc)
        if ($installerTime.AddSeconds($ClockSkewSeconds) -lt $payloadTime) {
            throw "Installer predates payload '$($record.role)'; it cannot contain this payload."
        }
    }

    $manifest = [ordered]@{
        schemaVersion = $script:SchemaVersion
        manifestType = 'intelligent-terminal-reproducible-build'
        createdAtUtc = [datetime]::UtcNow.ToString('o')
        build = [ordered]@{
            id = "$($state.source.commit.Substring(0, 12))-$($state.source.fingerprint.Substring(0, 12))-$($Platform.ToLowerInvariant())-$($Configuration.ToLowerInvariant())"
            startedAtUtc = $state.buildStartedUtc
            completedAtUtc = [datetime]::UtcNow.ToString('o')
            configuration = $Configuration
            platform = $Platform
            runner = [ordered]@{
                machine = $env:COMPUTERNAME
                githubRunId = $env:GITHUB_RUN_ID
                githubRunAttempt = $env:GITHUB_RUN_ATTEMPT
                githubWorkflow = $env:GITHUB_WORKFLOW
            }
        }
        source = $state.source
        product = $state.product
        toolchain = Get-ToolchainIdentity
        artifacts = $records.ToArray()
        gates = [ordered]@{
            sourceUnchanged = $true
            installerFresh = $true
            requiredPayloadRoles = @($RequiredPayloadRole | ForEach-Object { $_.ToLowerInvariant() })
            protocolManifestMatched = [bool]$protocolPayloadPath
            allowOlderPayloadRoles = @($AllowOlderPayloadRole)
        }
    }

    Write-JsonAtomically -Path $resolvedOutputPath -Value $manifest
    $sidecar = Write-ManifestSidecar -Path $resolvedOutputPath
    Write-Host "[repro-build] Manifest: $resolvedOutputPath"
    Write-Host "[repro-build] SHA-256: $(Get-FileSha256 -Path $resolvedOutputPath)"
    [pscustomobject]@{
        Manifest = Get-Item -LiteralPath $resolvedOutputPath
        Sidecar = Get-Item -LiteralPath $sidecar
    }
}

function Invoke-Verify {
    if ([string]::IsNullOrWhiteSpace($ManifestPath)) {
        throw '-ManifestPath is required in Verify mode.'
    }

    $resolvedManifestPath = Resolve-AbsolutePath -Path $ManifestPath -BasePath $script:ResolvedRepoRoot
    $manifest = Get-Content -LiteralPath $resolvedManifestPath -Raw | ConvertFrom-Json
    if ($manifest.schemaVersion -ne $script:SchemaVersion -or $manifest.manifestType -ne 'intelligent-terminal-reproducible-build') {
        throw "Unsupported or invalid build manifest: $resolvedManifestPath"
    }

    $sidecarPath = "$resolvedManifestPath.sha256"
    if (-not (Test-Path $sidecarPath -PathType Leaf)) {
        throw "Manifest sidecar is missing: $sidecarPath"
    }
    $expectedManifestHash = ((Get-Content -LiteralPath $sidecarPath -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
    $actualManifestHash = Get-FileSha256 -Path $resolvedManifestPath
    if ($expectedManifestHash -ne $actualManifestHash) {
        throw "Manifest hash mismatch: expected $expectedManifestHash, got $actualManifestHash."
    }

    $manifestDirectory = Split-Path $resolvedManifestPath -Parent
    foreach ($artifact in $manifest.artifacts) {
        $artifactPath = Resolve-AbsolutePath -Path ([string]$artifact.path).Replace('/', '\') -BasePath $manifestDirectory
        if (-not (Test-Path $artifactPath -PathType Leaf)) {
            throw "Manifest artifact '$($artifact.role)' is missing: $artifactPath"
        }
        $item = Get-Item -LiteralPath $artifactPath
        if ($item.Length -ne [long]$artifact.length) {
            throw "Artifact '$($artifact.role)' length mismatch: expected $($artifact.length), got $($item.Length)."
        }
        $actualHash = Get-FileSha256 -Path $artifactPath
        if ($actualHash -ne [string]$artifact.sha256) {
            throw "Artifact '$($artifact.role)' hash mismatch: expected $($artifact.sha256), got $actualHash."
        }
    }

    if ($VerifyCurrentSource) {
        $currentSource = Get-SourceIdentity
        if ($currentSource.fingerprint -ne $manifest.source.fingerprint) {
            throw "Current source fingerprint does not match the build: expected $($manifest.source.fingerprint), got $($currentSource.fingerprint)."
        }
    }

    Write-Host "[repro-build] Verified $($manifest.artifacts.Count) artifacts from $resolvedManifestPath"
    Get-Item -LiteralPath $resolvedManifestPath
}

$script:ResolvedRepoRoot = Resolve-AbsolutePath -Path $RepoRoot -BasePath (Get-Location).Path
if ($Mode -ne 'Verify' -or $VerifyCurrentSource) {
    $gitRoot = (Invoke-GitText -Arguments @('rev-parse', '--show-toplevel')).Text
    if ([System.IO.Path]::GetFullPath($gitRoot) -ine $script:ResolvedRepoRoot.TrimEnd('\')) {
        throw "RepoRoot must be the Git worktree root. Expected '$gitRoot', got '$script:ResolvedRepoRoot'."
    }
}

switch ($Mode) {
    'Begin' { Invoke-Begin }
    'Complete' { Invoke-Complete }
    'Verify' { Invoke-Verify }
}
