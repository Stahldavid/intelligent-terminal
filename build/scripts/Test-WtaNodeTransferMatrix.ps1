[CmdletBinding()]
param(
    [string]$Alias = 'do-codex',
    [string]$TargetId = 'ssh:do-codex',
    [string]$WtaExe = (Join-Path $PSScriptRoot '..\..\tools\wta\target\debug\wta.exe'),
    [ValidateRange(1, 256)]
    [int]$MediumSizeMiB = 16,
    [switch]$VerifyCancellation,
    [ValidateRange(64, 1024)]
    [int]$CancellationSizeMiB = 512
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Invoke-WtaJson {
    param([Parameter(Mandatory)][string[]]$Arguments)

    $raw = & $script:ResolvedWta @Arguments --json
    if ($LASTEXITCODE -ne 0) {
        throw "wta failed: $($Arguments -join ' ')"
    }
    return ($raw -join "`n") | ConvertFrom-Json
}

function Remove-Transfer {
    param([Parameter(Mandatory)][object]$Transfer)

    $transferIdProperty = $Transfer.PSObject.Properties['transfer_id']
    if (-not $transferIdProperty -or [string]::IsNullOrWhiteSpace([string]$transferIdProperty.Value)) {
        Write-Warning 'Skipped cleanup for a transfer result without transfer_id.'
        return
    }
    $transferId = [string]$transferIdProperty.Value
    $remotePathProperty = $Transfer.PSObject.Properties['remote_path']
    if ($remotePathProperty -and
        -not [string]::IsNullOrWhiteSpace([string]$remotePathProperty.Value)) {
        & ssh.exe -o BatchMode=yes -o StrictHostKeyChecking=yes $Alias -- `
            '$HOME/.local/state/intelligent-terminal-node/current/wta-node' `
            file abort-upload --transfer $transferId 2>$null | Out-Null
    }
    try {
        Invoke-WtaJson @('compute', 'transfer', 'delete', $transferId) | Out-Null
    }
    catch {
        Write-Warning "Could not delete transfer record $transferId."
    }
}

if ($Alias -ne 'do-codex' -or $TargetId -ne 'ssh:do-codex') {
    throw 'The physical transfer matrix is restricted to the dedicated non-production do-codex target.'
}
$script:ResolvedWta = (Resolve-Path -LiteralPath $WtaExe).Path
$target = Invoke-WtaJson @('compute', 'target', 'get', $TargetId)
if ($target.disabled -or $target.health -ne 'healthy' -or
    $target.endpoint.ssh_alias -ne $Alias) {
    throw "Target $TargetId must be enabled, healthy and mapped to $Alias."
}

$runId = "transfer-matrix-$([guid]::NewGuid().ToString('N'))"
$tempRoot = Join-Path ([IO.Path]::GetTempPath()) $runId
[IO.Directory]::CreateDirectory($tempRoot) | Out-Null
$transfers = [Collections.Generic.List[object]]::new()
$fileRoots = [Collections.Generic.List[string]]::new()
$cases = [Collections.Generic.List[object]]::new()

try {
    $fixtures = @(
        [pscustomobject]@{ Name = 'empty.bin'; Size = 0L; Content = $null },
        [pscustomobject]@{ Name = 'unicode-ç-שלום-テスト.txt'; Size = 0L; Content = 'Unicode ✓ transfer' },
        [pscustomobject]@{ Name = 'medium.bin'; Size = [long]$MediumSizeMiB * 1MB; Content = $null }
    )
    foreach ($fixture in $fixtures) {
        $source = Join-Path $tempRoot $fixture.Name
        if ($null -ne $fixture.Content) {
            [IO.File]::WriteAllText($source, $fixture.Content, [Text.UTF8Encoding]::new($false))
        }
        else {
            $stream = [IO.File]::Open($source, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
            try {
                $stream.SetLength($fixture.Size)
            }
            finally {
                $stream.Dispose()
            }
        }
        $sourceHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash.ToLowerInvariant()
        $uploaded = Invoke-WtaJson @(
            'compute', 'transfer', 'upload',
            '--target', $TargetId,
            '--source', $source,
            '--workspace', $runId,
            '--surface', "surface-$($cases.Count)"
        )
        $transfers.Add($uploaded)
        if ($uploaded.state -ne 'succeeded' -or
            $uploaded.sha256 -ne $sourceHash -or
            $uploaded.bytes_transferred -ne $uploaded.size_bytes) {
            throw "Upload failed integrity checks for $($fixture.Name)."
        }
        $lastSeparator = $uploaded.remote_path.LastIndexOf('/')
        if ($lastSeparator -le 0 -or $lastSeparator -eq $uploaded.remote_path.Length - 1) {
            throw "Upload returned an invalid remote path for $($fixture.Name)."
        }
        $remoteRoot = $uploaded.remote_path.Substring(0, $lastSeparator)
        $remoteRelativePath = $uploaded.remote_path.Substring($lastSeparator + 1)
        $fileRootId = "file-root-$runId-$($cases.Count)"
        $fileRoot = Invoke-WtaJson @(
            'compute', 'file', 'authorize',
            '--id', $fileRootId,
            '--target', $TargetId,
            '--workspace', $runId,
            '--label', "Transfer matrix $($fixture.Name)",
            '--path', $remoteRoot,
            '--source', 'project'
        )
        if ($fileRoot.id -ne $fileRootId -or -not $fileRoot.active) {
            throw "Remote file root authorization failed for $($fixture.Name)."
        }
        $fileRoots.Add($fileRootId)
        $download = Join-Path $tempRoot "download-$($fixture.Name)"
        $downloaded = Invoke-WtaJson @(
            'compute', 'file', 'download',
            '--target', $TargetId,
            '--workspace', $runId,
            '--root', $fileRootId,
            '--path', $remoteRelativePath,
            '--destination', $download,
            '--overwrite'
        )
        $transfers.Add($downloaded)
        $downloadHash = (Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($downloaded.state -ne 'succeeded' -or
            $downloaded.sha256 -ne $sourceHash -or
            $downloadHash -ne $sourceHash) {
            throw "Download failed integrity checks for $($fixture.Name)."
        }
        $cases.Add([pscustomobject]@{
            Name = $fixture.Name
            SizeBytes = (Get-Item -LiteralPath $source).Length
            Sha256 = $sourceHash
            UploadId = $uploaded.transfer_id
            DownloadId = $downloaded.transfer_id
            Passed = $true
        })
    }

    $cancellation = $null
    if ($VerifyCancellation) {
        $source = Join-Path $tempRoot 'cancel-me.bin'
        $stream = [IO.File]::Open($source, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
        try {
            $stream.SetLength([long]$CancellationSizeMiB * 1MB)
        }
        finally {
            $stream.Dispose()
        }
        $known = @((Invoke-WtaJson @('compute', 'transfer', 'list')).transfer_id)
        $stdout = Join-Path $tempRoot 'cancel.stdout'
        $stderr = Join-Path $tempRoot 'cancel.stderr'
        $process = Start-Process -FilePath $script:ResolvedWta -ArgumentList @(
            'compute', 'transfer', 'upload',
            '--target', $TargetId,
            '--source', $source,
            '--workspace', $runId,
            '--surface', 'surface-cancel',
            '--json'
        ) -RedirectStandardOutput $stdout -RedirectStandardError $stderr -WindowStyle Hidden -PassThru
        $candidate = $null
        $deadline = [datetime]::UtcNow.AddSeconds(20)
        while ([datetime]::UtcNow -lt $deadline -and -not $candidate) {
            Start-Sleep -Milliseconds 100
            $candidate = @(Invoke-WtaJson @('compute', 'transfer', 'list')) |
                Where-Object {
                    $_.transfer_id -notin $known -and
                    $_.workspace_id -eq $runId -and
                    $_.display_name -eq 'cancel-me.bin'
                } |
                Select-Object -First 1
        }
        if (-not $candidate) {
            if (-not $process.HasExited) {
                $process.Kill($true)
            }
            throw 'Could not observe the in-flight cancellation transfer.'
        }
        $null = Invoke-WtaJson @('compute', 'transfer', 'cancel', $candidate.transfer_id)
        if (-not $process.WaitForExit(30000)) {
            $process.Kill($true)
            throw 'Cancelled transfer process did not terminate.'
        }
        $record = Invoke-WtaJson @('compute', 'transfer', 'get', $candidate.transfer_id)
        $transfers.Add($record)
        if ($record.state -ne 'cancelled') {
            throw "Expected cancelled transfer, got '$($record.state)'."
        }
        $remote = & ssh.exe -o BatchMode=yes -o StrictHostKeyChecking=yes $Alias -- `
            '$HOME/.local/state/intelligent-terminal-node/current/wta-node' file list
        if ($LASTEXITCODE -ne 0) {
            throw 'Could not inspect remote transfer cleanup.'
        }
        if (($remote -join "`n").Contains($candidate.transfer_id)) {
            throw 'Cancelled transfer left a remote incoming/final payload.'
        }
        $cancellation = [pscustomobject]@{
            TransferId = $candidate.transfer_id
            SizeMiB = $CancellationSizeMiB
            State = $record.state
            RemoteCleanupVerified = $true
        }
    }

    [pscustomobject]@{
        RunId = $runId
        TargetId = $TargetId
        Cases = $cases.ToArray()
        Cancellation = $cancellation
        MatrixPassed = $true
    }
}
finally {
    foreach ($fileRootId in $fileRoots) {
        try {
            Invoke-WtaJson @('compute', 'file', 'revoke', $fileRootId) | Out-Null
        }
        catch {
            Write-Warning "Could not revoke remote file root $fileRootId."
        }
    }
    foreach ($transfer in $transfers) {
        Remove-Transfer -Transfer $transfer
    }
    if (Test-Path -LiteralPath $tempRoot) {
        Remove-Item -LiteralPath $tempRoot -Recurse -Force
    }
}
