[CmdletBinding()]
param(
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\IntelligentTerminal",
    [string]$StartMenuDir = "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Intelligent Terminal",
    [switch]$RemoveDevMsix,
    [switch]$Quiet
)

$ErrorActionPreference = 'Stop'

$comRegistrationScript = Join-Path $PSScriptRoot 'ComProxyRegistration.ps1'
if (-not (Test-Path -LiteralPath $comRegistrationScript -PathType Leaf)) {
    throw "COM registration helper not found: $comRegistrationScript"
}
. $comRegistrationScript

function Write-Status {
    param([string]$Message)

    if (-not $Quiet) {
        Write-Host $Message
    }
}

function Remove-InstallDirFromUserPath {
    param([string]$PathToRemove)

    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ([string]::IsNullOrWhiteSpace($current)) {
        return
    }

    $updated = @(
        $current.Split(';') | Where-Object { $_ -and $_ -ne $PathToRemove }
    ) -join ';'

    [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
}

Write-Status "Stopping Intelligent Terminal processes from $InstallDir ..."
Get-Process |
    Where-Object { $_.Path -like "$InstallDir*" } |
    Stop-Process -Force -ErrorAction SilentlyContinue

$comProxy = Join-Path $InstallDir 'OpenConsoleProxy.dll'
if (Test-Path -LiteralPath $comProxy -PathType Leaf) {
    Write-Status 'Removing the per-user terminal protocol proxy registration ...'
    Unregister-PerUserComProxy -ProxyPath $comProxy
}

if ($RemoveDevMsix) {
    $devPackages = @(
        Get-AppxPackage -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -in @('WindowsTerminalDev', 'IntelligentTerminal') }
    )
    if ($devPackages.Count -gt 0) {
        Write-Status 'Removing Intelligent Terminal development MSIX package(s) ...'
        $devPackages | Remove-AppxPackage
    }
}

if (Test-Path $InstallDir) {
    Write-Status "Removing $InstallDir ..."
    Remove-Item $InstallDir -Recurse -Force
}

if (Test-Path $StartMenuDir) {
    Write-Status "Removing $StartMenuDir ..."
    Remove-Item $StartMenuDir -Recurse -Force
}

Write-Status 'Removing install directory from user PATH ...'
Remove-InstallDirFromUserPath -PathToRemove $InstallDir

Write-Status 'Uninstall complete.'
