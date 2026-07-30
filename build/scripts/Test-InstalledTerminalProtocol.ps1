[CmdletBinding()]
param(
    [string]$InstallRoot = "$env:LOCALAPPDATA\Programs\IntelligentTerminal",

    [ValidateRange(1, 120)]
    [int]$TimeoutSeconds = 20,

    [switch]$InnerProbe,

    [string]$OutputPath
)

$ErrorActionPreference = 'Stop'

$terminalPath = Join-Path $InstallRoot 'WindowsTerminal.exe'
$wtcliPath = Join-Path $InstallRoot 'wtcli.exe'

foreach ($requiredPath in $terminalPath, $wtcliPath) {
    if (-not (Test-Path -LiteralPath $requiredPath -PathType Leaf)) {
        throw "Required installed component not found: $requiredPath"
    }
}

if ($InnerProbe) {
    if ([string]::IsNullOrWhiteSpace($OutputPath)) {
        throw '-OutputPath is required with -InnerProbe.'
    }

    $rawOutput = @(& $wtcliPath --json info 2>&1)
    $result = [ordered]@{
        exitCode = $LASTEXITCODE
        output = ($rawOutput -join [Environment]::NewLine)
        hasTerminalScope = -not [string]::IsNullOrWhiteSpace($env:WT_COM_CLSID)
    }
    Set-Content -LiteralPath $OutputPath -Value ($result | ConvertTo-Json -Depth 3) -Encoding utf8
    exit $result.exitCode
}

$ownsOutputPath = [string]::IsNullOrWhiteSpace($OutputPath)
if ($ownsOutputPath) {
    $OutputPath = Join-Path ([System.IO.Path]::GetTempPath()) (
        'intelligent-terminal-protocol-probe-{0}.json' -f [guid]::NewGuid().ToString('N')
    )
}

try {
    Remove-Item -LiteralPath $OutputPath -Force -ErrorAction SilentlyContinue

    $arguments = @(
        'new-tab',
        '--title',
        'Protocol Probe',
        'pwsh.exe',
        '-NoProfile',
        '-ExecutionPolicy',
        'Bypass',
        '-File',
        $PSCommandPath,
        '-InstallRoot',
        $InstallRoot,
        '-InnerProbe',
        '-OutputPath',
        $OutputPath
    )
    & $terminalPath @arguments
    if ($null -ne $LASTEXITCODE -and $LASTEXITCODE -ne 0) {
        throw "Failed to create the in-terminal protocol probe surface (exit $LASTEXITCODE)."
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while (-not (Test-Path -LiteralPath $OutputPath -PathType Leaf) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 250
    }
    if (-not (Test-Path -LiteralPath $OutputPath -PathType Leaf)) {
        throw "Timed out after $TimeoutSeconds seconds waiting for the in-terminal protocol probe."
    }

    $envelope = Get-Content -LiteralPath $OutputPath -Raw | ConvertFrom-Json
    if (-not $envelope.hasTerminalScope) {
        throw 'The probe surface did not receive WT_COM_CLSID.'
    }
    if ($envelope.exitCode -ne 0) {
        throw "wtcli authentication failed inside the installed terminal: $($envelope.output)"
    }

    $protocol = $envelope.output | ConvertFrom-Json
    if (-not $protocol.connected) {
        throw 'wtcli did not report a connected protocol server.'
    }
    if ($protocol.protocol_version -ne '3.1') {
        throw "Expected protocol 3.1, received '$($protocol.protocol_version)'."
    }

    [pscustomobject]@{
        connected = $protocol.connected
        protocolVersion = $protocol.protocol_version
        installRoot = $InstallRoot
    }
}
finally {
    if ($ownsOutputPath) {
        Remove-Item -LiteralPath $OutputPath -Force -ErrorAction SilentlyContinue
    }
}
