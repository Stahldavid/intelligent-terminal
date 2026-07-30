[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$installerPath = Join-Path $repoRoot 'installer\install-local-terminal.ps1'
$source = Get-Content -LiteralPath $installerPath -Raw

foreach ($invariant in @(
    '\$settingsRestorePending = \$false',
    '\$settingsRestorePending = \$true',
    'if \(\$settingsRestorePending -and',
    'Restored user settings after installation failure',
    '\[System\.IO\.IOException\], \[System\.UnauthorizedAccessException\]',
    '\(Get-Date\)\.AddSeconds\(10\)',
    "'OpenConsole\.exe'",
    "'wta-node\.exe'",
    "'wtcli\.exe'",
    'Get-ExecutablePathWithinInstallDir',
    'function Get-Sha256Hash',
    '\[System\.Security\.Cryptography\.SHA256\]::Create\(\)',
    '\$proxyStoreName = ''proxies''',
    'Remove-DirectoryContents \$InstallDir -ExcludeNames',
    '\$proxyVersionDir',
    'Versioned COM proxy hash mismatch',
    'Register-PerUserComProxy -ProxyPath \$comProxy'
)) {
    if ($source -notmatch $invariant) {
        throw "Installer settings transaction is missing invariant: $invariant"
    }
}

$backupIndex = $source.IndexOf('$settingsRestorePending = $true', [StringComparison]::Ordinal)
$removeIndex = $source.IndexOf('Remove-DirectoryContents $InstallDir', [StringComparison]::Ordinal)
$finallyIndex = $source.LastIndexOf('if ($settingsRestorePending -and', [StringComparison]::Ordinal)
if ($backupIndex -lt 0 -or $removeIndex -le $backupIndex -or $finallyIndex -le $removeIndex) {
    throw 'Settings backup, destructive install replacement and finally restoration are not ordered transactionally.'
}

Write-Host '[installer-settings-transaction] bounded cleanup retry and failure restoration verified.'
