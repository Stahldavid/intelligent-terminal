[CmdletBinding()]
param(
    [ValidateSet('ARM64', 'x64', 'x86')]
    [string]$Platform = 'ARM64',

    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Debug',

    [string]$Destination = (Join-Path $PSScriptRoot '..\..\artifacts\local-installer'),

    [string]$TerminalMsix,

    [string]$XamlAppx,

    [switch]$BuildTerminal,

    [switch]$AllowPrebuiltTerminal,

    [switch]$SkipWtaBuild,

    [string]$WtaExePath
)

$ErrorActionPreference = 'Stop'

function Write-Status {
    param([string]$Message)

    Write-Host "[local-installer] $Message"
}

function Ensure-Directory {
    param([string]$Path)

    if (-not (Test-Path $Path -PathType Container)) {
        New-Item -ItemType Directory -Path $Path | Out-Null
    }
}

function Resolve-AbsolutePath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,

        [string]$BasePath = (Get-Location).Path
    )

    if ([System.IO.Path]::IsPathRooted($Path)) {
        return [System.IO.Path]::GetFullPath($Path)
    }

    return [System.IO.Path]::GetFullPath((Join-Path $BasePath $Path))
}

function Assert-PayloadProtocolManifest {
    param(
        [Parameter(Mandatory = $true)]
        [string]$PayloadRoot,

        [Parameter(Mandatory = $true)]
        [string]$SourceManifestPath
    )

    $payloadManifestPath = Join-Path $PayloadRoot 'protocol-version.json'
    if (-not (Test-Path $payloadManifestPath -PathType Leaf)) {
        throw "Terminal payload has no protocol-version.json. Refusing to combine a legacy or stale MSIX with current agent components."
    }

    $sourceManifest = Get-Content $SourceManifestPath -Raw | ConvertFrom-Json
    $payloadManifest = Get-Content $payloadManifestPath -Raw | ConvertFrom-Json
    foreach ($field in 'protocolVersion', 'componentVersion') {
        if (-not $payloadManifest.$field -or $payloadManifest.$field -ne $sourceManifest.$field) {
            throw "Terminal payload $field '$($payloadManifest.$field)' does not match source '$($sourceManifest.$field)'. Rebuild the Terminal package."
        }
    }

    foreach ($component in 'WindowsTerminal.exe', 'wtcli.exe') {
        $componentPath = Join-Path $PayloadRoot $component
        if (-not (Test-Path $componentPath -PathType Leaf)) {
            throw "Terminal payload is missing $component. The package must carry its own matching protocol client and server."
        }
    }

    return $payloadManifest
}

function Find-DumpbinPath {
    $command = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere -PathType Leaf)) {
        throw 'Could not find dumpbin.exe or vswhere.exe to verify native payload dependencies.'
    }

    $installationPath = (& $vswhere -latest -products * -property installationPath).Trim()
    if (-not $installationPath) {
        throw 'No Visual Studio installation was found while resolving dumpbin.exe.'
    }

    $candidate = Get-ChildItem (Join-Path $installationPath 'VC\Tools\MSVC') -Recurse -Filter dumpbin.exe |
        Where-Object { $_.FullName -match '\\bin\\Hostx64\\x64\\dumpbin\.exe$' } |
        Sort-Object FullName -Descending |
        Select-Object -First 1
    if (-not $candidate) {
        throw "Could not find x64 dumpbin.exe under $installationPath."
    }
    return $candidate.FullName
}

function Assert-PayloadNativeRuntime {
    param(
        [Parameter(Mandatory = $true)]
        [string]$PayloadRoot
    )

    $terminalApp = Join-Path $PayloadRoot 'TerminalApp.dll'
    if (-not (Test-Path $terminalApp -PathType Leaf)) {
        throw "Terminal payload is missing $terminalApp."
    }

    $dumpbin = Find-DumpbinPath
    $dependencies = (& $dumpbin /nologo /dependents $terminalApp 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) {
        throw "dumpbin failed while verifying native dependencies for $terminalApp.`n$dependencies"
    }

    if ($dependencies -match '(?im)^\s*WebView2Loader\.dll\s*$') {
        throw @"
TerminalApp.dll imports WebView2Loader.dll. Intelligent Terminal requires the
official WebView2 static-loader mode so the MSIX, portable ZIP and bootstrap
installer cannot drift into a startup-crashing runtime layout.
"@
    }
}

function Get-RustTarget {
    param([string]$PlatformName)

    switch ($PlatformName) {
        'ARM64' { return 'aarch64-pc-windows-msvc' }
        'x64' { return 'x86_64-pc-windows-msvc' }
        'x86' { return 'i686-pc-windows-msvc' }
        default { throw "Unsupported platform: $PlatformName" }
    }
}

function Get-XamlDependencyArch {
    param([string]$PlatformName)

    switch ($PlatformName) {
        'ARM64' { return 'arm64' }
        'x64' { return 'x64' }
        'x86' { return 'x86' }
        default { throw "Unsupported platform: $PlatformName" }
    }
}

function Find-CargoPath {
    $command = Get-Command cargo.exe -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    $fallback = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    if (Test-Path $fallback -PathType Leaf) {
        return $fallback
    }

    throw 'Could not find cargo.exe. Install Rust or add cargo.exe to PATH.'
}

function Get-InstalledRustTargets {
    $rustupPath = Join-Path $env:USERPROFILE '.cargo\bin\rustup.exe'
    if (-not (Test-Path $rustupPath -PathType Leaf)) {
        return @()
    }

    $targets = & $rustupPath target list --installed
    if ($LASTEXITCODE -ne 0) {
        throw 'rustup target list --installed failed.'
    }

    return @($targets | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
}

function Invoke-RustBuild {
    param(
        [Parameter(Mandatory = $true)]
        [string]$CargoPath,

        [Parameter(Mandatory = $true)]
        [string]$ManifestPath,

        [Parameter(Mandatory = $true)]
        [string]$RustTarget,

        [Parameter(Mandatory = $true)]
        [string]$RepoRoot
    )

    # Cargo's config discovery walks up from the current working directory,
    # not from the manifest path. Pin CWD to the repo root so Cargo finds the
    # repo-root .cargo/config.toml that supplies `+crt-static` — even when
    # this script is launched from outside the repo.
    #
    # Important: do NOT push into the manifest's directory. tools/wta/ has its
    # own rust-toolchain.toml, so letting rustup discover that file from CWD
    # can change toolchain resolution compared to the repo-root configuration
    # this script relies on for local builds.
    Push-Location $RepoRoot
    try {
        # Bound linker/codegen pressure so packaging remains usable on a
        # developer workstation while still allowing two independent crates
        # to make progress.
        & $CargoPath build --manifest-path $ManifestPath --release --target $RustTarget -j 2
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed for $ManifestPath."
        }
    }
    finally {
        Pop-Location
    }
}

function New-SelfExtractingInstaller {
    param(
        [Parameter(Mandatory = $true)]
        [string]$BootstrapExe,

        [Parameter(Mandatory = $true)]
        [string]$PayloadRoot,

        [Parameter(Mandatory = $true)]
        [string]$OutputPath
    )

    $bundleFiles = @(
        'install.cmd',
        'install-local-terminal.ps1',
        'ComProxyRegistration.ps1',
        'payload.zip'
    )
    $footerMagic = [System.Text.Encoding]::ASCII.GetBytes('WTA-INSTALLER-V1')

    Copy-Item -Path $BootstrapExe -Destination $OutputPath -Force

    $outputStream = [System.IO.File]::Open($OutputPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::Read)
    try {
        $outputStream.Seek(0, [System.IO.SeekOrigin]::End) | Out-Null
        $manifestEntries = New-Object System.Collections.Generic.List[string]

        foreach ($fileName in $bundleFiles) {
            $sourcePath = Join-Path $PayloadRoot $fileName
            if (-not (Test-Path $sourcePath -PathType Leaf)) {
                throw "Installer bundle input not found: $sourcePath"
            }

            $offset = [UInt64]$outputStream.Position
            $inputStream = [System.IO.File]::OpenRead($sourcePath)
            try {
                $inputStream.CopyTo($outputStream)
            }
            finally {
                $inputStream.Dispose()
            }

            $length = [UInt64]($outputStream.Position - [Int64]$offset)
            $manifestEntries.Add(("file|{0}|{1}|{2}" -f $fileName, $offset, $length))
        }

        $manifestText = ($manifestEntries -join "`n") + "`n"
        $manifestBytes = [System.Text.Encoding]::UTF8.GetBytes($manifestText)
        $outputStream.Write($manifestBytes, 0, $manifestBytes.Length)

        $manifestLengthBytes = [BitConverter]::GetBytes([UInt64]$manifestBytes.Length)
        $outputStream.Write($footerMagic, 0, $footerMagic.Length)
        $outputStream.Write($manifestLengthBytes, 0, $manifestLengthBytes.Length)
        $outputStream.Flush()
    }
    finally {
        $outputStream.Dispose()
    }
}

function Find-TerminalMsix {
    param(
        [Parameter(Mandatory = $true)]
        [string]$AppPackagesRoot,

        [Parameter(Mandatory = $true)]
        [string]$PlatformName,

        [Parameter(Mandatory = $true)]
        [string]$ConfigurationName
    )

    $patterns = @()
    if ($ConfigurationName -eq 'Release') {
        $patterns += "CascadiaPackage_.*_{0}\.(msix|appx)$" -f $PlatformName
    }
    $patterns += "CascadiaPackage_.*_{0}_{1}\.(msix|appx)$" -f $PlatformName, $ConfigurationName

    $candidate = Get-ChildItem -Path $AppPackagesRoot -Recurse -File |
        Where-Object {
            if ($_.FullName -match '\\Dependencies\\') {
                return $false
            }

            foreach ($pattern in $patterns) {
                if ($_.Name -match $pattern) {
                    return $true
                }
            }

            return $false
        } |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1

    if (-not $candidate) {
        throw "Could not find a Cascadia package for $PlatformName/$ConfigurationName under $AppPackagesRoot."
    }

    return $candidate.FullName
}

function Find-XamlAppx {
    param(
        [Parameter(Mandatory = $true)]
        [string]$TerminalPackagePath,

        [Parameter(Mandatory = $true)]
        [string]$PlatformName
    )

    $dependencyArch = Get-XamlDependencyArch -PlatformName $PlatformName
    $dependencyRoot = Join-Path (Split-Path $TerminalPackagePath -Parent) ("Dependencies\{0}" -f $dependencyArch)

    if (-not (Test-Path $dependencyRoot -PathType Container)) {
        throw "Could not find the dependency folder $dependencyRoot."
    }

    $candidate = Get-ChildItem -Path $dependencyRoot -File -Filter 'Microsoft.UI.Xaml*.appx' |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1

    if (-not $candidate) {
        throw "Could not find a Microsoft.UI.Xaml dependency package under $dependencyRoot."
    }

    return $candidate.FullName
}

function Get-AppxPackageIdentity {
    param(
        [Parameter(Mandatory = $true)]
        [string]$PackagePath
    )

    Add-Type -AssemblyName System.IO.Compression.FileSystem

    $archive = [System.IO.Compression.ZipFile]::OpenRead($PackagePath)
    try {
        $manifestEntry = $archive.GetEntry('AppxManifest.xml')
        if (-not $manifestEntry) {
            throw "Could not find AppxManifest.xml inside $PackagePath."
        }

        $reader = New-Object System.IO.StreamReader($manifestEntry.Open())
        try {
            $manifestContent = $reader.ReadToEnd()
        }
        finally {
            $reader.Dispose()
        }
    }
    finally {
        $archive.Dispose()
    }

    [xml]$manifestXml = $manifestContent
    $namespaceManager = New-Object System.Xml.XmlNamespaceManager($manifestXml.NameTable)
    $namespaceManager.AddNamespace('appx', 'http://schemas.microsoft.com/appx/manifest/foundation/windows10')
    $identityNode = $manifestXml.SelectSingleNode('/appx:Package/appx:Identity', $namespaceManager)
    if (-not $identityNode) {
        throw "Could not find the Identity node in AppxManifest.xml from $PackagePath."
    }

    return [pscustomobject]@{
        Name = $identityNode.GetAttribute('Name')
        Version = $identityNode.GetAttribute('Version')
        Publisher = $identityNode.GetAttribute('Publisher')
        ProcessorArchitecture = $identityNode.GetAttribute('ProcessorArchitecture')
    }
}

function Get-AppxManifestIdentity {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ManifestPath
    )

    [xml]$manifestXml = Get-Content -Path $ManifestPath -Raw
    $namespaceManager = New-Object System.Xml.XmlNamespaceManager($manifestXml.NameTable)
    $namespaceManager.AddNamespace('appx', 'http://schemas.microsoft.com/appx/manifest/foundation/windows10')
    $identityNode = $manifestXml.SelectSingleNode('/appx:Package/appx:Identity', $namespaceManager)
    if (-not $identityNode) {
        throw "Could not find the Identity node in $ManifestPath."
    }

    return [pscustomobject]@{
        Name = $identityNode.GetAttribute('Name')
        Version = $identityNode.GetAttribute('Version')
        Publisher = $identityNode.GetAttribute('Publisher')
        # The source manifest intentionally omits this attribute; MSBuild
        # materializes it in the built package. GetAttribute returns an empty
        # string instead of violating StrictMode on the source XML node.
        ProcessorArchitecture = $identityNode.GetAttribute('ProcessorArchitecture')
    }
}

function Get-SingleChildDirectoryOrSelf {
    param([string]$RootPath)

    $children = @(Get-ChildItem -Path $RootPath -Force)
    if ($children.Count -eq 1 -and $children[0].PSIsContainer) {
        return $children[0].FullName
    }

    return $RootPath
}

function Build-TerminalPackage {
    param(
        [Parameter(Mandatory = $true)]
        [string]$RepoRoot,

        [Parameter(Mandatory = $true)]
        [string]$PlatformName,

        [Parameter(Mandatory = $true)]
        [string]$ConfigurationName
    )

    $openConsoleModule = Join-Path $RepoRoot 'tools\OpenConsole.psm1'
    $solutionPath = Join-Path $RepoRoot 'OpenConsole.slnx'
    $packagesConfig = Join-Path $RepoRoot 'dep\nuget\packages.config'
    $packagesDirectory = Join-Path $RepoRoot 'packages'
    $nugetConfig = Join-Path $RepoRoot 'NuGet.Config'
    $packageProject = Join-Path $RepoRoot 'src\cascadia\CascadiaPackage\CascadiaPackage.wapproj'

    Write-Status "Building CascadiaPackage for $PlatformName/$ConfigurationName ..."

    # Enter-VsDevShell initializes Visual Studio's bundled VCPKG_ROOT. Preserve
    # an explicitly selected standalone tool across that environment import so
    # CI can use a release that understands the selected VS/toolset.
    $requestedVcpkgRoot = $env:VCPKG_ROOT
    $requestedVcpkgDisableMetrics = $env:VCPKG_DISABLE_METRICS
    Import-Module $openConsoleModule -Force
    Set-MsbuildDevEnvironment
    if (-not [string]::IsNullOrWhiteSpace($requestedVcpkgRoot)) {
        $env:VCPKG_ROOT = $requestedVcpkgRoot
    }
    if (-not [string]::IsNullOrWhiteSpace($requestedVcpkgDisableMetrics)) {
        $env:VCPKG_DISABLE_METRICS = $requestedVcpkgDisableMetrics
    }

    & "$RepoRoot\dep\nuget\nuget.exe" restore $solutionPath
    if ($LASTEXITCODE -ne 0) {
        throw "NuGet restore failed for $solutionPath."
    }

    & "$RepoRoot\dep\nuget\nuget.exe" restore $packagesConfig `
        -PackagesDirectory $packagesDirectory `
        -ConfigFile $nugetConfig `
        -NonInteractive
    if ($LASTEXITCODE -ne 0) {
        throw "NuGet restore failed for $packagesConfig."
    }

    # Release packages must use the repository's canonical drivers. They
    # serialize the large C++/WinRT PCH builds, prebuild generated-header and
    # WinMD producers, and refresh glob-based package inputs. Keeping those
    # invariants in one place avoids a second, subtly different package graph
    # in this installer script.
    $buildDriver = switch ("$PlatformName/$ConfigurationName") {
        'x64/Release' { Join-Path $RepoRoot '_build_msix_x64.cmd' }
        'ARM64/Release' { Join-Path $RepoRoot '_build_msix_arm64.cmd' }
        default { $null }
    }

    if ($buildDriver) {
        & "$env:SystemRoot\System32\cmd.exe" /d /c $buildDriver
        if ($LASTEXITCODE -ne 0) {
            throw "Terminal package driver failed: $buildDriver."
        }
        return
    }

    # Debug and x86 remain supported for development. Keep their fallback
    # deterministic and memory-bounded as well.
    & msbuild.exe $packageProject `
        "/p:Platform=$PlatformName" `
        "/p:Configuration=$ConfigurationName" `
        "/p:SolutionDir=$RepoRoot\" `
        "/p:OpenConsoleDir=$RepoRoot\" `
        '/p:WindowsTerminalBranding=Dev' `
        '/p:GenerateAppxPackageOnBuild=true' `
        '/p:AppxBundle=Never' `
        '/p:MultiProcessorCompilation=false' `
        '/p:CL_MPCount=1' `
        '/m:1' `
        '/nodeReuse:false' `
        /nologo
    if ($LASTEXITCODE -ne 0) {
        throw "msbuild failed for $packageProject."
    }
}

$repoRoot = Resolve-AbsolutePath -Path (Join-Path $PSScriptRoot '..\..')
$destinationRoot = Resolve-AbsolutePath -Path $Destination
$appPackagesRoot = Join-Path $repoRoot 'src\cascadia\CascadiaPackage\AppPackages'
$unpackagedScript = Join-Path $repoRoot 'build\scripts\New-UnpackagedTerminalDistribution.ps1'
$installerScript = Join-Path $repoRoot 'installer\install-local-terminal.ps1'
$uninstallerScript = Join-Path $repoRoot 'installer\uninstall-local-terminal.ps1'
$comRegistrationScript = Join-Path $repoRoot 'installer\ComProxyRegistration.ps1'
$installerCmd = Join-Path $repoRoot 'installer\install.cmd'
$installerBootstrapManifest = Join-Path $repoRoot 'installer\bootstrap\Cargo.toml'
$plannerPromptTemplate = Join-Path $repoRoot 'tools\wta\prompts\terminal-agent.md'
$devManifestPath = Join-Path $repoRoot 'src\cascadia\CascadiaPackage\Package-Dev.appxmanifest'
$protocolManifestPath = Join-Path $repoRoot 'src\cascadia\TerminalProtocol\protocol-version.json'

if (-not (Test-Path $unpackagedScript -PathType Leaf)) {
    throw "Could not find $unpackagedScript."
}
if (-not (Test-Path $installerScript -PathType Leaf)) {
    throw "Could not find $installerScript."
}
if (-not (Test-Path $uninstallerScript -PathType Leaf)) {
    throw "Could not find $uninstallerScript."
}
if (-not (Test-Path $comRegistrationScript -PathType Leaf)) {
    throw "Could not find $comRegistrationScript."
}
if (-not (Test-Path $installerCmd -PathType Leaf)) {
    throw "Could not find $installerCmd."
}
if (-not (Test-Path $installerBootstrapManifest -PathType Leaf)) {
    throw "Could not find $installerBootstrapManifest."
}
if (-not (Test-Path $plannerPromptTemplate -PathType Leaf)) {
    throw "Could not find $plannerPromptTemplate."
}
if (-not (Test-Path $devManifestPath -PathType Leaf)) {
    throw "Could not find $devManifestPath."
}
if (-not (Test-Path $protocolManifestPath -PathType Leaf)) {
    throw "Could not find $protocolManifestPath."
}

$expectedManifestIdentity = Get-AppxManifestIdentity -ManifestPath $devManifestPath

Ensure-Directory -Path $destinationRoot

$usingExplicitPrebuiltTerminal = -not [string]::IsNullOrWhiteSpace($TerminalMsix)
if (-not $BuildTerminal -and -not $usingExplicitPrebuiltTerminal) {
    Write-Status "Building Terminal package by default so protocol components come from one source state ..."
    $BuildTerminal = $true
}
if ($BuildTerminal -and $usingExplicitPrebuiltTerminal) {
    throw 'Choose either -BuildTerminal or -TerminalMsix, not both.'
}
if ($usingExplicitPrebuiltTerminal -and -not $AllowPrebuiltTerminal) {
    throw 'Using a prebuilt Terminal package requires -AllowPrebuiltTerminal. Its packaged protocol manifest will still be verified.'
}

$cargoPath = Find-CargoPath
$rustTarget = Get-RustTarget -PlatformName $Platform
$installedTargets = @(Get-InstalledRustTargets)

if ($installedTargets.Count -gt 0 -and $installedTargets -notcontains $rustTarget) {
    throw "Rust target $rustTarget is not installed. Install it with rustup target add $rustTarget."
}

# CascadiaPackage intentionally fail-closes unless the exact architecture and
# Cargo profile binaries already exist. Build (or resolve) WTA before invoking
# the package graph so the MSIX and the final bootstrap installer consume the
# same wta.exe/wta-node.exe pair.
if ($SkipWtaBuild) {
    if (-not $WtaExePath) {
        throw 'Use -WtaExePath when -SkipWtaBuild is set.'
    }
    $resolvedWtaExePath = Resolve-AbsolutePath -Path $WtaExePath
} else {
    Write-Status "Building wta.exe for $rustTarget with a static CRT ..."
    $manifestPath = Join-Path $repoRoot 'tools\wta\Cargo.toml'
    Invoke-RustBuild -CargoPath $cargoPath -ManifestPath $manifestPath -RustTarget $rustTarget -RepoRoot $repoRoot
    $resolvedWtaExePath = Join-Path $repoRoot ("tools\wta\target\{0}\release\wta.exe" -f $rustTarget)
}

if (-not (Test-Path $resolvedWtaExePath -PathType Leaf)) {
    throw "wta.exe not found: $resolvedWtaExePath"
}

$resolvedWtaNodeExePath = Join-Path (Split-Path -Parent $resolvedWtaExePath) 'wta-node.exe'
if (-not (Test-Path $resolvedWtaNodeExePath -PathType Leaf)) {
    throw "wta-node.exe not found beside wta.exe: $resolvedWtaNodeExePath"
}

if ($BuildTerminal) {
    $expectedWtaDirectory = Join-Path $repoRoot ("tools\wta\target\{0}\release" -f $rustTarget)
    $expectedWtaExePath = Join-Path $expectedWtaDirectory 'wta.exe'
    $expectedWtaNodeExePath = Join-Path $expectedWtaDirectory 'wta-node.exe'
    if ([System.IO.Path]::GetFullPath($resolvedWtaExePath) -ne [System.IO.Path]::GetFullPath($expectedWtaExePath) -or
        [System.IO.Path]::GetFullPath($resolvedWtaNodeExePath) -ne [System.IO.Path]::GetFullPath($expectedWtaNodeExePath)) {
        throw "A source-built Terminal package requires the exact WTA pair under $expectedWtaDirectory. Build WTA normally or point -WtaExePath at that exact target output."
    }
    Build-TerminalPackage -RepoRoot $repoRoot -PlatformName $Platform -ConfigurationName $Configuration
}

if ($TerminalMsix) {
    $TerminalMsix = Resolve-AbsolutePath -Path $TerminalMsix
} else {
    $TerminalMsix = Find-TerminalMsix -AppPackagesRoot $appPackagesRoot -PlatformName $Platform -ConfigurationName $Configuration
}

if ($XamlAppx) {
    $XamlAppx = Resolve-AbsolutePath -Path $XamlAppx
} else {
    $XamlAppx = Find-XamlAppx -TerminalPackagePath $TerminalMsix -PlatformName $Platform
}

if (-not (Test-Path $TerminalMsix -PathType Leaf)) {
    throw "Terminal package not found: $TerminalMsix"
}
if (-not (Test-Path $XamlAppx -PathType Leaf)) {
    throw "XAML package not found: $XamlAppx"
}

$packageIdentity = Get-AppxPackageIdentity -PackagePath $TerminalMsix
$installerVersion = $packageIdentity.Version

if ($BuildTerminal -and $installerVersion -ne $expectedManifestIdentity.Version) {
    throw "Built package version $installerVersion does not match source manifest version $($expectedManifestIdentity.Version). Refusing to package a stale MSIX."
}

$timestamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$stageRoot = Join-Path $destinationRoot ("stage-{0}-{1}-{2}" -f $Platform.ToLowerInvariant(), $Configuration.ToLowerInvariant(), $timestamp)
$terminalZipRoot = Join-Path $stageRoot 'terminal-zip'
$payloadExtractRoot = Join-Path $stageRoot 'payload-extracted'
$installerSourceRoot = Join-Path $stageRoot 'installer-source'
$payloadZip = Join-Path $stageRoot 'payload.zip'
$setupExeName = "intelligent-terminal-{0}-{1}-{2}-setup.exe" -f $installerVersion, $Platform.ToLowerInvariant(), $Configuration.ToLowerInvariant()
$setupExePath = Join-Path $destinationRoot $setupExeName

Ensure-Directory -Path $stageRoot
Ensure-Directory -Path $terminalZipRoot
Ensure-Directory -Path $payloadExtractRoot
Ensure-Directory -Path $installerSourceRoot

Write-Status "Creating unpackaged Terminal distribution from:"
Write-Status "  Terminal package: $TerminalMsix"
Write-Status "  XAML dependency:  $XamlAppx"
Write-Status "  Version:          $installerVersion"
$unpackagedZip = & $unpackagedScript -TerminalAppX $TerminalMsix -XamlAppX $XamlAppx -Destination $terminalZipRoot -PortableMode

if (-not $unpackagedZip) {
    throw 'New-UnpackagedTerminalDistribution.ps1 did not return an output ZIP.'
}

$unpackagedZipPath = $unpackagedZip.FullName
if (-not (Test-Path $unpackagedZipPath -PathType Leaf)) {
    throw "Unpackaged Terminal ZIP not found: $unpackagedZipPath"
}

Write-Status "Expanding unpackaged Terminal layout ..."
Expand-Archive -Path $unpackagedZipPath -DestinationPath $payloadExtractRoot -Force
$payloadRoot = Get-SingleChildDirectoryOrSelf -RootPath $payloadExtractRoot
$payloadProtocol = Assert-PayloadProtocolManifest -PayloadRoot $payloadRoot -SourceManifestPath $protocolManifestPath
Assert-PayloadNativeRuntime -PayloadRoot $payloadRoot

Write-Status "Injecting wta.exe into the unpackaged payload ..."
Copy-Item -Path $resolvedWtaExePath -Destination (Join-Path $payloadRoot 'wta.exe') -Force
Write-Status "Injecting the native wta-node.exe runtime ..."
Copy-Item -Path $resolvedWtaNodeExePath -Destination (Join-Path $payloadRoot 'wta-node.exe') -Force

$linuxNodePath = Join-Path $repoRoot 'tools\wta\remote\linux-x64\wta-node'
if (Test-Path $linuxNodePath -PathType Leaf) {
    Write-Status "Injecting the verified Linux x64 wta-node runtime ..."
    Copy-Item -Path $linuxNodePath -Destination (Join-Path $payloadRoot 'wta-node-linux-x64') -Force
}

# wtcli.exe is a protocol component and must stay byte-for-byte identical to
# the copy built into the selected Terminal MSIX. Never replace it from the
# repository's bin directory: that was the source of server=2.4/client=3.0
# installations when a stale MSIX was combined with a current incremental
# wtcli build.
$packagedWtcli = Join-Path $payloadRoot 'wtcli.exe'
Write-Status "Keeping MSIX-built wtcli.exe paired with its Terminal protocol server."

$payloadPromptDir = Join-Path $payloadRoot 'prompts'
Ensure-Directory -Path $payloadPromptDir
Write-Status "Injecting planner prompt template into the payload ..."
Copy-Item -Path $plannerPromptTemplate -Destination (Join-Path $payloadPromptDir 'terminal-agent.default.md') -Force
Write-Status 'Injecting uninstall and COM registration support into the payload ...'
Copy-Item -Path $uninstallerScript -Destination (Join-Path $payloadRoot 'uninstall-local-terminal.ps1') -Force
Copy-Item -Path $comRegistrationScript -Destination (Join-Path $payloadRoot 'ComProxyRegistration.ps1') -Force

$payloadMetadata = [ordered]@{
    productName = 'Intelligent Terminal'
    version = $installerVersion
    packageName = $packageIdentity.Name
    publisher = $packageIdentity.Publisher
    processorArchitecture = $packageIdentity.ProcessorArchitecture
    platform = $Platform
    configuration = $Configuration
    protocolVersion = $payloadProtocol.protocolVersion
    componentVersion = $payloadProtocol.componentVersion
    components = [ordered]@{
        windowsTerminalSha256 = (Get-FileHash (Join-Path $payloadRoot 'WindowsTerminal.exe') -Algorithm SHA256).Hash
        wtcliSha256 = (Get-FileHash $packagedWtcli -Algorithm SHA256).Hash
        wtaSha256 = (Get-FileHash (Join-Path $payloadRoot 'wta.exe') -Algorithm SHA256).Hash
        wtaNodeWindowsSha256 = (Get-FileHash (Join-Path $payloadRoot 'wta-node.exe') -Algorithm SHA256).Hash
        wtaNodeLinuxX64Sha256 = if (Test-Path (Join-Path $payloadRoot 'wta-node-linux-x64')) {
            (Get-FileHash (Join-Path $payloadRoot 'wta-node-linux-x64') -Algorithm SHA256).Hash
        } else {
            $null
        }
    }
    createdAtUtc = (Get-Date).ToUniversalTime().ToString('o')
}
$payloadMetadataPath = Join-Path $payloadRoot 'intelligent-terminal-install-metadata.json'
Set-Content -Path $payloadMetadataPath -Value ($payloadMetadata | ConvertTo-Json -Depth 4) -Encoding utf8

if (Test-Path $payloadZip -PathType Leaf) {
    Remove-Item $payloadZip -Force
}

Write-Status "Packing installer payload ..."
& "$env:SystemRoot\System32\tar.exe" -c --format=zip -f $payloadZip -C (Split-Path $payloadRoot -Parent) (Split-Path $payloadRoot -Leaf)
if ($LASTEXITCODE -ne 0) {
    throw 'Creating payload.zip failed.'
}

Copy-Item -Path $installerScript -Destination (Join-Path $installerSourceRoot 'install-local-terminal.ps1') -Force
Copy-Item -Path $comRegistrationScript -Destination (Join-Path $installerSourceRoot 'ComProxyRegistration.ps1') -Force
Copy-Item -Path $installerCmd -Destination (Join-Path $installerSourceRoot 'install.cmd') -Force
Copy-Item -Path $payloadZip -Destination (Join-Path $installerSourceRoot 'payload.zip') -Force

Write-Status "Building installer bootstrap for $rustTarget ..."
Invoke-RustBuild -CargoPath $cargoPath -ManifestPath $installerBootstrapManifest -RustTarget $rustTarget -RepoRoot $repoRoot
$bootstrapExePath = Join-Path $repoRoot ("installer\bootstrap\target\{0}\release\intelligent-terminal-installer-bootstrap.exe" -f $rustTarget)
if (-not (Test-Path $bootstrapExePath -PathType Leaf)) {
    throw "Installer bootstrap not found: $bootstrapExePath"
}

if (Test-Path $setupExePath -PathType Leaf) {
    Remove-Item $setupExePath -Force
}

Write-Status "Creating target-architecture setup executable ..."
New-SelfExtractingInstaller -BootstrapExe $bootstrapExePath -PayloadRoot $installerSourceRoot -OutputPath $setupExePath

Write-Status "Installer created: $setupExePath"
Get-Item $setupExePath
