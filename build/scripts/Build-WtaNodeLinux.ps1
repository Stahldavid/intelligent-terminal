[CmdletBinding()]
param(
    [string]$Distro = 'Ubuntu-22.04',

    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release',

    [string]$Destination = (Join-Path $PSScriptRoot '..\..\tools\wta\remote\linux-x64\wta-node')
)

$ErrorActionPreference = 'Stop'

function Invoke-WslText {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    $output = & wsl.exe -d $Distro -- @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "WSL command failed with exit code $LASTEXITCODE`: $($Arguments -join ' ')"
    }
    return ($output -join "`n").Trim()
}

function ConvertTo-BashLiteral {
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Value)

    return "'" + $Value.Replace("'", "'`"`'`"`'") + "'"
}

$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$destinationPath = if ([System.IO.Path]::IsPathRooted($Destination)) {
    [System.IO.Path]::GetFullPath($Destination)
}
else {
    [System.IO.Path]::GetFullPath((Join-Path $repoRoot $Destination))
}
$destinationDirectory = Split-Path -Parent $destinationPath
if (-not (Test-Path $destinationDirectory -PathType Container)) {
    New-Item -ItemType Directory -Path $destinationDirectory | Out-Null
}

$sourceArchiveRoot = Join-Path ([IO.Path]::GetTempPath()) "wta-node-linux-source-$([guid]::NewGuid().ToString('N'))"
$sourceArchive = Join-Path $sourceArchiveRoot 'source.tar'
$sourceList = Join-Path $sourceArchiveRoot 'source-files.txt'
[IO.Directory]::CreateDirectory($sourceArchiveRoot) | Out-Null
$sourceFiles = @(
    & git.exe -C $repoRoot ls-files --cached --others --exclude-standard -- `
        tools/wta dep/telemetry/ProjectTelemetry.h |
        Where-Object { $_ -and $_ -notlike 'tools/wta/remote/*' }
)
if ($LASTEXITCODE -ne 0 -or $sourceFiles.Count -eq 0) {
    throw 'Could not enumerate the wta-node source set.'
}
foreach ($sourceFile in $sourceFiles) {
    if ($sourceFile.IndexOfAny([char[]]"`r`n") -ge 0) {
        throw "Source paths containing line breaks are unsupported: $sourceFile"
    }
}
[IO.File]::WriteAllLines($sourceList, $sourceFiles, [Text.UTF8Encoding]::new($false))
& tar.exe -C $repoRoot -cf $sourceArchive -T $sourceList
if ($LASTEXITCODE -ne 0 -or -not (Test-Path $sourceArchive -PathType Leaf)) {
    throw 'Could not create the bounded wta-node source archive.'
}

$distros = (& wsl.exe --list --quiet) -replace "`0", '' | ForEach-Object { $_.Trim() } |
    Where-Object { $_ }
if ($LASTEXITCODE -ne 0 -or $distros -notcontains $Distro) {
    throw "WSL distribution '$Distro' is unavailable. Installed: $($distros -join ', ')"
}

$destinationWsl = Invoke-WslText -Arguments @(
    'bash',
    '-lc',
    "wslpath -a -- $(ConvertTo-BashLiteral $destinationPath)"
)
$sourceArchiveWsl = Invoke-WslText -Arguments @(
    'bash',
    '-lc',
    "wslpath -a -- $(ConvertTo-BashLiteral $sourceArchive)"
)
$profile = if ($Configuration -eq 'Release') { 'release' } else { 'debug' }
$cargoFlag = if ($Configuration -eq 'Release') { '--release' } else { '' }

$destinationLiteral = ConvertTo-BashLiteral $destinationWsl
$sourceArchiveLiteral = ConvertTo-BashLiteral $sourceArchiveWsl
$profileLiteral = ConvertTo-BashLiteral $profile
$cargoFlagLiteral = ConvertTo-BashLiteral $cargoFlag

# Cargo and rustc perform many metadata-heavy filesystem operations. Sources
# are staged once into the distro's ext4 filesystem and only the final ELF is
# copied back to Windows. This avoids the well-known /mnt/c I/O penalty while
# keeping the Windows checkout as the single source of truth.
$script = @"
set -euo pipefail
destination=$destinationLiteral
source_archive=$sourceArchiveLiteral
profile=$profileLiteral
cargo_flag=$cargoFlagLiteral
cache_root="`$HOME/.cache/intelligent-terminal"
stage="`$cache_root/linux-build"
next="`$cache_root/linux-build.next"
target_dir="`$cache_root/cargo-target"

for candidate in "`$stage" "`$next"; do
  case "`$candidate" in
    "`$HOME"/.cache/intelligent-terminal/*) ;;
    *) echo "unsafe staging path: `$candidate" >&2; exit 64 ;;
  esac
done

mkdir -p "`$cache_root" "`$target_dir"
rm -rf -- "`$next"
mkdir -p "`$next"
# Windows creates one bounded source archive using native NTFS I/O. WSL reads
# that archive sequentially, avoiding hundreds of high-latency metadata calls
# through /mnt/c and never walking the ignored multi-GiB Cargo target tree.
tar -C "`$next" -xf "`$source_archive"
rm -rf -- "`$stage"
mv -- "`$next" "`$stage"

cd "`$stage"
if [ -n "`$cargo_flag" ]; then
  CARGO_TARGET_DIR="`$target_dir" cargo build --locked --manifest-path tools/wta/Cargo.toml --bin wta-node "`$cargo_flag"
else
  CARGO_TARGET_DIR="`$target_dir" cargo build --locked --manifest-path tools/wta/Cargo.toml --bin wta-node
fi

install -m 0755 "`$target_dir/`$profile/wta-node" "`$destination"
sha256sum "`$destination"
"@

Write-Host "[wta-node-linux] Building inside $Distro ext4 cache..."
try {
    $encodedScript = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($script))
    $result = Invoke-WslText -Arguments @(
        'bash',
        '-lc',
        "printf %s $encodedScript | base64 -d | bash"
    )
}
finally {
    if (Test-Path -LiteralPath $sourceArchiveRoot) {
        Remove-Item -LiteralPath $sourceArchiveRoot -Recurse -Force
    }
}
if (-not (Test-Path $destinationPath -PathType Leaf)) {
    throw "WSL build completed without producing $destinationPath"
}

$hash = (Get-FileHash -Algorithm SHA256 -Path $destinationPath).Hash.ToLowerInvariant()
Write-Host "[wta-node-linux] $result"
Write-Host "[wta-node-linux] Artifact: $destinationPath"
Write-Host "[wta-node-linux] SHA-256: $hash"

[pscustomobject]@{
    Distro = $Distro
    Configuration = $Configuration
    Artifact = $destinationPath
    Sha256 = $hash
}
