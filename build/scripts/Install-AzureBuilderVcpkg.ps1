[CmdletBinding()]
param(
    [string]$InstallRoot = 'C:\Toolchains\vcpkg',
    [string]$RegistryCommit = '927f62e4b8838bd7e441e9c45103a16ffd75007e'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$root = [IO.Path]::GetFullPath($InstallRoot)
$allowedRoot = [IO.Path]::GetFullPath('C:\Toolchains')
if (-not $root.StartsWith($allowedRoot.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw "The vcpkg install root must be below $allowedRoot."
}
if ($RegistryCommit -notmatch '^[0-9a-f]{40}$') {
    throw "The vcpkg registry commit is invalid: $RegistryCommit"
}

if (Test-Path -LiteralPath $root -PathType Container) {
    $origin = (& git.exe -C $root remote get-url origin).Trim()
    if ($origin -notmatch 'github\.com[/\\]microsoft[/\\]vcpkg(?:\.git)?$') {
        throw "Unexpected vcpkg origin: $origin"
    }

    # A partial clone makes manifest version resolution perform lazy network
    # fetches from several vcpkg workers. On constrained Windows builders those
    # concurrent Git/DNS helpers can fail to start, and an older release tag
    # alone may not contain a project's newer builtin-baseline. This directory
    # is a tool cache created by this script, so replace only this exact,
    # validated path with a complete registry clone.
    $promisor = (& git.exe -C $root config --get remote.origin.promisor 2>$null)
    if ($promisor -eq 'true') {
        Remove-Item -LiteralPath $root -Recurse -Force
    }
}

if (-not (Test-Path -LiteralPath $root -PathType Container)) {
    & git.exe clone https://github.com/microsoft/vcpkg.git $root
    if ($LASTEXITCODE -ne 0) {
        throw 'vcpkg full clone failed.'
    }
}

& git.exe -C $root fetch --tags --force origin
if ($LASTEXITCODE -ne 0) {
    throw 'Could not refresh the vcpkg registry.'
}
& git.exe -C $root checkout --detach $RegistryCommit
if ($LASTEXITCODE -ne 0) {
    throw "Could not check out vcpkg registry commit $RegistryCommit."
}

& (Join-Path $root 'bootstrap-vcpkg.bat') -disableMetrics
if ($LASTEXITCODE -ne 0) {
    throw 'vcpkg bootstrap failed.'
}

$version = (& (Join-Path $root 'vcpkg.exe') version | Out-String).Trim()
$commit = (& git.exe -C $root rev-parse HEAD).Trim()
[pscustomobject]@{
    root = $root
    registryCommit = $RegistryCommit
    commit = $commit
    version = $version
} | ConvertTo-Json -Compress
