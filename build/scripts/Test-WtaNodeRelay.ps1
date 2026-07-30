[CmdletBinding()]
param(
    [string]$Distro = 'Ubuntu-22.04',
    [string]$NodeBinary = 'tools/wta/remote/linux-x64/wta-node'
)

$ErrorActionPreference = 'Stop'

function Invoke-NodeRpc {
    param(
        [Parameter(Mandatory)][string]$Method,
        [Parameter(Mandatory)][hashtable]$Params,
        [switch]$ExpectFailure
    )

    $request = @{
        jsonrpc = '2.0'
        id = [guid]::NewGuid().ToString('N')
        method = $Method
        params = $Params
    } | ConvertTo-Json -Compress -Depth 12
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = 'wsl.exe'
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($argument in @('-d', $Distro, '--', $script:NodeWsl, 'bridge')) {
        $info.ArgumentList.Add($argument)
    }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    if (-not $process.Start()) {
        throw "Could not start wta-node bridge for $Method."
    }
    try {
        $process.StandardInput.WriteLine($request)
        $process.StandardInput.Flush()
        $process.StandardInput.Close()
        $line = $process.StandardOutput.ReadLineAsync().
            WaitAsync([TimeSpan]::FromSeconds(15)).GetAwaiter().GetResult()
        if ([string]::IsNullOrWhiteSpace($line)) {
            $stderr = $process.StandardError.ReadToEnd()
            throw "wta-node bridge returned no response for $Method. $stderr"
        }
        $response = $line | ConvertFrom-Json
    }
    finally {
        if (-not $process.HasExited) {
            $process.Kill($true)
        }
        $process.WaitForExit()
        $process.Dispose()
    }
    if ($ExpectFailure) {
        if ($null -eq $response.error) {
            throw "$Method unexpectedly succeeded."
        }
    }
    elseif ($null -ne $response.error) {
        throw "$Method failed: $($response.error.message)"
    }
    return $response
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$nodePath = (Resolve-Path (Join-Path $repoRoot $NodeBinary)).Path
$script:NodeWsl = (& wsl.exe -d $Distro -- wslpath -a ($nodePath -replace '\\', '/')).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($script:NodeWsl)) {
    throw 'Could not resolve the Linux node artifact inside WSL.'
}

$workspace = "relay-workspace-$([guid]::NewGuid().ToString('N'))"
$surface = "relay-surface-$([guid]::NewGuid().ToString('N'))"
$otherSurface = "relay-other-$([guid]::NewGuid().ToString('N'))"
$scope = @{
    workspace_id = $workspace
    surface_id = $surface
}

$issued = Invoke-NodeRpc -Method 'relay.capability.issue' -Params @{
    scope = $scope
    operations = @('notify', 'status', 'progress', 'focus', 'list')
    ttl_ms = 60000
}
$token = $issued.result.token
if ([string]::IsNullOrWhiteSpace($token)) {
    throw 'Relay capability issuance returned no token.'
}

$nonce = [guid]::NewGuid().ToString('N')
$event = Invoke-NodeRpc -Method 'relay.notify' -Params @{
    authorization = @{ token = $token; nonce = $nonce }
    scope = $scope
    title = 'Relay E2E'
    body = 'survived a separate bridge connection'
    level = 'info'
    metadata = @{ source = 'wsl-e2e' }
}
if ($event.result.scope.surface_id -ne $surface) {
    throw 'Relay event was recorded against the wrong surface.'
}

# Every RPC above launches a distinct bridge process. Listing the event from a
# third process proves that the authority and journal belong to the daemon,
# rather than to one transient SSH/stdio attachment.
$listed = Invoke-NodeRpc -Method 'relay.list' -Params @{
    authorization = @{
        token = $token
        nonce = [guid]::NewGuid().ToString('N')
    }
    scope = $scope
    after_sequence = 0
    limit = 20
}
$matching = @($listed.result.events | Where-Object event_id -eq $event.result.event_id)
if ($matching.Count -ne 1) {
    throw 'A new bridge connection could not resume the daemon relay journal.'
}

$null = Invoke-NodeRpc -Method 'relay.notify' -Params @{
    authorization = @{ token = $token; nonce = $nonce }
    scope = $scope
    title = 'Replay'
    body = ''
    level = 'info'
    metadata = @{}
} -ExpectFailure

$null = Invoke-NodeRpc -Method 'relay.notify' -Params @{
    authorization = @{
        token = $token
        nonce = [guid]::NewGuid().ToString('N')
    }
    scope = @{ workspace_id = $workspace; surface_id = $otherSurface }
    title = 'Cross-surface'
    body = ''
    level = 'info'
    metadata = @{}
} -ExpectFailure

$revoked = Invoke-NodeRpc -Method 'relay.capability.revoke' -Params @{ token = $token }
if (-not $revoked.result.revoked) {
    throw 'Relay capability was not revoked.'
}
$null = Invoke-NodeRpc -Method 'relay.list' -Params @{
    authorization = @{
        token = $token
        nonce = [guid]::NewGuid().ToString('N')
    }
    scope = $scope
    after_sequence = 0
    limit = 20
} -ExpectFailure

[pscustomobject]@{
    Distro = $Distro
    WorkspaceId = $workspace
    SurfaceId = $surface
    EventId = $event.result.event_id
    Sequence = $event.result.sequence
    JournalSurvivedReconnect = $true
    ReplayRejected = $true
    CrossSurfaceRejected = $true
    RevocationRejectedFurtherUse = $true
}
