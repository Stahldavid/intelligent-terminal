# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)

function Read-RepoFile([string] $RelativePath) {
    $path = Join-Path $repoRoot $RelativePath
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Required file not found: $RelativePath"
    }
    Get-Content -LiteralPath $path -Raw
}

function Assert-Contains([string] $Text, [string] $Pattern, [string] $Message) {
    if ($Text -notmatch $Pattern) {
        throw $Message
    }
}

function Assert-Absent([string] $Text, [string] $Pattern, [string] $Message) {
    if ($Text -match $Pattern) {
        throw $Message
    }
}

$model = Read-RepoFile 'tools\wta\src\compute\model.rs'
foreach ($contract in @(
    'RemoteFileRootPolicy',
    'ExecutionEnvironment',
    'LaunchMethod',
    'AccessEndpoint',
    'EnvironmentConnectionSupervisor',
    'RuntimeRestoreReference'
)) {
    Assert-Contains $model "struct\s+$contract|enum\s+$contract" `
        "Canonical compute contract '$contract' is missing."
}
Assert-Contains $model 'Disconnected[\s\S]*Connecting[\s\S]*Authenticating[\s\S]*Synchronizing[\s\S]*Connected[\s\S]*Offline[\s\S]*Reconnecting[\s\S]*AuthBlocked[\s\S]*VersionBlocked[\s\S]*Failed' `
    'The environment connection state machine is incomplete.'

$files = Read-RepoFile 'tools\wta\src\compute\files.rs'
Assert-Absent $files 'WTA_NODE_ALLOW_ROOT_OUTSIDE_HOME' `
    'Remote file policy must not be widened by an environment variable.'
Assert-Contains $files 'pub fn open_root' 'Remote roots must be opened through a scoped capability.'
Assert-Contains $files 'pub fn close_root' 'Remote root revocation must close the bridge grant.'
Assert-Contains $files 'root_id[\s\S]*workspace_id' `
    'Remote file operations must bind opaque root and workspace identities.'
Assert-Contains $files 'prepare_download_scoped' `
    'Remote downloads must snapshot only a policy-authorized resolved path.'
Assert-Contains $files 'workspace_and_root_identity_are_fail_closed' `
    'Workspace/root forgery regression coverage is missing.'
Assert-Contains $files 'read_only_root_rejects_all_mutations' `
    'Read-only capability regression coverage is missing.'
Assert-Contains $files 'traversal_and_absolute_paths_are_rejected' `
    'Traversal/symlink boundary regression coverage is missing.'
Assert-Contains $files 'scoped_download_manifest_never_exposes_canonical_paths' `
    'Download path-redaction regression coverage is missing.'

$node = Read-RepoFile 'tools\wta\src\compute\node.rs'
foreach ($method in @(
    'file.open_root',
    'file.close_root',
    'file.list_directory',
    'file.read_text',
    'file.prepare_download'
)) {
    Assert-Contains $node ([regex]::Escape($method)) "Node RPC '$method' is missing."
}
Assert-Contains $node 'assert!\(!value\.rpc_methods\.contains\(&"file\.roots"\.to_string\(\)\)\)' `
    'The node capability handshake must prove that host-root enumeration is absent.'

$nodeCli = Read-RepoFile 'tools\wta\src\bin\wta-node.rs'
Assert-Absent $nodeCli 'WTA_NODE_ALLOW_ROOT_OUTSIDE_HOME|root_b64|source_b64|PrepareDownload\s*\{' `
    'wta-node exposes a raw-path File Explorer/download bypass.'

$main = Read-RepoFile 'tools\wta\src\main.rs'
Assert-Contains $main 'fn public_file_root_policy' 'The redacted public root projection is missing.'
$publicRootMatch = [regex]::Match(
    $main,
    '(?s)fn public_file_root_policy\(.*?\n\}')
if (-not $publicRootMatch.Success) {
    throw 'Could not inspect public_file_root_policy.'
}
Assert-Absent $publicRootMatch.Value 'canonical_path' `
    'The public root projection leaks its canonical filesystem path.'
Assert-Contains $main 'file\.open_root' 'The broker must grant the root only for the active RPC bridge.'
Assert-Contains $main 'file\.close_root' 'The broker must revoke the bridge grant after each operation.'
Assert-Contains $main 'file\.prepare_download' 'The CLI download path must use the scoped node RPC.'
$transfer = Read-RepoFile 'tools\wta\src\compute\transfer.rs'
Assert-Contains $transfer 'unscoped remote downloads are disabled' `
    'The legacy unscoped transfer download must fail closed.'

$connection = Read-RepoFile 'tools\wta\src\compute\connection.rs'
Assert-Contains $connection 'pub fn begin_for_target' 'The canonical connection supervisor entry point is missing.'
Assert-Contains $connection 'const RETRY_DELAYS_SECONDS:\s*&\[u64\]\s*=\s*&\[3,\s*6,\s*12,\s*24,\s*48,\s*60\]' `
    'Reconnect backoff must be deterministic and capped.'
Assert-Contains $connection 'one_supervisor_owns_environment_state_and_backoff' `
    'Single-supervisor regression coverage is missing.'
Assert-Contains $connection 'future_public_endpoint_contracts_are_disabled_fail_closed' `
    'Future public endpoints must remain fail-closed.'

$restore = Read-RepoFile 'tools\wta\src\compute\restore.rs'
Assert-Contains $restore 'runtime_references' 'Restore must persist stable runtime references.'
Assert-Contains $restore 'ReconnectEnvironment' 'Restore must plan environment reconnection.'
Assert-Contains $restore 'restore_persists_stable_environment_and_runtime_ids_only' `
    'Stable restore identity regression coverage is missing.'
Assert-Contains $restore 'for forbidden in \[[\s\S]*"forwarded_port"[\s\S]*"worker_pid"[\s\S]*"ssh_pid"[\s\S]*"tunnel_path"[\s\S]*"auth"' `
    'Restore lacks a regression assertion against ephemeral transport and authentication details.'

$context = Read-RepoFile 'tools\wta\src\workspace\context.rs'
foreach ($projection in @('environments', 'endpoints', 'connections', 'file_roots')) {
    Assert-Contains $context "pub $projection\s*:" `
        "Canonical workspace context projection '$projection' is missing."
}
Assert-Absent $context 'pub canonical_path' `
    'Workspace context must not expose canonical remote root paths.'

$browser = Read-RepoFile 'src\cascadia\TerminalApp\BrowserPaneContent.cpp'
foreach ($policy in @(
    'put_AreDevToolsEnabled\(FALSE\)',
    'put_IsWebMessageEnabled\(FALSE\)',
    'put_AreHostObjectsAllowed\(FALSE\)',
    'put_IsPasswordAutosaveEnabled\(FALSE\)',
    'put_IsGeneralAutofillEnabled\(FALSE\)',
    'add_DownloadStarting',
    'args->put_Cancel\(TRUE\)'
)) {
    Assert-Contains $browser $policy "WebView2 isolation policy '$policy' is missing."
}
Assert-Contains $browser '--proxy-server=socks5://127\.0\.0\.1:' `
    'Browser traffic must use the surface-scoped loopback SSH proxy.'
Assert-Contains $browser '_userDataFolder\.c_str\(\)' `
    'Browser surfaces must use their isolated user-data directory.'

$sidebar = Read-RepoFile 'src\cascadia\TerminalApp\WorkspaceSidebar.cpp'
Assert-Contains $sidebar 'acknowledge-wide-scope' `
    'The UI must require an explicit broad-root acknowledgement.'
Assert-Contains $sidebar 'metadata\.connections' `
    'Agents & Tasks must consume the canonical environment connection projection.'
Assert-Contains $sidebar 'rss_bytes' 'PTY RSS metrics are missing from the canonical task view.'
Assert-Contains $sidebar 'user_cpu_ms' 'PTY CPU metrics are missing from the canonical task view.'

[pscustomobject]@{
    FileRoots = 'policy-scoped'
    RawPathBypass = 'disabled'
    Downloads = 'opaque-root + relative-path'
    EnvironmentContracts = 'present'
    ConnectionSupervisor = 'single-owner/fail-closed'
    RestoreIdentity = 'stable-only'
    BrowserIsolation = 'enforced'
    WorkspaceContext = 'canonical'
    PtyMetrics = 'present'
    Status = 'ok'
} | ConvertTo-Json
