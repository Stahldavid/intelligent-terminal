[CmdletBinding()]
param(
    [string]$ResourceGroup = 'RG-INTELLIGENT-TERMINAL-BUILD-ISRAELCENTRAL',
    [string]$VmName = 'vm-intelligent-terminal-build-01',
    [string]$SshUser = 'itbuildadmin',
    [string]$SshKeyPath = (Join-Path $env:USERPROFILE '.ssh\intelligent-terminal-build-azure_ed25519'),
    [string]$KnownHostsPath = (Join-Path $env:USERPROFILE '.ssh\known_hosts'),
    [string]$RepoRoot,
    [string]$Destination,
    [switch]$SkipLinuxNodeBuild,
    [switch]$KeepRemoteInputs
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
if ([string]::IsNullOrWhiteSpace($scriptRoot)) {
    throw 'Could not resolve the Azure build controller script directory.'
}
if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
    $RepoRoot = Join-Path $scriptRoot '..\..'
}
if ([string]::IsNullOrWhiteSpace($Destination)) {
    $Destination = Join-Path $scriptRoot '..\..\artifacts\azure-build'
}

function Invoke-Checked {
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [Parameter(Mandatory)][string[]]$Arguments,
        [string]$FailureMessage = "$FilePath failed."
    )

    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$FailureMessage Exit code: $LASTEXITCODE"
    }
}

function New-SourceOverlay {
    param(
        [Parameter(Mandatory)][string]$Root,
        [Parameter(Mandatory)][string]$StagingRoot
    )

    $patch = Join-Path $StagingRoot 'tracked.patch'
    Invoke-Checked git.exe @(
        '-C', $Root, 'diff', '--binary', '--full-index', '--no-ext-diff',
        "--output=$patch", 'HEAD', '--', '.'
    ) 'Could not capture the tracked dirty-worktree patch.'

    $untrackedRoot = Join-Path $StagingRoot 'untracked'
    [IO.Directory]::CreateDirectory($untrackedRoot) | Out-Null
    $raw = & git.exe -C $Root ls-files --others --exclude-standard -z
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not enumerate untracked source files.'
    }
    $entries = (($raw -join "`n") -split [char]0) | Where-Object { $_ }
    foreach ($relative in $entries) {
        $source = [IO.Path]::GetFullPath((Join-Path $Root $relative))
        $relativeNormalized = $relative.Replace('/', [IO.Path]::DirectorySeparatorChar)
        $destination = [IO.Path]::GetFullPath((Join-Path $untrackedRoot $relativeNormalized))
        if (-not $source.StartsWith($Root.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase) -or
            -not $destination.StartsWith($untrackedRoot.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
            throw "Untracked path escapes its source root: $relative"
        }
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "Untracked source is not a regular file: $relative"
        }
        $parent = Split-Path $destination -Parent
        [IO.Directory]::CreateDirectory($parent) | Out-Null
        Copy-Item -LiteralPath $source -Destination $destination
    }
    if ($entries.Count -eq 0) {
        [IO.File]::WriteAllText((Join-Path $untrackedRoot '.wta-empty-overlay'), '')
    }
    $archive = Join-Path $StagingRoot 'untracked.zip'
    Compress-Archive -Path (Join-Path $untrackedRoot '*') -DestinationPath $archive -CompressionLevel Optimal
    return [pscustomobject]@{
        Patch = $patch
        UntrackedArchive = $archive
        UntrackedCount = $entries.Count
    }
}

function Invoke-RemotePowerShell {
    param(
        [Parameter(Mandatory)][string]$HostName,
        [Parameter(Mandatory)][string]$Script
    )

    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($Script))
    Invoke-Checked ssh.exe @(
        '-i', $script:ResolvedKey,
        '-o', 'BatchMode=yes',
        '-o', 'ConnectTimeout=20',
        '-o', 'StrictHostKeyChecking=yes',
        '-o', "UserKnownHostsFile=$script:ResolvedKnownHosts",
        "$SshUser@$HostName", '--',
        'powershell.exe', '-NoLogo', '-NoProfile', '-NonInteractive',
        '-EncodedCommand', $encoded
    ) 'Remote PowerShell command failed.'
}

$repo = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $RepoRoot).Path)
$script:ResolvedKey = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $SshKeyPath).Path)
$script:ResolvedKnownHosts = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $KnownHostsPath).Path)
$destinationRoot = [IO.Path]::GetFullPath($Destination)
$remoteBuilder = [IO.Path]::GetFullPath((Join-Path $scriptRoot 'Invoke-RemoteWindowsBuild.ps1'))
$runId = "run-$([datetime]::UtcNow.ToString('yyyyMMdd-HHmmss'))-$([guid]::NewGuid().ToString('N').Substring(0, 8))"
$localStaging = Join-Path ([IO.Path]::GetTempPath()) "intelligent-terminal-azure-$runId"
$localOutput = Join-Path $destinationRoot $runId
$remoteInput = "C:\IntelligentTerminalBuild\input\$runId"
$remoteOutput = "C:\IntelligentTerminalBuild\output\$runId"
$remoteWork = "C:\IntelligentTerminalBuild\work\$runId"
$started = $false
$remoteHost = $null
$primaryError = $null
$linuxNodeAttestation = $null

try {
    $vm = az vm show -g $ResourceGroup -n $VmName -o json | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or $null -eq $vm) {
        throw "Azure build VM '$ResourceGroup/$VmName' was not found."
    }
    if ([string]$vm.tags.production -ne 'false' -or [string]$vm.tags.autoDeallocate -ne 'true') {
        throw 'The Azure builder must be tagged production=false and autoDeallocate=true.'
    }
    if ($vm.storageProfile.osDisk.osType -ne 'Windows') {
        throw 'The Azure builder is not a Windows VM.'
    }

    [IO.Directory]::CreateDirectory($localStaging) | Out-Null
    [IO.Directory]::CreateDirectory($localOutput) | Out-Null
    $buildStartedUtc = [datetime]::UtcNow

    if ($SkipLinuxNodeBuild) {
        throw '-SkipLinuxNodeBuild is disabled for distributable Azure builds because every installer requires current-run Linux helper provenance.'
    }
    & (Join-Path $scriptRoot 'Build-WtaNodeLinux.ps1') -Configuration Release
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not build the Linux helper.'
    }

    $statePath = Join-Path $localStaging 'build-start.json'
    & (Join-Path $scriptRoot 'New-ReproducibleBuildManifest.ps1') `
        -Mode Begin `
        -RepoRoot $repo `
        -StatePath $statePath `
        -BuildStartedUtc $buildStartedUtc
    $sourceState = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
    $linuxNodeArtifact = Get-Item -LiteralPath (
        Join-Path $repo 'tools\wta\remote\linux-x64\wta-node')
    $linuxNodeAttestation = Join-Path $localStaging 'linux-node-current-run.json'
    [ordered]@{
        schemaVersion = 1
        attestationType = 'wta-linux-node-current-run'
        role = 'wta-node-linux-x64'
        sourceFingerprint = [string]$sourceState.source.fingerprint
        buildStartedUtc = $buildStartedUtc.ToUniversalTime().ToString('o')
        producedAtUtc = [datetime]::UtcNow.ToString('o')
        length = $linuxNodeArtifact.Length
        sha256 = (Get-FileHash -LiteralPath $linuxNodeArtifact.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    } | ConvertTo-Json -Depth 4 |
        Set-Content -LiteralPath $linuxNodeAttestation -Encoding utf8NoBOM
    $overlay = New-SourceOverlay -Root $repo -StagingRoot $localStaging
    Copy-Item -LiteralPath $remoteBuilder -Destination (Join-Path $localStaging 'Invoke-RemoteWindowsBuild.ps1')

    Write-Host "[azure-build] Starting $ResourceGroup/$VmName ..."
    Invoke-Checked az @('vm', 'start', '-g', $ResourceGroup, '-n', $VmName, '--only-show-errors') `
        'Could not start the Azure build VM.'
    $started = $true
    $remoteHost = (az vm show -g $ResourceGroup -n $VmName -d --query publicIps -o tsv).Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($remoteHost)) {
        throw 'The Azure build VM has no reachable public IP.'
    }

    Invoke-RemotePowerShell -HostName $remoteHost -Script @"
`$ErrorActionPreference = 'Stop'
[IO.Directory]::CreateDirectory('$remoteInput') | Out-Null
[IO.Directory]::CreateDirectory('$remoteOutput') | Out-Null
"@
    foreach ($file in @(
            $overlay.Patch,
            $overlay.UntrackedArchive,
            (Join-Path $localStaging 'Invoke-RemoteWindowsBuild.ps1'),
            $linuxNodeAttestation
        )) {
        Invoke-Checked scp.exe @(
            '-i', $script:ResolvedKey,
            '-o', 'BatchMode=yes',
            '-o', 'ConnectTimeout=20',
            '-o', 'StrictHostKeyChecking=yes',
            '-o', "UserKnownHostsFile=$script:ResolvedKnownHosts",
            $file,
            "$SshUser@$remoteHost`:$($remoteInput.Replace('\', '/'))/"
        ) "Could not upload $(Split-Path $file -Leaf) to the Azure builder."
    }

    $origin = (& git.exe -C $repo remote get-url origin).Trim()
    $remoteCommand = @"
`$ErrorActionPreference = 'Stop'
& 'C:\Program Files\PowerShell\7\pwsh.exe' -NoLogo -NoProfile -NonInteractive -File '$remoteInput\Invoke-RemoteWindowsBuild.ps1' -Origin '$origin' -Commit '$($sourceState.source.commit)' -ExpectedSourceFingerprint '$($sourceState.source.fingerprint)' -InputRoot '$remoteInput' -OutputRoot '$remoteOutput' -LinuxNodeAttestationPath '$remoteInput\$(Split-Path $linuxNodeAttestation -Leaf)'
"@
    try {
        Invoke-RemotePowerShell -HostName $remoteHost -Script $remoteCommand
    }
    catch {
        # Preserve the exact compiler/packaging failure before the finally
        # block removes remote inputs and deallocates the VM.
        try {
            Invoke-Checked scp.exe @(
                '-i', $script:ResolvedKey,
                '-o', 'BatchMode=yes',
                '-o', 'ConnectTimeout=20',
                '-o', 'StrictHostKeyChecking=yes',
                '-o', "UserKnownHostsFile=$script:ResolvedKnownHosts",
                '-r',
                "$SshUser@$remoteHost`:$($remoteOutput.Replace('\', '/'))/*",
                "$localOutput\"
            ) 'Could not download Azure failure evidence.'
        }
        catch {
            Write-Warning "Azure failure evidence download failed: $($_.Exception.Message)"
        }
        throw
    }

    Invoke-Checked scp.exe @(
        '-i', $script:ResolvedKey,
        '-o', 'BatchMode=yes',
        '-o', 'ConnectTimeout=20',
        '-o', 'StrictHostKeyChecking=yes',
        '-o', "UserKnownHostsFile=$script:ResolvedKnownHosts",
        '-r',
        "$SshUser@$remoteHost`:$($remoteOutput.Replace('\', '/'))/*",
        "$localOutput\"
    ) 'Could not download Azure build evidence.'

    $result = Get-Content -LiteralPath (Join-Path $localOutput 'result.json') -Raw | ConvertFrom-Json
    if (-not $result.ok -or $result.sourceFingerprint -ne $sourceState.source.fingerprint) {
        throw 'Azure builder returned an unsuccessful or mismatched result.'
    }
    & (Join-Path $scriptRoot 'New-ReproducibleBuildManifest.ps1') `
        -Mode Verify `
        -RepoRoot $repo `
        -ManifestPath (Join-Path $localOutput 'evidence\build-manifest.json')
    Write-Host "[azure-build] PASS installer=$($result.installer) sha256=$($result.installerSha256)"
    Get-Item -LiteralPath (Join-Path $localOutput 'evidence' $result.installer)
}
catch {
    $primaryError = $_
    throw
}
finally {
    if ($remoteHost -and -not $KeepRemoteInputs) {
        try {
            Invoke-RemotePowerShell -HostName $remoteHost -Script @"
foreach (`$path in @('$remoteInput', '$remoteOutput', '$remoteWork')) {
    if (`$path.StartsWith('C:\IntelligentTerminalBuild\', [StringComparison]::OrdinalIgnoreCase) -and
        (Test-Path -LiteralPath `$path)) {
        for (`$attempt = 1; `$attempt -le 5; `$attempt++) {
            try {
                Remove-Item -LiteralPath `$path -Recurse -Force
                break
            }
            catch {
                if (`$attempt -eq 5) { throw }
                Start-Sleep -Milliseconds 500
            }
        }
    }
}
"@
        }
        catch {
            Write-Warning "Remote cleanup failed: $($_.Exception.Message)"
        }
    }
    if ($started) {
        Write-Host "[azure-build] Deallocating $ResourceGroup/$VmName ..."
        az vm deallocate -g $ResourceGroup -n $VmName --only-show-errors
        if ($LASTEXITCODE -ne 0) {
            if ($primaryError) {
                Write-Warning 'The build failed and VM deallocation also failed. Deallocate it immediately in Azure.'
            }
            else {
                throw 'The build completed but Azure VM deallocation failed.'
            }
        }
    }
    if (Test-Path -LiteralPath $localStaging) {
        Remove-Item -LiteralPath $localStaging -Recurse -Force
    }
}
