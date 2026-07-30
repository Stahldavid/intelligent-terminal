[CmdletBinding()]
param(
    [string]$Distro = 'Ubuntu-22.04',
    [string]$WtaNodeLinux = (Join-Path $PSScriptRoot '..\..\tools\wta\remote\linux-x64\wta-node')
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

function Convert-ToBashLiteral {
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Value)

    "'" + $Value.Replace("'", "'`"`'`"`'") + "'"
}

if (-not (Test-Path -LiteralPath $WtaNodeLinux -PathType Leaf)) {
    throw "Linux wta-node is missing: $WtaNodeLinux"
}

$node = Convert-ToWslPath $WtaNodeLinux
$session = 'pty-e2e-' + [Guid]::NewGuid().ToString('N').Substring(0, 16)
$nodeLiteral = Convert-ToBashLiteral $node
$sessionLiteral = Convert-ToBashLiteral $session
$script = @"
set -euo pipefail
node=$nodeLiteral
session=$sessionLiteral
output="`$(mktemp)"
cleanup() {
  "`$node" pty stop --session "`$session" >/dev/null 2>&1 || true
  rm -f -- "`$output"
}
trap cleanup EXIT

set +e
(printf 'persistent-pty-probe\n'; sleep 1) |
  timeout 2s "`$node" pty start --session "`$session" --cols 100 --rows 40 -- /bin/cat \
  >"`$output"
code=`$?
set -e
if [ "`$code" -ne 0 ] && [ "`$code" -ne 124 ]; then
  cat "`$output" >&2
  exit "`$code"
fi
grep -q 'persistent-pty-probe' "`$output"

status="`$(`$node pty status --session "`$session")"
printf '%s' "`$status" | grep -q '"state": "detached"'
pid="`$(printf '%s' "`$status" | sed -n 's/.*"pid": \([0-9][0-9]*\).*/\1/p')"
test -n "`$pid"
kill -0 "`$pid"

listed="`$(`$node pty list)"
printf '%s' "`$listed" | grep -q "`$session"

printf '{"session_id":"%s","pid":%s,"state":"detached","reattachable":true}\n' \
  "`$session" "`$pid"
"@

$encoded = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($script))
$result = & wsl.exe -d $Distro -- bash -lc "printf %s $encoded | base64 -d | bash"
if ($LASTEXITCODE -ne 0) {
    throw "Persistent PTY E2E failed with exit code $LASTEXITCODE."
}

$parsed = $result | ConvertFrom-Json
if (-not $parsed.reattachable -or $parsed.state -ne 'detached') {
    throw "Persistent PTY E2E returned an invalid result: $result"
}

Write-Host "[wta-node-pty] PASS session=$($parsed.session_id) pid=$($parsed.pid)"
$parsed
