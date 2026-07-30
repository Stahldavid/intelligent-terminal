[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$AttestationPath,

    [Parameter(Mandatory)]
    [string]$ArtifactPath,

    [Parameter(Mandatory)]
    [string]$ExpectedSourceFingerprint,

    [switch]$NormalizeForPackaging
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Resolve-RequiredFile {
    param([Parameter(Mandatory)][string]$Path)

    $item = Get-Item -LiteralPath $Path -ErrorAction Stop
    if ($item.PSIsContainer -or $item.Length -le 0) {
        throw "Expected a non-empty file: $Path"
    }
    return $item
}

function ConvertFrom-RoundtripUtc {
    param(
        [Parameter(Mandatory)][string]$Value,
        [Parameter(Mandatory)][string]$Field
    )

    try {
        $parsed = [datetime]::Parse(
            $Value,
            [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::RoundtripKind)
    }
    catch {
        throw "Linux node attestation field '$Field' is not a round-trip timestamp."
    }
    return $parsed.ToUniversalTime()
}

$attestationFile = Resolve-RequiredFile -Path $AttestationPath
$artifact = Resolve-RequiredFile -Path $ArtifactPath
$attestation = Get-Content -LiteralPath $attestationFile.FullName -Raw | ConvertFrom-Json

if ($attestation.schemaVersion -ne 1 -or
    [string]$attestation.attestationType -ne 'wta-linux-node-current-run') {
    throw 'Unsupported Linux node build attestation.'
}
if ([string]$attestation.role -ne 'wta-node-linux-x64') {
    throw "Unexpected Linux node attestation role '$($attestation.role)'."
}
if ([string]$attestation.sourceFingerprint -ne $ExpectedSourceFingerprint) {
    throw 'Linux node attestation does not belong to the source being packaged.'
}

$startedAtUtc = ConvertFrom-RoundtripUtc `
    -Value ([string]$attestation.buildStartedUtc) `
    -Field 'buildStartedUtc'
$producedAtUtc = ConvertFrom-RoundtripUtc `
    -Value ([string]$attestation.producedAtUtc) `
    -Field 'producedAtUtc'
if ($producedAtUtc -lt $startedAtUtc) {
    throw 'Linux node attestation predates this build.'
}
if ($producedAtUtc -gt [datetime]::UtcNow.AddMinutes(5)) {
    throw 'Linux node attestation is implausibly far in the future.'
}

$actualHash = (Get-FileHash -LiteralPath $artifact.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
$expectedHash = [string]$attestation.sha256
if ($expectedHash -notmatch '^[0-9a-f]{64}$' -or $actualHash -ne $expectedHash) {
    throw 'Linux node artifact hash does not match its current-run attestation.'
}
if ([long]$attestation.length -ne $artifact.Length) {
    throw 'Linux node artifact length does not match its current-run attestation.'
}

if ($NormalizeForPackaging) {
    # ZIP has no timezone metadata. Expand-Archive can reinterpret an NTFS
    # timestamp on a builder in another timezone, so only an artifact whose
    # content and current-run provenance were verified above receives a fresh
    # packaging timestamp.
    $artifact.LastWriteTimeUtc = [datetime]::UtcNow
}

[pscustomobject]@{
    Artifact = $artifact.FullName
    Sha256 = $actualHash
    SourceFingerprint = [string]$attestation.sourceFingerprint
    ProducedAtUtc = $producedAtUtc.ToString('o')
    NormalizedForPackaging = [bool]$NormalizeForPackaging
}
