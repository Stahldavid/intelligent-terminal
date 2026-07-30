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
    return Get-Content -LiteralPath $path -Raw
}

function Assert-Contains(
    [string] $Text,
    [string] $Pattern,
    [string] $Message
) {
    if ($Text -notmatch $Pattern) {
        throw $Message
    }
}

$serverPath = Join-Path $repoRoot 'src\cascadia\WindowsTerminal\TerminalProtocolComServer.cpp'
$serverLines = Get-Content -LiteralPath $serverPath
$methodStarts = @()
for ($index = 0; $index -lt $serverLines.Count; $index++) {
    if ($serverLines[$index] -match '^STDMETHODIMP TerminalProtocolComServer::(\w+)') {
        $methodStarts += [pscustomobject]@{
            Name = $Matches[1]
            Line = $index
        }
    }
}

if ($methodStarts.Count -eq 0) {
    throw 'No TerminalProtocolComServer methods were discovered.'
}

for ($methodIndex = 0; $methodIndex -lt $methodStarts.Count; $methodIndex++) {
    $method = $methodStarts[$methodIndex]
    if ($method.Name -eq 'Authenticate') {
        continue
    }
    $end = if ($methodIndex + 1 -lt $methodStarts.Count) {
        $methodStarts[$methodIndex + 1].Line - 1
    } else {
        $serverLines.Count - 1
    }
    $body = $serverLines[$method.Line..$end] -join "`n"
    if ($body -notmatch 'RETURN_IF_PROTOCOL_UNAUTHENTICATED\(\)') {
        throw "Terminal Protocol method '$($method.Name)' is missing the authentication guard."
    }
}

$server = $serverLines -join "`n"
Assert-Contains $server 'presented == s_capabilityToken' `
    'Authenticate must compare the presented token with the host capability.'
Assert-Contains $server 'Capability::Validate\(s_capabilityToken,\s*presented\)' `
    'Authenticate must validate signed scoped capabilities when the host token does not match.'
Assert-Contains $server 'v\["protocol_version"\] = "3\.1"' `
    'Terminal Protocol server must advertise version 3.1.'
Assert-Contains $server '_claims->Has\(operation\)' `
    'Scoped calls must check the operation bit declared by the capability.'
Assert-Contains $server '_claims->surfaceId == session' `
    'Surface capabilities must be restricted to their bound terminal session.'
Assert-Contains $server '_workspaceForSession\(sessionId\)' `
    'Workspace capabilities must resolve and verify the target session workspace.'
Assert-Contains $server 'if \(!_isEventAuthorized\(eventJson,\s*CapabilityOperation::Subscribe\)\)\s*\{\s*return;\s*\}' `
    'Events must be filtered for each subscriber before entering its delivery queue.'
Assert-Contains $server '_isEventAuthorized\(jsonStr,\s*CapabilityOperation::SendEvent\)' `
    'Incoming events must be checked against their own SendEvent operation and scope.'
Assert-Contains $server 'Scoped subscribers never receive unscoped events' `
    'Unscoped events must fail closed for scoped subscribers.'
Assert-Contains $server 'RETURN_HR_IF\(E_ACCESSDENIED,\s*!_hasOperation\(CapabilityOperation::CreateTab\)\)' `
    'CreateTab must remain unavailable to scoped capabilities that do not advertise it.'
Assert-Contains $server '__intellterm_surface_v1__' `
    'Surface creation must use the reserved ABI-compatible SplitPane transport.'
Assert-Contains $server '__intellterm_managed_surface_v1__' `
    'Managed surface creation must use the reserved ABI-compatible SplitPane transport.'
Assert-Contains $server '__intellterm_browser_surface_v1__' `
    'Browser surface creation must use the reserved ABI-compatible SplitPane transport.'
Assert-Contains $server 'RETURN_HR_IF\(E_ACCESSDENIED,\s*!_isSessionAuthorized\(sessionId,\s*CapabilityOperation::CreateSurface\)\)' `
    'Every surface creation transport must require the exact authorized target session and scoped operation.'
Assert-Contains $server 'createBrowserSurfaceDirection[\s\S]*CreateProtocolBrowserSurface\(' `
    'The reserved browser transport must dispatch only to the native browser-surface constructor.'
Assert-Contains $server 'const auto paneScopedOperation[\s\S]*CapabilityOperation::ClosePane[\s\S]*CapabilityOperation::FocusPane' `
    'Surface tokens may navigate and clean up only sibling surfaces in the same pane.'
Assert-Contains $server 'sourcePane && targetPane && \*sourcePane == \*targetPane' `
    'Pane-scoped navigation must compare both canonical workspace and pane identity.'

$capability = Read-RepoFile 'src\inc\TerminalProtocolCapability.h'
Assert-Contains $capability 'BCRYPT_SHA256_ALGORITHM' `
    'Scoped capabilities must use HMAC-SHA256.'
Assert-Contains $capability '_constantTimeEqual' `
    'Capability MAC comparison must use the constant-time comparison helper.'
Assert-Contains $capability 'expiresAtUnixSeconds <= now' `
    'Capability validation must enforce expiry.'
Assert-Contains $capability 'Scope::Surface && surfaceId\.empty\(\)' `
    'A surface capability without a surface identity must fail closed.'
Assert-Contains $capability 'Scope::Workspace && workspaceId\.empty\(\)' `
    'A workspace capability without a workspace identity must fail closed.'
Assert-Contains $capability '_canonicalIdentifier\(std::wstring\{ workspaceId \}\)' `
    'Workspace capabilities must canonicalize braced Tab StableIds before signing.'
Assert-Contains $capability 'claims\.workspaceId = details::_canonicalIdentifier' `
    'Validated workspace claims must canonicalize legacy braced identifiers before authorization.'
Assert-Contains $capability "workspaceId\.find\(L'\|'\).*std::wstring_view::npos" `
    'Capability identities must reject the protocol field delimiter.'
Assert-Contains $capability 'constexpr uint64_t SurfaceOperations' `
    'The surface operation mask must be explicit and reviewable.'
Assert-Contains $capability 'constexpr uint64_t WorkspaceOperations' `
    'The workspace operation mask must be explicit and reviewable.'

foreach ($maskName in @('SurfaceOperations', 'WorkspaceOperations')) {
    $maskMatch = [regex]::Match(
        $capability,
        "(?s)constexpr uint64_t $maskName\s*=(.*?);")
    if (-not $maskMatch.Success) {
        throw "Could not inspect the $maskName capability mask."
    }
    if ($maskMatch.Groups[1].Value -match 'Operation::CreateTab') {
        throw "$maskName must not authorize host-level CreateTab."
    }
}

$conpty = Read-RepoFile 'src\cascadia\TerminalConnection\ConptyConnection.cpp'
Assert-Contains $conpty 'Capability::Mint\(' `
    'ConPTY launch must derive a scoped token instead of copying the host bearer.'
Assert-Contains $conpty 'Capability::Scope::Surface' `
    'Ordinary ConPTY children must receive a surface-scoped capability.'
Assert-Contains $conpty 'Capability::Scope::Workspace' `
    'Trusted WTA workspace helpers must receive a workspace-scoped capability.'
Assert-Contains $conpty 'std::exchange\(_trustedWorkspaceCapabilityId,\s*\{\}\)' `
    'The private workspace grant must be consumed exactly once during launch.'
Assert-Contains $conpty 'Command-line text\s*// is never treated as proof of privilege' `
    'ConPTY must document that command-line text is not an authorization primitive.'
Assert-Contains $conpty 'never fall back to the host secret' `
    'Scoped-token minting failure must explicitly fail closed.'
if ($conpty -match 'insert_or_assign\(L"WT_PROTOCOL_TOKEN",\s*buf\)') {
    throw 'ConPTY launch must never copy the raw host protocol token into a child environment.'
}
if ($conpty -match '_trustedWtaWorkspace|--owner-tab-id|CommandLineToArgvW') {
    throw 'ConPTY must never infer workspace privilege from the child command line.'
}

$terminalPage = Read-RepoFile 'src\cascadia\TerminalApp\TerminalPage.cpp'
Assert-Contains $terminalPage '_MakeTerminalPane\(args,\s*nullptr,\s*nullptr,\s*stableId\)' `
    'TerminalPage must explicitly grant workspace scope only to its internally-created WTA helper.'
Assert-Contains $terminalPage 'L"trustedWorkspaceCapabilityId"' `
    'The private grant must be transferred through the in-process connection settings.'

$terminalPageProtocol = Read-RepoFile 'src\cascadia\TerminalApp\TerminalPage.Protocol.cpp'
Assert-Contains $terminalPageProtocol 'CreateProtocolBrowserSurface\(' `
    'TerminalPage must expose the authenticated browser-surface protocol endpoint.'
Assert-Contains $terminalPageProtocol 'FindPaneBySessionId\(sessionId\)' `
    'Browser surface creation must resolve the exact authorized pane session.'
Assert-Contains $terminalPageProtocol 'foundPane->GetSurfaceStack\(\)' `
    'Browser creation must remain pane-local and require a surface stack.'

$wtcli = Read-RepoFile 'src\tools\wtcli\main.cpp'
Assert-Contains $wtcli 'RequiredProtocolVersion\{\s*"3\.1"\s*\}' `
    'wtcli must require Terminal Protocol 3.1.'
Assert-Contains $wtcli 'WT_PROTOCOL_TOKEN not set\. Refusing unauthenticated terminal control' `
    'wtcli must fail closed when WT_PROTOCOL_TOKEN is absent.'
Assert-Contains $wtcli 'wchar_t capabilityToken\[2048\]' `
    'wtcli must accept the signed scoped capability token length.'

$settings = Read-RepoFile 'src\cascadia\TerminalSettingsModel\MTSMSettings.h'
foreach ($setting in @('AiConfirmationReadOps', 'AiConfirmationCreateOps', 'AiConfirmationInputOps')) {
    Assert-Contains $settings "X\(hstring,\s*$setting,.*L`"prompt`"\)" `
        "$setting must default to prompt."
}

$spawn = Read-RepoFile 'tools\wta\src\protocol\acp\spawn.rs'
Assert-Contains $spawn 'env_remove\("WT_PROTOCOL_TOKEN"\)' `
    'ACP adapter launch must remove WT_PROTOCOL_TOKEN.'
Assert-Contains $spawn 'env_remove\("WT_COM_CLSID"\)' `
    'ACP adapter launch must remove WT_COM_CLSID.'
Assert-Contains $spawn 'unset CLAUDECODE WT_COM_CLSID WT_PROTOCOL_TOKEN' `
    'WSL ACP adapter launch must scrub terminal-control credentials.'

$team = Read-RepoFile 'tools\wta\src\main.rs'
Assert-Contains $team 'set WT_COM_CLSID=&& set WT_PROTOCOL_TOKEN=&&' `
    'Native-team Agent CLI launch must scrub terminal-control credentials.'

$unitProject = Read-RepoFile 'src\cascadia\ut_app\TerminalApp.UnitTests.vcxproj'
Assert-Contains $unitProject 'TerminalProtocolCapabilityTests\.cpp' `
    'The scoped-capability regression tests must remain part of TerminalApp.UnitTests.'

$unitTests = Read-RepoFile 'src\cascadia\ut_app\TerminalProtocolCapabilityTests.cpp'
Assert-Contains $unitTests 'L"\{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE\}"' `
    'WorkspaceClaimsRoundTrip must exercise the braced Tab StableId spelling used at runtime.'
foreach ($test in @(
    'SurfaceClaimsRoundTrip',
    'WorkspaceClaimsRoundTrip',
    'TamperAndWrongIssuerSecretFailClosed',
    'ExpiryIsCheckedOnEveryValidation',
    'NonceIsUnique',
    'DelimitedIdentityFailsClosed',
    'NativeChatSnapshotUsesDirectRoute',
    'DirectRoutePreservesSchemaForPageValidation'
)) {
    Assert-Contains $unitTests $test `
        "Required scoped-capability regression test '$test' is missing."
}

[pscustomobject]@{
    ProtocolVersion = '3.1'
    GuardedMethods = $methodStarts.Count - 1
    CapabilityScope = 'surface/workspace'
    EventFiltering = 'fail-closed'
    ConfirmationDefault = 'prompt'
    AgentCredentialScrubbing = 'verified'
    CapabilityTests = 8
    Status = 'ok'
} | ConvertTo-Json
