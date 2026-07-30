[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$subject = Join-Path $PSScriptRoot 'New-ReproducibleBuildManifest.ps1'
$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) "it-repro-manifest-$([Guid]::NewGuid().ToString('N'))"
$repo = Join-Path $testRoot 'repo'
$evidence = Join-Path $testRoot 'evidence'
$state = Join-Path $evidence 'build-start.json'
$manifest = Join-Path $evidence 'build-manifest.json'

function Invoke-Subject {
    param(
        [hashtable]$Parameters,
        [switch]$ExpectFailure
    )

    try {
        & $subject @Parameters | Out-Null
        if ($ExpectFailure) {
            throw 'Expected the subject to fail, but it succeeded.'
        }
    }
    catch {
        if ($ExpectFailure -and $_.Exception.Message -ne 'Expected the subject to fail, but it succeeded.') {
            Write-Host "[repro-build-test] Expected rejection: $($_.Exception.Message)"
            return
        }
        throw
    }
}

try {
    [System.IO.Directory]::CreateDirectory((Join-Path $repo 'src\cascadia\TerminalProtocol')) | Out-Null
    [System.IO.Directory]::CreateDirectory((Join-Path $repo 'src\cascadia\CascadiaPackage')) | Out-Null
    [System.IO.Directory]::CreateDirectory($evidence) | Out-Null

    [System.IO.File]::WriteAllText(
        (Join-Path $repo 'src\cascadia\TerminalProtocol\protocol-version.json'),
        '{"protocolVersion":"3.1","componentVersion":"0.9.4"}')
    [System.IO.File]::WriteAllText(
        (Join-Path $repo 'src\cascadia\CascadiaPackage\Package-Dev.appxmanifest'),
        '<?xml version="1.0"?><Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"><Identity Name="Test.IntelligentTerminal" Version="0.9.4.12" Publisher="CN=Test" ProcessorArchitecture="x64" /></Package>')
    [System.IO.File]::WriteAllText((Join-Path $repo 'tracked.txt'), 'source')

    & git -C $repo init --quiet
    & git -C $repo config user.name 'Manifest Self Test'
    & git -C $repo config user.email 'manifest-self-test@example.invalid'
    & git -C $repo add .
    & git -C $repo commit --quiet -m 'fixture'
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not create the test Git repository.'
    }

    $requestedBuildStart = [datetime]::UtcNow.AddSeconds(-2)
    Invoke-Subject -Parameters @{
        Mode = 'Begin'
        RepoRoot = $repo
        StatePath = $state
        RequireClean = $true
        BuildStartedUtc = $requestedBuildStart
    }
    $capturedState = Get-Content -LiteralPath $state -Raw | ConvertFrom-Json
    if (([datetime]$capturedState.buildStartedUtc).ToUniversalTime() -ne $requestedBuildStart.ToUniversalTime()) {
        throw 'Begin mode did not preserve the externally captured build start timestamp.'
    }

    $payloadRoot = Join-Path $evidence 'payload'
    [System.IO.Directory]::CreateDirectory($payloadRoot) | Out-Null
    foreach ($name in @(
        'WindowsTerminal.exe',
        'wtcli.exe',
        'wta.exe',
        'wta-node.exe',
        'wta-node-linux-x64'
    )) {
        [System.IO.File]::WriteAllText((Join-Path $payloadRoot $name), "fresh-$name")
    }
    Copy-Item `
        -LiteralPath (Join-Path $repo 'src\cascadia\TerminalProtocol\protocol-version.json') `
        -Destination (Join-Path $payloadRoot 'protocol-version.json')
    $installer = Join-Path $evidence 'intelligent-terminal-0.9.4.12-x64-release-setup.exe'
    Start-Sleep -Milliseconds 100
    [System.IO.File]::WriteAllText($installer, 'fresh-installer')

    $completeParameters = @{
        Mode = 'Complete'
        RepoRoot = $repo
        StatePath = $state
        OutputPath = $manifest
        InstallerPath = $installer
        Payload = @(
            "windows-terminal=$(Join-Path $payloadRoot 'WindowsTerminal.exe')",
        "wtcli=$(Join-Path $payloadRoot 'wtcli.exe')",
        "wta=$(Join-Path $payloadRoot 'wta.exe')",
        "wta-node-windows=$(Join-Path $payloadRoot 'wta-node.exe')",
        "wta-node-linux-x64=$(Join-Path $payloadRoot 'wta-node-linux-x64')",
            "protocol-manifest=$(Join-Path $payloadRoot 'protocol-version.json')"
        )
        Platform = 'x64'
        Configuration = 'Release'
    }
    Invoke-Subject -Parameters $completeParameters
    Invoke-Subject -Parameters @{
        Mode = 'Verify'
        RepoRoot = $repo
        ManifestPath = $manifest
        VerifyCurrentSource = $true
    }
    Invoke-Subject -Parameters @{
        Mode = 'Verify'
        RepoRoot = (Join-Path $testRoot 'no-git-checkout-required')
        ManifestPath = $manifest
    }

    Add-Content -LiteralPath (Join-Path $payloadRoot 'wta.exe') -Value 'tampered'
    Invoke-Subject -Parameters @{
        Mode = 'Verify'
        RepoRoot = $repo
        ManifestPath = $manifest
    } -ExpectFailure
    [System.IO.File]::WriteAllText((Join-Path $payloadRoot 'wta.exe'), 'fresh-wta.exe')

    $badProtocolDirectory = Join-Path $evidence 'bad-protocol'
    [System.IO.Directory]::CreateDirectory($badProtocolDirectory) | Out-Null
    $badProtocol = Join-Path $badProtocolDirectory 'protocol-version.json'
    [System.IO.File]::WriteAllText($badProtocol, '{"protocolVersion":"999","componentVersion":"0.9.4"}')
    $badProtocolParameters = $completeParameters.Clone()
    $badProtocolParameters.Payload = @(
        "windows-terminal=$(Join-Path $payloadRoot 'WindowsTerminal.exe')",
        "wtcli=$(Join-Path $payloadRoot 'wtcli.exe')",
        "wta=$(Join-Path $payloadRoot 'wta.exe')",
        "wta-node-windows=$(Join-Path $payloadRoot 'wta-node.exe')",
        "wta-node-linux-x64=$(Join-Path $payloadRoot 'wta-node-linux-x64')",
        "protocol-manifest=$badProtocol"
    )
    Invoke-Subject -Parameters $badProtocolParameters -ExpectFailure

    $staleDirectory = Join-Path $evidence 'stale'
    [System.IO.Directory]::CreateDirectory($staleDirectory) | Out-Null
    $staleWta = Join-Path $staleDirectory 'wta.exe'
    [System.IO.File]::WriteAllText($staleWta, 'stale')
    (Get-Item -LiteralPath $staleWta).LastWriteTimeUtc = [datetime]::UtcNow.AddDays(-1)
    $staleParameters = $completeParameters.Clone()
    $staleParameters.Payload = @(
        "windows-terminal=$(Join-Path $payloadRoot 'WindowsTerminal.exe')",
        "wtcli=$(Join-Path $payloadRoot 'wtcli.exe')",
        "wta=$staleWta",
        "wta-node-windows=$(Join-Path $payloadRoot 'wta-node.exe')",
        "wta-node-linux-x64=$(Join-Path $payloadRoot 'wta-node-linux-x64')",
        "protocol-manifest=$(Join-Path $payloadRoot 'protocol-version.json')"
    )
    Invoke-Subject -Parameters $staleParameters -ExpectFailure

    Add-Content -LiteralPath (Join-Path $repo 'tracked.txt') -Value 'changed-during-build'
    Invoke-Subject -Parameters $completeParameters -ExpectFailure

    Write-Host '[repro-build-test] PASS: success, tamper, protocol mismatch, stale payload, and source drift gates.'
}
finally {
    if (Test-Path $testRoot -PathType Container) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
