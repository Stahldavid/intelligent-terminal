[CmdletBinding()]
param(
    [string]$Distro = 'Ubuntu-22.04',
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$Version = '22.23.1',
    [ValidatePattern('^[0-9a-fA-F]{64}$')]
    [string]$Sha256 = '9749e988f437343b7fa832c69ded82a312e41a03116d766797ac14f6f9eee578'
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($Distro) -or $Distro.StartsWith('-')) {
    throw 'Distro must be a concrete WSL distribution name.'
}

$archiveName = "node-v$Version-linux-x64.tar.xz"
$downloadUrl = "https://nodejs.org/dist/v$Version/$archiveName"
$linuxScript = @'
set -euo pipefail

version="$1"
expected_sha="$2"
download_url="$3"
archive_name="$4"
root="$HOME/.local/share/intelligent-terminal/toolchains"
downloads="$root/.downloads"
install_name="node-v${version}-linux-x64"
install_path="$root/$install_name"
archive_path="$downloads/$archive_name"

for required in curl sha256sum tar; do
    command -v "$required" >/dev/null 2>&1 || {
        printf 'Missing required WSL tool: %s\n' "$required" >&2
        exit 2
    }
done

mkdir -p "$downloads"
if [ ! -x "$install_path/bin/node" ]; then
    temp_archive="$archive_path.part.$$"
    stage_path="$root/.${install_name}.stage.$$"
    trap 'rm -f "$temp_archive"; rm -rf "$stage_path"' EXIT
    curl --fail --location --silent --show-error \
        --output "$temp_archive" "$download_url"
    printf '%s  %s\n' "$expected_sha" "$temp_archive" | sha256sum --check --status
    mv -f "$temp_archive" "$archive_path"
    mkdir -p "$stage_path"
    tar -xJf "$archive_path" --strip-components=1 -C "$stage_path"
    test -x "$stage_path/bin/node"
    mv "$stage_path" "$install_path"
    trap - EXIT
fi

link_path="$root/.node-current.$$"
ln -s "$install_name" "$link_path"
mv -Tf "$link_path" "$root/node-current"

PATH="$root/node-current/bin:$PATH"
export PATH
printf 'node=%s\n' "$("$root/node-current/bin/node" --version)"
printf 'npm=%s\n' "$("$root/node-current/bin/npm" --version)"
printf 'npx=%s\n' "$("$root/node-current/bin/npx" --version)"
printf 'path=%s\n' "$root/node-current/bin"
'@

$output = $linuxScript |
    & wsl.exe -d $Distro -- bash -s -- $Version $Sha256 $downloadUrl $archiveName
if ($LASTEXITCODE -ne 0) {
    throw "WSL Node.js runtime provisioning failed with exit code $LASTEXITCODE."
}

$output
