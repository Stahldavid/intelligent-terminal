[CmdletBinding()]
param(
    [string]$Distro = 'Ubuntu-22.04',
    [string]$WtaExe = (Join-Path $PSScriptRoot '..\..\tools\wta\target\x86_64-pc-windows-msvc\debug\wta.exe'),
    [string]$WtaNodeLinux = (Join-Path $PSScriptRoot '..\..\tools\wta\remote\linux-x64\wta-node'),
    [string]$Adapter = '@agentclientprotocol/codex-acp@1.1.7',
    [switch]$VerifyIsolation,
    [switch]$RequireAuthenticatedReattach
)

$ErrorActionPreference = 'Stop'

function Convert-ToWslPath {
    param([Parameter(Mandatory)][string]$Path)

    $resolved = (Resolve-Path -LiteralPath $Path).Path
    if ($resolved -notmatch '^([A-Za-z]):\\(.*)$') {
        throw "Only local drive paths can be translated to WSL: $resolved"
    }
    $drive = $Matches[1].ToLowerInvariant()
    $tail = $Matches[2].Replace('\', '/')
    "/mnt/$drive/$tail"
}

function Get-RemoteSessions {
    param([Parameter(Mandatory)][string]$NodePath)

    $raw = & wsl.exe -d $Distro -- $NodePath acp list
    if ($LASTEXITCODE -ne 0) {
        throw "wta-node acp list failed with exit code $LASTEXITCODE."
    }
    @($raw | ConvertFrom-Json)
}

function Invoke-AcpProbe {
    param(
        [Parameter(Mandatory)][ValidateSet('start', 'attach')]
        [string]$Action,
        [Parameter(Mandatory)][string]$SessionId,
        [Parameter(Mandatory)][string]$NodePath,
        [Parameter(Mandatory)][string]$LinuxPath
    )

    $remote = "wsl.exe -d $Distro -- env PATH=$LinuxPath $NodePath acp $Action --session $SessionId"
    if ($Action -eq 'start') {
        $remote += " -- npx -y $Adapter"
    }
    $raw = & $WtaExe probe-sessions --agent $remote --json
    if ($LASTEXITCODE -ne 0) {
        throw "ACP $Action probe failed with exit code $LASTEXITCODE."
    }
    $parsed = $raw | ConvertFrom-Json
    if (-not $parsed.list_sessions_ok) {
        throw "ACP $Action reached the adapter but session/list failed: $($parsed.list_sessions_error)"
    }
    $parsed
}

function Invoke-ReattachDiagnostic {
    param(
        [Parameter(Mandatory)][string]$SessionId,
        [Parameter(Mandatory)][string]$NodePath
    )

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = 'wsl.exe'
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($argument in @('-d', $Distro, '--', $NodePath, 'acp', 'attach', '--session', $SessionId)) {
        $startInfo.ArgumentList.Add($argument)
    }

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    if (-not $process.Start()) {
        throw 'Could not start the reattach transport.'
    }
    try {
        $request = @{
            jsonrpc = '2.0'
            id = 9001
            method = 'initialize'
            params = @{
                protocolVersion = 1
                clientCapabilities = @{ terminal = $true }
                clientInfo = @{ name = 'wta-node-reattach-e2e'; version = '0.9.4' }
            }
        } | ConvertTo-Json -Compress -Depth 6
        $process.StandardInput.WriteLine($request)
        $process.StandardInput.Flush()
        $readTask = $process.StandardOutput.ReadLineAsync()
        $line = $readTask.WaitAsync([TimeSpan]::FromSeconds(15)).GetAwaiter().GetResult()
        $initializeResponse = $line | ConvertFrom-Json
        if ($initializeResponse.id -ne 9001 -or -not $initializeResponse.result.agentInfo) {
            throw "Reattach did not replay a valid ACP initialize response: $line"
        }

        $sessionListRequest = @{
            jsonrpc = '2.0'
            id = 9002
            method = 'session/list'
            params = @{}
        } | ConvertTo-Json -Compress -Depth 4
        $process.StandardInput.WriteLine($sessionListRequest)
        $process.StandardInput.Flush()
        $listReadTask = $process.StandardOutput.ReadLineAsync()
        $listLine = $listReadTask.WaitAsync([TimeSpan]::FromSeconds(15)).GetAwaiter().GetResult()
        $sessionListResponse = $listLine | ConvertFrom-Json
        if ($sessionListResponse.id -ne 9002) {
            throw "Reattach returned an unexpected ACP response: $listLine"
        }

        $sessionListOk = $null -ne $sessionListResponse.result
        $authenticationGateDetected =
            -not $sessionListOk -and
            $sessionListResponse.error.message -eq 'Authentication required'
        if (-not $sessionListOk -and -not $authenticationGateDetected) {
            throw "Reattach session/list failed unexpectedly: $listLine"
        }

        [pscustomobject]@{
            initialize_response = $initializeResponse
            session_list_response = $sessionListResponse
            session_list_ok = $sessionListOk
            authentication_gate_detected = $authenticationGateDetected
            authentication_error = $sessionListResponse.error.message
        }
    }
    finally {
        $process.StandardInput.Close()
        if (-not $process.WaitForExit(5000)) {
            $process.Kill($true)
            $process.WaitForExit()
        }
        $process.Dispose()
    }
}

function Start-ProbedSession {
    param(
        [Parameter(Mandatory)][string]$SessionId,
        [Parameter(Mandatory)][string]$NodePath,
        [Parameter(Mandatory)][string]$LinuxPath
    )

    for ($attempt = 1; $attempt -le 3; $attempt++) {
        try {
            return Invoke-AcpProbe -Action start -SessionId $SessionId -NodePath $NodePath -LinuxPath $LinuxPath
        }
        catch {
            if ($attempt -eq 3) {
                throw
            }
            Start-Sleep -Milliseconds (250 * $attempt)
        }
    }
}

$resolvedWta = (Resolve-Path -LiteralPath $WtaExe).Path
$nodePath = Convert-ToWslPath -Path $WtaNodeLinux
$linuxHome = (& wsl.exe -d $Distro -- sh -lc 'printf %s "$HOME"').Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($linuxHome)) {
    throw "Could not resolve HOME in WSL distro $Distro."
}
$linuxPath = "$linuxHome/.local/share/intelligent-terminal/toolchains/node-current/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
$sessionIds = @("e2e-codex-$([Guid]::NewGuid().ToString('N').Substring(0, 12))")
if ($VerifyIsolation) {
    $sessionIds += "e2e-codex-$([Guid]::NewGuid().ToString('N').Substring(0, 12))"
}

# Warm only the pinned adapter package. This keeps the 10-second ACP probe
# focused on adapter startup instead of a first-run network download.
& wsl.exe -d $Distro -- env "PATH=$linuxPath" npx -y $Adapter --version | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "Could not warm $Adapter in WSL distro $Distro."
}

$startedSessionIds = [System.Collections.Generic.List[string]]::new()
try {
    $sessionEvidence = foreach ($sessionId in $sessionIds) {
        $first = Start-ProbedSession -SessionId $sessionId -NodePath $nodePath -LinuxPath $linuxPath
        $startedSessionIds.Add($sessionId)
        $record1 = Get-RemoteSessions -NodePath $nodePath |
            Where-Object session_id -eq $sessionId |
            Select-Object -First 1
        if (-not $record1 -or $record1.pid -le 0 -or $record1.state -ne 'detached') {
            throw "Persistent session $sessionId was not detached and alive after the first transport closed."
        }

        $reattach = Invoke-ReattachDiagnostic -SessionId $sessionId -NodePath $nodePath
        $record2 = Get-RemoteSessions -NodePath $nodePath |
            Where-Object session_id -eq $sessionId |
            Select-Object -First 1
        if (-not $record2 -or $record2.pid -ne $record1.pid) {
            throw "ACP reattach did not preserve the adapter PID for $sessionId."
        }
        if ($RequireAuthenticatedReattach -and -not $reattach.session_list_ok) {
            throw "Authenticated reattach is required but session/list failed: $($reattach.authentication_error)"
        }

        [pscustomobject]@{
            session_id = $sessionId
            pid_before_reattach = $record1.pid
            pid_after_reattach = $record2.pid
            initialize_ok = $null -ne $first.initialize_dump
            first_session_list_ok = [bool]$first.list_sessions_ok
            reattach_initialize_ok = $null -ne $reattach.initialize_response.result.agentInfo
            reattach_session_list_ok = [bool]$reattach.session_list_ok
            authentication_gate_detected = [bool]$reattach.authentication_gate_detected
            authentication_error = $reattach.authentication_error
        }
    }

    $pids = @($sessionEvidence | ForEach-Object pid_before_reattach)
    $isolationVerified = -not $VerifyIsolation -or
        (($pids | Select-Object -Unique).Count -eq $sessionEvidence.Count)
    if (-not $isolationVerified) {
        throw "Two surface sessions shared an adapter PID: $($pids -join ', ')."
    }

    $primary = $sessionEvidence[0]
    [ordered]@{
        schema_version = 1
        distro = $Distro
        adapter = $Adapter
        session_id = $primary.session_id
        pid_before_reattach = $primary.pid_before_reattach
        pid_after_reattach = $primary.pid_after_reattach
        initialize_ok = $primary.initialize_ok
        first_session_list_ok = $primary.first_session_list_ok
        reattach_initialize_ok = $primary.reattach_initialize_ok
        reattach_session_list_ok = $primary.reattach_session_list_ok
        authentication_gate_detected = $primary.authentication_gate_detected
        authentication_error = $primary.authentication_error
        persistent_transport_verified = $true
        isolation_requested = [bool]$VerifyIsolation
        surface_process_isolation_verified = [bool]$isolationVerified
        sessions = @($sessionEvidence)
        wta_exe = $resolvedWta
        wta_node = $nodePath
    } | ConvertTo-Json
}
finally {
    foreach ($sessionId in $startedSessionIds) {
        & wsl.exe -d $Distro -- $nodePath acp stop --session $sessionId | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "Failed to stop E2E ACP session $sessionId."
        }
    }
}
