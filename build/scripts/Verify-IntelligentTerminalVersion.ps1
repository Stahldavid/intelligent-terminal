[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$cargo = Get-Content (Join-Path $repo 'tools\wta\Cargo.toml') -Raw
$cargoVersion = [regex]::Match($cargo, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
if (-not $cargoVersion) {
    throw 'Could not read tools/wta/Cargo.toml package version.'
}

$manifestPaths = @(
    'src\cascadia\CascadiaPackage\Package.appxmanifest',
    'src\cascadia\CascadiaPackage\Package-Dev.appxmanifest',
    'src\cascadia\CascadiaPackage\Package-Pre.appxmanifest',
    'src\cascadia\CascadiaPackage\Package-Can.appxmanifest'
)
$expectedAppxVersion = "$cargoVersion.12"
foreach ($relative in $manifestPaths) {
    $path = Join-Path $repo $relative
    $manifest = [xml](Get-Content $path -Raw)
    $actual = $manifest.Package.Identity.Version
    if ($actual -ne $expectedAppxVersion) {
        throw "$relative has version $actual; expected $expectedAppxVersion."
    }

    $wtaApplications = @($manifest.SelectNodes(
        "//*[local-name()='Application' and @Id='Wta' and @Executable='wta.exe']"
    ))
    $wtaAliases = @($manifest.SelectNodes(
        "//*[local-name()='Application' and @Id='Wta']" +
        "//*[local-name()='ExecutionAlias' and @Alias='wta.exe']"
    ))
    if ($wtaApplications.Count -ne 1 -or $wtaAliases.Count -ne 1) {
        throw "$relative must expose exactly one wta.exe app execution alias."
    }
}

$server = Get-Content (Join-Path $repo 'src\cascadia\WindowsTerminal\TerminalProtocolComServer.cpp') -Raw
$componentVersion = [regex]::Match($server, 'v\["component_version"\]\s*=\s*"([^"]+)"').Groups[1].Value
if ($componentVersion -ne $cargoVersion) {
    throw "Terminal protocol component version $componentVersion does not match WTA $cargoVersion."
}

$serverProtocol = [regex]::Match($server, 'v\["protocol_version"\]\s*=\s*"([^"]+)"').Groups[1].Value
$client = Get-Content (Join-Path $repo 'src\tools\wtcli\main.cpp') -Raw
$clientProtocol = [regex]::Match(
    $client,
    '(?:Required|Minimum)ProtocolVersion\{\s*"([^"]+)"\s*\}'
).Groups[1].Value
if (-not $serverProtocol -or $serverProtocol -ne $clientProtocol) {
    throw "Protocol mismatch: Terminal=$serverProtocol wtcli=$clientProtocol."
}

$protocolManifestPath = Join-Path $repo 'src\cascadia\TerminalProtocol\protocol-version.json'
if (-not (Test-Path $protocolManifestPath -PathType Leaf)) {
    throw 'Terminal protocol artifact manifest is missing.'
}
$protocolManifest = Get-Content $protocolManifestPath -Raw | ConvertFrom-Json
if ($protocolManifest.protocolVersion -ne $serverProtocol) {
    throw "Protocol artifact manifest version $($protocolManifest.protocolVersion) does not match Terminal $serverProtocol."
}
if ($protocolManifest.componentVersion -ne $cargoVersion) {
    throw "Protocol artifact manifest component $($protocolManifest.componentVersion) does not match WTA $cargoVersion."
}

$packageProject = Get-Content (Join-Path $repo 'src\cascadia\CascadiaPackage\CascadiaPackage.wapproj') -Raw
if ($packageProject -notmatch 'TerminalProtocol\\protocol-version\.json' -or
    $packageProject -notmatch '<Link>protocol-version\.json</Link>') {
    throw 'CascadiaPackage must ship protocol-version.json beside the executable components.'
}

$localInstaller = Get-Content (Join-Path $repo 'build\scripts\New-WtaLocalInstaller.ps1') -Raw
if ($localInstaller -match 'Copy-Item\s+-Path\s+\$wtcliSource') {
    throw 'Local installer must not overwrite the MSIX-built wtcli.exe with an independently-built binary.'
}
foreach ($requiredInvariant in 'Assert-PayloadProtocolManifest', 'Building Terminal package by default') {
    if ($localInstaller -notmatch [regex]::Escape($requiredInvariant)) {
        throw "Local installer is missing invariant: $requiredInvariant."
    }
}

$protocolIdl = Get-Content (Join-Path $repo 'src\cascadia\TerminalProtocol\TerminalProtocol.idl') -Raw
foreach ($field in 'SurfaceId', 'SurfaceIndex', 'SurfaceCount') {
    if ($protocolIdl -notmatch "\b$field\b") {
        throw "Terminal protocol PaneInfo is missing $field."
    }
}
foreach ($field in 'surface_id', 'surface_index', 'surface_count') {
    if ($server -notmatch [regex]::Escape("v[`"$field`"]")) {
        throw "Terminal protocol JSON is missing $field."
    }
}

[pscustomobject]@{
    ProductVersion = $cargoVersion
    AppxVersion = $expectedAppxVersion
    ProtocolVersion = $serverProtocol
    ArtifactManifest = 'ok'
    InstallerAssembly = 'atomic'
    Status = 'ok'
} | ConvertTo-Json
