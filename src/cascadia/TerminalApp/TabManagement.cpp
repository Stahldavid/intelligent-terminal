// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.
//
// This file contains much of the code related to tab management for the
// TerminalPage. Things like opening new tabs, selecting different tabs,
// switching tabs, should all be handled in this file. Hypothetically, in the
// future, the contents of this file could be moved to a separate class
// entirely.
//

#include "pch.h"
#include "TerminalPage.h"
#include "Utils.h"
#include "../../types/inc/utils.hpp"
#include "../../inc/til/string.h"
#include "../inc/WtaProcess.h"
#include "../WinRTUtils/inc/WtExeUtils.h"
#include <til/io.h>
#include <json/json.h>

#include "AgentPaneContent.h"
#include "BrowserPaneContent.h"
#include "AgentPaneLog.h"
#include "SharedWta.h"
#include "TabRowControl.h"
#include "DebugTapConnection.h"
#include "DesktopNotification.h"
#include "..\TerminalSettingsModel\FileUtils.h"
#include "../TerminalSettingsAppAdapterLib/TerminalSettings.h"

#include <shlobj.h>
#include <chrono>

using namespace winrt;
using namespace winrt::Windows::Foundation::Collections;
using namespace winrt::Windows::UI::Xaml;
using namespace winrt::Windows::UI::Xaml::Controls;
using namespace winrt::Windows::UI::Core;
using namespace winrt::Windows::System;
using namespace winrt::Windows::ApplicationModel::DataTransfer;
using namespace winrt::Windows::UI::Text;
using namespace winrt::Windows::Storage;
using namespace winrt::Windows::Storage::Pickers;
using namespace winrt::Windows::Storage::Provider;
using namespace winrt::Microsoft::Terminal;
using namespace winrt::Microsoft::Terminal::Control;
using namespace winrt::Microsoft::Terminal::TerminalConnection;
using namespace winrt::Microsoft::Terminal::Settings::Model;
using namespace ::TerminalApp;
using namespace ::Microsoft::Console;

namespace winrt
{
    namespace MUX = Microsoft::UI::Xaml;
    namespace WUX = Windows::UI::Xaml;
    using IInspectable = Windows::Foundation::IInspectable;
}

namespace winrt::TerminalApp::implementation
{
    // Method Description:
    // - Open a new tab. This will create the TerminalControl hosting the
    //   terminal, and add a new Tab to our list of tabs. The method can
    //   optionally be provided a NewTerminalArgs, which will be used to create
    //   a tab using the values in that object.
    // Arguments:
    // - newTerminalArgs: An object that may contain a blob of parameters to
    //   control which profile is created and with possible other
    //   configurations. See TerminalSettings::CreateWithNewTerminalArgs for more details.
    // - existingConnection: An optional connection that is already established to a PTY
    //   for this tab to host instead of creating one.
    //   If not defined, the tab will create the connection.
    HRESULT TerminalPage::_OpenNewTab(const INewContentArgs& newContentArgs, bool openInBackground)
    try
    {
        if (const auto& newTerminalArgs{ newContentArgs.try_as<NewTerminalArgs>() })
        {
            const auto profile{ _settings.GetProfileForArgs(newTerminalArgs) };
            // GH#11114: GetProfileForArgs can return null if the index is higher
            // than the number of available profiles.
            if (!profile)
            {
                return S_FALSE;
            }
            const auto settings{ Settings::TerminalSettings::CreateWithNewTerminalArgs(_settings, newTerminalArgs) };

            // Try to handle auto-elevation
            if (_maybeElevate(newTerminalArgs, settings, profile))
            {
                return S_OK;
            }
            // We can't go in the other direction (elevated->unelevated)
            // unfortunately. This seems to be due to Centennial quirks. It works
            // unpackaged, but not packaged.
        }

        // This call to _MakePane won't return nullptr, we already checked that
        // case above with the _maybeElevate call.
        _CreateNewTabFromPane(_MakePane(newContentArgs, nullptr), -1, openInBackground);
        return S_OK;
    }
    CATCH_RETURN();

    // Method Description:
    // - Sets up state, event handlers, etc on a tab object that was just made.
    // Arguments:
    // - newTabImpl: the uninitialized tab.
    // - insertPosition: Optional parameter to indicate the position of tab.
    void TerminalPage::_InitializeTab(winrt::com_ptr<Tab> newTabImpl, uint32_t insertPosition, bool openInBackground)
    {
        newTabImpl->Initialize();

        // If insert position is not passed, calculate it
        if (insertPosition == -1)
        {
            insertPosition = _tabs.Size();
            if (_settings.GlobalSettings().NewTabPosition() == NewTabPosition::AfterCurrentTab)
            {
                auto currentTabIndex = _GetFocusedTabIndex();
                if (currentTabIndex.has_value())
                {
                    insertPosition = currentTabIndex.value() + 1;
                }
            }
        }

        // Add the new tab to the list of our tabs.
        _tabs.InsertAt(insertPosition, *newTabImpl);
        _mruTabs.Append(*newTabImpl);

        newTabImpl->SetDispatch(*_actionDispatch);
        newTabImpl->SetActionMap(_settings.ActionMap());

        // Give the tab its index in the _tabs vector so it can manage its own SwitchToTab command.
        _UpdateTabIndices();

        // Hookup our event handlers to the new terminal
        _RegisterTabEvents(*newTabImpl);

        // Don't capture a strong ref to the tab. If the tab is removed as this
        // is called, we don't really care anymore about handling the event.
        auto weakTab = make_weak(newTabImpl);

        // When the tab's active pane changes, we'll want to lookup a new icon
        // for it. The Title change will be propagated upwards through the tab's
        // PropertyChanged event handler.
        newTabImpl->ActivePaneChanged({ get_weak(), &TerminalPage::_activePaneChanged });
        newTabImpl->NewSurfaceRequested([weakThis{ get_weak() }](
                                            std::shared_ptr<Pane> targetPane,
                                            const INewContentArgs& contentArgs) {
            if (const auto page = weakThis.get())
            {
                page->_OpenNewSurface(targetPane, contentArgs);
            }
        });
        newTabImpl->SurfaceActionRequested([weakThis{ get_weak() }](
                                               std::shared_ptr<Pane> targetPane,
                                               const ActionAndArgs& action) {
            if (const auto page = weakThis.get())
            {
                if (targetPane)
                {
                    targetPane->FocusPane(targetPane);
                }
                page->_actionDispatch->DoAction(action);
            }
        });
        newTabImpl->SurfaceCollectionChanged(
            [weakThis{ get_weak() }, weakTab](
                std::shared_ptr<Pane> targetPane,
                const Windows::Foundation::Collections::ValueSet& change) {
                if (const auto page = weakThis.get())
                {
                    if (const auto tab = weakTab.get())
                    {
                        page->_NotifySurfaceLifecycleChanged(tab, targetPane, change);
                    }
                }
            });

        // The RaiseVisualBell event has been bubbled up to here from the pane,
        // the next part of the chain is bubbling up to app logic, which will
        // forward it to app host.
        newTabImpl->TabRaiseVisualBell([weakTab, weakThis{ get_weak() }]() {
            auto page{ weakThis.get() };
            auto tab{ weakTab.get() };

            if (page && tab)
            {
                page->RaiseVisualBell.raise(nullptr, nullptr);
            }
        });

        // When a tab requests a desktop toast notification, send the toast
        // and handle activation by summoning this window and switching to the tab.
        newTabImpl->TabToastNotificationRequested([weakThis{ get_weak() }, weakTab{ newTabImpl->get_weak() }](const winrt::hstring& title, const winrt::hstring& body, const winrt::TerminalApp::IPaneContent& content) {
            if (const auto page{ weakThis.get() })
            {
                if (const auto tab{ weakTab.get() })
                {
                    page->_MarkWorkspaceUnread(tab->StableId(), body.empty() ? title : body);
                    page->_SendDesktopNotification(title, body, tab, content);
                }
            }
        });

        auto tabViewItem = newTabImpl->TabViewItem();
        _tabView.TabItems().InsertAt(insertPosition, tabViewItem);
        _RefreshWorkspaceSidebar(true);

        // Set this tab's icon to the icon from the content
        _UpdateTabIcon(*newTabImpl);

        tabViewItem.PointerPressed({ this, &TerminalPage::_OnTabPointerPressed });

        // When the tab requests close, try to close it (prompt for approval, if required)
        newTabImpl->CloseRequested([weakTab, weakThis{ get_weak() }](auto&& /*s*/, auto&& /*e*/) {
            auto page{ weakThis.get() };
            auto tab{ weakTab.get() };

            if (page && tab)
            {
                page->_HandleCloseTabRequested(*tab);
            }
        });

        // When the tab is closed, remove it from our list of tabs.
        newTabImpl->Closed([weakTab, weakThis{ get_weak() }](auto&& /*s*/, auto&& /*e*/) {
            const auto page = weakThis.get();
            const auto tab = weakTab.get();

            if (page && tab)
            {
                page->_RemoveTab(*tab);
            }
        });

        // The tab might want us to toss focus into the control, especially when
        // transient UIs (like the context menu, or the renamer) are dismissed.
        newTabImpl->RequestFocusActiveControl([weakThis{ get_weak() }]() {
            if (const auto page{ weakThis.get() })
            {
                page->_FocusCurrentTab(false);
            }
        });

        // This kicks off TabView::SelectionChanged, in response to which
        // we'll attach the terminal's Xaml control to the Xaml root.
        if (!openInBackground)
        {
            _tabView.SelectedItem(tabViewItem);
        }
        else
        {
            // Add to visual tree hidden so TermControl initializes
            // (gets layout, creates TextBuffer, starts connection).
            // Cleaned up by _UpdatedSelectedTab on next tab switch.
            auto content = newTabImpl->Content();
            content.Opacity(0);
            content.IsHitTestVisible(false);
            _tabContent.Children().Append(content);
        }

        // Per-tab model: pre-warm a stashed agent pane on every new terminal
        // tab. The helper conpty child is spawned but the pane is immediately
        // stashed via `Tab::StashAgentPane`, so the user only sees the
        // terminal pane. Toggling the agent pane (`Ctrl+Shift+.` /
        // `Ctrl+Shift+/` / bottom-bar button) is just a stash/restore.
        // The point of pre-warming is autofix: autofix routes through the
        // agent helper, and gating it on "user has opened the pane at least
        // once" silently broke autofix on every fresh tab. With pre-warm,
        // autofix works on every tab from the moment the tab opens.
        //
        // The actual spawn is deferred to the same low-priority dispatcher
        // tick as the cross-window drag rename walk below — that way
        // (a) drag-in tabs that arrive with their own agent pane skip the
        // pre-warm (see the `agentLeavesSeen == 0` guard there), and
        // (b) tab initialization isn't blocked on conpty + helper spawn.

        // Cross-window agent-pane drag — finalize the rename and re-wire
        // bottom-bar events for any agent pane that arrived via the drag-in
        // path. `_MakeTerminalPane` re-wraps the ContentId-reattached pane
        // into AgentPaneContent and stashes the source StableId.
        //
        // CRITICAL: the agent pane is added to the Tab via a SUBSEQUENT
        // SplitPane action (cross-window drag serializes as NewTab + one
        // SplitPane per extra pane). At the moment _InitializeTab runs, the
        // Tab contains only the first pane — the agent pane SplitPane hasn't
        // executed yet. A synchronous walk here would miss the agent pane
        // entirely, `tab_renamed` would never fire, and the helper would keep
        // owning the old (now-gone) tab id; C++ would then immediately stash
        // the just-arrived agent pane based on the helper's stale
        // pane_open=false state, and the user sees "agent pane gone after
        // drag".
        //
        // Defer the walk to a low-priority dispatcher tick so subsequent
        // SplitPane actions land first. Idempotent: a regular new tab (no
        // agent pane) just no-ops here.
        if (auto dispatcher = winrt::Windows::System::DispatcherQueue::GetForCurrentThread())
        {
            auto weakSelf = get_weak();
            auto weakTab = make_weak(newTabImpl);
            dispatcher.TryEnqueue(winrt::Windows::System::DispatcherQueuePriority::Low, [weakSelf, weakTab]() {
                const auto self = weakSelf.get();
                const auto tabImplCom = weakTab.get();
                if (!self || !tabImplCom)
                {
                    return;
                }
                const auto rootPane = tabImplCom->GetRootPane();
                if (!rootPane)
                {
                    return;
                }
                const auto newTabId = tabImplCom->StableId();
                int agentLeavesSeen = 0;
                int isAgentPaneLeaves = 0;
                rootPane->WalkTree([self, &tabImplCom, &newTabId, &agentLeavesSeen, &isAgentPaneLeaves](const std::shared_ptr<Pane>& p) -> void {
                    if (!p)
                    {
                        return;
                    }
                    if (p->IsAgentPane())
                    {
                        ++isAgentPaneLeaves;
                    }
                    if (p->GetContent() && p->GetContent().try_as<winrt::TerminalApp::AgentPaneContent>())
                    {
                        ++agentLeavesSeen;
                    }
                    if (!p->IsAgentPane())
                    {
                        return;
                    }
                    const auto content = p->GetContent().try_as<winrt::TerminalApp::AgentPaneContent>();
                    if (!content)
                    {
                        return;
                    }
                    const auto impl = winrt::get_self<winrt::TerminalApp::implementation::AgentPaneContent>(content);
                    if (!impl)
                    {
                        return;
                    }
                    self->_WireAgentPaneEvents(content, tabImplCom);

                    if (const auto sourceProfileGuid = impl->TakePendingAgentSourceProfileGuid())
                    {
                        tabImplCom->AgentSourceProfileGuid(*sourceProfileGuid);
                    }
                    const auto oldTabId = impl->TakePendingRenameFromTabId();
                    if (oldTabId.empty() || oldTabId == newTabId)
                    {
                        _agentPaneLog(
                            std::string{ "_InitializeTab(deferred): agent pane found but skipping tab_renamed (oldEmpty=" } +
                            (oldTabId.empty() ? "true" : "false") + " sameAsNew=" +
                            (oldTabId == newTabId ? "true" : "false") + " new=" +
                            winrt::to_string(newTabId) + ")");
                        return;
                    }

                    Json::Value evt;
                    evt["type"] = "event";
                    evt["method"] = "tab_renamed";
                    Json::Value params;
                    params["old_tab_id"] = winrt::to_string(oldTabId);
                    params["new_tab_id"] = winrt::to_string(newTabId);
                    // Dest window id — helper updates stale self.window_id
                    // when rekeying (see app.rs tab_renamed handler).
                    params["window_id"] = std::to_string(self->_WindowProperties.WindowId());
                    evt["params"] = params;
                    Json::StreamWriterBuilder wb;
                    wb["indentation"] = "";
                    const auto payload = winrt::to_hstring(Json::writeString(wb, evt));
                    _agentPaneLog(
                        std::string{ "_InitializeTab(deferred): emitting tab_renamed old=" } +
                        winrt::to_string(oldTabId) + " new=" + winrt::to_string(newTabId));
                    self->ProtocolVtSequenceReceived.raise(*self, payload);
                });
                _agentPaneLog(
                    std::string{ "_InitializeTab(deferred): post-walk summary new=" } +
                    winrt::to_string(newTabId) +
                    " isAgentPaneLeaves=" + std::to_string(isAgentPaneLeaves) +
                    " agentLeavesSeen=" + std::to_string(agentLeavesSeen));

                // Pre-warm a stashed agent pane on this tab so the helper is
                // running from the start (autofix needs it). Skipped if the
                // tab already arrived with an agent pane via cross-window
                // drag-in that landed before this deferred tick fired
                // (`agentLeavesSeen > 0`). `_AutoCreateHiddenAgentPaneShared`
                // itself short-circuits if wta isn't available or policy
                // blocks all agents, and the early-return on
                // `GetActiveTerminalControl() == null` skips the settings tab.
                //
                // NOTE on the race with cross-window drag-in: pre-warm fires
                // here unconditionally (when `agentLeavesSeen == 0`), even
                // though a drag-in's SplitPane action might land AFTER this
                // tick and add a second AgentPaneContent on the same tab.
                // The de-duplication happens on the drag-in side instead:
                // `_MakeTerminalPane`'s re-wrap path closes any existing
                // agent pane on the destination tab before installing its
                // own. That's a per-tab decision (we know FOR SURE the drag
                // is targeting this specific tab because the focused tab is
                // its destination), so it doesn't suffer the false-positive
                // problem a global "is any drag in flight?" check would
                // have (window-A-drag would erroneously block window-B's
                // unrelated new-tab pre-warm).
                if (agentLeavesSeen == 0)
                {
                    _agentPaneLog(
                        std::string{ "_InitializeTab(deferred): pre-warming stashed agent pane on tab " } +
                        winrt::to_string(newTabId));
                    self->_AutoCreateHiddenAgentPaneShared(tabImplCom, /*intoSessionsView*/ false, /*autoStash*/ true);
                }
                else
                {
                    // Cross-window drag-in path: an agent pane arrived from
                    // another window and we just wired `_WireAgentPaneEvents`
                    // on it (via the walk above). Refresh the bottom bar
                    // explicitly so it picks up the autofix-state cache the
                    // helper already populated on this AgentPaneContent.
                    self->_UpdateBottomBarState();
                }
            });
        }
    }

    // Create another native terminal session inside an existing pane. We use
    // the same factory as ordinary panes so profiles, shell integration,
    // environment setup and TerminalProtocol registration remain canonical.
    // Only the resulting IPaneContent is transferred into the target stack;
    // no second workspace/tab object is created.
    void TerminalPage::_OpenNewSurface(
        const std::shared_ptr<Pane>& targetPane,
        const INewContentArgs& contentArgs)
    {
        if (!targetPane || !targetPane->GetSurfaceStack())
        {
            return;
        }

        if (const auto marker = contentArgs.try_as<NewTerminalArgs>())
        {
            constexpr std::wstring_view prefix{ L"__intellterm_managed_surface_v1__|" };
            constexpr std::wstring_view browserPrefix{ L"__intellterm_browser_surface_v1__|" };
            const std::wstring_view command{ marker.Commandline() };
            if (command.starts_with(prefix))
            {
                const auto payload = command.substr(prefix.size());
                const auto separator = payload.find(L'|');
                if (separator != std::wstring_view::npos &&
                    separator > 0 &&
                    separator + 1 < payload.size())
                {
                    _OpenManagedAgentSurface(
                        targetPane,
                        winrt::hstring{ payload.substr(0, separator) },
                        winrt::hstring{ payload.substr(separator + 1) });
                }
                return;
            }
            if (command.starts_with(browserPrefix))
            {
                const auto payload = command.substr(browserPrefix.size());
                const auto separator = payload.find(L'|');
                if (separator != std::wstring_view::npos && separator > 0)
                {
                    _OpenBrowserSurface(
                        targetPane,
                        winrt::hstring{ payload.substr(0, separator) },
                        winrt::hstring{ payload.substr(separator + 1) });
                }
                return;
            }
        }

        // A null request is the one-click duplicate path. This must create a
        // new terminal session, not serialize a live-content move. `Content`
        // adds the private ContentId and makes _MakePane reattach the existing
        // control, which previously made the + button appear to create a tab
        // while only moving the active surface through the temporary pane.
        // `None` preserves the active profile/cwd without an attach identity.
        // A profile selected from the SplitButton already arrives as canonical
        // NewTerminalArgs and is used unchanged.
        const auto args = contentArgs ? contentArgs : targetPane->GetTerminalArgsForPane(BuildStartupKind::None);
        const auto sourcePane = _MakePane(args, nullptr);
        if (!sourcePane || !sourcePane->GetSurfaceStack())
        {
            return;
        }

        if (const auto content = sourcePane->DetachActiveSurface())
        {
            targetPane->AddSurface(content);
            targetPane->FocusPane(targetPane);
        }
    }

    safe_void_coroutine TerminalPage::_OpenManagedAgentSurface(
        std::shared_ptr<Pane> targetPane,
        winrt::hstring targetId,
        winrt::hstring agentId)
    try
    {
        auto strong = get_strong();
        const auto tab = _GetFocusedTabImpl();
        if (!tab)
        {
            co_return;
        }
        co_await _CreateManagedAgentSurface(
            std::move(targetPane),
            tab,
            std::move(targetId),
            std::move(agentId));
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_OpenBrowserSurface(
        std::shared_ptr<Pane> targetPane,
        winrt::hstring remoteWorkspaceId,
        winrt::hstring initialUrl)
    try
    {
        auto strong = get_strong();
        const auto tab = _GetFocusedTabImpl();
        if (!tab)
        {
            co_return;
        }
        co_await _CreateBrowserSurface(
            std::move(targetPane),
            tab,
            std::move(remoteWorkspaceId),
            std::move(initialUrl));
    }
    CATCH_LOG();

    Windows::Foundation::IAsyncOperation<Protocol::TabCreationResult> TerminalPage::_CreateBrowserSurface(
        std::shared_ptr<Pane> targetPane,
        winrt::com_ptr<Tab> tab,
        winrt::hstring remoteWorkspaceId,
        winrt::hstring initialUrl)
    {
        auto strong = get_strong();
        Protocol::TabCreationResult result{};
        if (!targetPane || !tab || remoteWorkspaceId.empty() || initialUrl.empty() || !_hostingHwnd)
        {
            co_return result;
        }

        const auto quote = [](const std::wstring_view value) {
            std::wstring result;
            QuoteAndEscapeCommandlineArg(value, result);
            return result;
        };
        const auto generatedGuid = ::Microsoft::Console::Utils::CreateGuid();
        auto surfaceId = std::wstring{
            ::Microsoft::Console::Utils::GuidToString(generatedGuid)
        };
        surfaceId.erase(
            std::remove_if(surfaceId.begin(), surfaceId.end(), [](const wchar_t ch) {
                return ch == L'{' || ch == L'}';
            }),
            surfaceId.end());
        const auto recordId = std::wstring{ L"browser-" } + surfaceId;
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const auto openArgs =
            std::wstring{ L"compute browser open --id " } + quote(recordId) +
            L" --remote-workspace " + quote(remoteWorkspaceId) +
            L" --surface " + quote(surfaceId) +
            L" --url " + quote(initialUrl);
        const auto dispatcher = Dispatcher();
        co_await winrt::resume_background();
        const auto browserOutput =
            ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, openArgs, 30'000);
        Json::Value browser;
        if (!browserOutput.empty())
        {
            Json::CharReaderBuilder reader;
            std::string errors;
            std::istringstream input{ browserOutput };
            Json::parseFromStream(reader, input, &browser, &errors);
        }
        Json::Value proxy;
        if (browser.isObject())
        {
            const auto proxyId =
                winrt::to_hstring(browser.get("proxy_id", "").asString());
            if (!proxyId.empty())
            {
                const auto proxyOutput =
                    ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                        wtaPath,
                        std::wstring{ L"compute proxy get " } + quote(proxyId),
                        5'000);
                if (!proxyOutput.empty())
                {
                    Json::CharReaderBuilder reader;
                    std::string errors;
                    std::istringstream input{ proxyOutput };
                    Json::parseFromStream(reader, input, &proxy, &errors);
                }
            }
        }
        co_await wil::resume_foreground(dispatcher);

        const auto userDataFolder =
            winrt::to_hstring(browser.get("user_data_folder", "").asString());
        const auto browserRecordId =
            winrt::to_hstring(browser.get("browser_surface_id", "").asString());
        const auto canonicalSurfaceId =
            winrt::to_hstring(browser.get("surface_id", "").asString());
        const auto canonicalUrl =
            winrt::to_hstring(browser.get("current_url", "").asString());
        const auto proxyPort = proxy.get("local_port", 0).asUInt();
        if (!browser.isObject() ||
            !proxy.isObject() ||
            browser.get("state", "").asString() != "starting" ||
            proxy.get("state", "").asString() != "ready" ||
            browserRecordId.empty() ||
            canonicalSurfaceId.empty() ||
            userDataFolder.empty() ||
            proxyPort == 0 ||
            proxyPort > std::numeric_limits<uint16_t>::max())
        {
            _ShowControlNoticeDialog(
                L"Remote browser surface",
                L"The isolated WebView2 profile or surface-scoped SSH proxy could not be prepared.");
            co_return result;
        }

        const auto browserContent = winrt::make_self<BrowserPaneContent>();
        browserContent->Initialize(
            reinterpret_cast<uint64_t>(_hostingHwnd.value()),
            browserRecordId,
            canonicalSurfaceId,
            userDataFolder,
            gsl::narrow_cast<uint16_t>(proxyPort),
            canonicalUrl);
        targetPane->AddSurface(*browserContent);
        targetPane->FocusPane(targetPane);
        result.SessionId = generatedGuid;
        result.Pid = 0;
        for (uint32_t tabIndex = 0; tabIndex < _tabs.Size(); ++tabIndex)
        {
            if (_GetTabImpl(_tabs.GetAt(tabIndex)).get() == tab.get())
            {
                result.TabId = tabIndex;
                break;
            }
        }
        co_return result;
    }

    Windows::Foundation::IAsyncOperation<Protocol::TabCreationResult> TerminalPage::_CreateManagedAgentSurface(
        std::shared_ptr<Pane> targetPane,
        winrt::com_ptr<Tab> tab,
        winrt::hstring targetId,
        winrt::hstring agentId)
    {
        auto strong = get_strong();
        Protocol::TabCreationResult result{};
        if (!targetPane || !tab || targetId.empty() || agentId.empty())
        {
            co_return result;
        }

        const auto quote = [](const std::wstring_view value) {
            std::wstring result;
            QuoteAndEscapeCommandlineArg(value, result);
            return result;
        };
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const auto dispatcher = Dispatcher();
        const auto targetArgs = std::wstring{ L"compute target get " } + quote(targetId);
        co_await winrt::resume_background();
        const auto targetOutput =
            ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, targetArgs, 5'000);
        Json::Value target;
        if (!targetOutput.empty())
        {
            Json::CharReaderBuilder reader;
            std::string errors;
            std::istringstream input{ targetOutput };
            Json::parseFromStream(reader, input, &target, &errors);
        }
        co_await wil::resume_foreground(dispatcher);
        if (!target.isObject())
        {
            _ShowControlNoticeDialog(
                L"Managed agent surface",
                L"The selected compute target is unavailable. Discover or repair targets in Agents & Tasks.");
            co_return result;
        }

        const auto provider = winrt::to_hstring(target.get("provider", "").asString());
        const auto& endpoint = target["endpoint"];
        const auto sshAlias = winrt::to_hstring(endpoint.get("ssh_alias", "").asString());
        const auto wslDistro = winrt::to_hstring(endpoint.get("wsl_distro", "").asString());
        const bool remote = provider == L"ssh" || provider == L"azure";

        std::wstring remoteSessionId{ L"surface-" };
        const auto generatedGuid = std::wstring{
            ::Microsoft::Console::Utils::GuidToString(
                ::Microsoft::Console::Utils::CreateGuid())
        };
        for (const auto ch : generatedGuid)
        {
            if (ch != L'{' && ch != L'}')
            {
                remoteSessionId.push_back(ch);
            }
        }

        std::wstring persistentPtyCommand;
        if (remote)
        {
            const auto bootstrapArgs =
                std::wstring{ L"compute node bootstrap " } + quote(targetId);
            const auto ptyArgs =
                std::wstring{ L"compute node pty-command " } + quote(targetId) +
                L" --session " + quote(remoteSessionId);
            co_await winrt::resume_background();
            const auto bootstrapOutput =
                ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                    wtaPath,
                    bootstrapArgs,
                    70'000);
            const auto ptyOutput = bootstrapOutput.empty() ?
                                       std::string{} :
                                       ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                                           wtaPath,
                                           ptyArgs,
                                           12'000);
            co_await wil::resume_foreground(dispatcher);
            if (!ptyOutput.empty())
            {
                Json::Value pty;
                Json::CharReaderBuilder reader;
                std::string errors;
                std::istringstream input{ ptyOutput };
                if (Json::parseFromStream(reader, input, &pty, &errors))
                {
                    persistentPtyCommand =
                        winrt::to_hstring(pty.get("commandline", "").asString());
                }
            }
            if (persistentPtyCommand.empty())
            {
                _ShowControlNoticeDialog(
                    L"Managed agent surface",
                    L"The remote node could not be verified or its persistent PTY command could not be created.");
                co_return result;
            }
        }

        INewContentArgs contentArgs{ nullptr };
        if (provider == L"local")
        {
            // A managed local surface is a new PTY/agent owner. Never use the
            // ContentId-bearing move serialization here: that reattaches the
            // focused terminal, reuses its SessionId and can bind the agent to
            // an unrelated plain surface.
            contentArgs = targetPane->GetTerminalArgsForPane(BuildStartupKind::None);
        }
        else
        {
            NewTerminalArgs terminal;
            std::wstring command;
            if (provider == L"wsl" && !wslDistro.empty())
            {
                command = L"wsl.exe -d ";
                command += quote(wslDistro);
            }
            else if ((provider == L"ssh" || provider == L"azure") && !sshAlias.empty())
            {
                command = persistentPtyCommand;
            }
            else
            {
                _ShowControlNoticeDialog(
                    L"Managed agent surface",
                    L"The target does not expose a launchable local, WSL, or SSH endpoint.");
                co_return result;
            }
            terminal.Commandline(winrt::hstring{ command });
            contentArgs = terminal;
        }

        const auto sourcePane = _MakePane(contentArgs, nullptr);
        if (!sourcePane || !sourcePane->GetSurfaceStack())
        {
            co_return result;
        }
        const auto content = sourcePane->DetachActiveSurface();
        if (!content)
        {
            co_return result;
        }
        targetPane->AddSurface(content);
        targetPane->FocusPane(targetPane);
        const auto surfaceSessionId = targetPane->GetSessionId();
        if (surfaceSessionId == winrt::guid{})
        {
            co_return result;
        }
        Tab::SurfaceAgentRuntime runtime;
        runtime.agentId = agentId;
        runtime.source = provider == L"wsl" ? L"wsl" :
                         (provider == L"ssh" || provider == L"azure") ? L"ssh" :
                                                                      L"host";
        runtime.wslDistro = wslDistro;
        runtime.sshTarget =
            runtime.source == L"ssh" ? targetId : winrt::hstring{};
        runtime.remoteSessionId = winrt::hstring{ remoteSessionId };
        tab->SetSurfaceAgentRuntime(surfaceSessionId, runtime);
        if (runtime.source == L"ssh")
        {
            tab->SetSurfaceRemoteRuntime(
                surfaceSessionId,
                Tab::SurfaceRemoteRuntime{ targetId, winrt::hstring{ remoteSessionId } });
        }

        const bool hadAgentPane = tab->FindAgentPane() != nullptr;
        const bool wasVisible = hadAgentPane && !tab->HasStashedAgentPane();
        if (hadAgentPane)
        {
            _TeardownAgentPane(tab);
        }

        const auto paneId = targetPane->Id().value_or(0);
        std::wstring bindingArgs =
            L"compute binding create --window " +
            quote(winrt::to_hstring(_WindowProperties.WindowId())) +
            L" --workspace " + quote(tab->StableId()) +
            L" --pane " + quote(winrt::to_hstring(paneId)) +
            L" --surface " + quote(winrt::to_hstring(surfaceSessionId)) +
            L" --kind managed_agent --target " + quote(targetId) +
            L" --agent " + quote(agentId) +
            L" --adapter acp --remote-session " + quote(remoteSessionId);
        co_await winrt::resume_background();
        const auto bindingOutput =
            ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                wtaPath,
                bindingArgs,
                12'000);
        co_await wil::resume_foreground(dispatcher);

        if (bindingOutput.empty())
        {
            _ShowControlNoticeDialog(
                L"Managed agent surface",
                remote ?
                    L"The remote node could not be verified or the surface binding failed. The terminal surface remains open as a plain shell." :
                    L"The managed surface binding failed. The terminal surface remains open as a plain shell.");
            tab->ClearSurfaceAgentRuntime(surfaceSessionId);
        }
        else
        {
            _RequestWorkspaceSidebarMetadata(tab, true);
            if (hadAgentPane)
            {
                _AutoCreateHiddenAgentPaneShared(tab, false, !wasVisible);
            }

            for (uint32_t tabIdx = 0; tabIdx < _tabs.Size(); ++tabIdx)
            {
                if (_GetTabImpl(_tabs.GetAt(tabIdx)).get() == tab.get())
                {
                    result.TabId = tabIdx;
                    break;
                }
            }
            result.SessionId = surfaceSessionId;
            // PID is optional diagnostic metadata. The protocol bridge fills
            // it for ordinary surfaces where the helper is available in that
            // translation unit; managed creation must not duplicate that
            // private extraction logic here.
            result.Pid = 0;
        }
        co_return result;
    }

    safe_void_coroutine TerminalPage::_OpenRemoteWorkspace(winrt::hstring targetId)
    try
    {
        auto strong = get_strong();
        if (targetId.empty())
        {
            co_return;
        }

        ContentDialog confirmation;
        confirmation.Title(winrt::box_value(L"Connect remote workspace"));
        TextBlock explanation;
        explanation.Text(
            L"Intelligent Terminal will verify the OpenSSH target, trust its current host key, "
            L"install the signed wta-node helper, and create a persistent remote terminal. "
            L"Only continue if you recognize this target.");
        explanation.TextWrapping(TextWrapping::Wrap);
        confirmation.Content(explanation);
        confirmation.PrimaryButtonText(L"Trust and connect");
        confirmation.CloseButtonText(L"Cancel");
        confirmation.DefaultButton(ContentDialogButton::Primary);
        if (const auto presenter = _dialogPresenter.get())
        {
            const auto result = co_await presenter.ShowDialog(confirmation);
            if (result != ContentDialogResult::Primary)
            {
                co_return;
            }
        }
        else
        {
            co_return;
        }

        const auto quote = [](const std::wstring_view value) {
            std::wstring result;
            QuoteAndEscapeCommandlineArg(value, result);
            return result;
        };
        std::wstring remoteSessionId{ L"surface-" };
        const auto generatedGuid = std::wstring{
            ::Microsoft::Console::Utils::GuidToString(
                ::Microsoft::Console::Utils::CreateGuid())
        };
        for (const auto ch : generatedGuid)
        {
            if (ch != L'{' && ch != L'}')
            {
                remoteSessionId.push_back(ch);
            }
        }
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const auto trustArgs = std::wstring{ L"compute target trust " } + quote(targetId);
        const auto enableArgs = std::wstring{ L"compute target enable " } + quote(targetId);
        const auto bootstrapArgs = std::wstring{ L"compute node bootstrap " } + quote(targetId);
        const auto ptyArgs =
            std::wstring{ L"compute node pty-command " } + quote(targetId) +
            L" --session " + quote(remoteSessionId);

        co_await winrt::resume_background();
        const auto trusted =
            ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, trustArgs, 25'000);
        const auto enabled = trusted.empty() ?
                                 std::string{} :
                                 ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                                     wtaPath, enableArgs, 8'000);
        const auto bootstrapped = enabled.empty() ?
                                      std::string{} :
                                      ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                                          wtaPath, bootstrapArgs, 70'000);
        const auto ptyOutput = bootstrapped.empty() ?
                                   std::string{} :
                                   ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                                       wtaPath, ptyArgs, 12'000);
        co_await wil::resume_foreground(dispatcher);

        Json::Value pty;
        if (!ptyOutput.empty())
        {
            Json::CharReaderBuilder reader;
            std::string errors;
            std::istringstream input{ ptyOutput };
            Json::parseFromStream(reader, input, &pty, &errors);
        }
        const auto commandline =
            winrt::to_hstring(pty.get("commandline", "").asString());
        if (commandline.empty())
        {
            _ShowControlNoticeDialog(
                L"Remote workspace unavailable",
                L"Trust, authentication, bootstrap, or persistent-session setup failed. "
                L"No unverified remote workspace was created.");
            co_return;
        }

        NewTerminalArgs terminal;
        terminal.Commandline(commandline);
        _OpenNewTerminalViaDropdown(terminal);
        const auto tab = _GetFocusedTabImpl();
        const auto pane = tab ? tab->GetActivePane() : nullptr;
        const auto surfaceSessionId = pane ? pane->GetSessionId() : winrt::guid{};
        if (!tab || !pane || surfaceSessionId == winrt::guid{})
        {
            _ShowControlNoticeDialog(
                L"Remote workspace",
                L"The remote process was prepared, but the native workspace could not be created.");
            co_return;
        }

        const auto remoteWorkspaceId =
            winrt::hstring{ std::wstring{ L"remote-workspace-" } + remoteSessionId.substr(8) };
        const auto paneId = pane->Id().value_or(0);
        const auto workspaceArgs =
            std::wstring{ L"compute remote-workspace create --id " } +
            quote(remoteWorkspaceId) +
            L" --window " + quote(winrt::to_hstring(_WindowProperties.WindowId())) +
            L" --workspace " + quote(tab->StableId()) +
            L" --target " + quote(targetId);
        const auto bindingArgs =
            std::wstring{ L"compute binding create --window " } +
            quote(winrt::to_hstring(_WindowProperties.WindowId())) +
            L" --workspace " + quote(tab->StableId()) +
            L" --pane " + quote(winrt::to_hstring(paneId)) +
            L" --surface " + quote(winrt::to_hstring(surfaceSessionId)) +
            L" --kind plain_terminal --target " + quote(targetId) +
            L" --remote-session " + quote(remoteSessionId);

        co_await winrt::resume_background();
        const auto workspaceOutput =
            ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                wtaPath, workspaceArgs, 20'000);
        const auto bindingOutput = workspaceOutput.empty() ?
                                       std::string{} :
                                       ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                                           wtaPath, bindingArgs, 12'000);
        co_await wil::resume_foreground(dispatcher);
        if (workspaceOutput.empty() || bindingOutput.empty())
        {
            _ShowControlNoticeDialog(
                L"Remote workspace diagnostics",
                L"The persistent terminal is open, but its Compute Store registration failed. "
                L"It will behave as an unmanaged remote shell until repaired.");
        }
        else
        {
            tab->SetSurfaceRemoteRuntime(
                surfaceSessionId,
                Tab::SurfaceRemoteRuntime{ targetId, winrt::hstring{ remoteSessionId } });
        }
        _RequestWorkspaceSidebarMetadata(tab, true);
    }
    CATCH_LOG();

    // Method Description:
    // - Create a new tab using a specified pane as the root.
    // Arguments:
    // - pane: The pane to use as the root.
    // - insertPosition: Optional parameter to indicate the position of tab.
    TerminalApp::Tab TerminalPage::_CreateNewTabFromPane(std::shared_ptr<Pane> pane, uint32_t insertPosition, bool openInBackground)
    {
        if (pane)
        {
            auto newTabImpl = winrt::make_self<Tab>(pane);
            _InitializeTab(newTabImpl, insertPosition, openInBackground);
            return *newTabImpl;
        }
        return nullptr;
    }

    // Method Description:
    // - Get the icon of the currently focused terminal control, and set its
    //   tab's icon to that icon.
    // Arguments:
    // - tab: the Tab to update the title for.
    void TerminalPage::_UpdateTabIcon(Tab& tab)
    {
        // Don't change the icon when an agent pane has focus — same as title.
        if (const auto activePane = tab.GetActivePane(); activePane && activePane->IsAgentPane())
        {
            return;
        }
        if (const auto content{ tab.GetActiveContent() })
        {
            const auto& icon{ content.Icon() };
            const auto theme = _settings.GlobalSettings().CurrentTheme();
            const auto iconStyle = (theme && theme.Tab()) ? theme.Tab().IconStyle() : IconStyle::Default;

            tab.UpdateIcon(icon, iconStyle);
        }
    }

    // Method Description:
    // - Handle changes to the tab width set by the user
    void TerminalPage::_UpdateTabWidthMode()
    {
        _tabView.TabWidthMode(_settings.GlobalSettings().TabWidthMode());
    }

    // Method Description:
    // - Handle changes in tab layout.
    void TerminalPage::_UpdateTabView()
    {
        // The tab row should only be visible if:
        // - we're not in focus mode
        // - we're not in full screen, or the user has enabled fullscreen tabs
        // - there is more than one tab, or the user has chosen to always show tabs
        const auto isVisible = !_isInFocusMode &&
                               (!_isFullscreen || _showTabsFullscreen) &&
                               (_settings.GlobalSettings().ShowTabsInTitlebar() ||
                                (_tabs.Size() > 1) ||
                                _settings.GlobalSettings().AlwaysShowTabs());

        if (_tabView)
        {
            // collapse/show the tabs themselves
            _tabView.Visibility(isVisible ? Visibility::Visible : Visibility::Collapsed);
        }
        if (_tabRow)
        {
            // collapse/show the row that the tabs are in.
            // NaN is the special value XAML uses for "Auto" sizing.
            _tabRow.Height(isVisible ? NAN : 0);
        }
    }

    // Method Description:
    // - Duplicates the current focused tab
    void TerminalPage::_DuplicateFocusedTab()
    {
        if (const auto activeTab{ _GetFocusedTabImpl() })
        {
            _DuplicateTab(*activeTab);
        }
    }

    // Method Description:
    // - Duplicates specified tab
    // Arguments:
    // - tab: tab to duplicate
    void TerminalPage::_DuplicateTab(const Tab& tab)
    {
        try
        {
            // TODO: GH#5047 - We're duplicating the whole profile, which might
            // be a dangling reference to old settings.
            //
            // In the future, it may be preferable to just duplicate the
            // current control's live settings (which will include changes
            // made through VT).
            uint32_t insertPosition = _tabs.Size();
            if (_settings.GlobalSettings().NewTabPosition() == NewTabPosition::AfterCurrentTab)
            {
                insertPosition = tab.TabViewIndex() + 1;
            }
            _CreateNewTabFromPane(_MakePane(nullptr, tab, nullptr), insertPosition);

            const auto runtimeTabText{ tab.GetTabText() };
            if (!runtimeTabText.empty())
            {
                if (auto newTab{ _GetFocusedTabImpl() })
                {
                    newTab->SetTabText(runtimeTabText);
                }
            }
        }
        CATCH_LOG();
    }

    // Method Description:
    // - Exports the content of the Terminal Buffer inside the tab
    // Arguments:
    // - tab: tab to export
    safe_void_coroutine TerminalPage::_ExportTab(const Tab& tab, winrt::hstring filepath)
    {
        // This will be used to set up the file picker "filter", to select .txt
        // files by default.
        static constexpr COMDLG_FILTERSPEC supportedFileTypes[] = {
            { L"Text Files (*.txt)", L"*.txt" },
            { L"All Files (*.*)", L"*.*" }
        };
        // An arbitrary GUID to associate with all instances of this
        // dialog, so they all re-open in the same path as they were
        // open before:
        static constexpr winrt::guid clientGuidExportFile{ 0xF6AF20BB, 0x0800, 0x48E6, { 0xB0, 0x17, 0xA1, 0x4C, 0xD8, 0x73, 0xDD, 0x58 } };

        try
        {
            if (const auto control{ tab.GetActiveTerminalControl() })
            {
                auto path = filepath;

                if (path.empty())
                {
                    // GH#11356 - we can't use the UWP apis for writing the file,
                    // because they don't work elevated (shocker) So just use the
                    // shell32 file picker manually.
                    std::wstring filename{ tab.Title() };
                    filename = til::clean_filename(filename);

                    // GH#20188: yield before the dialog so that the Enter from the Command Palette doesn't leak into the terminal.
                    // Low priority, so the Command Palette's close paints first.
                    co_await wil::resume_foreground(Dispatcher(), CoreDispatcherPriority::Low);
                    path = co_await SaveFilePicker(*_hostingHwnd, [filename = std::move(filename)](auto&& dialog) {
                        THROW_IF_FAILED(dialog->SetClientGuid(clientGuidExportFile));
                        try
                        {
                            // Default to the Downloads folder
                            auto folderShellItem{ winrt::capture<IShellItem>(&SHGetKnownFolderItem, FOLDERID_Downloads, KF_FLAG_DEFAULT, nullptr) };
                            dialog->SetDefaultFolder(folderShellItem.get());
                        }
                        CATCH_LOG(); // non-fatal
                        THROW_IF_FAILED(dialog->SetFileTypes(ARRAYSIZE(supportedFileTypes), supportedFileTypes));
                        THROW_IF_FAILED(dialog->SetFileTypeIndex(1)); // the array is 1-indexed
                        THROW_IF_FAILED(dialog->SetDefaultExtension(L"txt"));

                        // Default to using the tab title as the file name
                        THROW_IF_FAILED(dialog->SetFileName((filename + L".txt").c_str()));
                    });
                }
                else
                {
                    // The file picker isn't going to give us paths with
                    // environment variables, but the user might have set one in
                    // the settings. Expand those here.

                    path = winrt::hstring{ wil::ExpandEnvironmentStringsW<std::wstring>(path.c_str()) };
                }

                if (!path.empty())
                {
                    const auto buffer = control.ReadEntireBuffer();
                    til::io::write_utf8_string_to_file_atomic(std::filesystem::path{ std::wstring_view{ path } }, til::u16u8(buffer));
                }
            }
        }
        CATCH_LOG();
    }

    // Method Description:
    // - Record the configuration information of the last closed thing .
    // - Will occasionally prune the list so it doesn't grow infinitely.
    // Arguments:
    // - args: the list of actions to take to remake the pane/tab
    uint64_t TerminalPage::_AddPreviouslyClosedPaneOrTab(std::vector<ActionAndArgs>&& args)
    {
        // Just make sure we don't get infinitely large, but still
        // maintain a large replay buffer.
        if (const auto size = _previouslyClosedPanesAndTabs.size(); size > 150)
        {
            const auto it = _previouslyClosedPanesAndTabs.begin();
            // delete 50 at a time so that we don't have to do an erase
            // of the buffer every time when at capacity.
            _previouslyClosedPanesAndTabs.erase(it, it + (size - 100));
        }

        const auto id = _nextPreviouslyClosedPaneOrTabId++;
        _previouslyClosedPanesAndTabs.emplace_back(PreviouslyClosedPaneOrTabEntry{
            id,
            std::move(args),
        });
        return id;
    }

    // Method Description:
    // - If this window has a name, persist its current workspace layout to
    //   ApplicationState. Intended to be called from the close-pane / close-tab
    //   paths while tab/pane content is still alive (before it gets torn down).
    void TerminalPage::_SaveWorkspaceIfNeeded()
    {
        const auto& windowName = _WindowProperties.WindowName();
        if (!windowName.empty())
        {
            if (const auto layout = GetWindowLayout())
            {
                ApplicationState::SharedInstance().SaveWorkspace(windowName, layout);
            }
        }
    }

    // Method Description:
    // - Removes the tab (both TerminalControl and XAML) after prompting for approval
    // Arguments:
    // - tab: the tab to remove
    // - skipConfirmClose: if true, skip the confirmOnClose check. Used when
    //   an aggregate confirmation has already been shown (i.e. close other tabs)
    winrt::Windows::Foundation::IAsyncAction TerminalPage::_HandleCloseTabRequested(winrt::TerminalApp::Tab tab, bool skipConfirmClose)
    {
        winrt::com_ptr<TerminalPage> strong;

        if (tab.ReadOnly())
        {
            const auto weak = get_weak();

            auto warningResult = co_await _ShowCloseReadOnlyDialog();

            strong = weak.get();

            // If the user didn't explicitly click on close tab - leave
            if (!strong || warningResult != ContentDialogResult::Primary)
            {
                co_return;
            }
        }

        // Skip the per-tab confirmOnClose check when the caller has already
        // shown an aggregate confirmation dialog (e.g. _RemoveTabs).
        if (!skipConfirmClose)
        {
            const auto tabImpl = _GetTabImpl(tab);
            if (tabImpl && _ShouldWarnOnCloseTab(tabImpl))
            {
                const auto weak = get_weak();

                auto warningResult = co_await _ShowConfirmCloseDialog(ConfirmCloseDialogKind::Tab);
                strong = weak.get();
                if (!strong || warningResult != ContentDialogResult::Primary)
                {
                    co_return;
                }
            }
        }

        auto t = winrt::get_self<implementation::Tab>(tab);
        auto actions = t->BuildStartupActions(BuildStartupKind::None);
        const auto historyId = _AddPreviouslyClosedPaneOrTab(std::move(actions));
        if (const auto tabImpl = _GetTabImpl(tab))
        {
            _workspaceSidebarPendingHistoryIds[std::wstring{ tabImpl->StableId() }] = historyId;
        }

        // Per-tab model: each tab owns its own agent pane. Closing a tab
        // takes its agent pane with it — no rescue needed.

        // If this is the last tab in a named window, persist the workspace
        // layout now while tab content is still alive. After tab.Close()
        // the pane content will be torn down by the time _RemoveTab runs.
        if (_tabs.Size() == 1)
        {
            _SaveWorkspaceIfNeeded();
        }

        tab.Close();
    }

    // Removes the tab (both TerminalControl and XAML).
    // NOTE: Don't call this directly, but rather `tab.Close()`.
    // - movingAway: true when this _RemoveTab is the tail of a cross-window
    //   move (the tab's content is being reattached in another window via
    //   ContentId). In that case we MUST NOT fire `tab_closed` to wta —
    //   doing so would drop the helper's TabSession (messages + ACP session
    //   id) before the target window has a chance to emit `tab_renamed`,
    //   leaving the dragged agent pane with a fresh session and no history.
    //   The target window's `_InitializeTab` will broadcast `tab_renamed`,
    //   which rekeys the helper-side state under the new StableId.
    void TerminalPage::_RemoveTab(const winrt::TerminalApp::Tab& tab, bool movingAway)
    {
        uint32_t tabIndex{};
        if (!_tabs.IndexOf(tab, tabIndex))
        {
            // The tab is already removed
            return;
        }

        if (!movingAway)
        {
            _RecordRecentlyClosedWorkspace(tab);
        }

        // We use _removing flag to suppress _OnTabSelectionChanged events
        // that might get triggered while removing
        _removing = true;
        auto unsetRemoving = wil::scope_exit([&]() noexcept { _removing = false; });

        const auto focusedTabIndex{ _GetFocusedTabIndex() };

        // Capture the stable id before Shutdown clears state, then tell wta
        // to drop the matching TabSession so a future tab that reuses any
        // index slot starts with a clean conversation.
        winrt::hstring closedTabStableId{};
        size_t agentPanesOnTab = 0;
        std::shared_ptr<Pane> rootPaneForClose{};
        if (const auto tabImpl = _GetTabImpl(tab))
        {
            closedTabStableId = tabImpl->StableId();

            // Count agent panes on this tab BEFORE `tab.Shutdown()` runs.
            // We need this for the `SharedWta::ReleasePane` decrement
            // below — see the long comment after `tab.Shutdown()`.
            if (const auto rootPane = tabImpl->GetRootPane())
            {
                rootPaneForClose = rootPane;
                rootPane->WalkTree([&agentPanesOnTab](const std::shared_ptr<Pane>& p) -> void {
                    if (p && p->IsAgentPane())
                    {
                        ++agentPanesOnTab;
                    }
                });
            }
        }

        // Notify wta of every terminal pane in this tab BEFORE
        // `tab.Shutdown()` destroys their controls. Tab shutdown goes
        // through `Pane::Shutdown` -> `_setPaneContent(nullptr)` which
        // doesn't fire `Pane::Closed` and doesn't drive the connection
        // through its Closed state with our listener attached, so the
        // normal ConnectionStateChanged bridge never emits
        // `connection_state:closed`. Explicit emit here is what lets
        // wta demote agent-session rows bound to the tab's panes to
        // Ended on tab close (the `_HandleClosePaneRequested`
        // counterpart covers single-pane Ctrl+Shift+W).
        //
        // Skipped for `movingAway` — a cross-window tab drag is NOT a
        // close: the tab's panes are reattached in the target window via
        // ContentId, keeping the same connection `SessionId`. Emitting
        // `connection_state:closed` here would send wta a `PaneClosed`
        // for those (still-live) panes, flipping any agent session bound
        // to them (e.g. a `copilot` CLI a user ran in a shell pane) to
        // Ended/Historical even though the session is alive in the new
        // window. The registry is shared master-side across every window
        // in this WT process, so the moved session stays Live for both
        // the old and new window when we don't fire this. Mirrors the
        // `movingAway` guard on `_NotifyAgentTabClosed` below.
        if (!movingAway)
        {
            _NotifyPanesClosing(rootPaneForClose);
        }

        // NOTE: Workspace persistence for named windows used to live here,
        // but by the time _RemoveTab runs the pane content may already be
        // torn down (e.g. from the close-pane path). Instead, workspace
        // saves are handled earlier:
        //  - Close-pane (last pane): in _HandleClosePaneRequested
        //  - Close-tab: in _HandleCloseTabRequested

        // Removing the tab from the collection should destroy its control and disconnect its connection,
        // but it doesn't always do so. The UI tree may still be holding the control and preventing its destruction.
        tab.Shutdown();

        if (!movingAway)
        {
            _NotifyAgentTabClosed(closedTabStableId);

            // Preexisting latent leak (made worse by pre-warm): tab close
            // goes through `Tab::Shutdown` → `Pane::Shutdown`, which only
            // calls `_setPaneContent(nullptr)` on each leaf — it does NOT
            // raise `Pane::Closed`. The agent pane's `Pane::Closed` handler
            // registered in `_AutoCreateHiddenAgentPaneShared` calls
            // `SharedWta::ReleasePane()`, so without that event firing the
            // refcount never drops on tab close. With pre-warm every new
            // tab adds 1 to the refcount; closing tabs never decrements,
            // so the master process is kept alive past its last live pane
            // (only `~SharedWta` at process exit truly cleans it up).
            // Compensate by manually releasing once per agent pane that
            // was on the tab — equivalent to what the missed `Closed`
            // events would have done. Skipped for `movingAway` because
            // the helper survives a cross-window drag (the target window's
            // re-wrapped pane is the new owner), so decrementing here
            // would prematurely zero the refcount and tear down the
            // master that the dragged pane still depends on.
            for (size_t i = 0; i < agentPanesOnTab; ++i)
            {
                winrt::TerminalApp::implementation::SharedWta::Instance().ReleasePane();
            }
        }

        uint32_t mruIndex{};
        if (_mruTabs.IndexOf(tab, mruIndex))
        {
            _mruTabs.RemoveAt(mruIndex);
        }

        if (tab == _settingsTab)
        {
            _settingsTab = nullptr;
        }

        if (_stashed.draggedTab && *_stashed.draggedTab == tab)
        {
            _stashed.draggedTab = nullptr;
        }

        _tabs.RemoveAt(tabIndex);
        _tabView.TabItems().RemoveAt(tabIndex);
        _UpdateTabIndices();
        if (!closedTabStableId.empty())
        {
            _workspaceSidebarMetadata.erase(std::wstring{ closedTabStableId });
        }
        _RefreshWorkspaceSidebar(false);

        // To close the window here, we need to close the hosting window.
        if (_tabs.Size() == 0)
        {
            // If we are supposed to save state, make sure we clear it out
            // if the user manually closed all tabs.
            // Do this only if we are the last window; the monarch will notice
            // we are missing and remove us that way otherwise.
            CloseWindowRequested.raise(*this, nullptr);
        }
        else if (focusedTabIndex.has_value() && focusedTabIndex.value() == gsl::narrow_cast<uint32_t>(tabIndex))
        {
            // Manually select the new tab to get focus, rather than relying on TabView since:
            // 1. We want to customize this behavior (e.g., use MRU logic)
            // 2. In fullscreen (GH#5799) and focus (GH#7916) modes the _OnTabItemsChanged is not fired
            // 3. When rearranging tabs (GH#7916) _OnTabItemsChanged is suppressed

            const auto newSelectedTab = _mruTabs.GetAt(0);
            _UpdatedSelectedTab(newSelectedTab);
            _tabView.SelectedItem(newSelectedTab.TabViewItem());

            // Flush any deferred agent settings rebuild now that a
            // terminal tab is active. Per-tab model — no shared pane
            // reconciliation needed.
            _FlushPendingAgentRebuild();
        }

        // GH#5559 - If we were in the middle of a drag/drop, end it by clearing
        // out our state.
        if (_rearranging)
        {
            _rearranging = false;
            _rearrangeFrom = std::nullopt;
            _rearrangeTo = std::nullopt;
        }

    }

    // Method Description:
    // - Sets focus to the tab to the right or left the currently selected tab.
    void TerminalPage::_SelectNextTab(const bool bMoveRight, const Windows::Foundation::IReference<Microsoft::Terminal::Settings::Model::TabSwitcherMode>& customTabSwitcherMode)
    {
        const auto index{ _GetFocusedTabIndex().value_or(0) };
        const auto tabSwitchMode = customTabSwitcherMode ? customTabSwitcherMode.Value() : _settings.GlobalSettings().TabSwitcherMode();
        if (tabSwitchMode == TabSwitcherMode::Disabled)
        {
            auto tabCount = _tabs.Size();
            // Wraparound math. By adding tabCount and then calculating
            // modulo tabCount, we clamp the values to the range [0,
            // tabCount) while still supporting moving leftward from 0 to
            // tabCount - 1.
            const auto newTabIndex = ((tabCount + index + (bMoveRight ? 1 : -1)) % tabCount);
            _SelectTab(newTabIndex);
        }
        else
        {
            const auto p = LoadCommandPalette();
            p.SetTabs(_tabs, _mruTabs);

            // Otherwise, set up the tab switcher in the selected mode, with
            // the given ordering, and make it visible.
            p.EnableTabSwitcherMode(index, tabSwitchMode);
            p.Visibility(Visibility::Visible);
            p.SelectNextItem(bMoveRight);
        }
    }

    // Method Description:
    // - Sets focus to the desired tab. Returns false if the provided tabIndex
    //   is greater than the number of tabs we have.
    // - During startup, we'll immediately set the selected tab as focused.
    // - After startup, we'll dispatch an async method to set the selected
    //   item of the TabView, which will then also trigger a
    //   TabView::SelectionChanged, handled in
    //   TerminalPage::_OnTabSelectionChanged
    // Return Value:
    // true iff we were able to select that tab index, false otherwise
    bool TerminalPage::_SelectTab(uint32_t tabIndex)
    {
        // GH#9369 - if the argument is out of range, then clamp to the number
        // of available tabs. Previously, we'd just silently do nothing if the
        // value was greater than the number of tabs.
        tabIndex = std::clamp(tabIndex, 0u, _tabs.Size() - 1);

        auto tab{ _tabs.GetAt(tabIndex) };
        // GH#11107 - Always just set the item directly first so that if
        // tab movement is done as part of multiple actions following calls
        // to _GetFocusedTab will return the correct tab.
        _tabView.SelectedItem(tab.TabViewItem());

        if (_startupState == StartupState::InStartup)
        {
            _UpdatedSelectedTab(tab);
        }
        else
        {
            _SetFocusedTab(tab);
        }

        return true;
    }

    // Method Description:
    // - This method is called once a tab was selected in tab switcher
    //   We'll use this event to select the relevant tab
    // Arguments:
    // - tab - tab to select
    // Return Value:
    // - <none>
    void TerminalPage::_OnSwitchToTabRequested(const IInspectable& /*sender*/, const winrt::TerminalApp::Tab& tab)
    {
        uint32_t index{};
        if (_tabs.IndexOf(tab, index))
        {
            _SelectTab(index);
        }
    }

    // Method Description:
    // - Returns the index in our list of tabs of the currently focused tab. If
    //      no tab is currently selected, returns nullopt.
    // Return Value:
    // - the index of the currently focused tab if there is one, else nullopt
    std::optional<uint32_t> TerminalPage::_GetFocusedTabIndex() const noexcept
    {
        // GH#1117: This is a workaround because _tabView.SelectedIndex()
        //          sometimes return incorrect result after removing some tabs
        uint32_t focusedIndex;
        if (_tabView.TabItems().IndexOf(_tabView.SelectedItem(), focusedIndex))
        {
            return focusedIndex;
        }
        return std::nullopt;
    }

    // Method Description:
    // - Returns the index in our list of tabs of the currently focused tab. If
    //      no tab is currently selected, returns nullopt.
    // Return Value:
    // - the index of the currently focused tab if there is one, else nullopt
    std::optional<uint32_t> TerminalPage::_GetTabIndex(const TerminalApp::Tab& tab) const noexcept
    {
        uint32_t i;
        if (_tabs.IndexOf(tab, i))
        {
            return i;
        }
        return std::nullopt;
    }

    // Method Description:
    // - returns the currently focused tab. This might return null,
    //   so make sure to check the result!
    winrt::TerminalApp::Tab TerminalPage::_GetFocusedTab() const noexcept
    {
        if (auto index{ _GetFocusedTabIndex() })
        {
            return _tabs.GetAt(*index);
        }
        return nullptr;
    }

    // Method Description:
    // - returns a com_ptr to the currently focused tab implementation. This might return null,
    //   so make sure to check the result!
    winrt::com_ptr<Tab> TerminalPage::_GetFocusedTabImpl() const noexcept
    {
        if (auto tab{ _GetFocusedTab() })
        {
            return _GetTabImpl(tab);
        }
        return nullptr;
    }

    // Method Description:
    // - returns a tab corresponding to a view item. This might return null,
    //   so make sure to check the result!
    winrt::TerminalApp::Tab TerminalPage::_GetTabByTabViewItem(const IInspectable& tabViewItem) const noexcept
    {
        uint32_t tabIndexFromControl{};
        const auto items{ _tabView.TabItems() };
        if (items.IndexOf(tabViewItem, tabIndexFromControl) && tabIndexFromControl < _tabs.Size())
        {
            // If IndexOf returns true, we've actually got an index
            return _tabs.GetAt(tabIndexFromControl);
        }
        return nullptr;
    }

    // Method Description:
    // - An async method for changing the focused tab on the UI thread. This
    //   method will _only_ set the selected item of the TabView, which will
    //   then also trigger a TabView::SelectionChanged event, which we'll handle
    //   in TerminalPage::_OnTabSelectionChanged, where we'll mark the new tab
    //   as focused.
    // Arguments:
    // - tab: tab to focus.
    // Return Value:
    // - <none>
    safe_void_coroutine TerminalPage::_SetFocusedTab(const winrt::TerminalApp::Tab tab)
    {
        // GH#1117: This is a workaround because _tabView.SelectedIndex(tabIndex)
        //          sometimes set focus to an incorrect tab after removing some tabs
        auto weakThis{ get_weak() };

        if (!_tabView.Dispatcher().HasThreadAccess())
        {
            co_await winrt::resume_foreground(_tabView.Dispatcher());
        }

        if (auto page{ weakThis.get() })
        {
            // Make sure the tab was not removed
            uint32_t tabIndex{};
            if (_tabs.IndexOf(tab, tabIndex))
            {
                _tabView.SelectedItem(tab.TabViewItem());
            }
        }
    }

    // Method Description:
    // - Disables read-only mode on pane if the user wishes to close it and read-only mode is enabled.
    // Arguments:
    // - pane: the pane that is about to be closed.
    // Return Value:
    // - bool indicating whether the (read-only) pane can be closed.
    winrt::Windows::Foundation::IAsyncOperation<bool> TerminalPage::_PaneConfirmCloseReadOnly(std::shared_ptr<Pane> pane)
    {
        if (pane->ContainsReadOnly())
        {
            const auto weak = get_weak();

            auto warningResult = co_await _ShowCloseReadOnlyDialog();

            const auto strong = weak.get();

            // If the user didn't explicitly click on close tab - leave
            if (!strong || warningResult != ContentDialogResult::Primary)
            {
                co_return false;
            }

            // Clean read-only mode to prevent additional prompt if closing the pane triggers closing of a hosting tab
            pane->WalkTree([](const auto& p) {
                if (const auto control{ p->GetTerminalControl() })
                {
                    if (control.ReadOnly())
                    {
                        control.ToggleReadOnly();
                    }
                }
            });
        }
        co_return true;
    }

    // Method Description:
    // - Removes the pane from the tab it belongs to.
    // Arguments:
    // - pane: the pane to close.
    void TerminalPage::_HandleClosePaneRequested(std::shared_ptr<Pane> pane)
    {
        winrt::com_ptr<Tab> owningTab;
        for (uint32_t tabIndex = 0; tabIndex < _tabs.Size() && !owningTab; ++tabIndex)
        {
            const auto candidateTab = _GetTabImpl(_tabs.GetAt(tabIndex));
            if (!candidateTab)
            {
                continue;
            }

            if (const auto root = candidateTab->GetRootPane())
            {
                root->WalkTree([&](const std::shared_ptr<Pane>& candidatePane) {
                    if (candidatePane == pane)
                    {
                        owningTab = candidateTab;
                    }
                });
            }
        }

        if (owningTab && owningTab->GetLeafPaneCount() == 1)
        {
            // Closing the final pane closes its tab. Record the complete tab
            // startup actions so both native undo and the sidebar restore the
            // same session, rather than splitting into whichever tab happens
            // to be focused later.
            const auto historyId = _AddPreviouslyClosedPaneOrTab(
                owningTab->BuildStartupActions(BuildStartupKind::None));
            _workspaceSidebarPendingHistoryIds[std::wstring{ owningTab->StableId() }] = historyId;
        }
        else
        {
            // Recreate a pane within its surviving tab. BuildStartupActions
            // returns the first pane and the remaining actions assume that
            // pane has already been created.
            auto state = pane->BuildStartupActions(0, 1, BuildStartupKind::None);
            ActionAndArgs splitPaneAction{};
            splitPaneAction.Action(ShortcutAction::SplitPane);
            SplitPaneArgs splitPaneArgs{ SplitDirection::Automatic, state.firstPane->GetTerminalArgsForPane(BuildStartupKind::None) };
            splitPaneAction.Args(splitPaneArgs);
            state.args.emplace(state.args.begin(), std::move(splitPaneAction));
            _AddPreviouslyClosedPaneOrTab(std::move(state.args));
        }

        // Notify wta of pane closure BEFORE destruction (see
        // `_NotifyPanesClosing` for the revoker-race rationale). Must
        // happen before `pane->Close()` since Close destroys the
        // TermControl and the SessionId becomes unresolvable.
        _NotifyPanesClosing(pane);

        // If this is the last pane on the last tab of a named window, persist
        // the workspace layout now while the pane content is still alive.
        // We can't wait until _RemoveTab, because pane->Close() below will
        // destroy the content before _RemoveTab is reached.
        if (_tabs.Size() == 1)
        {
            if (const auto activeTab{ _GetFocusedTabImpl() })
            {
                if (activeTab->GetLeafPaneCount() == 1)
                {
                    _SaveWorkspaceIfNeeded();
                }
            }
        }

        // If specified, detach before closing to directly update the pane structure
        pane->Close();
    }

    // Method Description:
    // - Close the currently focused pane. If the pane is the last pane in the
    //   tab, the tab will also be closed. This will happen when we handle the
    //   tab's Closed event.
    safe_void_coroutine TerminalPage::_CloseFocusedPane()
    {
        if (const auto activeTab{ _GetFocusedTabImpl() })
        {
            _UnZoomIfNeeded();

            if (const auto pane{ activeTab->GetActivePane() })
            {
                const auto weak = get_weak();

                // Check if we should warn before closing a single pane
                // (only triggers on Always — Automatic doesn't warn for single pane)
                const auto setting = _settings.GlobalSettings().ConfirmOnClose();
                if (setting == ConfirmOnClose::Always)
                {
                    // If this is the last pane, closing it closes the tab,
                    // so use the tab dialog text instead.
                    const auto kind = activeTab->GetLeafPaneCount() == 1 ? ConfirmCloseDialogKind::Tab : ConfirmCloseDialogKind::Pane;
                    auto warningResult = co_await _ShowConfirmCloseDialog(kind);

                    // Hold a strong reference to `this` for the rest of the
                    // method; we may be the last holder after `co_await`.
                    auto strong = weak.get();
                    if (!strong || warningResult != ContentDialogResult::Primary)
                    {
                        co_return;
                    }
                }

                if (co_await _PaneConfirmCloseReadOnly(pane))
                {
                    if (const auto strong = weak.get())
                    {
                        _HandleClosePaneRequested(pane);
                    }
                }
            }
        }
    }

    // Method Description:
    // - Close all panes with the given IDs sequentially.
    // - Shows a single aggregate confirmation dialog upfront if the confirmOnClose setting warrants it.
    // Arguments:
    // - weakTab: weak reference to the tab that the panes belong to.
    // - paneIds: collection of the IDs of the panes that are marked for removal.
    safe_void_coroutine TerminalPage::_ClosePanes(weak_ref<Tab> weakTab, std::vector<uint32_t> paneIds)
    {
        // Show a single aggregate confirmation for closing multiple panes.
        if (_settings.GlobalSettings().ConfirmOnClose() != ConfirmOnClose::Never)
        {
            const auto weak = get_weak();
            auto warningResult = co_await _ShowConfirmCloseDialog(ConfirmCloseDialogKind::MultiplePanes);

            // Hold a strong reference to `this` after the co_await; we may
            // be the last holder if the page was being torn down.
            auto strong = weak.get();
            if (!strong || warningResult != ContentDialogResult::Primary)
            {
                co_return;
            }
        }
        _CloseRemainingPanes(weakTab, std::move(paneIds));
    }

    // Method Description:
    // - Recursively closes panes by ID, chaining each close via the
    //   ClosedByParent callback. Called after confirmation has already
    //   been handled by _ClosePanes.
    // Arguments:
    // - weakTab: weak reference to the tab that the panes belong to
    // - paneIds: remaining pane IDs to close
    void TerminalPage::_CloseRemainingPanes(weak_ref<Tab> weakTab, std::vector<uint32_t> paneIds)
    {
        if (auto strongTab{ weakTab.get() })
        {
            // Close all unfocused panes one by one
            while (!paneIds.empty())
            {
                const auto id = paneIds.back();
                paneIds.pop_back();

                if (const auto pane{ strongTab->GetRootPane()->FindPane(id) })
                {
                    pane->ClosedByParent([ids{ std::move(paneIds) }, weakThis{ get_weak() }, weakTab]() {
                        if (auto strongThis{ weakThis.get() })
                        {
                            strongThis->_CloseRemainingPanes(weakTab, std::move(ids));
                        }
                    });
                    // Close the pane which will eventually trigger the closed by parent event
                    _HandleClosePaneRequested(pane);
                    break;
                }
            }
        }
    }

    // Method Description:
    // - Close the tab at the given index.
    void TerminalPage::_CloseTabAtIndex(uint32_t index)
    {
        if (index >= _tabs.Size())
        {
            return;
        }
        if (auto tab{ _tabs.GetAt(index) })
        {
            _HandleCloseTabRequested(tab);
        }
    }

    // Method Description:
    // - Closes provided tabs one by one
    // - Shows a single aggregate confirmation dialog upfront if the confirmOnClose setting warrants it.
    // Arguments:
    // - tabs - tabs to remove
    safe_void_coroutine TerminalPage::_RemoveTabs(const std::vector<winrt::TerminalApp::Tab> tabs)
    {
        if (tabs.empty())
        {
            co_return;
        }

        // Show a single aggregate confirmation instead of per-tab dialogs.
        const auto weak = get_weak();
        if (_settings.GlobalSettings().ConfirmOnClose() != ConfirmOnClose::Never)
        {
            auto warningResult = co_await _ShowConfirmCloseDialog(ConfirmCloseDialogKind::MultipleTabs);

            // Hold a strong reference to `this` after the co_await so that
            // the for-loop below can safely dispatch on us.
            auto strong = weak.get();
            if (!strong || warningResult != ContentDialogResult::Primary)
            {
                co_return;
            }
        }

        for (auto& tab : tabs)
        {
            winrt::Windows::Foundation::IAsyncAction action{ nullptr };
            if (const auto strong = weak.get())
            {
                action = _HandleCloseTabRequested(tab, /*skipConfirmClose*/ true);
            }

            if (!action)
            {
                co_return;
            }

            co_await action;
        }
    }
    // Method Description:
    // - Responds to changes in the TabView's item list by changing the
    //   tabview's visibility.
    // - This method is also invoked when tabs are dragged / dropped as part of
    //   tab reordering and this method hands that case as well in concert with
    //   TabDragStarting and TabDragCompleted handlers that are set up in
    //   TerminalPage::Create()
    // Arguments:
    // - sender: the control that originated this event
    // - eventArgs: the event's constituent arguments
    void TerminalPage::_OnTabItemsChanged(const IInspectable& /*sender*/, const Windows::Foundation::Collections::IVectorChangedEventArgs& eventArgs)
    {
        if (_rearranging)
        {
            if (eventArgs.CollectionChange() == Windows::Foundation::Collections::CollectionChange::ItemRemoved)
            {
                _rearrangeFrom = eventArgs.Index();
            }

            if (eventArgs.CollectionChange() == Windows::Foundation::Collections::CollectionChange::ItemInserted)
            {
                _rearrangeTo = eventArgs.Index();
            }
        }

        if (const auto p = CommandPaletteElement())
        {
            p.Visibility(Visibility::Collapsed);
        }
        _UpdateTabView();
        _RefreshWorkspaceSidebar(false);
    }

    void TerminalPage::_OnTabPointerPressed(const IInspectable& sender, const Windows::UI::Xaml::Input::PointerRoutedEventArgs& e)
    {
        if (!_tabItemMiddleClickHookEnabled || !e.GetCurrentPoint(nullptr).Properties().IsMiddleButtonPressed())
        {
            return;
        }

        const auto tabViewItem = sender.try_as<MUX::Controls::TabViewItem>();
        if (!tabViewItem || !tabViewItem.CapturePointer(e.Pointer()))
        {
            return;
        }

        _tabItemMiddleClickExited = false;

        _tabItemMiddleClickPointerEntered = tabViewItem.PointerEntered(winrt::auto_revoke, [this](auto&&, auto&& e) {
            _tabItemMiddleClickExited = false;
            e.Handled(true);
        });
        _tabItemMiddleClickPointerExited = tabViewItem.PointerExited(winrt::auto_revoke, [this](auto&&, auto&& e) {
            _tabItemMiddleClickExited = true;
            e.Handled(true);
        });
        _tabItemMiddleClickPointerCaptureLost = tabViewItem.PointerCaptureLost(winrt::auto_revoke, [this](auto&& sender, auto&& e) {
            // The WinUI TabView calls CapturePointer() internally and it's not reference counted,
            // so when it calls ReleasePointerCapture() in its PointerReleased handler,
            // we get a PointerCaptureLost before we receive the PointerReleased event.
            // This makes typical handling of PointerReleased events on our side difficult.
            // Well, whatever, now we just hook PointerCaptureLost because we know WinUI will trigger it.

            _tabItemMiddleClickPointerEntered.revoke();
            _tabItemMiddleClickPointerExited.revoke();
            _tabItemMiddleClickPointerCaptureLost.revoke();

            if (!_tabItemMiddleClickExited && !e.GetCurrentPoint(nullptr).Properties().IsMiddleButtonPressed())
            {
                _OnTabPointerReleasedCloseTab(std::move(sender));
            }

            e.Handled(true);
        });
        e.Handled(true);
    }

    safe_void_coroutine TerminalPage::_OnTabPointerReleasedCloseTab(IInspectable sender)
    {
        // WinUI asynchronously updates its tab view items, so it may happen that we're given a
        // `TabViewItem` that still contains a `Tab` which has actually already been removed.
        // First we must yield once, to flush out whatever TabView is currently doing.
        const auto weak = get_weak();
        co_await wil::resume_foreground(Dispatcher());
        const auto strong = weak.get();
        if (!strong)
        {
            co_return;
        }

        const auto tab = _GetTabByTabViewItem(sender);
        if (!tab)
        {
            co_return;
        }

        // `tab.Shutdown()` in `_RemoveTab()` sets the content to null = This checks if the tab is closed.
        if (tab.Content())
        {
            _HandleCloseTabRequested(tab);
        }
    }

    void TerminalPage::_UpdatedSelectedTab(const winrt::TerminalApp::Tab& tab)
    {
        // Unfocus all the tabs.
        for (const auto& tab : _tabs)
        {
            tab.Focus(FocusState::Unfocused);
        }

        try
        {
            _tabContent.Children().Clear();
            auto content = tab.Content();
            content.Opacity(1.0);
            content.IsHitTestVisible(true);
            _tabContent.Children().Append(content);

            // GH#7409: If the tab switcher is open, then we _don't_ want to
            // automatically focus the new tab here. The tab switcher wants
            // to be able to "preview" the selected tab as the user tabs
            // through the menu, but if we toss the focus to the control
            // here, then the user won't be able to navigate the ATS any
            // longer.
            //
            // When the tab switcher is eventually dismissed, the focus will
            // get tossed back to the focused terminal control, so we don't
            // need to worry about focus getting lost.
            const auto p = CommandPaletteElement();
            if (!p || p.Visibility() != Visibility::Visible)
            {
                tab.Focus(FocusState::Programmatic);
                _UpdateMRUTab(tab);
                _updateAllTabCloseButtons();
            }

            tab.TabViewItem().StartBringIntoView();

            // Raise an event that our title changed
            TitleChanged.raise(*this, nullptr);

            _updateThemeColors();

            auto tabImpl = _GetTabImpl(tab);
            if (tabImpl)
            {
                auto profile = tabImpl->GetFocusedProfile();
                _UpdateBackground(profile);
            }

            // Refresh the bottom bar's *visibility* synchronously here
            // so it tracks the tab type immediately. Tab kind alone
            // (terminal/agent vs Settings/etc.) determines whether the
            // bar is shown at all. Without this call, switching from a
            // terminal tab to the Settings tab leaves the bar visible
            // forever: PR #54 moved bottom-bar refresh entirely onto
            // the wta `agent_state_changed` callback path, but Settings
            // tabs have no helper and never fire that callback.
            //
            // Crucially we use the visibility-only helper rather than
            // the full `_UpdateBottomBarState` — the latter would also
            // recompute the agent-state-dependent UI (toggle lit-state,
            // diagnostics) from the local AgentPaneContent mirror,
            // which can lag wta after a cross-window drag or other
            // helper-state mutations. Letting the subsequent
            // `OnAgentStateChanged` callback own that refresh keeps
            // the bar's agent UI authoritative.
            _UpdateBottomBarVisibility();

            // Bottom-bar refresh is now driven by wta — fire `tab_changed`
            // so wta re-projects this tab's authoritative agent-pane state
            // (`project_active_tab_state` → `agent_state_changed`).
            // `OnAgentStateChanged` applies the snapshot to the local
            // AgentPaneContent mirror and refreshes the bottom bar. Avoids
            // the stale-mirror race that bit after cross-window drag
            // (helper state diverged from local cache).
            if (auto tabImplForNotify = _GetTabImpl(tab))
            {
                auto& sidebarMetadata = _WorkspaceSidebarMetadataFor(tabImplForNotify);
                sidebarMetadata.unread = 0;
                _RequestWorkspaceSidebarMetadata(tabImplForNotify);
                _NotifyAgentTabChanged(tabImplForNotify->StableId());
                _NotifyAgentFocusChanged(tabImplForNotify);
            }

            _RefreshWorkspaceSidebar(false);

            _adjustProcessPriorityThrottled->Run();
        }
        CATCH_LOG();
    }

    void TerminalPage::_UpdateBackground(const winrt::Microsoft::Terminal::Settings::Model::Profile& profile)
    {
        if (profile && _settings.GlobalSettings().UseBackgroundImageForWindow())
        {
            _SetBackgroundImage(profile.DefaultAppearance());
        }
    }

    // Method Description:
    // - Responds to the TabView control's Selection Changed event (to move a
    //      new terminal control into focus) when not in in the middle of a tab rearrangement.
    // Arguments:
    // - sender: the control that originated this event
    // - eventArgs: the event's constituent arguments
    void TerminalPage::_OnTabSelectionChanged(const IInspectable& sender, const WUX::Controls::SelectionChangedEventArgs& /*eventArgs*/)
    {
        if (!_rearranging && !_removing)
        {
            auto tabView = sender.as<MUX::Controls::TabView>();
            auto selectedIndex = tabView.SelectedIndex();
            if (selectedIndex >= 0 && selectedIndex < gsl::narrow_cast<int32_t>(_tabs.Size()))
            {
                const auto tab{ _tabs.GetAt(selectedIndex) };
                _UpdatedSelectedTab(tab);
            }
            // Flush any deferred agent-stack rebuild now that a real
            // terminal tab is active. Per-tab model — no shared pane
            // reconciliation needed.
            _FlushPendingAgentRebuild();
        }
    }

    // Method Description:
    // - Updates all tabs with their current index in _tabs.
    // Arguments:
    // - <none>
    // Return Value:
    // - <none>
    void TerminalPage::_UpdateTabIndices()
    {
        const auto size = _tabs.Size();
        for (uint32_t i = 0; i < size; ++i)
        {
            auto tab{ _tabs.GetAt(i) };
            auto tabImpl{ winrt::get_self<Tab>(tab) };
            tabImpl->UpdateTabViewIndex(i, size);
        }
    }

    // Method Description:
    // - Bumps the tab in its in-order index up to the top of the mru list.
    // Arguments:
    // - tab: tab to bump.
    // Return Value:
    // - <none>
    void TerminalPage::_UpdateMRUTab(const winrt::TerminalApp::Tab& tab)
    {
        uint32_t mruIndex;
        if (_mruTabs.IndexOf(tab, mruIndex))
        {
            if (mruIndex > 0)
            {
                _mruTabs.RemoveAt(mruIndex);
                _mruTabs.InsertAt(0, tab);
            }
        }
    }

    // Method Description:
    // - Moves the tab to another index in the tabs row (if required).
    // Arguments:
    // - currentTabIndex: the current index of the tab to move
    // - suggestedNewTabIndex: the new index of the tab, might get clamped to fit int the tabs row boundaries
    // Return Value:
    // - <none>
    void TerminalPage::_TryMoveTab(const uint32_t currentTabIndex,
                                   const int32_t suggestedNewTabIndex)
    {
        auto newTabIndex = gsl::narrow_cast<uint32_t>(std::clamp<int32_t>(suggestedNewTabIndex, 0, _tabs.Size() - 1));
        if (currentTabIndex != newTabIndex)
        {
            auto tab = _tabs.GetAt(currentTabIndex);
            auto tabViewItem = tab.TabViewItem();
            _tabs.RemoveAt(currentTabIndex);
            _tabs.InsertAt(newTabIndex, tab);
            _UpdateTabIndices();

            _tabView.TabItems().RemoveAt(currentTabIndex);
            _tabView.TabItems().InsertAt(newTabIndex, tabViewItem);
            _tabView.SelectedItem(tabViewItem);
            _RefreshWorkspaceSidebar(false);

            if (auto autoPeer = Automation::Peers::FrameworkElementAutomationPeer::FromElement(*this))
            {
                const auto tabTitle = tab.Title();
                autoPeer.RaiseNotificationEvent(Automation::Peers::AutomationNotificationKind::ActionCompleted,
                                                Automation::Peers::AutomationNotificationProcessing::ImportantMostRecent,
                                                RS_fmt(L"TerminalPage_TabMovedAnnouncement_Direction", tabTitle, newTabIndex + 1),
                                                L"TerminalPageMoveTabWithDirection" /* unique name for this notification category */);
            }
        }
    }

    void TerminalPage::_TabDragStarted(const IInspectable& /*sender*/,
                                       const IInspectable& /*eventArgs*/)
    {
        _rearranging = true;
        _rearrangeFrom = std::nullopt;
        _rearrangeTo = std::nullopt;
    }

    void TerminalPage::_TabDragCompleted(const IInspectable& /*sender*/,
                                         const IInspectable& /*eventArgs*/)
    {
        auto& from{ _rearrangeFrom };
        auto& to{ _rearrangeTo };

        if (from.has_value() && to.has_value() && to != from)
        {
            try
            {
                auto& tabs{ _tabs };
                auto tab = tabs.GetAt(from.value());
                tabs.RemoveAt(from.value());
                tabs.InsertAt(to.value(), tab);
                _UpdateTabIndices();
            }
            CATCH_LOG();
        }

        _rearranging = false;

        if (to.has_value() &&
            *to < gsl::narrow_cast<int32_t>(TabRow().TabView().TabItems().Size()))
        {
            // Selecting the dropped tab
            TabRow().TabView().SelectedIndex(to.value());
        }

        from = std::nullopt;
        to = std::nullopt;
    }

    void TerminalPage::_DismissTabContextMenus()
    {
        for (const auto& tab : _tabs)
        {
            if (tab.TabViewItem().ContextFlyout())
            {
                tab.TabViewItem().ContextFlyout().Hide();
            }
        }
    }

    void TerminalPage::_FocusCurrentTab(const bool focusAlways)
    {
        // We don't want to set focus on the tab if fly-out is open as it will
        // be closed TODO GH#5400: consider checking we are not in the opening
        // state, by hooking both Opening and Open events
        if (focusAlways || !_newTabButton.Flyout().IsOpen())
        {
            // Return focus to the active control
            if (auto tab{ _GetFocusedTab() })
            {
                tab.Focus(FocusState::Programmatic);
                _UpdateMRUTab(tab);
                _updateAllTabCloseButtons();
            }
        }
    }

    bool TerminalPage::_HasMultipleTabs() const
    {
        return _tabs.Size() > 1;
    }

    // Method Description:
    // - Attempts to find and focus the given tab in this window.
    // Arguments:
    // - tab: The tab to focus.
    // Return Value:
    // - true if the tab was found and focused, false otherwise.
    bool TerminalPage::FocusTab(const winrt::TerminalApp::Tab& tab)
    {
        if (const auto tabIndex{ _GetTabIndex(tab) })
        {
            _SelectTab(tabIndex.value());
            return true;
        }
        return false;
    }

    // Method Description:
    // - Sends a desktop toast notification with the given title and body.
    //   When the toast is activated (clicked), the window is summoned and
    //   the originating tab is focused.
    // Arguments:
    // - tabTitle: The title to display in the notification.
    // - body: The body text. If empty, a standard tab-activity message is built.
    // - tab: The tab to switch to when the toast is activated.
    void TerminalPage::_SendDesktopNotification(const winrt::hstring& tabTitle, const winrt::hstring& body, const winrt::com_ptr<Tab>& tab, const winrt::TerminalApp::IPaneContent& content)
    {
        // Don't send a notification if the window is focused and the requesting
        // pane is the active pane. The user is already looking at it.
        if (_activated && tab == _GetFocusedTabImpl())
        {
            if (const auto activePane{ tab->GetActivePane() })
            {
                if (activePane->GetContent() == content)
                {
                    return;
                }
            }
        }

        // Preserve every notification in the workspace/surface unread model,
        // but suppress repeated desktop toasts from a reconnecting or noisy
        // remote process. The cooldown is scoped to the workspace, which is
        // also the remote-host boundary for managed SSH workspaces.
        auto& sidebarMetadata = _WorkspaceSidebarMetadataFor(tab);
        const auto now = static_cast<uint64_t>(
            std::chrono::duration_cast<std::chrono::milliseconds>(
                std::chrono::system_clock::now().time_since_epoch())
                .count());
        constexpr uint64_t desktopNotificationCooldownMs = 3'000;
        if (sidebarMetadata.lastDesktopNotificationMs != 0 &&
            now - sidebarMetadata.lastDesktopNotificationMs < desktopNotificationCooldownMs)
        {
            return;
        }
        sidebarMetadata.lastDesktopNotificationMs = now;

        // Build the notification message.
        // If a custom body is provided (e.g. from OSC 777), use the title/body directly.
        // Otherwise, build the standard tab-activity notification message.
        winrt::hstring notificationTitle;
        winrt::hstring message;
        if (!body.empty())
        {
            notificationTitle = tabTitle;
            message = body;
        }
        else
        {
            // Use the window name if available for context; otherwise just use the tab title.
            // Use the raw WindowName (not WindowNameForDisplay) so we don't include
            // the "<unnamed window>" placeholder in the notification body.
            const auto windowName = _WindowProperties ? _WindowProperties.WindowName() : winrt::hstring{};
            if (!windowName.empty())
            {
                message = RS_fmt(L"NotificationMessage_TabActivityInWindow", std::wstring_view{ tabTitle }, std::wstring_view{ windowName });
            }
            else
            {
                message = RS_fmt(L"NotificationMessage_TabActivity", std::wstring_view{ tabTitle });
            }
            notificationTitle = CascadiaSettings::ApplicationDisplayName();
        }

        // Use the Tab object's identity hash as a stable toast tag.
        // This survives tab reordering and cross-window moves.
        const auto tabHash = std::hash<winrt::Windows::Foundation::IUnknown>{}(*tab);
#ifdef _WIN64
        const hstring tabTag{ fmt::format(FMT_COMPILE(L"wt-tab-{:016x}"), tabHash) };
#else
        const hstring tabTag{ fmt::format(FMT_COMPILE(L"wt-tab-{:08x}"), tabHash) };
#endif

        const implementation::DesktopNotificationArgs args{
            .Title = notificationTitle,
            .Message = message,
            .Tag = tabTag
        };

        implementation::DesktopNotification::SendNotification(args, [weakThis{ get_weak() }, weakTab{ tab->get_weak() }, weakContent{ winrt::make_weak(content) }]() {
            if (const auto page{ weakThis.get() })
            {
                // The toast Activated callback runs on a background thread.
                // Marshal to the UI thread for tab focus and window summon.
                page->Dispatcher().RunAsync(winrt::Windows::UI::Core::CoreDispatcherPriority::Normal, [weakPage{ page->get_weak() }, weakTab, weakContent]() {
                    if (const auto p{ weakPage.get() })
                    {
                        if (const auto t{ weakTab.get() })
                        {
                            // Try to find and focus the tab in this window first.
                            if (const auto tabIndex{ p->_GetTabIndex(*t) })
                            {
                                p->SummonWindowRequested.raise(nullptr, nullptr);
                                p->_SelectTab(tabIndex.value());

                                // Focus the specific pane that raised the notification.
                                if (const auto paneContent{ weakContent.get() })
                                {
                                    const auto rootPane = t->GetRootPane();
                                    rootPane->WalkTree([&](const auto& pane) {
                                        if (pane->GetContent() == paneContent)
                                        {
                                            rootPane->FocusPane(pane);
                                        }
                                    });
                                }
                            }
                            else
                            {
                                // The tab may have moved to another window.
                                // Raise FocusTabRequested so the emperor can
                                // search all windows for it.
                                p->FocusTabRequested.raise(nullptr, *t);
                            }
                        }
                        else
                        {
                            // Tab was closed. Just summon this window.
                            p->SummonWindowRequested.raise(nullptr, nullptr);
                        }
                    }
                });
            }
        });
    }
}
