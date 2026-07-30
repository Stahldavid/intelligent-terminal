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

function Assert-Contains(
    [string] $Text,
    [string] $Pattern,
    [string] $Message
) {
    if ($Text -notmatch $Pattern) {
        throw $Message
    }
}

function Assert-NotContains(
    [string] $Text,
    [string] $Pattern,
    [string] $Message
) {
    if ($Text -match $Pattern) {
        throw $Message
    }
}

$xaml = Read-RepoFile 'src\cascadia\TerminalApp\AgentPaneContent.xaml'
foreach ($name in @(
    'NativeChatRoot',
    'NativeChatMessages',
    'NativePermissionCard',
    'NativeChatComposer',
    'NativeChatCancelButton',
    'NativeChatSendButton'
)) {
    Assert-Contains $xaml "x:Name=`"$name`"" `
        "The native chat dock is missing the '$name' XAML control."
}
if ($xaml -match 'WebView2|WebView') {
    throw 'The native chat dock must not create or reference a WebView control.'
}
Assert-NotContains $xaml 'AgentScopeSelector|AgentScopeSurface|AgentScopeWorkspace|AgentScopeTeam' `
    'The contextual chat dock must not expose a manual Surface/Workspace/Team selector.'
Assert-Contains $xaml 'x:Name="AgentContextText"' `
    'The contextual chat dock must identify the terminal it follows.'

$contentHeader = Read-RepoFile 'src\cascadia\TerminalApp\AgentPaneContent.h'
Assert-Contains $contentHeader 'ApplyNativeChatSnapshot' `
    'AgentPaneContent must expose immutable native chat snapshot application.'
Assert-Contains $contentHeader 'NativeChatAction' `
    'AgentPaneContent must expose native composer/permission actions.'
Assert-NotContains $contentHeader 'ScopeChanged|_AgentScopeSelectionChanged' `
    'AgentPaneContent must not expose manual chat-scope selection.'

$content = Read-RepoFile 'src\cascadia\TerminalApp\AgentPaneContent.cpp'
Assert-Contains $content 'sequence <= _nativeSnapshotSequence' `
    'Native chat rendering must reject delayed or duplicate snapshots.'
Assert-Contains $content '_nativeWorkspaceId\.empty\(\) \|\| _nativeScopeKey\.empty\(\)' `
    'Native actions must fail closed without workspace and scope identities.'
Assert-Contains $content '_raiseNativeChatAction\("permission"' `
    'Native permission buttons must route through the structured action channel.'
Assert-Contains $content 'NativeChatScroll\(\)\.ChangeView' `
    'Native streaming snapshots must keep the conversation viewport current.'

$page = Read-RepoFile 'src\cascadia\TerminalApp\TerminalPage.cpp'
Assert-Contains $page 'OnNativeChatSnapshot' `
    'TerminalPage must receive native chat snapshots.'
Assert-Contains $page '_FindTabByStableId\(workspaceId\)' `
    'Native chat snapshots must route by canonical workspace identity.'
Assert-Contains $page '_RaiseProtocolEvent\("native_chat_action"' `
    'Native chat actions must use the existing authenticated protocol channel.'
Assert-NotContains $page 'agent_scope_changed' `
    'TerminalPage must route chat from focus rather than a manual scope selector.'
Assert-Contains $page 'pane->GetProfile\(\)' `
    'The passive context must identify the followed terminal surface, not the agent companion.'
Assert-Contains $page '_SessionToggleButtonOnClick' `
    'The bottom-bar Sessions & history button must retain its click handler.'
Assert-Contains $page '_RequestAgentStateForTab\(activeTab, "sessions", /\*pane_open\*/ true\)' `
    'The Sessions & history button must request the sessions view for the active workspace.'
Assert-Contains $page 'params\["pane_id"\] = agentPaneSessionId' `
    'Surface-scoped helpers must receive set_agent_state through the agent pane session id.'

$pageXaml = Read-RepoFile 'src\cascadia\TerminalApp\TerminalPage.xaml'
Assert-Contains $pageXaml 'x:Name="SessionToggleButton"' `
    'The bottom bar must expose the Sessions & history button.'
Assert-Contains $pageXaml 'AutomationProperties.Name="Sessions and history"' `
    'The Sessions & history button must expose a stable accessible name for UI automation.'
Assert-Contains $pageXaml 'Click="_SessionToggleButtonOnClick"' `
    'The Sessions & history button must be wired to its handler.'
Assert-Contains $pageXaml 'Text="Sessions &amp; history"' `
    'The session-management control must describe both live sessions and history.'

$parsing = Read-RepoFile 'src\cascadia\TerminalProtocol\ProtocolParsing.h'
Assert-Contains $parsing 'NativeChatSnapshot' `
    'Terminal Protocol must have a direct native-chat dispatch route.'
Assert-Contains $parsing 'method == "native_chat_snapshot"' `
    'Terminal Protocol must classify native chat snapshots explicitly.'

$server = Read-RepoFile 'src\cascadia\WindowsTerminal\TerminalProtocolComServer.cpp'
Assert-Contains $server '_dispatchNativeChatSnapshotToPage' `
    'The COM server must dispatch native chat snapshots on the UI thread.'

$wtcli = Read-RepoFile 'src\tools\wtcli\main.cpp'
Assert-Contains $wtcli '"publish-stdin"' `
    'wtcli must provide the persistent native-chat event transport.'
Assert-Contains $wtcli 'MaximumEventBytes = 1024 \* 1024' `
    'The persistent event transport must bound each input document.'
Assert-Contains $wtcli 'std::cout << "ok\\n" << std::flush' `
    'The persistent transport must acknowledge successful delivery.'

$wta = Read-RepoFile 'tools\wta\src\app.rs'
Assert-Contains $wta 'fn publish_native_chat_snapshot_if_changed' `
    'WTA must project conversation state into immutable native snapshots.'
Assert-Contains $wta 'fn handle_native_chat_action' `
    'WTA must consume structured native chat actions.'
Assert-Contains $wta 'scope_key\.is_empty\(\) \|\| scope_key != self\.active_tab_key\(\)' `
    'Native chat actions must require an exact current scope key.'
Assert-Contains $wta 'recv_timeout\(std::time::Duration::from_secs\(5\)\)' `
    'Persistent event acknowledgement must be bounded by a timeout.'
Assert-Contains $wta 'publish_event_blocking\(&payload\)' `
    'The persistent bridge must retain an ordered compatibility fallback.'
Assert-NotContains $wta 'selected_scope_key|agent_scope: String' `
    'WTA must not retain a sticky manual chat scope that overrides terminal focus.'
Assert-Contains $wta 'self\.active_scope_key = Some\(scope_key\.clone\(\)\)' `
    'A validated focus event must activate its exact surface conversation.'
Assert-Contains $wta 'let scope_key = self\.scope_for_workspace\(target_tab\)' `
    'Agent-state projection must read the focused surface scope for its workspace.'
Assert-Contains $wta 'workspace_agent_state_projection_reads_the_focused_surface_scope' `
    'Focused-surface agent-state projection must retain a regression test.'
Assert-Contains $wta 'WtProtocolFailure' `
    'A protocol/component mismatch must surface an actionable chat error.'
Assert-Contains $wta 'wt_protocol_failure_keeps_chat_alive_and_surfaces_repair_action' `
    'The degraded protocol state must retain a regression test.'

$resourcesEn = Read-RepoFile 'src\cascadia\TerminalApp\Resources\en-US\Resources.resw'
$resourcesPt = Read-RepoFile 'src\cascadia\TerminalApp\Resources\pt-BR\Resources.resw'
foreach ($resource in @(
    'NativeChatComposer.PlaceholderText',
    'NativeChatSendButton.Content',
    'NativeChatCancelButton.Content',
    'NativePermissionTitle.Text',
    'NativeChatPlanLabel',
    'NativeChatStatusPermissionsLabel',
    'AgentFollowingContextPrefix',
    'WorkspaceFleetDescription'
)) {
    Assert-Contains $resourcesEn ([regex]::Escape("name=`"$resource`"")) `
        "English native-chat resource '$resource' is missing."
    Assert-Contains $resourcesPt ([regex]::Escape("name=`"$resource`"")) `
        "Portuguese native-chat resource '$resource' is missing."
}

$tests = Read-RepoFile 'tools\wta\src\app.rs'
foreach ($test in @(
    'native_chat_text_truncates_on_unicode_scalar_boundaries',
    'native_chat_action_requires_owned_workspace_and_exact_scope',
    'native_chat_submit_uses_the_existing_prompt_pipeline',
    'native_chat_permission_resolves_only_an_advertised_option',
    'legacy_explicit_destination_is_separate_but_next_focus_returns_to_surface'
)) {
    Assert-Contains $tests $test "Native chat regression test '$test' is missing."
}

[pscustomobject]@{
    Rendering = 'native-xaml'
    WebView2 = 'absent'
    SnapshotRouting = 'focused-surface'
    ManualScopeSelector = 'absent'
    Delivery = 'persistent-acknowledged-with-fallback'
    Locales = @('en-US', 'pt-BR')
    RustTests = 4
    Status = 'ok'
} | ConvertTo-Json -Depth 3
