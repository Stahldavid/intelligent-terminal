param(
    [string]$Distro = "Ubuntu-22.04",
    [string]$NodeBinary = "tools/wta/remote/linux-x64/wta-node"
)

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$nodePath = (Resolve-Path (Join-Path $repoRoot $NodeBinary)).Path
$nodeWsl = (& wsl.exe -d $Distro -- wslpath -a ($nodePath -replace '\\', '/')).Trim()
if (-not $nodeWsl) {
    throw "Could not resolve the Linux node artifact inside WSL."
}

$session = "transfer-e2e-$([guid]::NewGuid().ToString('N'))"
$payload = "verified-file-transfer-$session"
$tempRoot = Join-Path $env:TEMP $session
New-Item -ItemType Directory -Force -Path $tempRoot | Out-Null
$source = Join-Path $tempRoot "payload.txt"
[System.IO.File]::WriteAllText($source, $payload, [System.Text.UTF8Encoding]::new($false))
$digest = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash.ToLowerInvariant()
$size = (Get-Item -LiteralPath $source).Length

try {
    $preparedJson = & wsl.exe -d $Distro -- $nodeWsl file prepare-upload `
        --transfer $session `
        --name payload.txt `
        --size $size `
        --sha256 $digest
    if ($LASTEXITCODE -ne 0) {
        throw "prepare-upload failed."
    }
    $prepared = $preparedJson | ConvertFrom-Json
    $sourceWsl = (& wsl.exe -d $Distro -- wslpath -a ($source -replace '\\', '/')).Trim()
    & wsl.exe -d $Distro -- cp -- $sourceWsl $prepared.incoming_path
    if ($LASTEXITCODE -ne 0) {
        throw "Could not stage the upload payload."
    }

    $committedJson = & wsl.exe -d $Distro -- $nodeWsl file commit-upload --transfer $session
    if ($LASTEXITCODE -ne 0) {
        throw "commit-upload failed."
    }
    $committed = $committedJson | ConvertFrom-Json
    if ($committed.state -ne "succeeded") {
        throw "Unexpected transfer state: $($committed.state)"
    }
    $remoteDigest = (& wsl.exe -d $Distro -- sha256sum -- $committed.final_path).Split()[0]
    if ($remoteDigest -ne $digest) {
        throw "Committed payload digest mismatch."
    }

    Write-Host "[wta-node-transfer] PASS transfer=$session sha256=$digest"
}
finally {
    & wsl.exe -d $Distro -- $nodeWsl file abort-upload --transfer $session 2>$null | Out-Null
    Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}
