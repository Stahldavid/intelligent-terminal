# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$scriptPath = Join-Path $PSScriptRoot 'New-WtaLocalInstaller.ps1'
$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    $scriptPath,
    [ref]$tokens,
    [ref]$parseErrors)

if ($parseErrors.Count -ne 0) {
    throw "New-WtaLocalInstaller.ps1 has $($parseErrors.Count) PowerShell parse error(s)."
}

$wtaAssignments = @($ast.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
            $node.Left -is [System.Management.Automation.Language.VariableExpressionAst] -and
            $node.Left.VariablePath.UserPath -eq 'resolvedWtaExePath'
        }, $true))
$terminalBuildCalls = @($ast.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.CommandAst] -and
            $node.GetCommandName() -eq 'Build-TerminalPackage'
        }, $true))

if ($wtaAssignments.Count -eq 0) {
    throw 'The installer no longer resolves wta.exe before packaging.'
}
if ($terminalBuildCalls.Count -ne 1) {
    throw "Expected exactly one Build-TerminalPackage invocation; found $($terminalBuildCalls.Count)."
}

$lastWtaResolutionLine = ($wtaAssignments | ForEach-Object { $_.Extent.EndLineNumber } | Measure-Object -Maximum).Maximum
$terminalBuildLine = $terminalBuildCalls[0].Extent.StartLineNumber
if ($lastWtaResolutionLine -ge $terminalBuildLine) {
    throw "WTA is resolved at or after line $lastWtaResolutionLine, but Terminal packaging starts at line $terminalBuildLine. CascadiaPackage requires the exact WTA binaries first."
}

$source = Get-Content -LiteralPath $scriptPath -Raw
foreach ($requiredFragment in @(
        '@(Get-InstalledRustTargets)',
        'tools\wta\target\{0}\release',
        'wta-node.exe not found beside wta.exe',
        'A source-built Terminal package requires the exact WTA pair')) {
    if (-not $source.Contains($requiredFragment, [System.StringComparison]::Ordinal)) {
        throw "Missing installer build-order invariant: $requiredFragment"
    }
}

foreach ($driverName in @('_build_msix_x64.cmd', '_build_msix_arm64.cmd')) {
    $driverPath = Join-Path (Split-Path -Parent $PSScriptRoot) "..\$driverName"
    $driverSource = Get-Content -LiteralPath $driverPath -Raw
    if ($driverSource -notmatch '(?ms)if errorlevel 1 \(\s*echo CascadiaPackage build failed\.\s*exit /b 1\s*\)' -or
        $driverSource -notmatch '(?ms)echo Exit code: 0\s*exit /b 0') {
        throw "$driverName must return an explicit zero after a successful CascadiaPackage build."
    }
    if ($driverSource.Contains('set BUILD_EXIT=%ERRORLEVEL%', [System.StringComparison]::Ordinal)) {
        throw "$driverName must not propagate a stale ERRORLEVEL environment variable."
    }
}

$azureControllerPath = Join-Path $PSScriptRoot 'Invoke-AzureWindowsBuild.ps1'
$azureControllerSource = Get-Content -LiteralPath $azureControllerPath -Raw
$captureIndex = $azureControllerSource.IndexOf('$buildStartedUtc = [datetime]::UtcNow', [System.StringComparison]::Ordinal)
$linuxBuildIndex = $azureControllerSource.IndexOf("'Build-WtaNodeLinux.ps1'", [System.StringComparison]::Ordinal)
$manifestIndex = $azureControllerSource.IndexOf("'New-ReproducibleBuildManifest.ps1'", [System.StringComparison]::Ordinal)
if ($captureIndex -lt 0 -or $linuxBuildIndex -lt 0 -or $manifestIndex -lt 0) {
    throw 'Azure controller is missing the build-start, Linux helper, or manifest stage.'
}
if ($captureIndex -ge $linuxBuildIndex -or $linuxBuildIndex -ge $manifestIndex) {
    throw 'Azure controller must capture build start, then build the Linux helper, then capture source identity.'
}
if (-not $azureControllerSource.Contains('-BuildStartedUtc $buildStartedUtc', [System.StringComparison]::Ordinal)) {
    throw 'Azure controller must pass the pre-artifact build timestamp into the reproducible manifest.'
}
foreach ($requiredFragment in @(
        "throw '-SkipLinuxNodeBuild is disabled for distributable Azure builds",
        "attestationType = 'wta-linux-node-current-run'",
        '-LinuxNodeAttestationPath')) {
    if (-not $azureControllerSource.Contains($requiredFragment, [System.StringComparison]::Ordinal)) {
        throw "Azure controller is missing the Linux helper provenance invariant: $requiredFragment"
    }
}

$remoteBuilderPath = Join-Path $PSScriptRoot 'Invoke-RemoteWindowsBuild.ps1'
$remoteBuilderSource = Get-Content -LiteralPath $remoteBuilderPath -Raw
foreach ($requiredFragment in @(
        '[Parameter(Mandatory)]',
        '[string]$LinuxNodeAttestationPath',
        "'build\scripts\Confirm-LinuxNodeBuildAttestation.ps1'",
        '-NormalizeForPackaging')) {
    if (-not $remoteBuilderSource.Contains($requiredFragment, [System.StringComparison]::Ordinal)) {
        throw "Remote builder is missing the Linux helper attestation invariant: $requiredFragment"
    }
}

Write-Host '[wta-installer-order] PASS: exact WTA pair is produced and validated before Terminal packaging.'
