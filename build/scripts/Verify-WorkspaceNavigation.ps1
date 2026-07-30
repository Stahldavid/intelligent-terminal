[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$terminalPageXaml = Get-Content -Raw (Join-Path $repoRoot 'src\cascadia\TerminalApp\TerminalPage.xaml')
$terminalPageHeader = Get-Content -Raw (Join-Path $repoRoot 'src\cascadia\TerminalApp\TerminalPage.h')
$workspaceSidebar = Get-Content -Raw (Join-Path $repoRoot 'src\cascadia\TerminalApp\WorkspaceSidebar.cpp')
$tabManagement = Get-Content -Raw (Join-Path $repoRoot 'src\cascadia\TerminalApp\TabManagement.cpp')
$terminalPageImplementation = Get-Content -Raw (Join-Path $repoRoot 'src\cascadia\TerminalApp\TerminalPage.cpp')
$tabHeader = Get-Content -Raw (Join-Path $repoRoot 'src\cascadia\TerminalApp\Tab.h')
$tabImplementation = Get-Content -Raw (Join-Path $repoRoot 'src\cascadia\TerminalApp\Tab.cpp')
$surfaceStack = Get-Content -Raw (Join-Path $repoRoot 'src\cascadia\TerminalApp\SurfaceStackPaneContent.cpp')
$surfaceStackIdl = Get-Content -Raw (Join-Path $repoRoot 'src\cascadia\TerminalApp\TerminalPaneContent.idl')

$failures = [System.Collections.Generic.List[string]]::new()

function Assert-Contains {
    param(
        [string] $Text,
        [string] $Pattern,
        [string] $Message
    )

    if ($Text -notmatch $Pattern) {
        $failures.Add($Message)
    }
}

function Assert-NotContains {
    param(
        [string] $Text,
        [string] $Pattern,
        [string] $Message
    )

    if ($Text -match $Pattern) {
        $failures.Add($Message)
    }
}

# Composition invariant: a physical-pixel separator column is always painted
# opaquely. This prevents the transparent app root from leaking through at
# fractional DPI while the wider Thumb remains purely an input target.
Assert-Contains $terminalPageXaml 'x:Name="WorkspaceSidebarSeparatorColumn"[\s\S]*?Width="1"' `
    'The sidebar has no dedicated one-pixel separator column.'
Assert-Contains $terminalPageXaml 'x:Name="WorkspaceSidebarSeparatorUnderlay"[\s\S]*?Grid\.Column="1"[\s\S]*?Background="\{ThemeResource SystemControlBackgroundAltHighBrush\}"' `
    'The sidebar separator column has no opaque underlay.'
Assert-Contains $terminalPageXaml 'x:Name="TabContent"[\s\S]*?Grid\.Column="2"' `
    'Terminal content does not begin immediately after the opaque separator column.'
Assert-Contains $terminalPageXaml 'x:Name="WorkspaceSidebarSplitter"[\s\S]*?Grid\.Column="1"[\s\S]*?HorizontalAlignment="Center"' `
    'The sidebar splitter is not centered over the opaque separator column.'
Assert-Contains $terminalPageXaml 'x:Name="WorkspaceSidebarSplitter"[\s\S]*?Canvas\.ZIndex="[1-9][0-9]*"' `
    'The splitter is not explicitly layered above the adjacent surfaces.'
Assert-NotContains $terminalPageXaml 'x:Name="WorkspaceSidebarRoot"[\s\S]{0,400}?BorderThickness="0,0,1,0"' `
    'The sidebar root still paints a translucent border directly over the transparent app root.'
Assert-Contains $workspaceSidebar 'WorkspaceSidebarSeparatorColumn\(\)\.Width\(' `
    'Sidebar visibility does not collapse and restore the separator column with the sidebar.'
Assert-Contains $workspaceSidebar '_workspaceSidebarWidth = std::round\(' `
    'Sidebar resize width is not rounded before updating the XAML column.'

# Presentation invariant: top tabs and vertical workspaces are two views of the
# same Tab objects, never simultaneous competing navigation surfaces.
Assert-Contains $workspaceSidebar 'TabViewItem\(\)\.Visibility\(' `
    'Sidebar visibility does not switch the native TabViewItem presentation.'
Assert-Contains $workspaceSidebar '_newTabButton\.Visibility\(nativeHeaderVisibility\)' `
    'The duplicate native new-tab button is not hidden with native tab headers.'
Assert-Contains $workspaceSidebar 'WorkspaceSidebarNewButton\(\)\.Flyout\(nativeNewTabMenu\)' `
    'The sidebar does not reuse the canonical native new-tab flyout.'
Assert-Contains $workspaceSidebar '_OpenNewTerminalViaDropdown\(NewTerminalArgs\{\}\)' `
    'The sidebar primary new-terminal action does not reuse the native creation path.'
Assert-Contains $workspaceSidebar 'row\.tab->GetTabColor\(\)' `
    'Sidebar accent is not sourced from the native Tab color.'
Assert-Contains $workspaceSidebar '_CreateNewTabFlyoutIcon\(row\.tab->Icon\(\)\)' `
    'Sidebar icon is not sourced from the native Tab icon.'
Assert-Contains $workspaceSidebar 'title\.Text\(row\.tab->Title\(\)\)' `
    'Sidebar title is not sourced from the native Tab title.'
Assert-NotContains $workspaceSidebar 'colorMenu\.Text\(L"Workspace color"\)' `
    'Sidebar still exposes a second per-workspace color system.'
Assert-NotContains $workspaceSidebar 'iconMenu\.Text\(L"Workspace icon"\)' `
    'Sidebar still exposes a second per-workspace icon system.'
Assert-Contains $workspaceSidebar 'return a\.tabIndex < b\.tabIndex;' `
    'Sidebar order is not the canonical native tab order.'
Assert-NotContains $workspaceSidebar 'button\.ContextFlyout\(row\.tab->TabViewItem\(\)\.ContextFlyout\(\)\)' `
    'Sidebar cards share the native TabViewItem MenuFlyout instance; WinUI flyouts must have one visual owner.'
Assert-Contains $workspaceSidebar 'CreateContextMenuForTarget\(button,\s*true\)' `
    'Sidebar cards do not create their own presenter from the canonical tab context-menu builder.'
Assert-Contains $workspaceSidebar 'button\.ContextFlyout\(sidebarMenu\)' `
    'Sidebar cards do not attach their independently-owned context-menu presenter.'
Assert-Contains $workspaceSidebar '_EnsureWorkspaceContextMenuExtension\(' `
    'Workspace-only commands are not composed as an extension of the native tab menu.'
Assert-Contains $tabHeader 'CreateContextMenuForTarget\(' `
    'Tab does not expose a canonical context-menu builder for multiple navigation surfaces.'
Assert-Contains $tabImplementation 'Controls::MenuFlyout Tab::CreateContextMenuForTarget\(' `
    'Tab does not construct independent context-menu presenters.'
Assert-Contains $tabHeader 'SetContextMenuTarget\(' `
    'Tabs have no presentation-aware context-menu anchor.'
Assert-Contains $tabImplementation '_contextMenuTarget\.get\(\)' `
    'Rename/color interactions do not resolve the current navigation anchor.'
Assert-Contains $tabImplementation '_tabColorPickup\.ShowAt\(contextTarget\)' `
    'The tab color picker is still anchored to the hidden horizontal tab header.'
Assert-Contains $tabImplementation 'PropertyChanged\.raise\(\*this,[\s\S]*?L"TabColor"' `
    'Tab color changes do not notify the sidebar presentation.'

# Restore invariant: sidebar recent items reference native history by identity,
# never by assuming both lists have matching indexes.
Assert-Contains $terminalPageHeader 'uint64_t nativeHistoryId\{ 0 \};' `
    'Recently closed workspace metadata does not reference native history by identity.'
Assert-Contains $workspaceSidebar 'entry\.id == recent\.nativeHistoryId' `
    'Sidebar restore still correlates its history with native history by position.'
Assert-Contains $tabManagement 'owningTab->BuildStartupActions\(BuildStartupKind::None\)' `
    'Closing a final pane does not record complete native tab startup actions.'

# Visual ownership invariant: ordinary terminals are promoted to a
# SurfaceStackPaneContent. Wrapping one as AgentPaneContent must detach the
# active surface from that visual tree before the wrapper attaches its root.
# Merely clearing Pane's outer Border leaves the TermControl parented by the
# surface stack and causes WinUI to fail-fast with E_INVALIDARG at launch.
Assert-Contains $terminalPageImplementation 'if \(const auto surfaceStack = rawPane->GetSurfaceStack\(\)\)[\s\S]*?rawPane->DetachActiveSurface\(\)' `
    'Agent pane wrapping does not detach a terminal from its SurfaceStack visual parent.'
Assert-Contains $terminalPageImplementation 'else[\s\S]*?rootGrid\.Children\(\)\.GetAt\(0\)\.try_as<winrt::Windows::UI::Xaml::Controls::Border>' `
    'Agent pane wrapping no longer preserves the legacy non-surface-stack detach fallback.'

# Surface creation invariant: the per-workspace surface dropdown is a
# destination-aware projection of the canonical newTabMenu tree. It must not
# regress to a second flat ActiveProfiles list.
Assert-Contains $surfaceStack '_newSurfaceButton\.MinWidth\(64\)[\s\S]*?_newSurfaceButton\.Width\(64\)' `
    'The per-pane SplitButton can collapse its secondary profile-dropdown hit target.'
Assert-Contains $surfaceStack 'SplitButtonPrimaryButtonSize[\s\S]*?36\.0[\s\S]*?SplitButtonSecondaryButtonSize[\s\S]*?28\.0' `
    'The per-pane SplitButton does not reserve deterministic primary and dropdown hit areas.'
Assert-Contains $surfaceStack '_newSurfaceButton\.Click\([\s\S]*?NewSurfaceRequested\.raise\(\*self, nullptr\)' `
    'The primary per-pane + no longer duplicates the active surface profile.'
Assert-Contains $surfaceStack '_newSurfaceButton\.Flyout\(flyout\)' `
    'The secondary per-pane dropdown is not attached to the SplitButton.'
Assert-Contains $terminalPageImplementation 'makeConfiguredPane[\s\S]*?pane->UpdateSettings\(_settings\)' `
    'New terminal panes do not receive current settings, leaving the per-pane SplitButton flyout null until a settings reload.'
Assert-Contains $terminalPageImplementation 'auto resultPane = makeConfiguredPane\(paneContent\)' `
    'The normal terminal creation path bypasses initial settings/flyout initialization.'
Assert-Contains $terminalPageImplementation 'auto debugPane = makeConfiguredPane\(debugContent\)' `
    'The debug terminal creation path bypasses initial settings/flyout initialization.'
Assert-Contains $surfaceStack 'NewSurfaceRequested\.raise\(\*self, NewTerminalArgs\{ profileIndex \}\)' `
    'A profile selected from the per-pane dropdown does not preserve its profile identity.'
Assert-Contains $surfaceStack 'settings\.GlobalSettings\(\)\.NewTabMenu\(\)' `
    'The surface dropdown does not consume the canonical newTabMenu configuration.'
Assert-Contains $surfaceStack 'NewTabMenuEntryType::Folder' `
    'The surface dropdown no longer supports nested newTabMenu folders.'
Assert-Contains $surfaceStack 'NewTabMenuEntryType::RemainingProfiles' `
    'The surface dropdown no longer supports remainingProfiles entries.'
Assert-Contains $surfaceStack 'NewTabMenuEntryType::MatchProfiles' `
    'The surface dropdown no longer supports matchProfiles entries.'
Assert-Contains $surfaceStack 'NewTabMenuEntryType::Action' `
    'The surface dropdown no longer supports configured action entries.'
Assert-NotContains $surfaceStack 'const auto profiles = settings\.ActiveProfiles\(\)' `
    'The surface dropdown regressed to a duplicate flat ActiveProfiles menu.'
Assert-Contains $surfaceStack 'action\.Action\(\) == ShortcutAction::NewTab[\s\S]*?NewSurfaceRequested\.raise' `
    'newTab actions selected inside a workspace are not destination-translated into surfaces.'
Assert-Contains $surfaceStack 'ActionRequested\.raise\(\*this, action\)' `
    'Non-newTab menu actions are not delegated to the canonical native action dispatcher.'
Assert-Contains $surfaceStackIdl 'event Windows\.Foundation\.TypedEventHandler<SurfaceStackPaneContent, Object> ActionRequested' `
    'SurfaceStackPaneContent does not expose native action delegation.'

# Lifecycle invariant: every mutation publishes an immutable ValueSet snapshot.
# Reading shared "last mutation" state after an async UI hop would collapse a
# Close-other-surfaces burst into repeated copies of its final event.
Assert-Contains $surfaceStack 'Windows::Foundation::Collections::ValueSet change' `
    'Surface lifecycle changes do not carry an immutable per-event snapshot.'
Assert-Contains $surfaceStack 'change\.Insert\(L"surface_id"' `
    'Surface lifecycle snapshots omit the stable terminal session identity.'
Assert-Contains $surfaceStack '_raiseSurfaceChanged\(L"created"' `
    'Surface creation does not emit an explicit lifecycle event.'
Assert-Contains $surfaceStack '_raiseSurfaceChanged\([\s\S]*?L"activated"' `
    'Surface activation does not emit an explicit lifecycle event.'
Assert-Contains $surfaceStack '_raiseSurfaceChanged\(L"closed"' `
    'Surface close does not emit an explicit lifecycle event.'
Assert-Contains $surfaceStack '_raiseSurfaceChanged\([\s\S]*?L"moved"' `
    'Surface moves do not emit an explicit lifecycle event.'
Assert-Contains $tabImplementation 'const auto changeSnapshot = change\.try_as<winrt::Windows::Foundation::Collections::ValueSet>\(\)' `
    'Tab lifecycle routing does not retain the immutable surface event snapshot.'
Assert-Contains $terminalPageImplementation '_RaiseProtocolEvent\(method, params\)' `
    'Surface lifecycle changes are not published through the native protocol event bridge.'

# Agent-operations invariant: the workspace dashboard is a persistent,
# focused-workspace projection of the WTA control plane. It must not regress to
# a modal all-workspace list or mutate team state files directly.
Assert-Contains $terminalPageXaml 'x:Name="WorkspaceFleetOverlay"[\s\S]*?Visibility="Collapsed"' `
    'Agents & Tasks is not implemented as a persistent in-page workspace overlay.'
Assert-Contains $terminalPageXaml 'x:Name="WorkspaceFleetQueuedTasks"' `
    'Agents & Tasks has no queued task projection.'
Assert-Contains $terminalPageXaml 'x:Name="WorkspaceFleetRunningTasks"' `
    'Agents & Tasks has no running task projection.'
Assert-Contains $terminalPageXaml 'x:Name="WorkspaceFleetCompletedTasks"' `
    'Agents & Tasks has no completed task projection.'
Assert-NotContains $terminalPageXaml 'x:Name="WorkspaceFleetDialog"' `
    'The obsolete modal agent fleet duplicated the persistent Agents & Tasks dashboard.'
Assert-Contains $workspaceSidebar 'const auto focused = _GetFocusedTabImpl\(\);[\s\S]*?_WorkspaceSidebarMetadataFor\(focused\)' `
    'Agents & Tasks is not scoped to the currently focused workspace.'
Assert-Contains $workspaceSidebar 'context\["teams"\]' `
    'Workspace metadata does not consume native team snapshots from WTA context.'
Assert-Contains $workspaceSidebar 'context\["teams"\][\s\S]*?metadata\.tasks\.emplace_back' `
    'Workspace metadata discards native team task snapshots.'
Assert-Contains $workspaceSidebar 'std::wstring\{ L"team " \} \+ std::wstring\{ arguments \}' `
    'Dashboard actions do not delegate to the canonical wta team CLI.'
Assert-NotContains $workspaceSidebar 'state\.json|events\.jsonl' `
    'The UI directly references native team persistence files instead of WTA commands.'
Assert-Contains $terminalPageXaml 'x:Name="WorkspaceTeamComposerDialog"[\s\S]*?x:Name="WorkspaceTeamWorkerAgent"[\s\S]*?Text="codex"' `
    'Agents & Tasks cannot create a Codex-first native worker.'
Assert-Contains $workspaceSidebar 'L"team create --root "[\s\S]*?L" --workspace-id "' `
    'New native teams are not bound to the focused workspace stable ID.'
Assert-Contains $workspaceSidebar 'L"team add-worker --root "' `
    'The dashboard cannot launch native agent workers.'
Assert-Contains $workspaceSidebar 'L"team add-task --root "' `
    'The dashboard cannot create durable native tasks.'

if ($failures.Count -gt 0) {
    $failures | ForEach-Object { Write-Error $_ }
    exit 1
}

Write-Host 'Workspace navigation invariants verified.'
