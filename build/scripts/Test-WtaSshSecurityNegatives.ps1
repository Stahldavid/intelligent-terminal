[CmdletBinding()]
param(
    [string]$Alias = 'do-codex'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($Alias -ne 'do-codex') {
    throw "Security-negative SSH tests are restricted to the dedicated non-production alias 'do-codex'."
}

$tempRoot = Join-Path ([IO.Path]::GetTempPath()) "wta-ssh-negative-$([guid]::NewGuid().ToString('N'))"
[IO.Directory]::CreateDirectory($tempRoot) | Out-Null

try {
    $effective = & ssh.exe -G $Alias
    if ($LASTEXITCODE -ne 0) {
        throw "OpenSSH could not resolve $Alias."
    }
    $user = (($effective | Where-Object { $_ -match '^user\s+' } | Select-Object -First 1) -replace '^user\s+', '').Trim()
    $hostKeyAlias = (($effective | Where-Object { $_ -match '^hostkeyalias\s+' } | Select-Object -First 1) -replace '^hostkeyalias\s+', '').Trim()
    if ($user -ne 'codex-agent') {
        throw "Alias $Alias must use the dedicated codex-agent account, not '$user'."
    }
    if ([string]::IsNullOrWhiteSpace($hostKeyAlias) -or $hostKeyAlias -eq 'none') {
        $hostKeyAlias = (($effective | Where-Object { $_ -match '^hostname\s+' } | Select-Object -First 1) -replace '^hostname\s+', '').Trim()
    }

    $wrongKey = Join-Path $tempRoot 'wrong-host-key'
    $keyInfo = [Diagnostics.ProcessStartInfo]::new()
    $keyInfo.FileName = (Get-Command ssh-keygen.exe -ErrorAction Stop).Source
    $keyInfo.UseShellExecute = $false
    foreach ($argument in @('-q', '-t', 'ed25519', '-N', '', '-f', $wrongKey)) {
        $keyInfo.ArgumentList.Add($argument)
    }
    $keyProcess = [Diagnostics.Process]::Start($keyInfo)
    $keyProcess.WaitForExit()
    if ($keyProcess.ExitCode -ne 0) {
        throw 'Could not generate the ephemeral wrong host-key fixture.'
    }
    $publicFields = (Get-Content -LiteralPath "$wrongKey.pub" -Raw).Trim().Split(' ')
    if ($publicFields.Count -lt 2) {
        throw 'The ephemeral public-key fixture is invalid.'
    }
    $isolatedKnownHosts = Join-Path $tempRoot 'known_hosts'
    [IO.File]::WriteAllText(
        $isolatedKnownHosts,
        "$hostKeyAlias $($publicFields[0]) $($publicFields[1])`n",
        [Text.UTF8Encoding]::new($false))

    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = (Get-Command ssh.exe -ErrorAction Stop).Source
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($argument in @(
        '-o', 'BatchMode=yes',
        '-o', 'ConnectTimeout=10',
        '-o', 'StrictHostKeyChecking=yes',
        '-o', 'UpdateHostKeys=no',
        '-o', "UserKnownHostsFile=$isolatedKnownHosts",
        $Alias, '--', 'true'
    )) {
        $info.ArgumentList.Add($argument)
    }
    $process = [Diagnostics.Process]::Start($info)
    $stdout = $process.StandardOutput.ReadToEnd()
    $stderr = $process.StandardError.ReadToEnd()
    $process.WaitForExit()
    if ($process.ExitCode -eq 0) {
        throw 'SSH accepted an intentionally incorrect isolated host key.'
    }
    if ($stderr -notmatch '(?i)(host identification has changed|host key verification failed|offending .* key)') {
        throw "SSH failed for an unexpected reason instead of host-key mismatch: $stderr"
    }

    $output = & cargo test --manifest-path tools/wta/Cargo.toml --lib `
        compute::ssh::tests::wildcard_and_option_like_aliases_are_rejected `
        -- --exact 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "The deterministic SSH alias rejection test failed: $output"
    }

    [pscustomobject]@{
        Alias = $Alias
        User = $user
        HostKeyAlias = $hostKeyAlias
        RealKnownHostsUntouched = $true
        WrongHostKeyRejected = $true
        OptionInjectionRejected = $true
        WildcardsRejected = $true
    }
}
finally {
    if (Test-Path -LiteralPath $tempRoot) {
        Remove-Item -LiteralPath $tempRoot -Recurse -Force
    }
}
