[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$PayloadZip,

    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\IntelligentTerminal",

    [switch]$NoPathUpdate,

    [switch]$NoShortcuts,

    [string]$StartMenuDir = "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Intelligent Terminal",

    [switch]$Quiet
)

$ErrorActionPreference = 'Stop'

$comRegistrationScript = Join-Path $PSScriptRoot 'ComProxyRegistration.ps1'
if (-not (Test-Path -LiteralPath $comRegistrationScript -PathType Leaf)) {
    throw "COM registration helper not found: $comRegistrationScript"
}
. $comRegistrationScript

$PromptConfigDir = Join-Path $env:LOCALAPPDATA 'IntelligentTerminal\prompts'
$PromptUserPath = Join-Path $PromptConfigDir 'terminal-agent.md'
$PromptDefaultPath = Join-Path $PromptConfigDir 'terminal-agent.default.md'
$InstallMetadataFileName = 'intelligent-terminal-install-metadata.json'

# Legacy paths from the prior "Agentic Terminal" name — cleaned up on install.
$LegacyInstallDir = Join-Path $env:LOCALAPPDATA 'Programs\AgenticTerminal'
$LegacyStartMenuDir = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Agentic Terminal'
$LegacyPromptConfigDir = Join-Path $env:LOCALAPPDATA 'AgenticTerminal\prompts'

function Write-Status {
    param([string]$Message)

    if (-not $Quiet) {
        Write-Host $Message
    }
}

function Ensure-Directory {
    param([string]$Path)

    if (-not (Test-Path $Path -PathType Container)) {
        New-Item -ItemType Directory -Path $Path | Out-Null
    }
}

function Get-Sha256Hash {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )

    # Use the BCL directly. Get-FileHash is module-backed in Windows
    # PowerShell and is not guaranteed to autoload in a stripped installer
    # process.
    $stream = [System.IO.File]::OpenRead($Path)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        return ([BitConverter]::ToString($sha.ComputeHash($stream)) -replace '-', '').ToLowerInvariant()
    }
    finally {
        $sha.Dispose()
        $stream.Dispose()
    }
}

function Remove-DirectoryContents {
    param(
        [string]$Path,
        [string[]]$ExcludeNames = @()
    )

    if (-not (Test-Path $Path -PathType Container)) {
        return
    }

    # A just-terminated WinUI/WebView process can retain image mappings for a
    # short time after its process object disappears. Treat that as a bounded
    # transient rather than leaving an upgrade half-installed.
    $deadline = (Get-Date).AddSeconds(10)
    do {
        try {
            Get-ChildItem $Path -Force |
                Where-Object { $_.Name -notin $ExcludeNames } |
                Remove-Item -Recurse -Force
            return
        }
        catch [System.IO.IOException], [System.UnauthorizedAccessException] {
            if ((Get-Date) -ge $deadline) {
                throw
            }
            Start-Sleep -Milliseconds 250
        }
    }
    while ($true)
}

function Add-InstallDirToUserPath {
    param([string]$PathToAdd)

    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = @()
    if (-not [string]::IsNullOrWhiteSpace($current)) {
        $parts = $current.Split(';') | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    }

    if ($parts -contains $PathToAdd) {
        return
    }

    $updated = @($parts + $PathToAdd) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
}

function Remove-PathFromUserPath {
    param([string]$PathToRemove)

    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ([string]::IsNullOrWhiteSpace($current)) {
        return
    }

    $remaining = @(
        $current.Split(';') |
            Where-Object { $_ -and ($_ -ne $PathToRemove) }
    )
    $updated = $remaining -join ';'
    if ($updated -ne $current) {
        [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
    }
}

function Migrate-LegacyPrompts {
    $legacyUserPrompt = Join-Path $LegacyPromptConfigDir 'terminal-agent.md'
    if (-not (Test-Path $legacyUserPrompt -PathType Leaf)) {
        return
    }

    if (Test-Path $PromptUserPath -PathType Leaf) {
        return
    }

    Ensure-Directory $PromptConfigDir
    Copy-Item -Path $legacyUserPrompt -Destination $PromptUserPath -Force
    Write-Status "Migrated customized planner prompt from $LegacyPromptConfigDir."
}

function Remove-LegacyAgenticInstall {
    if (Test-Path $LegacyInstallDir -PathType Container) {
        Write-Status "Removing legacy AgenticTerminal install at $LegacyInstallDir ..."
        try {
            Stop-RunningInstalledProcesses -InstallRoot $LegacyInstallDir
        } catch {
            Write-Status "  Warning: failed to stop legacy processes: $_"
        }
        Remove-Item $LegacyInstallDir -Recurse -Force -ErrorAction SilentlyContinue
    }

    if (Test-Path $LegacyStartMenuDir -PathType Container) {
        Write-Status "Removing legacy Start menu folder $LegacyStartMenuDir ..."
        Remove-Item $LegacyStartMenuDir -Recurse -Force -ErrorAction SilentlyContinue
    }

    Remove-PathFromUserPath -PathToRemove $LegacyInstallDir
}

function Read-InstallMetadata {
    param([string]$RootPath)

    $metadataPath = Join-Path $RootPath $InstallMetadataFileName
    if (-not (Test-Path $metadataPath -PathType Leaf)) {
        return $null
    }

    return Get-Content $metadataPath -Raw | ConvertFrom-Json
}

function Get-MetadataVersionLabel {
    param($Metadata)

    if ($null -eq $Metadata) {
        return $null
    }

    $parts = @()
    if (-not [string]::IsNullOrWhiteSpace($Metadata.productName)) {
        $parts += [string]$Metadata.productName
    }
    if (-not [string]::IsNullOrWhiteSpace($Metadata.version)) {
        $parts += [string]$Metadata.version
    }

    $qualifiers = @()
    if (-not [string]::IsNullOrWhiteSpace($Metadata.platform)) {
        $qualifiers += [string]$Metadata.platform
    }
    if (-not [string]::IsNullOrWhiteSpace($Metadata.configuration)) {
        $qualifiers += [string]$Metadata.configuration
    }

    $label = $parts -join ' '
    if ($qualifiers.Count -gt 0) {
        $label = '{0} ({1})' -f $label, ($qualifiers -join ' ')
    }

    return $label
}

function Get-ExecutablePathWithinInstallDir {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ExecutablePath,

        [Parameter(Mandatory = $true)]
        [string]$InstallRoot
    )

    if ([string]::IsNullOrWhiteSpace($ExecutablePath)) {
        return $null
    }

    $normalizedInstallRoot = [System.IO.Path]::GetFullPath($InstallRoot).TrimEnd('\') + '\'
    $normalizedExecutablePath = [System.IO.Path]::GetFullPath($ExecutablePath)
    if (-not $normalizedExecutablePath.StartsWith($normalizedInstallRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $null
    }

    return $normalizedExecutablePath
}

function Get-RunningInstalledProcesses {
    param([string]$InstallRoot)

    # Query every executable shipped by the product that can outlive the UI.
    # Path validation below is the actual ownership boundary, so identically
    # named processes from Windows Terminal or another checkout are untouched.
    $processNames = @(
        'WindowsTerminal.exe',
        'OpenConsole.exe',
        'wta.exe',
        'wta-node.exe',
        'wtcli.exe',
        'wtai.exe',
        'elevate-shim.exe'
    )
    $filter = ($processNames | ForEach-Object { "Name = '$_'" }) -join ' OR '
    $candidates = Get-CimInstance Win32_Process -Filter $filter -ErrorAction SilentlyContinue
    $running = @()

    foreach ($candidate in $candidates) {
        $matchedExecutablePath = Get-ExecutablePathWithinInstallDir -ExecutablePath $candidate.ExecutablePath -InstallRoot $InstallRoot
        if ($matchedExecutablePath) {
            $running += [pscustomobject]@{
                ProcessId = [int]$candidate.ProcessId
                Name = [string]$candidate.Name
                ExecutablePath = $matchedExecutablePath
            }
        }
    }

    return @($running | Sort-Object Name, ProcessId -Unique)
}

function Stop-RunningInstalledProcesses {
    param([string]$InstallRoot)

    $running = @(Get-RunningInstalledProcesses -InstallRoot $InstallRoot)
    if ($running.Count -eq 0) {
        return
    }

    Write-Status "Stopping running Intelligent Terminal processes ..."
    foreach ($processInfo in $running) {
        Write-Status ("  Stopping {0} (PID {1})" -f $processInfo.Name, $processInfo.ProcessId)
        Stop-Process -Id $processInfo.ProcessId -Force -ErrorAction SilentlyContinue
    }

    $deadline = (Get-Date).AddSeconds(10)
    do {
        Start-Sleep -Milliseconds 200
        $remaining = @(Get-RunningInstalledProcesses -InstallRoot $InstallRoot)
        if ($remaining.Count -eq 0) {
            return
        }
    } while ((Get-Date) -lt $deadline)

    $remainingSummary = ($remaining | ForEach-Object { "{0} (PID {1})" -f $_.Name, $_.ProcessId }) -join ', '
    throw "Timed out waiting for installed Intelligent Terminal processes to exit: $remainingSummary"
}

function New-Shortcut {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ShortcutPath,

        [Parameter(Mandatory = $true)]
        [string]$TargetPath,

        [string]$WorkingDirectory
    )

    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($ShortcutPath)
    $shortcut.TargetPath = $TargetPath
    if ($WorkingDirectory) {
        $shortcut.WorkingDirectory = $WorkingDirectory
    }
    $shortcut.Save()
}

function Seed-PlannerPromptFiles {
    param(
        [Parameter(Mandatory = $true)]
        [string]$InstallRoot
    )

    $bundledPromptPath = Join-Path $InstallRoot 'prompts\terminal-agent.default.md'
    if (-not (Test-Path $bundledPromptPath -PathType Leaf)) {
        Write-Status "Bundled planner prompt template not found; skipping prompt seeding."
        return
    }

    Ensure-Directory $PromptConfigDir
    $existingDefaultContent = $null
    $existingUserContent = $null

    if (Test-Path $PromptDefaultPath -PathType Leaf) {
        $existingDefaultContent = Get-Content $PromptDefaultPath -Raw
    }
    if (Test-Path $PromptUserPath -PathType Leaf) {
        $existingUserContent = Get-Content $PromptUserPath -Raw
    }

    Copy-Item -Path $bundledPromptPath -Destination $PromptDefaultPath -Force

    if (-not (Test-Path $PromptUserPath -PathType Leaf)) {
        Copy-Item -Path $bundledPromptPath -Destination $PromptUserPath -Force
    } elseif ($null -ne $existingDefaultContent -and $existingUserContent -eq $existingDefaultContent) {
        Copy-Item -Path $bundledPromptPath -Destination $PromptUserPath -Force
    }
}

if (-not (Test-Path $PayloadZip -PathType Leaf)) {
    throw "Payload zip not found: $PayloadZip"
}

$payloadRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("intelligent-terminal-install-" + [Guid]::NewGuid().ToString("N"))
$expandedRoot = Join-Path $payloadRoot 'expanded'
$settingsBackup = $null
$settingsRestorePending = $false

try {
    Ensure-Directory $payloadRoot
    Ensure-Directory $expandedRoot

    Write-Status "Extracting installer payload..."
    Expand-Archive -Path $PayloadZip -DestinationPath $expandedRoot -Force

    $sourceRoot = $expandedRoot
    $children = @(Get-ChildItem $expandedRoot)
    if ($children.Count -eq 1 -and $children[0].PSIsContainer) {
        $sourceRoot = $children[0].FullName
    }

    $incomingMetadata = Read-InstallMetadata -RootPath $sourceRoot
    $incomingVersionLabel = Get-MetadataVersionLabel -Metadata $incomingMetadata
    if ($incomingVersionLabel) {
        Write-Status "Preparing to install $incomingVersionLabel"
    }

    $installedMetadata = Read-InstallMetadata -RootPath $InstallDir
    $installedVersionLabel = Get-MetadataVersionLabel -Metadata $installedMetadata
    if ($installedVersionLabel) {
        Write-Status "Existing install detected: $installedVersionLabel"
    }

    Remove-LegacyAgenticInstall

    Ensure-Directory $InstallDir
    Stop-RunningInstalledProcesses -InstallRoot $InstallDir
    Write-Status "Installing to $InstallDir ..."

    # Preserve user settings across upgrades.
    $settingsDir = Join-Path $InstallDir 'settings'
    if (Test-Path $settingsDir -PathType Container) {
        $settingsBackup = Join-Path ([System.IO.Path]::GetTempPath()) "intelligent-terminal-settings-backup-$([System.IO.Path]::GetRandomFileName())"
        Copy-Item -Path $settingsDir -Destination $settingsBackup -Recurse -Force
        $settingsRestorePending = $true
        Write-Status "Backed up settings to $settingsBackup"
    }

    # COM proxy/stub DLLs may remain mapped in long-lived clients after the
    # terminal exits. Never overwrite or delete a mapped proxy in place.
    # Keep the legacy root copy (if any), install the new proxy under its
    # content hash, and atomically move COM registration to that immutable path.
    $proxyFileName = 'OpenConsoleProxy.dll'
    $proxyStoreName = 'proxies'
    Remove-DirectoryContents $InstallDir -ExcludeNames @($proxyFileName, $proxyStoreName)
    Get-ChildItem -Path (Join-Path $sourceRoot '*') -Force |
        Where-Object { $_.Name -ne $proxyFileName } |
        Copy-Item -Destination $InstallDir -Recurse -Force

    $incomingComProxy = Join-Path $sourceRoot $proxyFileName
    if (-not (Test-Path -LiteralPath $incomingComProxy -PathType Leaf)) {
        throw "Incoming terminal payload is missing $proxyFileName."
    }
    $proxyHash = Get-Sha256Hash -Path $incomingComProxy
    $proxyVersionDir = Join-Path (Join-Path $InstallDir $proxyStoreName) $proxyHash
    Ensure-Directory $proxyVersionDir
    $comProxy = Join-Path $proxyVersionDir $proxyFileName
    if (-not (Test-Path -LiteralPath $comProxy -PathType Leaf)) {
        Copy-Item -LiteralPath $incomingComProxy -Destination $comProxy
    }
    if ((Get-Sha256Hash -Path $comProxy) -ne $proxyHash) {
        throw "Versioned COM proxy hash mismatch at $comProxy."
    }

    # Retain a root compatibility copy for tooling that expects the historical
    # package layout, but never replace it while another process may map it.
    $legacyRootProxy = Join-Path $InstallDir $proxyFileName
    if (-not (Test-Path -LiteralPath $legacyRootProxy -PathType Leaf)) {
        Copy-Item -LiteralPath $incomingComProxy -Destination $legacyRootProxy
    }

    # Restore preserved settings.
    if ($settingsBackup -and (Test-Path $settingsBackup -PathType Container)) {
        Ensure-Directory $settingsDir
        Copy-Item -Path (Join-Path $settingsBackup '*') -Destination $settingsDir -Recurse -Force
        $settingsRestorePending = $false
        Remove-Item $settingsBackup -Recurse -Force -ErrorAction SilentlyContinue
        Write-Status "Restored user settings"
    }

    $terminalExe = Join-Path $InstallDir 'WindowsTerminal.exe'
    $wtaExe = Join-Path $InstallDir 'wta.exe'

    Write-Status 'Registering the version-matched terminal protocol proxy for this user ...'
    $proxyRegistrations = @(Register-PerUserComProxy -ProxyPath $comProxy)
    foreach ($proxyRegistration in $proxyRegistrations) {
        Write-Status ("  {0} -> {1}" -f $proxyRegistration.InterfaceId, $proxyRegistration.ProxyPath)
    }

    if (-not $NoShortcuts) {
        Ensure-Directory $StartMenuDir

        if (Test-Path $terminalExe -PathType Leaf) {
            New-Shortcut -ShortcutPath (Join-Path $StartMenuDir 'Intelligent Terminal.lnk') -TargetPath $terminalExe -WorkingDirectory $InstallDir
        }
        if (Test-Path $wtaExe -PathType Leaf) {
            New-Shortcut -ShortcutPath (Join-Path $StartMenuDir 'WTA.lnk') -TargetPath $wtaExe -WorkingDirectory $InstallDir
        }
    }

    if (-not $NoPathUpdate) {
        Write-Status "Adding install directory to user PATH ..."
        Add-InstallDirToUserPath -PathToAdd $InstallDir
    }

    Migrate-LegacyPrompts

    Write-Status "Seeding planner prompt files in $PromptConfigDir ..."
    Seed-PlannerPromptFiles -InstallRoot $InstallDir

    Write-Status "Installation complete."
}
finally {
    # The settings backup is a transaction journal, not best-effort cleanup.
    # If any package copy, COM registration or shortcut operation failed after
    # the backup was taken, restore the user's settings before propagating the
    # failure. Leave the backup intact if restoration itself cannot complete.
    if ($settingsRestorePending -and
        $settingsBackup -and
        (Test-Path $settingsBackup -PathType Container)) {
        Ensure-Directory $settingsDir
        Remove-DirectoryContents $settingsDir
        Copy-Item -Path (Join-Path $settingsBackup '*') -Destination $settingsDir -Recurse -Force
        $settingsRestorePending = $false
        Remove-Item $settingsBackup -Recurse -Force -ErrorAction SilentlyContinue
        Write-Status "Restored user settings after installation failure"
    }

    if (Test-Path $payloadRoot -PathType Container) {
        Remove-Item $payloadRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
