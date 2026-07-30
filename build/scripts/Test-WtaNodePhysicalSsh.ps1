[CmdletBinding()]
param(
    [string]$Alias = 'do-codex',
    [string]$TargetId,
    [string]$WtaExe = (Join-Path $PSScriptRoot '..\..\tools\wta\target\debug\wta.exe'),
    [string]$Adapter = '@agentclientprotocol/codex-acp@1.1.7',
    [switch]$RequireAuthenticatedAcp,
    [string]$OutputPath
)

$ErrorActionPreference = 'Stop'

function Invoke-WtaJson {
    param([Parameter(Mandatory)][string[]]$Arguments)

    $raw = & $script:ResolvedWta @Arguments --json
    if ($LASTEXITCODE -ne 0) {
        throw "wta failed: $($Arguments -join ' ')"
    }
    ($raw -join "`n") | ConvertFrom-Json
}

function Invoke-RemoteJson {
    param(
        [Parameter(Mandatory)][string[]]$Arguments,
        [switch]$AllowFailure
    )

    $raw = & ssh.exe -o BatchMode=yes -o ConnectTimeout=10 `
        -o StrictHostKeyChecking=yes $Alias -- "`$HOME/$script:ActivePath" @Arguments
    if ($LASTEXITCODE -ne 0 -and -not $AllowFailure) {
        throw "remote wta-node failed: $($Arguments -join ' ')"
    }
    if ([string]::IsNullOrWhiteSpace(($raw -join ''))) {
        return $null
    }
    ($raw -join "`n") | ConvertFrom-Json
}

function Start-SshNodeProcess {
    param(
        [Parameter(Mandatory)][string[]]$NodeArguments,
        [switch]$Tty
    )

    $info = [System.Diagnostics.ProcessStartInfo]::new()
    $info.FileName = 'ssh.exe'
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    if ($Tty) {
        $info.ArgumentList.Add('-tt')
    }
    foreach ($argument in @(
        '-o', 'BatchMode=yes',
        '-o', 'ConnectTimeout=10',
        '-o', 'StrictHostKeyChecking=yes',
        $Alias,
        '--',
        "`$HOME/$script:ActivePath"
    ) + $NodeArguments) {
        $info.ArgumentList.Add($argument)
    }
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $info
    if (-not $process.Start()) {
        throw 'Could not start the physical SSH transport.'
    }
    $process
}

function Stop-ExactProcess {
    param([System.Diagnostics.Process]$Process)

    if ($null -eq $Process) {
        return
    }
    try {
        $Process.StandardInput.Close()
        if (-not $Process.WaitForExit(1500)) {
            $Process.Kill($true)
            $Process.WaitForExit()
        }
    }
    finally {
        $Process.Dispose()
    }
}

function Read-AcpResponse {
    param(
        [Parameter(Mandatory)][System.Diagnostics.Process]$Process,
        [Parameter(Mandatory)][int]$Id
    )

    $deadline = [DateTime]::UtcNow.AddSeconds(45)
    while ([DateTime]::UtcNow -lt $deadline) {
        $remaining = $deadline - [DateTime]::UtcNow
        $line = $Process.StandardOutput.ReadLineAsync().
            WaitAsync($remaining).GetAwaiter().GetResult()
        if ($null -eq $line) {
            $stderr = $Process.StandardError.ReadToEnd()
            throw "ACP transport ended before response $Id. $stderr"
        }
        try {
            $message = $line | ConvertFrom-Json
            if ($message.id -eq $Id) {
                return $message
            }
        }
        catch {
            # The adapter may emit non-protocol startup text on stderr/stdout.
            # Ignore only non-JSON lines; an expected response still has a
            # bounded deadline.
        }
    }
    throw "ACP response $Id timed out."
}

function Invoke-AcpHandshake {
    param(
        [Parameter(Mandatory)][ValidateSet('start', 'attach')]
        [string]$Action,
        [Parameter(Mandatory)][string]$SessionId
    )

    $arguments = @('acp', $Action, '--session', $SessionId)
    if ($Action -eq 'start') {
        $arguments += @('--', 'npx', '-y', $Adapter)
    }
    $process = Start-SshNodeProcess -NodeArguments $arguments
    try {
        $initialize = @{
            jsonrpc = '2.0'
            id = 7101
            method = 'initialize'
            params = @{
                protocolVersion = 1
                clientCapabilities = @{ terminal = $true }
                clientInfo = @{
                    name = 'intelligent-terminal-physical-ssh-e2e'
                    version = '1'
                }
            }
        } | ConvertTo-Json -Compress -Depth 8
        $process.StandardInput.WriteLine($initialize)
        $process.StandardInput.Flush()
        $initializeResponse = Read-AcpResponse -Process $process -Id 7101
        if ($null -eq $initializeResponse.result.agentInfo) {
            throw "ACP initialize failed: $($initializeResponse | ConvertTo-Json -Compress -Depth 8)"
        }

        $process.StandardInput.WriteLine(
            (@{
                jsonrpc = '2.0'
                id = 7102
                method = 'session/list'
                params = @{}
            } | ConvertTo-Json -Compress -Depth 4)
        )
        $process.StandardInput.Flush()
        $listResponse = Read-AcpResponse -Process $process -Id 7102
        $authenticated = $null -ne $listResponse.result
        $authenticationGate =
            -not $authenticated -and
            $listResponse.error.message -eq 'Authentication required'
        if (-not $authenticated -and -not $authenticationGate) {
            throw "ACP session/list failed unexpectedly: $($listResponse | ConvertTo-Json -Compress -Depth 8)"
        }
        [pscustomobject]@{
            initialize_ok = $true
            authenticated = $authenticated
            authentication_gate_detected = $authenticationGate
            authentication_error = $listResponse.error.message
        }
    }
    finally {
        Stop-ExactProcess -Process $process
    }
}

if ($Alias -ne 'do-codex') {
    throw "Physical SSH parity is restricted to the dedicated non-production alias 'do-codex'."
}

$script:ResolvedWta = (Resolve-Path -LiteralPath $WtaExe).Path
$sshConfig = & ssh.exe -G $Alias
if ($LASTEXITCODE -ne 0) {
    throw "OpenSSH could not resolve alias $Alias."
}
$resolvedUser = ($sshConfig | Where-Object { $_ -match '^user\s+' } |
    Select-Object -First 1) -replace '^user\s+', ''
if ($resolvedUser -ne 'codex-agent') {
    throw "Alias $Alias must resolve to the dedicated codex-agent account, not '$resolvedUser'."
}

$discovered = @(Invoke-WtaJson -Arguments @('compute', 'target', 'discover', '--save'))
$target = $discovered |
    Where-Object { $_.endpoint.ssh_alias -eq $Alias } |
    Select-Object -First 1
if ($null -eq $target) {
    $targets = @(Invoke-WtaJson -Arguments @('compute', 'target', 'list'))
    $target = $targets |
        Where-Object { $_.endpoint.ssh_alias -eq $Alias } |
        Select-Object -First 1
}
if ($null -eq $target) {
    throw "No compute target maps to SSH alias $Alias."
}
if ($TargetId -and $target.id -ne $TargetId) {
    throw "Resolved target '$($target.id)' does not match requested '$TargetId'."
}
$TargetId = $target.id

# Trust is explicit and fail-closed. `preview-trust` must succeed before the
# host key can be accepted or refreshed.
$null = Invoke-WtaJson -Arguments @('compute', 'target', 'preview-trust', $TargetId)
$null = Invoke-WtaJson -Arguments @('compute', 'target', 'trust', $TargetId)
$null = Invoke-WtaJson -Arguments @('compute', 'target', 'enable', $TargetId)
$bootstrap = Invoke-WtaJson -Arguments @('compute', 'node', 'bootstrap', $TargetId)
$target = Invoke-WtaJson -Arguments @('compute', 'target', 'get', $TargetId)
$script:ActivePath = $target.metadata.node_installation.active_path
if ([string]::IsNullOrWhiteSpace($script:ActivePath)) {
    throw 'Bootstrap did not persist the canonical active wta-node path.'
}

$ptySession = "physical-pty-$([Guid]::NewGuid().ToString('N').Substring(0, 16))"
$workspaceId = "physical-e2e-$([Guid]::NewGuid().ToString('N').Substring(0, 12))"
$acpSessions = @(
    "physical-acp-$([Guid]::NewGuid().ToString('N').Substring(0, 16))",
    "physical-acp-$([Guid]::NewGuid().ToString('N').Substring(0, 16))"
)
$transferId = $null
$downloadTransferId = $null
$fileRootId = $null
$tempRoot = Join-Path $env:TEMP "intelligent-terminal-$workspaceId"
New-Item -ItemType Directory -Path $tempRoot | Out-Null

try {
    # Start a real remote PTY, send a marker, then kill only the SSH
    # attachment. The remote child must remain alive and reattachable.
    $marker = "physical-pty-marker-$([Guid]::NewGuid().ToString('N'))"
    $firstAttachment = Start-SshNodeProcess -Tty -NodeArguments @(
        'pty', 'start', '--session', $ptySession, '--', '/bin/sh', '-l'
    )
    $firstAttachment.StandardInput.WriteLine("printf '$marker\n'")
    $firstAttachment.StandardInput.Flush()
    Start-Sleep -Milliseconds 750
    Stop-ExactProcess -Process $firstAttachment

    $ptyBefore = Invoke-RemoteJson -Arguments @('pty', 'status', '--session', $ptySession)
    if ($ptyBefore.pid -le 0 -or $ptyBefore.state -ne 'detached') {
        throw "Persistent PTY did not survive transport loss: $($ptyBefore | ConvertTo-Json -Compress)"
    }

    $secondAttachment = Start-SshNodeProcess -Tty -NodeArguments @(
        'pty', 'attach', '--session', $ptySession
    )
    try {
        $deadline = [DateTime]::UtcNow.AddSeconds(15)
        $foundMarker = $false
        while ([DateTime]::UtcNow -lt $deadline -and -not $foundMarker) {
            $remaining = $deadline - [DateTime]::UtcNow
            $line = $secondAttachment.StandardOutput.ReadLineAsync().
                WaitAsync($remaining).GetAwaiter().GetResult()
            if ($null -eq $line) {
                break
            }
            $foundMarker = $line.Contains($marker)
        }
        if (-not $foundMarker) {
            throw 'Reattached PTY did not replay the expected backlog marker.'
        }
    }
    finally {
        Stop-ExactProcess -Process $secondAttachment
    }
    $ptyAfter = Invoke-RemoteJson -Arguments @('pty', 'status', '--session', $ptySession)
    if ($ptyAfter.pid -ne $ptyBefore.pid) {
        throw "PTY reattach changed the remote PID ($($ptyBefore.pid) -> $($ptyAfter.pid))."
    }

    # Warm only the pinned ACP adapter on the dedicated build/agent host.
    & ssh.exe -o BatchMode=yes -o StrictHostKeyChecking=yes $Alias -- `
        npx -y $Adapter --version *> $null
    if ($LASTEXITCODE -ne 0) {
        throw "Could not warm pinned adapter $Adapter on $Alias."
    }

    $acpEvidence = foreach ($sessionId in $acpSessions) {
        $first = Invoke-AcpHandshake -Action start -SessionId $sessionId
        $recordBefore = @(Invoke-RemoteJson -Arguments @('acp', 'list')) |
            Where-Object session_id -eq $sessionId |
            Select-Object -First 1
        if ($null -eq $recordBefore -or $recordBefore.pid -le 0) {
            throw "Remote ACP session $sessionId did not persist."
        }
        $second = Invoke-AcpHandshake -Action attach -SessionId $sessionId
        $recordAfter = @(Invoke-RemoteJson -Arguments @('acp', 'list')) |
            Where-Object session_id -eq $sessionId |
            Select-Object -First 1
        if ($recordAfter.pid -ne $recordBefore.pid) {
            throw "ACP reattach changed PID for $sessionId."
        }
        if ($RequireAuthenticatedAcp -and -not $second.authenticated) {
            throw "Authenticated ACP was required but $sessionId reported '$($second.authentication_error)'."
        }
        [pscustomobject]@{
            session_id = $sessionId
            pid = $recordBefore.pid
            initialize_ok = $first.initialize_ok -and $second.initialize_ok
            authenticated = $second.authenticated
            authentication_gate_detected = $second.authentication_gate_detected
        }
    }
    if (@($acpEvidence.pid | Select-Object -Unique).Count -ne $acpEvidence.Count) {
        throw 'Two managed surfaces shared one remote ACP adapter PID.'
    }

    $source = Join-Path $tempRoot 'physical-transfer.txt'
    [IO.File]::WriteAllText(
        $source,
        "verified-transfer-$workspaceId",
        [Text.UTF8Encoding]::new($false)
    )
    $transfer = Invoke-WtaJson -Arguments @(
        'compute', 'transfer', 'upload',
        '--target', $TargetId,
        '--source', $source,
        '--workspace', $workspaceId
    )
    $transferId = $transfer.transfer_id
    if ($transfer.state -ne 'succeeded' -or
        $transfer.bytes_transferred -ne $transfer.size_bytes) {
        throw "Verified transfer did not finish cleanly: $($transfer | ConvertTo-Json -Compress)"
    }
    $lastSeparator = $transfer.remote_path.LastIndexOf('/')
    if ($lastSeparator -le 0 -or $lastSeparator -eq $transfer.remote_path.Length - 1) {
        throw "Upload returned an invalid remote path: $($transfer.remote_path)"
    }
    $remoteRoot = $transfer.remote_path.Substring(0, $lastSeparator)
    $remoteRelativePath = $transfer.remote_path.Substring($lastSeparator + 1)
    $fileRootId = "file-root-$workspaceId"
    $fileRoot = Invoke-WtaJson -Arguments @(
        'compute', 'file', 'authorize',
        '--id', $fileRootId,
        '--target', $TargetId,
        '--workspace', $workspaceId,
        '--label', 'Physical SSH E2E transfer root',
        '--path', $remoteRoot,
        '--source', 'project'
    )
    if ($fileRoot.id -ne $fileRootId -or -not $fileRoot.active) {
        throw "Remote file root authorization failed: $($fileRoot | ConvertTo-Json -Compress)"
    }
    $downloadPath = Join-Path $tempRoot 'physical-transfer.downloaded.txt'
    $download = Invoke-WtaJson -Arguments @(
        'compute', 'file', 'download',
        '--target', $TargetId,
        '--workspace', $workspaceId,
        '--root', $fileRootId,
        '--path', $remoteRelativePath,
        '--destination', $downloadPath,
        '--overwrite'
    )
    $downloadTransferId = $download.transfer_id
    if ($download.state -ne 'succeeded' -or
        $download.bytes_transferred -ne $download.size_bytes -or
        $download.sha256 -ne $transfer.sha256 -or
        -not (Test-Path -LiteralPath $downloadPath) -or
        (Get-Content -LiteralPath $downloadPath -Raw) -ne "verified-transfer-$workspaceId") {
        throw "Verified download did not finish cleanly: $($download | ConvertTo-Json -Compress)"
    }

    $evidencePath = Join-Path $tempRoot 'redacted-evidence.json'
    $evidence = Invoke-WtaJson -Arguments @(
        'compute', 'evidence', 'export',
        '--output', $evidencePath,
        '--redact'
    )
    $evidenceText = Get-Content -LiteralPath $evidencePath -Raw
    if ($evidenceText.Contains($source) -or -not $evidence.redacted) {
        throw 'Evidence export exposed a local source path or was not marked redacted.'
    }

    $resultJson = [ordered]@{
        schema_version = 1
        target_id = $TargetId
        ssh_alias = $Alias
        ssh_user = $resolvedUser
        node_version = $bootstrap.version
        node_sha256 = $bootstrap.sha256
        pty_session_id = $ptySession
        pty_pid_before = $ptyBefore.pid
        pty_pid_after = $ptyAfter.pid
        pty_reattach_verified = $true
        acp_surface_isolation_verified = $true
        acp_sessions = @($acpEvidence)
        authenticated_acp_required = [bool]$RequireAuthenticatedAcp
        transfer_id = $transfer.transfer_id
        transfer_sha256 = $transfer.sha256
        transfer_verified = $true
        file_root_id = $fileRootId
        scoped_file_download_verified = $true
        download_transfer_id = $download.transfer_id
        download_sha256 = $download.sha256
        download_verified = $true
        redacted_evidence_verified = $true
    } | ConvertTo-Json -Depth 8
    if (-not [string]::IsNullOrWhiteSpace($OutputPath)) {
        $absoluteOutputPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
        $outputParent = Split-Path -Parent $absoluteOutputPath
        if ($outputParent) {
            New-Item -ItemType Directory -Path $outputParent -Force | Out-Null
        }
        Set-Content -LiteralPath $absoluteOutputPath -Value $resultJson -Encoding utf8
    }
    $resultJson
}
finally {
    Invoke-RemoteJson -Arguments @('pty', 'stop', '--session', $ptySession) -AllowFailure | Out-Null
    foreach ($sessionId in $acpSessions) {
        Invoke-RemoteJson -Arguments @('acp', 'stop', '--session', $sessionId) -AllowFailure | Out-Null
    }
    if ($transferId) {
        Invoke-RemoteJson -Arguments @('file', 'abort-upload', '--transfer', $transferId) -AllowFailure | Out-Null
        try {
            Invoke-WtaJson -Arguments @('compute', 'transfer', 'delete', $transferId) | Out-Null
        }
        catch {
            Write-Warning "Could not delete local transfer record $transferId."
        }
    }
    if ($downloadTransferId) {
        Invoke-RemoteJson -Arguments @('file', 'abort-upload', '--transfer', $downloadTransferId) -AllowFailure | Out-Null
        try {
            Invoke-WtaJson -Arguments @('compute', 'transfer', 'delete', $downloadTransferId) | Out-Null
        }
        catch {
            Write-Warning "Could not delete local transfer record $downloadTransferId."
        }
    }
    if ($fileRootId) {
        try {
            Invoke-WtaJson -Arguments @('compute', 'file', 'revoke', $fileRootId) | Out-Null
        }
        catch {
            Write-Warning "Could not revoke remote file root $fileRootId."
        }
    }
    if (Test-Path -LiteralPath $tempRoot) {
        Remove-Item -LiteralPath $tempRoot -Recurse -Force
    }
}
