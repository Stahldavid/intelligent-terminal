[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$subject = Join-Path $PSScriptRoot 'Confirm-LinuxNodeBuildAttestation.ps1'
$testRoot = Join-Path ([IO.Path]::GetTempPath()) "it-linux-node-attestation-$([guid]::NewGuid().ToString('N'))"
$artifact = Join-Path $testRoot 'wta-node-linux-x64'
$attestation = Join-Path $testRoot 'attestation.json'
$fingerprint = 'a' * 64

function Write-Attestation {
    param(
        [string]$Hash,
        [long]$Length,
        [datetime]$Started,
        [datetime]$Produced,
        [string]$SourceFingerprint = $fingerprint
    )

    [ordered]@{
        schemaVersion = 1
        attestationType = 'wta-linux-node-current-run'
        role = 'wta-node-linux-x64'
        sourceFingerprint = $SourceFingerprint
        buildStartedUtc = $Started.ToUniversalTime().ToString('o')
        producedAtUtc = $Produced.ToUniversalTime().ToString('o')
        length = $Length
        sha256 = $Hash
    } | ConvertTo-Json | Set-Content -LiteralPath $attestation -Encoding utf8NoBOM
}

function Assert-Rejected {
    param([scriptblock]$Action, [string]$Reason)

    try {
        & $Action
        throw "Expected rejection: $Reason"
    }
    catch {
        if ($_.Exception.Message -eq "Expected rejection: $Reason") {
            throw
        }
        Write-Host "[linux-node-attestation-test] Expected rejection ($Reason): $($_.Exception.Message)"
    }
}

try {
    [IO.Directory]::CreateDirectory($testRoot) | Out-Null
    [IO.File]::WriteAllText($artifact, 'current-run-linux-node')
    $artifactItem = Get-Item -LiteralPath $artifact
    $hash = (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash.ToLowerInvariant()
    $started = [datetime]::UtcNow.AddSeconds(-2)
    $produced = [datetime]::UtcNow.AddSeconds(-1)

    Write-Attestation -Hash $hash -Length $artifactItem.Length -Started $started -Produced $produced
    $artifactItem.LastWriteTimeUtc = [datetime]::UtcNow.AddHours(3)
    & $subject `
        -AttestationPath $attestation `
        -ArtifactPath $artifact `
        -ExpectedSourceFingerprint $fingerprint `
        -NormalizeForPackaging | Out-Null
    if ((Get-Item -LiteralPath $artifact).LastWriteTimeUtc -gt [datetime]::UtcNow.AddMinutes(1)) {
        throw 'Verified artifact kept the timezone-corrupted future timestamp.'
    }

    Write-Attestation -Hash ('0' * 64) -Length $artifactItem.Length -Started $started -Produced $produced
    Assert-Rejected -Reason 'hash mismatch' -Action {
        & $subject -AttestationPath $attestation -ArtifactPath $artifact -ExpectedSourceFingerprint $fingerprint
    }

    Write-Attestation -Hash $hash -Length $artifactItem.Length -Started $produced -Produced $started
    Assert-Rejected -Reason 'predates build' -Action {
        & $subject -AttestationPath $attestation -ArtifactPath $artifact -ExpectedSourceFingerprint $fingerprint
    }

    Write-Attestation `
        -Hash $hash `
        -Length $artifactItem.Length `
        -Started $started `
        -Produced $produced `
        -SourceFingerprint ('b' * 64)
    Assert-Rejected -Reason 'wrong source' -Action {
        & $subject -AttestationPath $attestation -ArtifactPath $artifact -ExpectedSourceFingerprint $fingerprint
    }

    Write-Host '[linux-node-attestation-test] PASS: current-run provenance normalizes ZIP time and rejects hash, time, and source mismatches.'
}
finally {
    if (Test-Path -LiteralPath $testRoot) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
