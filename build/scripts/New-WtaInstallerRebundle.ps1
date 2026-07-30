[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$BaseInstaller,

    [Parameter(Mandatory = $true)]
    [string]$OutputPath,

    [string]$InstallerScript,

    [string]$ComRegistrationScript,

    [string]$InstallerCommand,

    [string]$ProvenancePath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$footerMagicText = 'WTA-INSTALLER-V1'
$footerLength = 24L
$bundleOrder = @(
    'install.cmd',
    'install-local-terminal.ps1',
    'ComProxyRegistration.ps1',
    'payload.zip'
)

function Resolve-RequiredFile {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,

        [Parameter(Mandatory = $true)]
        [string]$Description
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "$Description not found: $Path"
    }

    return (Resolve-Path -LiteralPath $Path).Path
}

function Read-ExactBytes {
    param(
        [Parameter(Mandatory = $true)]
        [System.IO.Stream]$Stream,

        [Parameter(Mandatory = $true)]
        [int]$Length
    )

    $bytes = [byte[]]::new($Length)
    $read = 0
    while ($read -lt $Length) {
        $count = $Stream.Read($bytes, $read, $Length - $read)
        if ($count -le 0) {
            throw "Unexpected end of installer stream after $read of $Length bytes."
        }
        $read += $count
    }
    return $bytes
}

function Copy-ExactRange {
    param(
        [Parameter(Mandatory = $true)]
        [System.IO.Stream]$InputStream,

        [Parameter(Mandatory = $true)]
        [System.IO.Stream]$OutputStream,

        [Parameter(Mandatory = $true)]
        [UInt64]$Offset,

        [Parameter(Mandatory = $true)]
        [UInt64]$Length
    )

    $InputStream.Seek([Int64]$Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
    $remaining = $Length
    $buffer = [byte[]]::new(1024 * 1024)
    while ($remaining -gt 0) {
        $requested = [int][Math]::Min([UInt64]$buffer.Length, $remaining)
        $read = $InputStream.Read($buffer, 0, $requested)
        if ($read -le 0) {
            throw "Unexpected end of installer while copying $Length bytes from offset $Offset."
        }
        $OutputStream.Write($buffer, 0, $read)
        $remaining -= [UInt64]$read
    }
}

function Read-InstallerLayout {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )

    $stream = [System.IO.File]::Open(
        $Path,
        [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::Read,
        [System.IO.FileShare]::Read)
    try {
        if ($stream.Length -lt $footerLength) {
            throw 'Installer payload footer is missing.'
        }

        $stream.Seek(-$footerLength, [System.IO.SeekOrigin]::End) | Out-Null
        $magic = [System.Text.Encoding]::ASCII.GetString((Read-ExactBytes -Stream $stream -Length 16))
        if ($magic -ne $footerMagicText) {
            throw "Installer footer magic is invalid: '$magic'."
        }

        $manifestLength = [BitConverter]::ToUInt64(
            (Read-ExactBytes -Stream $stream -Length 8),
            0)
        if ($manifestLength -gt [UInt64]($stream.Length - $footerLength)) {
            throw 'Installer manifest length exceeds the bundle.'
        }

        $manifestOffset = [UInt64]$stream.Length - [UInt64]$footerLength - $manifestLength
        $stream.Seek([Int64]$manifestOffset, [System.IO.SeekOrigin]::Begin) | Out-Null
        $manifestText = [System.Text.Encoding]::UTF8.GetString(
            (Read-ExactBytes -Stream $stream -Length ([int]$manifestLength)))

        $entries = @()
        foreach ($line in ($manifestText -split "`r?`n")) {
            if ([string]::IsNullOrWhiteSpace($line)) {
                continue
            }

            $parts = $line.Split('|')
            if ($parts.Count -ne 4 -or $parts[0] -ne 'file') {
                throw "Invalid installer manifest entry: $line"
            }
            if ($parts[1] -notin $bundleOrder) {
                throw "Unexpected installer bundle entry: $($parts[1])"
            }

            $offset = [UInt64]::Parse($parts[2])
            $length = [UInt64]::Parse($parts[3])
            if ($offset + $length -gt $manifestOffset) {
                throw "Installer entry $($parts[1]) exceeds the embedded payload region."
            }
            $entries += [pscustomobject]@{
                Name = $parts[1]
                Offset = $offset
                Length = $length
            }
        }

        if ($entries.Count -ne $bundleOrder.Count) {
            throw "Expected $($bundleOrder.Count) installer entries, found $($entries.Count)."
        }
        foreach ($requiredName in $bundleOrder) {
            if (@($entries | Where-Object Name -eq $requiredName).Count -ne 1) {
                throw "Installer entry must appear exactly once: $requiredName"
            }
        }

        $bootstrapLength = [UInt64](($entries | Measure-Object Offset -Minimum).Minimum)
        if ($bootstrapLength -eq 0) {
            throw 'Installer bootstrap prefix is empty.'
        }

        return [pscustomobject]@{
            Entries = $entries
            BootstrapLength = $bootstrapLength
            ManifestOffset = $manifestOffset
            ManifestLength = $manifestLength
        }
    }
    finally {
        $stream.Dispose()
    }
}

function Get-RangeHash {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,

        [Parameter(Mandatory = $true)]
        [UInt64]$Offset,

        [Parameter(Mandatory = $true)]
        [UInt64]$Length
    )

    $stream = [System.IO.File]::OpenRead($Path)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $stream.Seek([Int64]$Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
        $remaining = $Length
        $buffer = [byte[]]::new(1024 * 1024)
        while ($remaining -gt 0) {
            $requested = [int][Math]::Min([UInt64]$buffer.Length, $remaining)
            $read = $stream.Read($buffer, 0, $requested)
            if ($read -le 0) {
                throw 'Unexpected end of file while hashing installer range.'
            }
            $sha.TransformBlock($buffer, 0, $read, $null, 0) | Out-Null
            $remaining -= [UInt64]$read
        }
        $sha.TransformFinalBlock([byte[]]::new(0), 0, 0) | Out-Null
        return ([BitConverter]::ToString($sha.Hash) -replace '-', '').ToLowerInvariant()
    }
    finally {
        $sha.Dispose()
        $stream.Dispose()
    }
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$BaseInstaller = Resolve-RequiredFile -Path $BaseInstaller -Description 'Base installer'
if ([string]::IsNullOrWhiteSpace($InstallerScript)) {
    $InstallerScript = Join-Path $repoRoot 'installer\install-local-terminal.ps1'
}
if ([string]::IsNullOrWhiteSpace($ComRegistrationScript)) {
    $ComRegistrationScript = Join-Path $repoRoot 'installer\ComProxyRegistration.ps1'
}
if ([string]::IsNullOrWhiteSpace($InstallerCommand)) {
    $InstallerCommand = Join-Path $repoRoot 'installer\install.cmd'
}

$InstallerScript = Resolve-RequiredFile -Path $InstallerScript -Description 'Installer script'
$ComRegistrationScript = Resolve-RequiredFile -Path $ComRegistrationScript -Description 'COM registration script'
$InstallerCommand = Resolve-RequiredFile -Path $InstallerCommand -Description 'Installer command'
$OutputPath = [System.IO.Path]::GetFullPath($OutputPath)
if ([string]::IsNullOrWhiteSpace($ProvenancePath)) {
    $ProvenancePath = "$OutputPath.provenance.json"
}
$ProvenancePath = [System.IO.Path]::GetFullPath($ProvenancePath)

$outputDirectory = Split-Path -Parent $OutputPath
$provenanceDirectory = Split-Path -Parent $ProvenancePath
New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null
New-Item -ItemType Directory -Path $provenanceDirectory -Force | Out-Null

$baseLayout = Read-InstallerLayout -Path $BaseInstaller
$basePayloadEntry = $baseLayout.Entries | Where-Object Name -eq 'payload.zip'
$currentSources = @{
    'install.cmd' = $InstallerCommand
    'install-local-terminal.ps1' = $InstallerScript
    'ComProxyRegistration.ps1' = $ComRegistrationScript
}

$baseStream = [System.IO.File]::OpenRead($BaseInstaller)
$outputStream = [System.IO.File]::Open(
    $OutputPath,
    [System.IO.FileMode]::Create,
    [System.IO.FileAccess]::ReadWrite,
    [System.IO.FileShare]::Read)
try {
    Copy-ExactRange `
        -InputStream $baseStream `
        -OutputStream $outputStream `
        -Offset 0 `
        -Length $baseLayout.BootstrapLength

    $newEntries = @()
    foreach ($name in $bundleOrder) {
        $offset = [UInt64]$outputStream.Position
        if ($name -eq 'payload.zip') {
            Copy-ExactRange `
                -InputStream $baseStream `
                -OutputStream $outputStream `
                -Offset $basePayloadEntry.Offset `
                -Length $basePayloadEntry.Length
        }
        else {
            $sourceStream = [System.IO.File]::OpenRead($currentSources[$name])
            try {
                $sourceStream.CopyTo($outputStream)
            }
            finally {
                $sourceStream.Dispose()
            }
        }

        $newEntries += [pscustomobject]@{
            Name = $name
            Offset = $offset
            Length = [UInt64]$outputStream.Position - $offset
        }
    }

    $manifestText = (($newEntries | ForEach-Object {
        'file|{0}|{1}|{2}' -f $_.Name, $_.Offset, $_.Length
    }) -join "`n") + "`n"
    $manifestBytes = [System.Text.Encoding]::UTF8.GetBytes($manifestText)
    $outputStream.Write($manifestBytes, 0, $manifestBytes.Length)
    $magicBytes = [System.Text.Encoding]::ASCII.GetBytes($footerMagicText)
    $outputStream.Write($magicBytes, 0, $magicBytes.Length)
    $lengthBytes = [BitConverter]::GetBytes([UInt64]$manifestBytes.Length)
    $outputStream.Write($lengthBytes, 0, $lengthBytes.Length)
    $outputStream.Flush()
}
finally {
    $outputStream.Dispose()
    $baseStream.Dispose()
}

$outputLayout = Read-InstallerLayout -Path $OutputPath
$baseBootstrapHash = Get-RangeHash `
    -Path $BaseInstaller `
    -Offset 0 `
    -Length $baseLayout.BootstrapLength
$outputBootstrapHash = Get-RangeHash `
    -Path $OutputPath `
    -Offset 0 `
    -Length $outputLayout.BootstrapLength
if ($baseBootstrapHash -ne $outputBootstrapHash) {
    throw 'Rebundled installer bootstrap differs from the attested base installer.'
}

$basePayloadHash = Get-RangeHash `
    -Path $BaseInstaller `
    -Offset $basePayloadEntry.Offset `
    -Length $basePayloadEntry.Length
$outputPayloadEntry = $outputLayout.Entries | Where-Object Name -eq 'payload.zip'
$outputPayloadHash = Get-RangeHash `
    -Path $OutputPath `
    -Offset $outputPayloadEntry.Offset `
    -Length $outputPayloadEntry.Length
if ($basePayloadHash -ne $outputPayloadHash) {
    throw 'Rebundled installer payload.zip differs from the attested base installer.'
}

$entryEvidence = @()
foreach ($entry in $outputLayout.Entries) {
    $entryEvidence += [ordered]@{
        name = $entry.Name
        offset = $entry.Offset
        length = $entry.Length
        sha256 = Get-RangeHash -Path $OutputPath -Offset $entry.Offset -Length $entry.Length
    }
}

$provenance = [ordered]@{
    schemaVersion = 1
    kind = 'installer-script-hotfix-rebundle'
    generatedAtUtc = [DateTime]::UtcNow.ToString('o')
    baseInstaller = [ordered]@{
        path = $BaseInstaller
        sha256 = (Get-FileHash -LiteralPath $BaseInstaller -Algorithm SHA256).Hash.ToLowerInvariant()
        bootstrapLength = $baseLayout.BootstrapLength
        bootstrapSha256 = $baseBootstrapHash
        payloadSha256 = $basePayloadHash
    }
    replacements = [ordered]@{
        installCommandSha256 = (Get-FileHash -LiteralPath $InstallerCommand -Algorithm SHA256).Hash.ToLowerInvariant()
        installerScriptSha256 = (Get-FileHash -LiteralPath $InstallerScript -Algorithm SHA256).Hash.ToLowerInvariant()
        comRegistrationScriptSha256 = (Get-FileHash -LiteralPath $ComRegistrationScript -Algorithm SHA256).Hash.ToLowerInvariant()
    }
    outputInstaller = [ordered]@{
        path = $OutputPath
        sha256 = (Get-FileHash -LiteralPath $OutputPath -Algorithm SHA256).Hash.ToLowerInvariant()
        entries = $entryEvidence
    }
}

$provenance | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $ProvenancePath -Encoding utf8
Write-Host "[installer-rebundle] PASS: $OutputPath"
Write-Host "[installer-rebundle] payload SHA-256 preserved: $basePayloadHash"
Write-Host "[installer-rebundle] provenance: $ProvenancePath"
