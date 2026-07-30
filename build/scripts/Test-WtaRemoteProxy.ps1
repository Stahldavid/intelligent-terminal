[CmdletBinding()]
param(
    [string]$Alias = 'do-codex',
    [string]$TargetId = 'ssh:do-codex',
    [string]$WtaExe = (Join-Path $PSScriptRoot '..\..\tools\wta\target\debug\wta.exe'),
    [switch]$VerifySupervisorCrash,
    [switch]$VerifyProtocolMatrix
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

function Invoke-WtaDetachedJson {
    param([Parameter(Mandatory)][string[]]$Arguments)

    $token = [guid]::NewGuid().ToString('N')
    $stdout = Join-Path ([IO.Path]::GetTempPath()) "wta-proxy-$token.stdout"
    $stderr = Join-Path ([IO.Path]::GetTempPath()) "wta-proxy-$token.stderr"
    $process = $null
    try {
        $process = Start-Process -FilePath $script:ResolvedWta `
            -ArgumentList @($Arguments + '--json') `
            -RedirectStandardOutput $stdout -RedirectStandardError $stderr `
            -WindowStyle Hidden -PassThru
        if (-not $process.WaitForExit(30000)) {
            $process.Kill($true)
            throw "wta timed out: $($Arguments -join ' ')"
        }
        # The timeout overload only waits for the process handle. The
        # parameterless call also drains Start-Process' redirected streams so
        # their file handles are released before deterministic cleanup.
        $process.WaitForExit()
        $exitCode = $process.ExitCode
        $process.Dispose()
        $process = $null
        $errorText = if (Test-Path -LiteralPath $stderr) {
            Get-Content -LiteralPath $stderr -Raw
        } else {
            ''
        }
        if ($exitCode -ne 0) {
            throw "wta failed: $($Arguments -join ' '): $errorText"
        }
        return (Get-Content -LiteralPath $stdout -Raw) | ConvertFrom-Json
    }
    finally {
        if ($process) {
            $process.Dispose()
        }
        foreach ($path in @($stdout, $stderr)) {
            if ([IO.File]::Exists($path)) {
                try {
                    [IO.File]::Delete($path)
                }
                catch {
                    # Some Windows process hosts release redirected handles a
                    # few milliseconds after Process.Dispose. A tiny harness
                    # stream must not mask the proxy lifecycle result.
                    Write-Verbose "Deferred cleanup of $path."
                }
            }
        }
    }
}

function Invoke-RemotePython {
    param([Parameter(Mandatory)][string]$Source)

    $encoded = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($Source))
    $expression = "import base64;exec(base64.b64decode('$encoded'))"
    $remoteCommand = "python3 -c `"$expression`""
    $raw = & ssh.exe -o BatchMode=yes -o StrictHostKeyChecking=yes $Alias -- `
        $remoteCommand
    if ($LASTEXITCODE -ne 0) {
        throw 'Remote Python command failed.'
    }
    return $raw
}

function Test-LoopbackPort {
    param(
        [Parameter(Mandatory)][int]$Port,
        [int]$TimeoutMs = 300
    )

    $client = [Net.Sockets.TcpClient]::new()
    try {
        $pending = $client.ConnectAsync('127.0.0.1', $Port)
        return $pending.Wait($TimeoutMs) -and $client.Connected
    }
    catch {
        return $false
    }
    finally {
        $client.Dispose()
    }
}

if ($Alias -ne 'do-codex' -or $TargetId -ne 'ssh:do-codex') {
    throw 'The physical proxy E2E is restricted to the dedicated non-production do-codex target.'
}
$script:ResolvedWta = (Resolve-Path -LiteralPath $WtaExe).Path
$target = Invoke-WtaJson @('compute', 'target', 'get', $TargetId)
if ($target.disabled -or $target.health -ne 'healthy' -or
    $target.endpoint.ssh_alias -ne $Alias) {
    throw "Target $TargetId must be enabled, healthy and mapped to $Alias."
}

$runId = "proxy-e2e-$([guid]::NewGuid().ToString('N'))"
$nonce = [guid]::NewGuid().ToString('N')
$proxy = $null
$crashProxy = $null
$remote = $null
try {
    $remotePort = Get-Random -Minimum 20000 -Maximum 49997
    $remoteHttpsPort = $remotePort + 1
    $remoteWebSocketPort = $remotePort + 2
    $remoteRoot = "/tmp/wta-proxy-e2e-$runId"
    $remoteUnit = "wta-proxy-e2e-$($runId.Substring($runId.Length - 32))"
    if ($remoteRoot -notmatch '^/tmp/wta-proxy-e2e-proxy-e2e-[0-9a-f]{32}$' -or
        $remoteUnit -notmatch '^wta-proxy-e2e-[0-9a-f]{32}$') {
        throw 'Generated remote proxy fixture identity is unsafe.'
    }
    $fixtureSource = @"
import pathlib
root = pathlib.Path("$remoteRoot")
root.mkdir(mode=0o700)
(root / "index.html").write_text("$nonce", encoding="utf-8")
(root / "https_server.py").write_text(r"""\
import functools, http.server, ssl
handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory="$remoteRoot")
server = http.server.ThreadingHTTPServer(("127.0.0.1", $remoteHttpsPort), handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain("$remoteRoot/cert.pem", "$remoteRoot/key.pem")
server.socket = context.wrap_socket(server.socket, server_side=True)
server.serve_forever()
""", encoding="utf-8")
(root / "websocket_server.py").write_text(r"""\
import base64, hashlib, socket
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", $remoteWebSocketPort))
listener.listen(4)
while True:
    connection, _ = listener.accept()
    request = b""
    while b"\r\n\r\n" not in request:
        chunk = connection.recv(4096)
        if not chunk:
            break
        request += chunk
    headers = {}
    for line in request.decode("latin1").split("\r\n")[1:]:
        if ":" in line:
            key, value = line.split(":", 1)
            headers[key.strip().lower()] = value.strip()
    client_key = headers.get("sec-websocket-key", "")
    accept = base64.b64encode(hashlib.sha1((client_key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
    response = ("HTTP/1.1 101 Switching Protocols\r\n"
                "Upgrade: websocket\r\n"
                "Connection: Upgrade\r\n"
                "Sec-WebSocket-Accept: " + accept + "\r\n\r\n").encode()
    payload = "$nonce".encode()
    connection.sendall(response + bytes([0x81, len(payload)]) + payload)
    connection.close()
""", encoding="utf-8")
"@
    Invoke-RemotePython -Source $fixtureSource | Out-Null
    if ($VerifyProtocolMatrix) {
        & ssh.exe -o BatchMode=yes -o StrictHostKeyChecking=yes $Alias -- `
            openssl req -x509 -newkey rsa:2048 -nodes `
            -keyout "$remoteRoot/key.pem" -out "$remoteRoot/cert.pem" `
            -subj /CN=localhost -days 1 2>$null | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw 'Could not create the exact short-lived HTTPS test certificate.'
        }
    }
    Write-Verbose "Created remote fixture $remoteRoot."
    $service = & ssh.exe -o BatchMode=yes -o StrictHostKeyChecking=yes $Alias -- `
        systemd-run --user "--unit=$remoteUnit" --collect --no-block `
        --property=StandardInput=null --property=StandardOutput=null --property=StandardError=null `
        python3 -m http.server $remotePort --bind 127.0.0.1 --directory $remoteRoot
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not start the exact remote systemd test service.'
    }
    Write-Verbose "Requested remote service $remoteUnit on port $remotePort."
    $remote = [pscustomobject]@{
        port = $remotePort
        httpsPort = $remoteHttpsPort
        webSocketPort = $remoteWebSocketPort
        root = $remoteRoot
        units = [Collections.Generic.List[string]]@($remoteUnit)
    }
    if ($VerifyProtocolMatrix) {
        $httpsUnit = "$remoteUnit-https"
        $webSocketUnit = "$remoteUnit-ws"
        & ssh.exe -o BatchMode=yes -o StrictHostKeyChecking=yes $Alias -- `
            systemd-run --user "--unit=$httpsUnit" --collect --no-block `
            --property=StandardInput=null --property=StandardOutput=null --property=StandardError=null `
            python3 "$remoteRoot/https_server.py" 2>$null | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw 'Could not start the exact remote HTTPS test service.'
        }
        $remote.units.Add($httpsUnit)
        & ssh.exe -o BatchMode=yes -o StrictHostKeyChecking=yes $Alias -- `
            systemd-run --user "--unit=$webSocketUnit" --collect --no-block `
            --property=StandardInput=null --property=StandardOutput=null --property=StandardError=null `
            python3 "$remoteRoot/websocket_server.py" 2>$null | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw 'Could not start the exact remote WebSocket test service.'
        }
        $remote.units.Add($webSocketUnit)
    }
    $active = ''
    $deadline = [datetime]::UtcNow.AddSeconds(10)
    while ([datetime]::UtcNow -lt $deadline -and $active -ne 'active') {
        Start-Sleep -Milliseconds 100
        $active = (& ssh.exe -o BatchMode=yes -o StrictHostKeyChecking=yes $Alias -- `
            systemctl --user is-active $remoteUnit 2>$null | Out-String).Trim()
    }
    if ($active -ne 'active') {
        throw "Remote test service $remoteUnit did not become active."
    }
    $ready = $false
    $deadline = [datetime]::UtcNow.AddSeconds(10)
    $probeSource = @"
import socket
connection = socket.create_connection(("127.0.0.1", $remotePort), timeout=1)
connection.close()
"@
    while ([datetime]::UtcNow -lt $deadline -and -not $ready) {
        try {
            Invoke-RemotePython -Source $probeSource | Out-Null
            $ready = $true
        }
        catch {
            Start-Sleep -Milliseconds 100
        }
    }
    if (-not $ready) {
        throw "Remote test service $remoteUnit did not bind 127.0.0.1:$remotePort."
    }
    if ($VerifyProtocolMatrix) {
        foreach ($port in @($remote.httpsPort, $remote.webSocketPort)) {
            $matrixReady = $false
            $deadline = [datetime]::UtcNow.AddSeconds(10)
            $matrixProbe = @"
import socket
connection = socket.create_connection(("127.0.0.1", $port), timeout=1)
connection.close()
"@
            while ([datetime]::UtcNow -lt $deadline -and -not $matrixReady) {
                try {
                    Invoke-RemotePython -Source $matrixProbe | Out-Null
                    $matrixReady = $true
                }
                catch {
                    Start-Sleep -Milliseconds 100
                }
            }
            if (-not $matrixReady) {
                throw "Remote protocol-matrix service did not bind 127.0.0.1:$port."
            }
        }
    }
    Write-Verbose "Remote service $remoteUnit is active."

    $proxy = Invoke-WtaDetachedJson @(
        'compute', 'proxy', 'open',
        '--target', $TargetId,
        '--workspace', $runId,
        '--surface', 'surface-browser'
    )
    Write-Verbose "Proxy $($proxy.proxy_id) is $($proxy.state) on $($proxy.local_port)."
    if ($proxy.state -ne 'ready' -or $proxy.local_address -ne '127.0.0.1' -or
        [int]$proxy.local_port -le 0) {
        throw 'Proxy did not reach a loopback-only ready state.'
    }

    $body = & curl.exe --fail --silent --show-error --max-time 15 `
        --socks5-hostname "$($proxy.local_address):$($proxy.local_port)" `
        "http://127.0.0.1:$($remote.port)/"
    if ($LASTEXITCODE -ne 0 -or ($body -join "`n").Trim() -ne $nonce) {
        throw 'Remote localhost response did not traverse the workspace proxy.'
    }
    Write-Verbose 'Remote localhost response verified.'
    $httpsVerified = $false
    $webSocketVerified = $false
    if ($VerifyProtocolMatrix) {
        $secureBody = & curl.exe --fail --silent --show-error --insecure --max-time 15 `
            --socks5-hostname "$($proxy.local_address):$($proxy.local_port)" `
            "https://127.0.0.1:$($remote.httpsPort)/"
        if ($LASTEXITCODE -ne 0 -or ($secureBody -join "`n").Trim() -ne $nonce) {
            throw 'HTTPS response did not traverse the workspace proxy.'
        }
        $httpsVerified = $true

        $webSocketResponse = & curl.exe --silent --show-error --include --max-time 15 `
            --socks5-hostname "$($proxy.local_address):$($proxy.local_port)" `
            --header 'Connection: Upgrade' --header 'Upgrade: websocket' `
            --header 'Sec-WebSocket-Version: 13' `
            --header 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' `
            "http://127.0.0.1:$($remote.webSocketPort)/"
        $webSocketText = ($webSocketResponse -join "`n")
        if ($LASTEXITCODE -ne 0 -or
            $webSocketText -notmatch '101 Switching Protocols' -or
            $webSocketText -notmatch [regex]::Escape($nonce)) {
            throw 'WebSocket upgrade and frame did not traverse the workspace proxy.'
        }
        $webSocketVerified = $true
    }

    $closed = Invoke-WtaJson @('compute', 'proxy', 'close', $proxy.proxy_id)
    Write-Verbose "Proxy $($proxy.proxy_id) close returned $($closed.state)."
    if ($closed.state -ne 'stopped') {
        throw "Expected stopped proxy, got '$($closed.state)'."
    }
    $deleted = Invoke-WtaJson @('compute', 'proxy', 'delete', $proxy.proxy_id)
    $proxy = $null
    $crashRecoveryVerified = $false

    if ($VerifySupervisorCrash) {
        $crashProxy = Invoke-WtaDetachedJson @(
            'compute', 'proxy', 'open',
            '--target', $TargetId,
            '--workspace', "$runId-crash",
            '--surface', 'surface-crash-recovery'
        )
        if ($crashProxy.state -ne 'ready' -or -not $crashProxy.worker_pid -or
            -not $crashProxy.ssh_pid) {
            throw 'Crash recovery fixture did not publish exact worker and SSH PIDs.'
        }

        $worker = Get-Process -Id ([int]$crashProxy.worker_pid) -ErrorAction Stop
        if (-not $worker.Path -or
            [IO.Path]::GetFullPath($worker.Path) -ne [IO.Path]::GetFullPath($script:ResolvedWta)) {
            throw 'Refusing to stop a proxy worker whose executable identity is not the tested WTA binary.'
        }
        Stop-Process -Id $worker.Id -Force

        $deadline = [datetime]::UtcNow.AddSeconds(10)
        while ([datetime]::UtcNow -lt $deadline -and
            (Test-LoopbackPort -Port ([int]$crashProxy.local_port))) {
            Start-Sleep -Milliseconds 100
        }
        if (Test-LoopbackPort -Port ([int]$crashProxy.local_port)) {
            throw 'The SSH listener survived its owning WTA supervisor.'
        }
        if (Get-Process -Id ([int]$crashProxy.ssh_pid) -ErrorAction SilentlyContinue) {
            throw 'The SSH child survived closure of its owning Windows Job Object.'
        }

        $changes = @(Invoke-WtaJson @(
            'compute', 'proxy', 'reconcile', '--stale-after-ms', '0'
        ))
        $reconciled = $changes | Where-Object proxy_id -eq $crashProxy.proxy_id |
            Select-Object -First 1
        if (-not $reconciled -or $reconciled.state -ne 'failed') {
            throw 'Proxy reconciliation did not fail closed after the supervisor crash.'
        }
        Invoke-WtaJson @('compute', 'proxy', 'delete', $crashProxy.proxy_id) | Out-Null
        $crashProxy = $null
        $crashRecoveryVerified = $true
    }

    [pscustomobject]@{
        RunId = $runId
        TargetId = $TargetId
        RemotePort = [int]$remote.port
        ProxyPort = [int]$closed.local_port
        LoopbackOnly = $closed.local_address -eq '127.0.0.1'
        RemoteLocalhostVerified = $true
        HttpsVerified = $httpsVerified
        WebSocketVerified = $webSocketVerified
        LifecycleVerified = $deleted.state -eq 'stopped'
        SupervisorCrashRecoveryVerified = $crashRecoveryVerified
        Passed = $true
    }
}
finally {
    if ($proxy) {
        Write-Verbose "Finally closing proxy $($proxy.proxy_id)."
        try {
            Invoke-WtaJson @('compute', 'proxy', 'close', $proxy.proxy_id) | Out-Null
        }
        catch {
            Write-Warning "Could not close proxy $($proxy.proxy_id)."
        }
    }
    if ($crashProxy) {
        try {
            $changes = @(Invoke-WtaJson @(
                'compute', 'proxy', 'reconcile', '--stale-after-ms', '0'
            ))
            $reconciled = $changes | Where-Object proxy_id -eq $crashProxy.proxy_id |
                Select-Object -First 1
            if ($reconciled -and $reconciled.state -in @('stopped', 'failed')) {
                Invoke-WtaJson @(
                    'compute', 'proxy', 'delete', $crashProxy.proxy_id
                ) | Out-Null
            }
        }
        catch {
            Write-Warning "Could not reconcile crash proxy $($crashProxy.proxy_id)."
        }
    }
    if ($remote) {
        foreach ($unit in $remote.units) {
            Write-Verbose "Finally stopping remote service $unit."
            if ([string]$unit -match '^wta-proxy-e2e-[0-9a-f]{32}(-https|-ws)?$') {
                & ssh.exe -o BatchMode=yes -o StrictHostKeyChecking=yes $Alias -- `
                    systemctl --user stop $unit 2>$null | Out-Null
            }
        }
        $cleanupSource = @"
import shutil
root = "$([string]$remote.root)"
if root.startswith("/tmp/wta-proxy-e2e-proxy-e2e-"):
    shutil.rmtree(root, ignore_errors=True)
"@
        try {
            Invoke-RemotePython -Source $cleanupSource | Out-Null
        }
        catch {
            Write-Warning "Could not clean the exact remote proxy fixture $($remote.root)."
        }
    }
}
