// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#include "pch.h"
#include "TerminalPage.h"

#include "AgentPaneContent.h"
#include "../inc/WtaProcess.h"
#include "../../types/inc/ColorFix.hpp"

#include <json/json.h>

#include <algorithm>
#include <cmath>
#include <chrono>
#include <filesystem>
#include <shlobj.h>
#include <sstream>

using namespace winrt;
using namespace winrt::Windows::Foundation;
using namespace winrt::Windows::Storage;
using namespace winrt::Windows::UI;
using namespace winrt::Windows::UI::Text;
using namespace winrt::Windows::UI::Xaml;
using namespace winrt::Windows::UI::Xaml::Automation;
using namespace winrt::Windows::UI::Xaml::Controls;
using namespace winrt::Windows::UI::Xaml::Controls::Primitives;
using namespace winrt::Windows::UI::Xaml::Media;
using namespace winrt::Windows::UI::Xaml::Shapes;
using namespace winrt::Microsoft::Terminal::Settings::Model;

namespace
{
    constexpr std::wstring_view SidebarStateKey{ L"IntelligentTerminal.WorkspaceSidebar.v1" };
    constexpr double SidebarDefaultWidth{ 292.0 };
    constexpr double SidebarMinWidth{ 232.0 };
    constexpr double SidebarMaxWidth{ 360.0 };
    constexpr uint64_t MetadataFreshnessMs{ 15'000 };
    constexpr std::wstring_view WorkspaceContextMenuTag{ L"IntelligentTerminal.WorkspaceContextMenu" };

    uint64_t _nowMs() noexcept
    {
        return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::milliseconds>(
                                         std::chrono::system_clock::now().time_since_epoch())
                                         .count());
    }

    std::wstring _lower(std::wstring value)
    {
        std::transform(value.begin(), value.end(), value.begin(), [](const wchar_t ch) {
            return static_cast<wchar_t>(std::towlower(ch));
        });
        return value;
    }

    bool _containsInsensitive(const winrt::hstring& value, const std::wstring& query)
    {
        return query.empty() || _lower(std::wstring{ value }).find(query) != std::wstring::npos;
    }

    std::wstring _quoteArg(const winrt::hstring& value)
    {
        std::wstring result{ L"\"" };
        for (const auto ch : std::wstring_view{ value })
        {
            if (ch == L'"')
            {
                result += L'\\';
            }
            result += ch;
        }
        result += L'"';
        return result;
    }

    SolidColorBrush _brush(const Color color)
    {
        return SolidColorBrush{ color };
    }

    Brush _themeBrush(const wchar_t* key, const Color fallback)
    {
        try
        {
            if (const auto value = Application::Current().Resources().TryLookup(winrt::box_value(key)))
            {
                if (const auto brush = value.try_as<Brush>())
                {
                    return brush;
                }
            }
        }
        CATCH_LOG();
        return _brush(fallback);
    }

    winrt::hstring _compactPath(const winrt::hstring& path)
    {
        std::wstring value{ path };
        if (value.empty())
        {
            return {};
        }

        wchar_t profile[MAX_PATH]{};
        if (GetEnvironmentVariableW(L"USERPROFILE", profile, ARRAYSIZE(profile)) > 0)
        {
            const std::wstring prefix{ profile };
            if (value.size() >= prefix.size() && _wcsnicmp(value.c_str(), prefix.c_str(), prefix.size()) == 0)
            {
                value.replace(0, prefix.size(), L"~");
            }
        }
        return winrt::hstring{ value };
    }

    winrt::hstring _remoteJoin(const winrt::hstring& left, const winrt::hstring& right)
    {
        if (left.empty())
        {
            return right;
        }
        if (right.empty())
        {
            return left;
        }
        std::wstring result{ left };
        const auto separator =
            result.find(L'\\') != std::wstring::npos && result.find(L'/') == std::wstring::npos ?
                L'\\' :
                L'/';
        if (result.back() != L'/' && result.back() != L'\\')
        {
            result.push_back(separator);
        }
        auto suffix = std::wstring_view{ right };
        while (!suffix.empty() && (suffix.front() == L'/' || suffix.front() == L'\\'))
        {
            suffix.remove_prefix(1);
        }
        result.append(suffix);
        return winrt::hstring{ result };
    }

    winrt::hstring _remoteParent(const winrt::hstring& path)
    {
        std::wstring value{ path };
        while (!value.empty() && (value.back() == L'/' || value.back() == L'\\'))
        {
            value.pop_back();
        }
        const auto separator = value.find_last_of(L"/\\");
        return separator == std::wstring::npos ? winrt::hstring{} :
                                                winrt::hstring{ value.substr(0, separator) };
    }

    std::wstring _workspaceSlug(const winrt::hstring& name)
    {
        std::wstring slug;
        bool separator = false;
        for (const auto ch : std::wstring_view{ name })
        {
            if (std::iswalnum(ch) || ch == L'-' || ch == L'_')
            {
                if (separator && !slug.empty())
                {
                    slug.push_back(L'-');
                }
                slug.push_back(static_cast<wchar_t>(std::towlower(ch)));
                separator = false;
            }
            else
            {
                separator = true;
            }
        }
        while (!slug.empty() && slug.back() == L'-')
        {
            slug.pop_back();
        }
        return slug.empty() ? L"agent-workspace" : slug;
    }

    winrt::hstring _jsonString(const Json::Value& value, const char* key)
    {
        return value.isObject() && value.isMember(key) && value[key].isString()
                   ? winrt::to_hstring(value[key].asString())
                   : winrt::hstring{};
    }

    Border _chip(const winrt::hstring& text, const Color accent)
    {
        Border border;
        border.CornerRadius(winrt::Windows::UI::Xaml::CornerRadius{ 4.0 });
        border.Padding(Thickness{ 5, 1, 5, 1 });
        auto background = accent;
        background.A = 0x24;
        border.Background(_brush(background));

        TextBlock label;
        label.Text(text);
        label.FontSize(10);
        label.Opacity(0.86);
        label.TextTrimming(TextTrimming::CharacterEllipsis);
        border.Child(label);
        return border;
    }
}

namespace winrt::TerminalApp::implementation
{
    void TerminalPage::_InitializeWorkspaceSidebar()
    {
        if (_workspaceSidebarInitialized)
        {
            return;
        }
        _workspaceSidebarInitialized = true;
        _LoadWorkspaceSidebarState();

        // Reuse the native new-tab flyout rather than maintaining a second
        // profile/settings menu for the sidebar. The only product-specific
        // extension is the declarative agent workspace composer, and it is
        // visible from both navigation presentations.
        if (const auto nativeNewTabMenu = _newTabButton.Flyout().try_as<MenuFlyout>())
        {
            nativeNewTabMenu.Items().Append(MenuFlyoutSeparator{});

            MenuFlyoutItem agentWorkspace;
            agentWorkspace.Text(L"New agent workspace from template…");
            FontIcon agentIcon;
            agentIcon.FontFamily(winrt::Windows::UI::Xaml::Media::FontFamily{
                L"Segoe Fluent Icons, Segoe MDL2 Assets"
            });
            agentIcon.Glyph(L"\xE77B");
            agentWorkspace.Icon(agentIcon);
            agentWorkspace.Click({ this, &TerminalPage::_WorkspaceSidebarComposerClicked });
            nativeNewTabMenu.Items().Append(agentWorkspace);

            WorkspaceSidebarNewButton().Flyout(nativeNewTabMenu);
        }
        _ApplyWorkspaceSidebarVisibility();
        _RefreshWorkspaceSidebar(true);
    }

    void TerminalPage::_LoadWorkspaceSidebarState()
    {
        try
        {
            const auto values = ApplicationData::Current().LocalSettings().Values();
            const auto boxed = values.TryLookup(winrt::hstring{ SidebarStateKey });
            const auto serialized = winrt::unbox_value_or<winrt::hstring>(boxed, {});
            if (serialized.empty())
            {
                return;
            }

            Json::Value root;
            Json::CharReaderBuilder reader;
            std::string errors;
            std::istringstream input{ winrt::to_string(serialized) };
            if (!Json::parseFromStream(reader, input, &root, &errors) || !root.isObject())
            {
                return;
            }

            _workspaceSidebarVisible = root.get("visible", true).asBool();
            _workspaceSidebarWidth = std::round(std::clamp(root.get("width", SidebarDefaultWidth).asDouble(),
                                                           SidebarMinWidth,
                                                           SidebarMaxWidth));

            if (const auto& entries = root["entries"]; entries.isObject())
            {
                for (const auto& key : entries.getMemberNames())
                {
                    const auto& value = entries[key];
                    WorkspaceSidebarMetadata metadata;
                    metadata.pinned = value.get("pinned", false).asBool();
                    metadata.group = _jsonString(value, "group");
                    metadata.lastSeenEventMs = value.get("last_seen_event_ms", Json::UInt64{ 0 }).asUInt64();
                    _workspaceSidebarPersistedMetadata.emplace(
                        _lower(std::wstring{ winrt::to_hstring(key) }),
                        std::move(metadata));
                }
            }

            if (const auto& groups = root["groups"]; groups.isObject())
            {
                for (const auto& key : groups.getMemberNames())
                {
                    _workspaceSidebarGroupCollapsed[_lower(std::wstring{ winrt::to_hstring(key) })] =
                        groups[key].get("collapsed", false).asBool();
                }
            }

            if (const auto& recent = root["recent"]; recent.isArray())
            {
                for (const auto& value : recent)
                {
                    RecentlyClosedWorkspace item;
                    item.title = _jsonString(value, "title");
                    item.cwd = _jsonString(value, "cwd");
                    item.workspaceId = _jsonString(value, "workspace_id");
                    item.workspaceName = _jsonString(value, "workspace_name");
                    item.manifestPath = _jsonString(value, "manifest_path");
                    item.group = _jsonString(value, "group");
                    item.closedAtMs = value.get("closed_at_ms", Json::UInt64{ 0 }).asUInt64();
                    if (!item.title.empty() || !item.cwd.empty())
                    {
                        _workspaceSidebarRecent.emplace_back(std::move(item));
                    }
                    if (_workspaceSidebarRecent.size() >= 10)
                    {
                        break;
                    }
                }
            }
        }
        CATCH_LOG();
    }

    void TerminalPage::_SaveWorkspaceSidebarState()
    {
        try
        {
            Json::Value root{ Json::objectValue };
            root["version"] = 2;
            root["visible"] = _workspaceSidebarVisible;
            root["width"] = _workspaceSidebarWidth;

            Json::Value entries{ Json::objectValue };
            for (const auto& [key, metadata] : _workspaceSidebarPersistedMetadata)
            {
                Json::Value value{ Json::objectValue };
                value["pinned"] = metadata.pinned;
                value["group"] = winrt::to_string(metadata.group);
                value["last_seen_event_ms"] = Json::UInt64{ metadata.lastSeenEventMs };
                entries[winrt::to_string(winrt::hstring{ key })] = std::move(value);
            }
            root["entries"] = std::move(entries);

            Json::Value groups{ Json::objectValue };
            for (const auto& [name, collapsed] : _workspaceSidebarGroupCollapsed)
            {
                groups[winrt::to_string(winrt::hstring{ name })]["collapsed"] = collapsed;
            }
            root["groups"] = std::move(groups);

            Json::Value recent{ Json::arrayValue };
            for (const auto& item : _workspaceSidebarRecent)
            {
                Json::Value value{ Json::objectValue };
                value["title"] = winrt::to_string(item.title);
                value["cwd"] = winrt::to_string(item.cwd);
                value["workspace_id"] = winrt::to_string(item.workspaceId);
                value["workspace_name"] = winrt::to_string(item.workspaceName);
                value["manifest_path"] = winrt::to_string(item.manifestPath);
                value["group"] = winrt::to_string(item.group);
                value["closed_at_ms"] = Json::UInt64{ item.closedAtMs };
                recent.append(std::move(value));
            }
            root["recent"] = std::move(recent);

            Json::StreamWriterBuilder writer;
            writer["indentation"] = "";
            ApplicationData::Current().LocalSettings().Values().Insert(
                winrt::hstring{ SidebarStateKey },
                winrt::box_value(winrt::to_hstring(Json::writeString(writer, root))));
        }
        CATCH_LOG();
    }

    void TerminalPage::_ApplyWorkspaceSidebarVisibility()
    {
        const auto visible = _workspaceSidebarVisible ? Visibility::Visible : Visibility::Collapsed;
        WorkspaceSidebarColumn().MinWidth(_workspaceSidebarVisible ? SidebarMinWidth : 0);
        WorkspaceSidebarColumn().MaxWidth(_workspaceSidebarVisible ? SidebarMaxWidth : 0);
        WorkspaceSidebarRoot().Visibility(visible);
        WorkspaceSidebarSplitter().Visibility(visible);
        WorkspaceSidebarSeparator().Visibility(visible);
        WorkspaceSidebarSeparatorUnderlay().Visibility(visible);
        WorkspaceSidebarSeparatorColumn().Width(GridLengthHelper::FromValueAndType(
            _workspaceSidebarVisible ? 1 : 0,
            GridUnitType::Pixel));
        WorkspaceSidebarRevealButton().Visibility(_workspaceSidebarVisible ? Visibility::Collapsed : Visibility::Visible);
        WorkspaceSidebarColumn().Width(GridLengthHelper::FromValueAndType(
            _workspaceSidebarVisible ? _workspaceSidebarWidth : 0,
            GridUnitType::Pixel));
        _ApplyWorkspaceNavigationPresentation();

        for (uint32_t index = 0; index < _tabs.Size(); ++index)
        {
            if (const auto tab = _GetTabImpl(_tabs.GetAt(index)))
            {
                if (const auto root = tab->GetRootPane())
                {
                    root->SetOuterLeftPaddingTrim(_workspaceSidebarVisible);
                }
            }
        }
    }

    void TerminalPage::_ApplyWorkspaceNavigationPresentation()
    {
        // The native Tab objects and _tabs collection remain canonical. Only
        // their navigation presentation changes: vertical cards while the
        // sidebar is open, native horizontal headers while it is collapsed.
        // Keeping both visible would create two competing navigation surfaces.
        const auto nativeHeaderVisibility = _workspaceSidebarVisible
                                                ? Visibility::Collapsed
                                                : Visibility::Visible;
        for (uint32_t index = 0; index < _tabs.Size(); ++index)
        {
            if (const auto tab = _GetTabImpl(_tabs.GetAt(index)))
            {
                tab->TabViewItem().Visibility(nativeHeaderVisibility);
                if (!_workspaceSidebarVisible)
                {
                    tab->SetContextMenuTarget(tab->TabViewItem().as<FrameworkElement>());
                }
            }
        }

        // The sidebar owns terminal creation while it is visible. Leaving the
        // titlebar button visible would retain a second, competing entry point.
        if (_newTabButton)
        {
            _newTabButton.Visibility(nativeHeaderVisibility);
        }
    }

    bool TerminalPage::_ReconcileWorkspaceSidebarTabOrder()
    {
        if (_workspaceSidebarReconcilingOrder || _tabs.Size() < 2)
        {
            return false;
        }

        std::vector<winrt::TerminalApp::Tab> desired;
        desired.reserve(_tabs.Size());
        for (uint32_t index = 0; index < _tabs.Size(); ++index)
        {
            desired.emplace_back(_tabs.GetAt(index));
        }

        std::stable_sort(desired.begin(), desired.end(), [&](const auto& left, const auto& right) {
            const auto leftImpl = _GetTabImpl(left);
            const auto rightImpl = _GetTabImpl(right);
            if (!leftImpl || !rightImpl)
            {
                return false;
            }

            const auto& leftMetadata = _WorkspaceSidebarMetadataFor(leftImpl);
            const auto& rightMetadata = _WorkspaceSidebarMetadataFor(rightImpl);
            if (leftMetadata.pinned != rightMetadata.pinned)
            {
                return leftMetadata.pinned > rightMetadata.pinned;
            }

            const auto leftGroup = _lower(std::wstring{ leftMetadata.group });
            const auto rightGroup = _lower(std::wstring{ rightMetadata.group });
            if (leftGroup != rightGroup)
            {
                return leftGroup < rightGroup;
            }
            return false;
        });

        bool changed = false;
        _workspaceSidebarReconcilingOrder = true;
        const auto clearGuard = wil::scope_exit([&]() noexcept {
            _workspaceSidebarReconcilingOrder = false;
        });
        for (uint32_t desiredIndex = 0; desiredIndex < desired.size(); ++desiredIndex)
        {
            uint32_t currentIndex = 0;
            if (_tabs.IndexOf(desired[desiredIndex], currentIndex) && currentIndex != desiredIndex)
            {
                _TryMoveTab(currentIndex, gsl::narrow_cast<int32_t>(desiredIndex));
                changed = true;
            }
        }
        return changed;
    }

    std::wstring TerminalPage::_WorkspaceSidebarPersistentKey(const winrt::com_ptr<Tab>& tab) const
    {
        if (!tab)
        {
            return {};
        }
        if (const auto metadata = _workspaceSidebarMetadata.find(std::wstring{ tab->StableId() });
            metadata != _workspaceSidebarMetadata.end() && !metadata->second.workspaceId.empty())
        {
            return L"workspace:" + _lower(std::wstring{ metadata->second.workspaceId });
        }
        // Ad-hoc tabs do not have a durable workspace identity. Using CWD or
        // title here made unrelated tabs silently share pin/group state. Keep
        // them isolated by runtime StableId; declarative workspaces migrate to
        // their durable WTA workspace id when metadata arrives.
        return L"tab:" + _lower(std::wstring{ tab->StableId() });
    }

    TerminalPage::WorkspaceSidebarMetadata& TerminalPage::_WorkspaceSidebarMetadataFor(const winrt::com_ptr<Tab>& tab)
    {
        const std::wstring stableId{ tab->StableId() };
        auto [it, inserted] = _workspaceSidebarMetadata.try_emplace(stableId);
        if (inserted)
        {
            if (const auto persisted = _workspaceSidebarPersistedMetadata.find(_WorkspaceSidebarPersistentKey(tab));
                persisted != _workspaceSidebarPersistedMetadata.end())
            {
                it->second.pinned = persisted->second.pinned;
                it->second.group = persisted->second.group;
                it->second.lastSeenEventMs = persisted->second.lastSeenEventMs;
            }
            if (const auto control = tab->GetActiveTerminalControl())
            {
                it->second.cwd = control.WorkingDirectory();
            }
        }
        return it->second;
    }

    void TerminalPage::_EnsureWorkspaceContextMenuExtension(
        const winrt::com_ptr<Tab>& tab,
        const MenuFlyout& contextMenu)
    {
        if (!tab || !contextMenu)
        {
            return;
        }

        for (const auto& item : contextMenu.Items())
        {
            if (const auto submenu = item.try_as<MenuFlyoutSubItem>())
            {
                const auto tag = winrt::unbox_value_or<winrt::hstring>(submenu.Tag(), {});
                if (tag == WorkspaceContextMenuTag)
                {
                    return;
                }
            }
        }

        contextMenu.Items().Append(MenuFlyoutSeparator{});

        MenuFlyoutSubItem workspaceMenu;
        workspaceMenu.Text(L"Workspace");
        workspaceMenu.Tag(winrt::box_value(winrt::hstring{ WorkspaceContextMenuTag }));
        FontIcon workspaceIcon;
        workspaceIcon.FontFamily(winrt::Windows::UI::Xaml::Media::FontFamily{
            L"Segoe Fluent Icons, Segoe MDL2 Assets"
        });
        workspaceIcon.Glyph(L"\xE8F1");
        workspaceMenu.Icon(workspaceIcon);
        contextMenu.Items().Append(workspaceMenu);

        const auto stableId = tab->StableId();
        contextMenu.Opening([weakThis = get_weak(), stableId](const auto& sender, auto&&) {
            const auto self = weakThis.get();
            const auto menu = sender.template try_as<MenuFlyout>();
            if (!self || !menu)
            {
                return;
            }

            for (const auto& item : menu.Items())
            {
                if (const auto submenu = item.try_as<MenuFlyoutSubItem>())
                {
                    const auto tag = winrt::unbox_value_or<winrt::hstring>(submenu.Tag(), {});
                    if (tag == WorkspaceContextMenuTag)
                    {
                        self->_PopulateWorkspaceContextMenuExtension(stableId, submenu);
                        break;
                    }
                }
            }
        });

        _PopulateWorkspaceContextMenuExtension(stableId, workspaceMenu);
    }

    void TerminalPage::_PopulateWorkspaceContextMenuExtension(
        const winrt::hstring& stableId,
        const MenuFlyoutSubItem& workspaceMenu)
    {
        workspaceMenu.Items().Clear();

        const auto tab = _FindTabByStableId(stableId);
        if (!tab)
        {
            return;
        }

        const auto& metadata = _WorkspaceSidebarMetadataFor(tab);

        ToggleMenuFlyoutItem pin;
        pin.Text(L"Pin workspace");
        pin.IsChecked(metadata.pinned);
        pin.Click([weakThis = get_weak(), stableId](auto&&, auto&&) {
            if (const auto self = weakThis.get())
            {
                if (const auto selectedTab = self->_FindTabByStableId(stableId))
                {
                    auto& value = self->_WorkspaceSidebarMetadataFor(selectedTab);
                    value.pinned = !value.pinned;
                    self->_workspaceSidebarPersistedMetadata[self->_WorkspaceSidebarPersistentKey(selectedTab)] = value;
                    self->_SaveWorkspaceSidebarState();
                    self->_RefreshWorkspaceSidebar(false);
                }
            }
        });
        workspaceMenu.Items().Append(pin);

        MenuFlyoutSubItem groupMenu;
        groupMenu.Text(L"Move to group");

        MenuFlyoutItem noGroup;
        noGroup.Text(L"Ungrouped");
        noGroup.Click([weakThis = get_weak(), stableId](auto&&, auto&&) {
            if (const auto self = weakThis.get())
            {
                if (const auto selectedTab = self->_FindTabByStableId(stableId))
                {
                    auto& value = self->_WorkspaceSidebarMetadataFor(selectedTab);
                    value.group = {};
                    self->_workspaceSidebarPersistedMetadata[self->_WorkspaceSidebarPersistentKey(selectedTab)] = value;
                    self->_SaveWorkspaceSidebarState();
                    self->_RefreshWorkspaceSidebar(false);
                }
            }
        });
        groupMenu.Items().Append(noGroup);

        std::vector<winrt::hstring> groups;
        for (const auto& [_, value] : _workspaceSidebarPersistedMetadata)
        {
            if (!value.group.empty() &&
                std::find(groups.begin(), groups.end(), value.group) == groups.end())
            {
                groups.push_back(value.group);
            }
        }
        std::sort(groups.begin(), groups.end(), [](const auto& left, const auto& right) {
            return _lower(std::wstring{ left }) < _lower(std::wstring{ right });
        });

        for (const auto& groupName : groups)
        {
            MenuFlyoutItem existing;
            existing.Text(groupName);
            existing.Click([weakThis = get_weak(), stableId, groupName](auto&&, auto&&) {
                if (const auto self = weakThis.get())
                {
                    if (const auto selectedTab = self->_FindTabByStableId(stableId))
                    {
                        auto& value = self->_WorkspaceSidebarMetadataFor(selectedTab);
                        value.group = groupName;
                        self->_workspaceSidebarPersistedMetadata[self->_WorkspaceSidebarPersistentKey(selectedTab)] = value;
                        self->_SaveWorkspaceSidebarState();
                        self->_RefreshWorkspaceSidebar(false);
                    }
                }
            });
            groupMenu.Items().Append(existing);
        }

        MenuFlyoutItem newGroup;
        newGroup.Text(L"New group…");
        newGroup.Click([weakThis = get_weak(), stableId](auto&&, auto&&) {
            if (const auto self = weakThis.get())
            {
                self->_PromptWorkspaceGroup(stableId);
            }
        });
        groupMenu.Items().Append(newGroup);
        workspaceMenu.Items().Append(groupMenu);
        workspaceMenu.Items().Append(MenuFlyoutSeparator{});

        if (!metadata.pullRequestUrl.empty())
        {
            MenuFlyoutItem openPr;
            openPr.Text(L"Open pull request");
            const auto url = metadata.pullRequestUrl;
            openPr.Click([url](auto&&, auto&&) {
                ShellExecuteW(nullptr, L"open", url.c_str(), nullptr, nullptr, SW_SHOWNORMAL);
            });
            workspaceMenu.Items().Append(openPr);
        }

        for (const auto port : metadata.listeningPorts)
        {
            MenuFlyoutItem openPort;
            openPort.Text(winrt::to_hstring(fmt::format("Open localhost:{}", port)));
            const auto url = winrt::to_hstring(fmt::format("http://localhost:{}", port));
            openPort.Click([url](auto&&, auto&&) {
                ShellExecuteW(nullptr, L"open", url.c_str(), nullptr, nullptr, SW_SHOWNORMAL);
            });
            workspaceMenu.Items().Append(openPort);
        }

        MenuFlyoutItem gitView;
        gitView.Text(L"Git status and diff");
        gitView.Click([weakThis = get_weak(), stableId](auto&&, auto&&) {
            if (const auto self = weakThis.get())
            {
                if (const auto selectedTab = self->_FindTabByStableId(stableId))
                {
                    uint32_t index = 0;
                    if (self->_tabs.IndexOf(*selectedTab, index))
                    {
                        self->_SelectTab(index);
                        self->_ShowWorkspaceGit();
                    }
                }
            }
        });
        workspaceMenu.Items().Append(gitView);

        if (metadata.persisted)
        {
            MenuFlyoutItem verify;
            verify.Text(L"Run trusted verifier");
            verify.Click([weakThis = get_weak(), stableId](auto&&, auto&&) {
                if (const auto self = weakThis.get())
                {
                    self->_VerifyWorkspace(stableId);
                }
            });
            workspaceMenu.Items().Append(verify);

            MenuFlyoutItem snapshot;
            snapshot.Text(L"Snapshot workspace state");
            const auto root = metadata.cwd;
            const auto name = metadata.workspaceName;
            snapshot.Click([weakThis = get_weak(), root, name](auto&&, auto&&) {
                if (const auto self = weakThis.get())
                {
                    self->_SnapshotDeclarativeWorkspace(root, name);
                }
            });
            workspaceMenu.Items().Append(snapshot);
        }
    }

    void TerminalPage::_RequestWorkspaceSidebarMetadata(const winrt::com_ptr<Tab>& tab, bool force)
    {
        if (!tab)
        {
            return;
        }
        auto& metadata = _WorkspaceSidebarMetadataFor(tab);
        if (metadata.cwd.empty())
        {
            return;
        }
        const auto now = _nowMs();
        if (metadata.refreshInFlight ||
            (!force && metadata.refreshedAtMs != 0 && now - metadata.refreshedAtMs < MetadataFreshnessMs))
        {
            return;
        }
        if (!force)
        {
            for (const auto& [otherId, cached] : _workspaceSidebarMetadata)
            {
                if (otherId == std::wstring{ tab->StableId() } ||
                    cached.cwd != metadata.cwd ||
                    cached.refreshInFlight ||
                    cached.refreshedAtMs == 0 ||
                    now - cached.refreshedAtMs >= MetadataFreshnessMs)
                {
                    continue;
                }

                const auto pinned = metadata.pinned;
                const auto group = metadata.group;
                const auto lastSeen = metadata.lastSeenEventMs;
                metadata = cached;
                metadata.pinned = pinned;
                metadata.group = group;
                metadata.lastSeenEventMs = lastSeen;
                return;
            }
        }
        metadata.refreshInFlight = true;
        _RefreshWorkspaceSidebarMetadata(tab->StableId(), metadata.cwd, force);
    }

    safe_void_coroutine TerminalPage::_RefreshWorkspaceSidebarMetadata(winrt::hstring stableId,
                                                                       winrt::hstring cwd,
                                                                       bool /*force*/)
    try
    {
        const auto weakThis = get_weak();
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const auto args = L"agent-workspace context --root " + _quoteArg(cwd) +
                          L" --tab-id " + _quoteArg(stableId);

        co_await winrt::resume_background();
        const auto output = ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, args, 8'000);

        Json::Value result;
        if (!output.empty())
        {
            Json::CharReaderBuilder reader;
            std::string errors;
            std::istringstream input{ output };
            Json::parseFromStream(reader, input, &result, &errors);
        }

        co_await wil::resume_foreground(dispatcher);
        const auto self = weakThis.get();
        if (!self)
        {
            co_return;
        }
        const auto it = self->_workspaceSidebarMetadata.find(std::wstring{ stableId });
        if (it == self->_workspaceSidebarMetadata.end())
        {
            co_return;
        }

        auto& metadata = it->second;
        metadata.refreshInFlight = false;
        metadata.refreshedAtMs = _nowMs();
        if (result.isObject())
        {
            const auto& workspace = result["workspace"];
            const auto& context = result["context"];
            metadata.workspaceName = _jsonString(workspace, "name");
            metadata.workspaceId = _jsonString(workspace, "workspace_id");
            metadata.manifestPath = _jsonString(workspace, "manifest_path");
            metadata.persisted = result.get("persisted", false).asBool();
            if (metadata.persisted && !metadata.workspaceName.empty() && metadata.workspaceName != L"Ad hoc")
            {
                // A declarative workspace name is a native tab title, not a
                // sidebar-only alias. Both navigation presentations therefore
                // observe and edit the same title.
                if (const auto tab = self->_FindTabByStableId(stableId))
                {
                    tab->SetTabText(metadata.workspaceName);
                }
            }

            if (const auto& git = context["git"]; git.isObject())
            {
                metadata.branch = _jsonString(git, "branch");
                metadata.changedFiles = git.get("changed_files", 0).asUInt();
                metadata.ahead = git.get("ahead", 0).asUInt();
                metadata.behind = git.get("behind", 0).asUInt();
            }
            else
            {
                metadata.branch = {};
                metadata.changedFiles = 0;
                metadata.ahead = 0;
                metadata.behind = 0;
            }

            if (const auto& pr = context["pull_request"]; pr.isObject())
            {
                const auto number = pr.get("number", 0).asInt();
                const auto state = pr.get("state", "").asString();
                metadata.pullRequest = number > 0
                                           ? winrt::to_hstring(fmt::format("#{} {}", number, state))
                                           : winrt::hstring{};
                metadata.pullRequestUrl = _jsonString(pr, "url");
            }
            else
            {
                metadata.pullRequest = {};
                metadata.pullRequestUrl = {};
            }

            metadata.ports = {};
            metadata.listeningPorts.clear();
            if (const auto& ports = context["listening_ports"]; ports.isArray() && !ports.empty())
            {
                std::string labels;
                for (Json::ArrayIndex index = 0; index < ports.size() && index < 3; ++index)
                {
                    if (!labels.empty()) labels += ", ";
                    const auto port = gsl::narrow_cast<uint16_t>(ports[index].get("port", 0).asUInt());
                    labels += ":" + std::to_string(port);
                    metadata.listeningPorts.push_back(port);
                }
                metadata.ports = winrt::to_hstring(labels);
            }

            size_t working = 0;
            size_t attention = 0;
            size_t errors = 0;
            metadata.agents.clear();
            metadata.teams.clear();
            metadata.tasks.clear();
            if (const auto& panes = workspace["panes"]; panes.isObject())
            {
                for (const auto& id : panes.getMemberNames())
                {
                    const auto& pane = panes[id];
                    WorkspaceSidebarMetadata::Agent agent;
                    agent.id = winrt::to_hstring(id);
                    agent.role = _jsonString(pane, "role");
                    agent.model = _jsonString(pane, "model");
                    agent.activity = _jsonString(pane, "activity");
                    agent.sessionId = _jsonString(pane, "session_id");
                    agent.cwd = _jsonString(pane, "cwd");
                    agent.command = _jsonString(pane, "command");
                    agent.notification = _jsonString(pane, "last_notification");
                    metadata.agents.emplace_back(std::move(agent));
                }
            }
            if (const auto& agents = context["agents"]; agents.isArray())
            {
                for (const auto& agent : agents)
                {
                    const auto activity = agent.get("activity", "").asString();
                    working += activity == "working";
                    attention += activity == "attention";
                    errors += activity == "error";
                }
            }
            // Native teams are projected from the same WTA control plane. The
            // Rust context collector includes only teams whose workspace_id
            // exactly matches this native Tab StableId, so a worker can never
            // appear under an arbitrary active workspace.
            if (const auto& teams = context["teams"]; teams.isArray())
            {
                for (const auto& team : teams)
                {
                    const auto teamName = _jsonString(team, "name");
                    const auto& tasks = team["tasks"];
                    WorkspaceSidebarMetadata::Team teamSnapshot;
                    teamSnapshot.id = _jsonString(team, "team_id");
                    teamSnapshot.name = teamName;
                    teamSnapshot.leader = _jsonString(team, "leader");
                    teamSnapshot.status = _jsonString(team, "status");
                    teamSnapshot.updatedAtMs = team.get("updated_at_ms", Json::UInt64{ 0 }).asUInt64();
                    if (const auto& workers = team["workers"]; workers.isObject())
                    {
                        teamSnapshot.workerCount = workers.size();
                        for (const auto& workerId : workers.getMemberNames())
                        {
                            const auto& worker = workers[workerId];
                            const auto status = worker.get("status", "starting").asString();
                            if (status == "stopped")
                            {
                                continue;
                            }

                            WorkspaceSidebarMetadata::Agent agent;
                            agent.id = winrt::to_hstring(workerId);
                            const auto workerRole = _jsonString(worker, "role");
                            agent.role = teamName.empty() ? workerRole :
                                                               (workerRole.empty() ? teamName : teamName + L" · " + workerRole);
                            agent.model = _jsonString(worker, "model");
                            agent.activity = winrt::to_hstring(status);
                            agent.sessionId = _jsonString(worker, "pane_session_id");
                            agent.cwd = _jsonString(worker, "cwd");
                            agent.command = _jsonString(worker, "agent");
                            agent.teamName = teamName;
                            agent.lastHeartbeatMs = worker.get("last_heartbeat_ms", Json::UInt64{ 0 }).asUInt64();
                            agent.coordinator = workerId == winrt::to_string(teamSnapshot.leader) ||
                                                _lower(std::wstring{ workerRole }) == L"coordinator";

                            const auto taskId = _jsonString(worker, "current_task_id");
                            agent.currentTaskId = taskId;
                            if (!taskId.empty() && tasks.isObject())
                            {
                                const auto taskKey = winrt::to_string(taskId);
                                if (tasks.isMember(taskKey))
                                {
                                    const auto title = _jsonString(tasks[taskKey], "title");
                                    if (!title.empty())
                                    {
                                        auto activity = std::wstring{ agent.activity };
                                        activity += L" · ";
                                        activity += std::wstring_view{ title };
                                        agent.activity = winrt::hstring{ activity };
                                    }
                                    agent.notification = _jsonString(tasks[taskKey], "error");
                                }
                            }

                            working += status == "working";
                            attention += status == "stale" || status == "stopping";
                            errors += status == "failed";
                            metadata.agents.emplace_back(std::move(agent));
                        }
                    }
                    if (tasks.isObject())
                    {
                        for (const auto& taskId : tasks.getMemberNames())
                        {
                            const auto& task = tasks[taskId];
                            WorkspaceSidebarMetadata::Task taskSnapshot;
                            taskSnapshot.teamName = teamName;
                            taskSnapshot.id = winrt::to_hstring(taskId);
                            taskSnapshot.title = _jsonString(task, "title");
                            taskSnapshot.status = _jsonString(task, "status");
                            taskSnapshot.owner = _jsonString(task, "owner");
                            taskSnapshot.result = _jsonString(task, "result");
                            taskSnapshot.error = _jsonString(task, "error");
                            taskSnapshot.attempts = task.get("attempts", 0).asUInt();
                            taskSnapshot.maxAttempts = task.get("max_attempts", 0).asUInt();
                            metadata.tasks.emplace_back(std::move(taskSnapshot));
                        }
                    }
                    metadata.teams.emplace_back(std::move(teamSnapshot));
                }
            }

            metadata.computeTargets.clear();
            metadata.surfaceBindings.clear();
            metadata.computeJobs.clear();
            metadata.fileTransfers.clear();
            metadata.remoteWorkspaces.clear();
            metadata.browsers.clear();
            metadata.environments.clear();
            metadata.connections.clear();
            metadata.computeEvents.clear();
            metadata.computeError = {};
            if (const auto& compute = context["compute"]; compute.isObject())
            {
                metadata.computeError = _jsonString(compute, "error");
                if (const auto& targets = compute["targets"]; targets.isArray())
                {
                    for (const auto& target : targets)
                    {
                        WorkspaceSidebarMetadata::ComputeTarget snapshot;
                        snapshot.id = _jsonString(target, "id");
                        snapshot.name = _jsonString(target, "display_name");
                        snapshot.provider = _jsonString(target, "provider");
                        snapshot.os = _jsonString(target, "os");
                        snapshot.arch = _jsonString(target, "arch");
                        snapshot.health = _jsonString(target, "health");
                        snapshot.trust = _jsonString(target, "trust_tier");
                        if (const auto& endpoint = target["endpoint"]; endpoint.isObject())
                        {
                            snapshot.sshAlias = _jsonString(endpoint, "ssh_alias");
                            snapshot.wslDistro = _jsonString(endpoint, "wsl_distro");
                        }
                        snapshot.agentSlots = target.get("agent_slots", 0).asUInt();
                        snapshot.buildSlots = target.get("build_slots", 0).asUInt();
                        snapshot.disabled = target.get("disabled", false).asBool();
                        metadata.computeTargets.emplace_back(std::move(snapshot));
                    }
                }
                if (const auto& bindings = compute["bindings"]; bindings.isArray())
                {
                    for (const auto& binding : bindings)
                    {
                        WorkspaceSidebarMetadata::SurfaceBinding snapshot;
                        snapshot.id = _jsonString(binding, "binding_id");
                        snapshot.surfaceId = _jsonString(binding, "surface_id");
                        snapshot.paneId = _jsonString(binding, "pane_id");
                        snapshot.kind = _jsonString(binding, "kind");
                        snapshot.state = _jsonString(binding, "state");
                        snapshot.agentId = _jsonString(binding, "agent_id");
                        snapshot.adapterKind = _jsonString(binding, "adapter_kind");
                        snapshot.acpSessionId = _jsonString(binding, "acp_session_id");
                        snapshot.homeTargetId = _jsonString(binding, "home_target_id");
                        snapshot.remoteSessionId = _jsonString(binding, "remote_session_id");
                        snapshot.environmentId = _jsonString(binding, "environment_id");
                        snapshot.worktreeId = _jsonString(binding, "worktree_id");
                        snapshot.writerLeaseId = _jsonString(binding, "writer_lease_id");

                        // Managed-agent bindings are canonical agent
                        // identities too. Project them into the same list as
                        // native team workers so Agents & Tasks never needs a
                        // second registry and can focus the exact surface.
                        if (snapshot.kind == L"managed_agent")
                        {
                            const auto alreadyProjected = std::find_if(
                                metadata.agents.begin(),
                                metadata.agents.end(),
                                [&](const auto& agent) {
                                    return !snapshot.surfaceId.empty() &&
                                           agent.sessionId == snapshot.surfaceId;
                                }) != metadata.agents.end();
                            if (!alreadyProjected)
                            {
                                WorkspaceSidebarMetadata::Agent agent;
                                agent.id = snapshot.agentId.empty() ? snapshot.id : snapshot.agentId;
                                agent.role = L"Managed surface";
                                agent.model = snapshot.adapterKind;
                                agent.activity = snapshot.state;
                                agent.sessionId = snapshot.surfaceId;
                                agent.command = snapshot.agentId;
                                agent.notification =
                                    snapshot.homeTargetId.empty()
                                        ? L"Local compute"
                                        : L"Home target: " + snapshot.homeTargetId;
                                metadata.agents.emplace_back(std::move(agent));
                                const auto state = winrt::to_string(snapshot.state);
                                working += state == "starting" ||
                                           state == "running" ||
                                           state == "reconnecting";
                                attention += state == "detached" ||
                                             state == "stopping";
                                errors += state == "failed" ||
                                          state == "lost";
                            }
                        }
                        metadata.surfaceBindings.emplace_back(std::move(snapshot));
                    }
                }
                if (const auto& jobs = compute["jobs"]; jobs.isArray())
                {
                    for (const auto& job : jobs)
                    {
                        WorkspaceSidebarMetadata::ComputeJob snapshot;
                        snapshot.id = _jsonString(job, "job_id");
                        snapshot.state = _jsonString(job, "state");
                        snapshot.targetId = _jsonString(job, "target_id");
                        snapshot.terminationReason = _jsonString(job, "termination_reason");
                        snapshot.attempt = job.get("attempt", 0).asUInt();
                        if (const auto& request = job["request"]; request.isObject())
                        {
                            snapshot.workload = _jsonString(request, "class");
                            snapshot.snapshotId = _jsonString(request, "snapshot_id");
                        }
                        metadata.computeJobs.emplace_back(std::move(snapshot));
                    }
                }
                if (const auto& transfers = compute["transfers"]; transfers.isArray())
                {
                    for (const auto& transfer : transfers)
                    {
                        WorkspaceSidebarMetadata::FileTransfer snapshot;
                        snapshot.id = _jsonString(transfer, "transfer_id");
                        snapshot.state = _jsonString(transfer, "state");
                        snapshot.targetId = _jsonString(transfer, "target_id");
                        snapshot.surfaceId = _jsonString(transfer, "surface_id");
                        snapshot.displayName = _jsonString(transfer, "display_name");
                        snapshot.remotePath = _jsonString(transfer, "remote_path");
                        snapshot.error = _jsonString(transfer, "error");
                        snapshot.sizeBytes =
                            transfer.get("size_bytes", Json::UInt64{ 0 }).asUInt64();
                        snapshot.bytesTransferred =
                            transfer.get("bytes_transferred", Json::UInt64{ 0 }).asUInt64();
                        metadata.fileTransfers.emplace_back(std::move(snapshot));
                    }
                }
                if (const auto& remotes = compute["remote_workspaces"]; remotes.isArray())
                {
                    for (const auto& remote : remotes)
                    {
                        WorkspaceSidebarMetadata::RemoteWorkspace snapshot;
                        snapshot.id = _jsonString(remote, "remote_workspace_id");
                        snapshot.targetId = _jsonString(remote, "target_id");
                        snapshot.environmentId = _jsonString(remote, "environment_id");
                        snapshot.state = _jsonString(remote, "state");
                        snapshot.lastError = _jsonString(remote, "last_error");
                        snapshot.reconnectAttempt = remote.get("reconnect_attempt", 0).asUInt();
                        metadata.remoteWorkspaces.emplace_back(std::move(snapshot));
                    }
                }
                if (const auto& browsers = compute["browsers"]; browsers.isArray())
                {
                    for (const auto& browser : browsers)
                    {
                        WorkspaceSidebarMetadata::BrowserSurface snapshot;
                        snapshot.id = _jsonString(browser, "browser_surface_id");
                        snapshot.surfaceId = _jsonString(browser, "surface_id");
                        snapshot.targetId = _jsonString(browser, "target_id");
                        snapshot.environmentId = _jsonString(browser, "environment_id");
                        snapshot.state = _jsonString(browser, "state");
                        snapshot.url = _jsonString(browser, "current_url");
                        snapshot.lastError = _jsonString(browser, "last_error");
                        metadata.browsers.emplace_back(std::move(snapshot));
                    }
                }
                if (const auto& environments = compute["environments"]; environments.isArray())
                {
                    for (const auto& environment : environments)
                    {
                        WorkspaceSidebarMetadata::ExecutionEnvironment snapshot;
                        snapshot.id = _jsonString(environment, "environment_id");
                        snapshot.targetId = _jsonString(environment, "target_id");
                        snapshot.runtimeVersion = _jsonString(environment, "runtime_version");
                        snapshot.state = _jsonString(environment, "lifecycle_state");
                        snapshot.launchMethod = _jsonString(environment, "launch_method");
                        snapshot.protocolVersion = environment.get("protocol_version", 0).asUInt();
                        metadata.environments.emplace_back(std::move(snapshot));
                    }
                }
                if (const auto& connections = compute["connections"]; connections.isArray())
                {
                    for (const auto& connection : connections)
                    {
                        WorkspaceSidebarMetadata::EnvironmentConnection snapshot;
                        snapshot.environmentId = _jsonString(connection, "environment_id");
                        snapshot.endpointId = _jsonString(connection, "current_endpoint_id");
                        snapshot.state = _jsonString(connection, "state");
                        snapshot.lastError = _jsonString(connection, "last_error");
                        snapshot.retryAttempt = connection.get("retry_attempt", 0).asUInt();
                        snapshot.nextRetryAtMs =
                            connection.get("next_retry_at_ms", Json::UInt64{ 0 }).asUInt64();
                        metadata.connections.emplace_back(std::move(snapshot));
                    }
                }
                if (const auto& events = compute["events"]; events.isArray())
                {
                    for (const auto& value : events)
                    {
                        WorkspaceSidebarMetadata::Event event;
                        event.id = _jsonString(value, "id");
                        event.kind = _jsonString(value, "kind");
                        event.source = _jsonString(value, "actor");
                        event.target = _jsonString(value, "subject_id");
                        event.timestampMs =
                            value.get("timestamp_ms", Json::UInt64{ 0 }).asUInt64();
                        const auto& payload = value["payload"];
                        event.summary = _jsonString(payload, "message");
                        if (event.summary.empty()) event.summary = _jsonString(payload, "state");
                        if (event.summary.empty()) event.summary = event.kind;
                        metadata.computeEvents.emplace_back(std::move(event));
                    }
                }
            }
            if (errors > 0)
            {
                metadata.agentActivity = winrt::to_hstring(fmt::format("{} error", errors));
            }
            else if (attention > 0)
            {
                metadata.agentActivity = winrt::to_hstring(fmt::format("{} waiting", attention));
            }
            else if (working > 0)
            {
                metadata.agentActivity = winrt::to_hstring(fmt::format("{} running", working));
            }
            else if (!metadata.agents.empty())
            {
                metadata.agentActivity = winrt::to_hstring(fmt::format("{} idle", metadata.agents.size()));
            }
            else
            {
                metadata.agentActivity = {};
            }

            metadata.events.clear();
            uint64_t latestEventMs = metadata.lastSeenEventMs;
            uint32_t unread = 0;
            for (const auto& computeEvent : metadata.computeEvents)
            {
                latestEventMs = std::max(latestEventMs, computeEvent.timestampMs);
                unread += computeEvent.timestampMs > metadata.lastSeenEventMs;
                metadata.events.emplace_back(computeEvent);
            }
            if (const auto& events = result["events"]; events.isArray())
            {
                for (const auto& value : events)
                {
                    WorkspaceSidebarMetadata::Event event;
                    event.id = _jsonString(value, "id");
                    event.kind = _jsonString(value, "kind");
                    event.source = _jsonString(value, "source");
                    event.target = _jsonString(value, "target");
                    event.timestampMs = value.get("timestamp_ms", Json::UInt64{ 0 }).asUInt64();
                    const auto& payload = value["payload"];
                    event.summary = _jsonString(payload, "text");
                    if (event.summary.empty()) event.summary = _jsonString(payload, "message");
                    if (event.summary.empty()) event.summary = _jsonString(payload, "summary");
                    if (event.summary.empty()) event.summary = event.kind;
                    latestEventMs = std::max(latestEventMs, event.timestampMs);
                    unread += event.timestampMs > metadata.lastSeenEventMs;
                    metadata.events.emplace_back(std::move(event));
                }
            }

            const auto focused = self->_GetFocusedTabImpl();
            if (focused && focused->StableId() == stableId)
            {
                metadata.lastSeenEventMs = latestEventMs;
                metadata.unread = 0;
            }
            else
            {
                metadata.unread = std::max(metadata.unread, unread);
            }

            // Rehydrate the per-surface remote routing cache from the canonical
            // Compute Store. This keeps verified file drop and remote lifecycle
            // behavior intact after native window/session restore without
            // creating a second persisted source of truth in the UI.
            if (const auto tab = self->_FindTabByStableId(stableId))
            {
                for (const auto& binding : metadata.surfaceBindings)
                {
                    if (binding.surfaceId.empty() ||
                        binding.homeTargetId.empty() ||
                        binding.remoteSessionId.empty())
                    {
                        continue;
                    }
                    try
                    {
                        tab->SetSurfaceRemoteRuntime(
                            winrt::guid{ binding.surfaceId },
                            Tab::SurfaceRemoteRuntime{
                                binding.homeTargetId,
                                binding.remoteSessionId });
                    }
                    CATCH_LOG();
                }
            }

            if (metadata.persisted && !metadata.workspaceId.empty())
            {
                const auto oldKey = L"tab:" + _lower(std::wstring{ stableId });
                const auto newKey = L"workspace:" + _lower(std::wstring{ metadata.workspaceId });
                if (const auto old = self->_workspaceSidebarPersistedMetadata.find(oldKey);
                    old != self->_workspaceSidebarPersistedMetadata.end() &&
                    self->_workspaceSidebarPersistedMetadata.find(newKey) == self->_workspaceSidebarPersistedMetadata.end())
                {
                    self->_workspaceSidebarPersistedMetadata[newKey] = old->second;
                }
                self->_workspaceSidebarPersistedMetadata[newKey] = metadata;
            }
        }

        // A repository may have several visible tabs. Reuse the expensive
        // Git/PR/ports result while retaining each tab's presentation state.
        for (auto& [otherId, other] : self->_workspaceSidebarMetadata)
        {
            if (otherId == std::wstring{ stableId } || other.cwd != cwd)
            {
                continue;
            }
            const auto pinned = other.pinned;
            const auto group = other.group;
            const auto lastSeen = other.lastSeenEventMs;
            other = metadata;
            other.pinned = pinned;
            other.group = group;
            other.lastSeenEventMs = lastSeen;
            other.refreshInFlight = false;
        }
        self->_SaveWorkspaceSidebarState();
        self->_RefreshWorkspaceSidebar(false);
    }
    CATCH_LOG();

    void TerminalPage::_RefreshWorkspaceSidebar(bool requestMetadata)
    {
        if (!_workspaceSidebarInitialized || !WorkspaceSidebarItems())
        {
            return;
        }

        _ApplyWorkspaceNavigationPresentation();
        const auto panel = WorkspaceSidebarItems();
        panel.Children().Clear();

        const auto focused = _GetFocusedTabImpl();
        const std::wstring focusedId = focused ? std::wstring{ focused->StableId() } : std::wstring{};
        const auto query = _lower(std::wstring{ WorkspaceSidebarSearch().Text() });

        struct Row
        {
            winrt::com_ptr<Tab> tab;
            uint32_t tabIndex;
        };
        std::vector<Row> rows;
        rows.reserve(_tabs.Size());
        for (uint32_t index = 0; index < _tabs.Size(); ++index)
        {
            const auto tab = _GetTabImpl(_tabs.GetAt(index));
            if (!tab)
            {
                continue;
            }
            if (const auto root = tab->GetRootPane())
            {
                root->SetOuterLeftPaddingTrim(_workspaceSidebarVisible);
            }
            auto& metadata = _WorkspaceSidebarMetadataFor(tab);
            if (requestMetadata)
            {
                _RequestWorkspaceSidebarMetadata(tab);
            }

            const auto matches = _containsInsensitive(tab->Title(), query) ||
                                 _containsInsensitive(metadata.cwd, query) ||
                                 _containsInsensitive(metadata.branch, query) ||
                                 _containsInsensitive(metadata.pullRequest, query) ||
                                 _containsInsensitive(metadata.group, query) ||
                                 _containsInsensitive(metadata.agentActivity, query);
            if (matches)
            {
                rows.push_back(Row{ tab, index });
            }
        }

        if (_ReconcileWorkspaceSidebarTabOrder())
        {
            _RefreshWorkspaceSidebar(false);
            return;
        }

        std::stable_sort(rows.begin(), rows.end(), [](const Row& a, const Row& b) {
            return a.tabIndex < b.tabIndex;
        });

        WorkspaceSidebarSummary().Text(query.empty()
                                           ? winrt::to_hstring(fmt::format(
                                                 "{} workspace{}",
                                                 _tabs.Size(),
                                                 _tabs.Size() == 1 ? "" : "s"))
                                           : winrt::to_hstring(fmt::format(
                                                 "{} of {} workspaces",
                                                 rows.size(),
                                                 _tabs.Size())));
        uint32_t totalUnread = 0;
        for (const auto& [_, metadata] : _workspaceSidebarMetadata)
        {
            totalUnread += metadata.unread;
        }
        WorkspaceSidebarAttentionCount().Text(totalUnread > 99 ? L"99+" : winrt::to_hstring(totalUnread));
        WorkspaceSidebarAttentionCount().Visibility(totalUnread > 0 ? Visibility::Visible : Visibility::Collapsed);
        AutomationProperties::SetName(
            WorkspaceSidebarAttentionButton(),
            totalUnread > 0
                ? winrt::to_hstring(fmt::format("Open attention center, {} unread", totalUnread))
                : L"Open attention center");

        if (focused && AgentMeshStatusText())
        {
            const auto& focusedMetadata = _WorkspaceSidebarMetadataFor(focused);
            uint32_t running = 0;
            uint32_t attentionCount = 0;
            for (const auto& agent : focusedMetadata.agents)
            {
                const auto status = _lower(std::wstring{ agent.activity });
                running += status.starts_with(L"working") || status.starts_with(L"running");
                attentionCount += status.starts_with(L"stale") ||
                                  status.starts_with(L"stopping") ||
                                  status.starts_with(L"error") ||
                                  status.starts_with(L"attention");
            }
            uint32_t openTasks = 0;
            for (const auto& task : focusedMetadata.tasks)
            {
                const auto status = _lower(std::wstring{ task.status });
                openTasks += status == L"pending" || status == L"assigned" || status == L"running";
            }
            AgentMeshStatusText().Text(
                focusedMetadata.agents.empty()
                    ? L"Agent mesh · no managed agents"
                    : winrt::to_hstring(fmt::format(
                          "Agent mesh · {} agent{} · {} running · {} task{}",
                          focusedMetadata.agents.size(),
                          focusedMetadata.agents.size() == 1 ? "" : "s",
                          running,
                          openTasks,
                          openTasks == 1 ? "" : "s")));
            const auto meshColor = attentionCount > 0
                                       ? Color{ 0xFF, 0xFF, 0xB9, 0x00 }
                                   : running > 0
                                       ? Color{ 0xFF, 0x4E, 0xCB, 0x71 }
                                       : Color{ 0xFF, 0x78, 0x78, 0x78 };
            AgentMeshStatusDot().Background(_brush(meshColor));
            AutomationProperties::SetName(
                AgentMeshStatusButton(),
                AgentMeshStatusText().Text() + L", open Agents and Tasks");
        }

        std::wstring renderedGroup;
        for (const auto& row : rows)
        {
            auto& metadata = _workspaceSidebarMetadata.at(std::wstring{ row.tab->StableId() });
            const auto group = metadata.pinned
                                   ? std::wstring{ L"pinned" }
                                   : (metadata.group.empty() ? std::wstring{ L"open workspaces" }
                                                              : _lower(std::wstring{ metadata.group }));
            const auto groupKey = metadata.pinned ? L"$pinned" : group;
            const bool isDefaultGroup = !metadata.pinned && metadata.group.empty();
            if (renderedGroup != groupKey)
            {
                renderedGroup = groupKey;
                if (!isDefaultGroup)
                {
                    const bool collapsed = _workspaceSidebarGroupCollapsed[groupKey];

                    Button header;
                    header.Margin(Thickness{ 0.0, panel.Children().Size() == 0 ? 0.0 : 8.0, 0.0, 2.0 });
                    header.Padding(Thickness{ 8, 5, 6, 5 });
                    header.HorizontalAlignment(HorizontalAlignment::Stretch);
                    header.HorizontalContentAlignment(HorizontalAlignment::Stretch);
                    header.Background(_brush(Color{ 0, 0, 0, 0 }));
                    header.BorderBrush(_brush(Color{ 0, 0, 0, 0 }));

                    Grid headerGrid;
                    headerGrid.ColumnDefinitions().Append(ColumnDefinition{});
                    headerGrid.ColumnDefinitions().GetAt(0).Width(GridLengthHelper::FromValueAndType(1, GridUnitType::Star));
                    headerGrid.ColumnDefinitions().Append(ColumnDefinition{});
                    headerGrid.ColumnDefinitions().GetAt(1).Width(GridLengthHelper::Auto());

                    TextBlock label;
                    label.Text(metadata.pinned ? L"Pinned" : metadata.group);
                    label.FontSize(11);
                    label.FontWeight(FontWeights::SemiBold());
                    label.Opacity(0.7);

                    FontIcon chevron;
                    chevron.FontFamily(winrt::Windows::UI::Xaml::Media::FontFamily{ L"Segoe Fluent Icons" });
                    chevron.FontSize(10);
                    chevron.Glyph(collapsed ? L"\xE76C" : L"\xE70D");
                    chevron.Opacity(0.62);
                    Grid::SetColumn(chevron, 1);
                    headerGrid.Children().Append(label);
                    headerGrid.Children().Append(chevron);
                    header.Content(headerGrid);
                    AutomationProperties::SetName(header, label.Text() + (collapsed ? L", collapsed" : L", expanded"));
                    header.Click([weakThis = get_weak(), groupKey](auto&&, auto&&) {
                        if (const auto self = weakThis.get())
                        {
                            self->_workspaceSidebarGroupCollapsed[groupKey] =
                                !self->_workspaceSidebarGroupCollapsed[groupKey];
                            self->_SaveWorkspaceSidebarState();
                            self->_RefreshWorkspaceSidebar(false);
                        }
                    });
                    panel.Children().Append(header);
                }
            }

            if (!isDefaultGroup && _workspaceSidebarGroupCollapsed[groupKey])
            {
                continue;
            }

            const bool selected = std::wstring{ row.tab->StableId() } == focusedId;
            const auto customColor = row.tab->GetTabColor();
            const auto accent = customColor.value_or(Color{ 0xFF, 0x4C, 0xC2, 0xFF });

            Button button;
            button.Padding(Thickness{ 0 });
            button.Margin(Thickness{ 0, 0, 0, 2 });
            button.HorizontalAlignment(HorizontalAlignment::Stretch);
            button.HorizontalContentAlignment(HorizontalAlignment::Stretch);
            button.VerticalContentAlignment(VerticalAlignment::Stretch);
            button.Background(_brush(Color{ 0, 0, 0, 0 }));
            button.BorderBrush(_brush(Color{ 0, 0, 0, 0 }));

            Border card;
            card.HorizontalAlignment(HorizontalAlignment::Stretch);
            card.MinHeight(54);
            card.CornerRadius(winrt::Windows::UI::Xaml::CornerRadius{ 6.0 });
            card.Padding(Thickness{ 10, 8, 9, 8 });
            card.BorderThickness(selected && customColor ? Thickness{ 1.5 } : Thickness{ 3, 0, 0, 0 });
            card.BorderBrush(_brush(accent));
            if (customColor)
            {
                // Match the native horizontal tab treatment: the selected item
                // uses the full identity color and inactive items retain a 30%
                // wash. This keeps workspace identity visible across selection
                // changes instead of reducing it to a three-pixel rail.
                auto background = accent;
                background.A = selected ? 0xFF : 77;
                card.Background(_brush(background));

                if (selected)
                {
                    constexpr auto lightnessThreshold = 0.6f;
                    const auto foreground = ColorFix::GetLightness(til::color{ accent }) >= lightnessThreshold ?
                                                Colors::Black() :
                                                Colors::White();
                    button.Foreground(_brush(foreground));
                }
            }
            else
            {
                card.Background(_themeBrush(
                    selected ? L"SystemControlHighlightListAccentLowBrush" : L"SystemControlHighlightListLowBrush",
                    selected ? Color{ 0xFF, 0x0A, 0x62, 0xA8 } : Color{ 0x10, 0xFF, 0xFF, 0xFF }));
            }

            StackPanel stack;
            stack.Spacing(3);

            Grid titleGrid;
            titleGrid.ColumnDefinitions().Append(ColumnDefinition{});
            titleGrid.ColumnDefinitions().GetAt(0).Width(GridLengthHelper::Auto());
            titleGrid.ColumnDefinitions().Append(ColumnDefinition{});
            titleGrid.ColumnDefinitions().GetAt(1).Width(GridLengthHelper::FromValueAndType(1, GridUnitType::Star));
            titleGrid.ColumnDefinitions().Append(ColumnDefinition{});
            titleGrid.ColumnDefinitions().GetAt(2).Width(GridLengthHelper::Auto());

            winrt::Windows::UI::Xaml::Shapes::Ellipse statusDot;
            statusDot.Width(7);
            statusDot.Height(7);
            statusDot.VerticalAlignment(VerticalAlignment::Center);
            auto dotColor = Color{ 0xFF, 0x78, 0x78, 0x78 };
            if (const auto agent = row.tab->FindAgentPaneContent())
            {
                const auto impl = winrt::get_self<implementation::AgentPaneContent>(agent);
                if (impl->GetAutofixState() == AgentPaneContent::AutofixState::Review)
                {
                    dotColor = Color{ 0xFF, 0xFF, 0xB9, 0x00 };
                }
                else if (impl->IsAgentConnected())
                {
                    dotColor = Color{ 0xFF, 0x16, 0xC6, 0x0C };
                    if (metadata.agentActivity.empty() && !impl->GetAgentName().empty())
                    {
                        metadata.agentActivity = impl->GetAgentName();
                    }
                }
                else if (!impl->GetAgentState().empty())
                {
                    dotColor = Color{ 0xFF, 0xE7, 0x48, 0x56 };
                }
            }
            statusDot.Fill(_brush(dotColor));

            TextBlock title;
            title.Text(row.tab->Title());
            title.FontSize(14);
            title.FontWeight(FontWeights::SemiBold());
            title.TextTrimming(TextTrimming::CharacterEllipsis);
            Grid::SetColumn(title, 1);

            StackPanel markers;
            markers.Orientation(Orientation::Horizontal);
            markers.Spacing(5);
            markers.Margin(Thickness{ 0, 0, 7, 0 });
            markers.VerticalAlignment(VerticalAlignment::Center);
            if (metadata.pinned)
            {
                FontIcon pinIcon;
                pinIcon.FontFamily(winrt::Windows::UI::Xaml::Media::FontFamily{ L"Segoe Fluent Icons" });
                pinIcon.FontSize(10);
                pinIcon.Glyph(L"\xE840");
                pinIcon.Opacity(0.7);
                markers.Children().Append(pinIcon);
            }
            if (const auto nativeIcon = _CreateNewTabFlyoutIcon(row.tab->Icon()))
            {
                nativeIcon.Width(14);
                nativeIcon.Height(14);
                nativeIcon.Opacity(0.82);
                markers.Children().Append(nativeIcon);
            }
            markers.Children().Append(statusDot);
            titleGrid.Children().Append(markers);
            titleGrid.Children().Append(title);
            if (metadata.unread > 0)
            {
                Border badge;
                badge.MinWidth(18);
                badge.Height(18);
                badge.Padding(Thickness{ 5, 0, 5, 0 });
                badge.CornerRadius(winrt::Windows::UI::Xaml::CornerRadius{ 9.0 });
                badge.Background(_brush(Color{ 0xFF, 0x4C, 0xC2, 0xFF }));
                TextBlock count;
                count.Text(metadata.unread > 99 ? L"99+" : winrt::to_hstring(metadata.unread));
                count.FontSize(10);
                count.HorizontalAlignment(HorizontalAlignment::Center);
                count.VerticalAlignment(VerticalAlignment::Center);
                badge.Child(count);
                Grid::SetColumn(badge, 2);
                titleGrid.Children().Append(badge);
            }
            stack.Children().Append(titleGrid);

            const bool compact = _workspaceSidebarWidth < 252.0;
            const auto statusText = !metadata.lastNotification.empty()
                                        ? metadata.lastNotification
                                        : metadata.agentActivity;
            if (!compact && !statusText.empty())
            {
                TextBlock status;
                status.Text(statusText);
                status.FontSize(11);
                status.Opacity(0.76);
                status.TextTrimming(TextTrimming::CharacterEllipsis);
                stack.Children().Append(status);
            }

            if (!compact && (!metadata.cwd.empty() || !metadata.branch.empty()))
            {
                std::wstring context;
                if (!metadata.branch.empty())
                {
                    context = metadata.branch;
                }
                if (!metadata.cwd.empty())
                {
                    if (!context.empty())
                    {
                        context += L"  ·  ";
                    }
                    context += std::wstring{ _compactPath(metadata.cwd) };
                }

                TextBlock contextLine;
                contextLine.Text(winrt::hstring{ context });
                contextLine.FontFamily(winrt::Windows::UI::Xaml::Media::FontFamily{ L"Cascadia Mono" });
                contextLine.FontSize(10);
                contextLine.Opacity(0.58);
                contextLine.TextTrimming(TextTrimming::CharacterEllipsis);
                stack.Children().Append(contextLine);
            }

            StackPanel chips;
            chips.Orientation(Orientation::Horizontal);
            chips.Spacing(4);
            if (metadata.changedFiles > 0)
            {
                chips.Children().Append(_chip(
                    winrt::to_hstring(fmt::format("{} changed", metadata.changedFiles)),
                    Color{ 0xFF, 0xFF, 0xB9, 0x00 }));
            }
            if (metadata.ahead > 0 || metadata.behind > 0)
            {
                chips.Children().Append(_chip(
                    winrt::to_hstring(fmt::format("↑{} ↓{}", metadata.ahead, metadata.behind)),
                    Color{ 0xFF, 0x6C, 0xB8, 0xFF }));
            }
            if (!metadata.pullRequest.empty()) chips.Children().Append(_chip(metadata.pullRequest, Color{ 0xFF, 0xC5, 0x86, 0xC0 }));
            if (!metadata.ports.empty()) chips.Children().Append(_chip(metadata.ports, Color{ 0xFF, 0x2D, 0xB9, 0xB0 }));
            if (!compact && chips.Children().Size() > 0)
            {
                stack.Children().Append(chips);
            }

            if (!compact && !metadata.agents.empty())
            {
                StackPanel agents;
                agents.Margin(Thickness{ 10, 3, 0, 0 });
                agents.Spacing(2);
                const auto count = std::min<size_t>(metadata.agents.size(), 4);
                for (size_t index = 0; index < count; ++index)
                {
                    const auto& agent = metadata.agents[index];
                    Grid agentRow;
                    agentRow.ColumnDefinitions().Append(ColumnDefinition{});
                    agentRow.ColumnDefinitions().GetAt(0).Width(GridLengthHelper::Auto());
                    agentRow.ColumnDefinitions().Append(ColumnDefinition{});
                    agentRow.ColumnDefinitions().GetAt(1).Width(GridLengthHelper::FromValueAndType(1, GridUnitType::Star));
                    agentRow.ColumnDefinitions().Append(ColumnDefinition{});
                    agentRow.ColumnDefinitions().GetAt(2).Width(GridLengthHelper::Auto());

                    TextBlock tree;
                    tree.Text(index + 1 == count ? L"└" : L"├");
                    tree.FontFamily(winrt::Windows::UI::Xaml::Media::FontFamily{ L"Cascadia Mono" });
                    tree.FontSize(10);
                    tree.Opacity(0.42);

                    TextBlock identity;
                    identity.Text(agent.role.empty() ? agent.id : agent.id + L" · " + agent.role);
                    identity.FontSize(10);
                    identity.TextTrimming(TextTrimming::CharacterEllipsis);
                    identity.Margin(Thickness{ 5, 0, 5, 0 });
                    Grid::SetColumn(identity, 1);

                    TextBlock activity;
                    activity.Text(agent.activity);
                    activity.FontSize(10);
                    activity.Opacity(0.64);
                    Grid::SetColumn(activity, 2);

                    agentRow.Children().Append(tree);
                    agentRow.Children().Append(identity);
                    agentRow.Children().Append(activity);
                    AutomationProperties::SetName(
                        agentRow,
                        identity.Text() + (agent.activity.empty() ? L"" : L", " + agent.activity));
                    agents.Children().Append(agentRow);
                }
                stack.Children().Append(agents);
            }

            card.Child(stack);
            button.Content(card);
            AutomationProperties::SetName(
                button,
                title.Text() + (metadata.unread > 0
                                    ? winrt::to_hstring(fmt::format(", {} unread", metadata.unread))
                                    : L""));
            std::wstring tooltip{ title.Text() };
            if (!metadata.cwd.empty())
            {
                tooltip += L"\n";
                tooltip += std::wstring{ metadata.cwd };
            }
            if (!metadata.branch.empty())
            {
                tooltip += L"\nBranch: ";
                tooltip += std::wstring{ metadata.branch };
            }
            if (!statusText.empty())
            {
                tooltip += L"\n";
                tooltip += std::wstring{ statusText };
            }
            ToolTipService::SetToolTip(button, winrt::box_value(winrt::hstring{ tooltip }));

            const auto stableId = row.tab->StableId();
            button.Click([weakThis = get_weak(), stableId](auto&&, auto&&) {
                if (const auto self = weakThis.get())
                {
                    if (const auto tab = self->_FindTabByStableId(stableId))
                    {
                        uint32_t index = 0;
                        if (self->_tabs.IndexOf(*tab, index))
                        {
                            auto& metadata = self->_workspaceSidebarMetadata[std::wstring{ stableId }];
                            metadata.unread = 0;
                            for (const auto& event : metadata.events)
                            {
                                metadata.lastSeenEventMs = std::max(metadata.lastSeenEventMs, event.timestampMs);
                            }
                            self->_workspaceSidebarPersistedMetadata[self->_WorkspaceSidebarPersistentKey(tab)] = metadata;
                            self->_SaveWorkspaceSidebarState();
                            self->_SelectTab(index);
                            self->_RefreshWorkspaceSidebar(false);
                        }
                    }
                }
            });

            // Commands and state rules are canonical in Tab, but WinUI visual
            // elements cannot safely share one MenuFlyout instance. Decorate
            // both independently-owned presenters with workspace commands.
            if (const auto nativeMenu = row.tab->TabViewItem().ContextFlyout().try_as<MenuFlyout>())
            {
                _EnsureWorkspaceContextMenuExtension(row.tab, nativeMenu);
            }
            const auto sidebarMenu = row.tab->CreateContextMenuForTarget(button, true);
            _EnsureWorkspaceContextMenuExtension(row.tab, sidebarMenu);
            button.ContextFlyout(sidebarMenu);
            panel.Children().Append(button);
        }

        if (rows.empty())
        {
            StackPanel emptyState;
            emptyState.Margin(Thickness{ 12, 28, 12, 0 });
            emptyState.Spacing(4);

            FontIcon icon;
            icon.FontFamily(winrt::Windows::UI::Xaml::Media::FontFamily{ L"Segoe Fluent Icons" });
            icon.FontSize(18);
            icon.Glyph(query.empty() ? L"\xE8B7" : L"\xE721");
            icon.HorizontalAlignment(HorizontalAlignment::Center);
            icon.Opacity(0.48);

            TextBlock message;
            message.Text(query.empty() ? L"No open workspaces" : L"No matching workspaces");
            message.FontSize(12);
            message.HorizontalAlignment(HorizontalAlignment::Center);
            message.Opacity(0.64);

            emptyState.Children().Append(icon);
            emptyState.Children().Append(message);
            panel.Children().Append(emptyState);
        }

        if (_workspaceSidebarShowRecent && !_workspaceSidebarRecent.empty())
        {
            TextBlock recentHeader;
            recentHeader.Text(L"RECENTLY CLOSED");
            recentHeader.Margin(Thickness{ 6, 12, 0, 4 });
            recentHeader.FontSize(10);
            recentHeader.FontWeight(FontWeights::SemiBold());
            recentHeader.CharacterSpacing(70);
            recentHeader.Opacity(0.56);
            panel.Children().Append(recentHeader);

            for (size_t index = 0; index < _workspaceSidebarRecent.size(); ++index)
            {
                const auto& item = _workspaceSidebarRecent[index];
                Button recent;
                recent.HorizontalContentAlignment(HorizontalAlignment::Stretch);
                recent.Padding(Thickness{ 8, 6, 8, 6 });
                recent.Background(_brush(Color{ 0x0C, 0xFF, 0xFF, 0xFF }));
                recent.BorderBrush(_brush(Color{ 0, 0, 0, 0 }));

                StackPanel content;
                TextBlock title;
                title.Text(item.title.empty() ? L"Workspace" : item.title);
                title.FontSize(12);
                title.TextTrimming(TextTrimming::CharacterEllipsis);
                content.Children().Append(title);
                if (!item.cwd.empty())
                {
                    TextBlock path;
                    path.Text(_compactPath(item.cwd));
                    path.FontFamily(winrt::Windows::UI::Xaml::Media::FontFamily{ L"Cascadia Mono" });
                    path.FontSize(10);
                    path.Opacity(0.48);
                    path.TextTrimming(TextTrimming::CharacterEllipsis);
                    content.Children().Append(path);
                }
                recent.Content(content);
                ToolTipService::SetToolTip(recent, winrt::box_value(L"Reopen workspace"));
                recent.Click([weakThis = get_weak(), index](auto&&, auto&&) {
                    if (const auto self = weakThis.get())
                    {
                        self->_RestoreRecentlyClosedWorkspace(index);
                    }
                });
                panel.Children().Append(recent);
            }
        }

        if (WorkspaceFleetOverlay().Visibility() == Visibility::Visible)
        {
            _RefreshWorkspaceFleet();
        }
    }

    void TerminalPage::_RecordRecentlyClosedWorkspace(const winrt::TerminalApp::Tab& tab)
    {
        const auto impl = _GetTabImpl(tab);
        if (!impl)
        {
            return;
        }
        const std::wstring stableId{ impl->StableId() };
        if (!_workspaceSidebarRecordedClosedIds.emplace(stableId).second)
        {
            return;
        }

        auto& metadata = _WorkspaceSidebarMetadataFor(impl);
        RecentlyClosedWorkspace item;
        item.title = tab.Title();
        item.cwd = metadata.cwd;
        item.workspaceId = metadata.workspaceId;
        item.workspaceName = metadata.workspaceName;
        item.manifestPath = metadata.manifestPath;
        item.group = metadata.group;
        item.closedAtMs = _nowMs();
        if (const auto pending = _workspaceSidebarPendingHistoryIds.find(stableId);
            pending != _workspaceSidebarPendingHistoryIds.end())
        {
            item.nativeHistoryId = pending->second;
            _workspaceSidebarPendingHistoryIds.erase(pending);
        }
        if (metadata.persisted && !metadata.cwd.empty() && !metadata.workspaceName.empty())
        {
            _SnapshotDeclarativeWorkspace(metadata.cwd, metadata.workspaceName);
        }
        _workspaceSidebarRecent.insert(_workspaceSidebarRecent.begin(), std::move(item));
        if (_workspaceSidebarRecent.size() > 10)
        {
            _workspaceSidebarRecent.resize(10);
        }
        _SaveWorkspaceSidebarState();
    }

    void TerminalPage::_MarkWorkspaceUnread(const winrt::hstring& stableId,
                                            const winrt::hstring& notification)
    {
        const auto focused = _GetFocusedTabImpl();
        if (focused && focused->StableId() == stableId)
        {
            return;
        }
        const auto tab = _FindTabByStableId(stableId);
        if (!tab)
        {
            return;
        }
        auto& metadata = _WorkspaceSidebarMetadataFor(tab);
        ++metadata.unread;
        metadata.lastNotification = notification;
        _RefreshWorkspaceSidebar(false);
    }

    safe_void_coroutine TerminalPage::_PromptWorkspaceGroup(winrt::hstring stableId)
    {
        auto strong = get_strong();
        if (const auto presenter = _dialogPresenter.get())
        {
            const auto dialog = FindName(L"WorkspaceGroupDialog").try_as<ContentDialog>();
            WorkspaceGroupNameTextBox().Text({});
            const auto result = co_await presenter.ShowDialog(dialog);
            if (result == ContentDialogResult::Primary)
            {
                auto name = WorkspaceGroupNameTextBox().Text();
                std::wstring trimmed{ name };
                const auto first = trimmed.find_first_not_of(L" \t\r\n");
                const auto last = trimmed.find_last_not_of(L" \t\r\n");
                if (first != std::wstring::npos)
                {
                    name = winrt::hstring{ trimmed.substr(first, last - first + 1) };
                    if (const auto tab = _FindTabByStableId(stableId))
                    {
                        auto& metadata = _WorkspaceSidebarMetadataFor(tab);
                        metadata.group = name;
                        _workspaceSidebarPersistedMetadata[_WorkspaceSidebarPersistentKey(tab)] = metadata;
                        _SaveWorkspaceSidebarState();
                        _RefreshWorkspaceSidebar(false);
                    }
                }
            }
        }
    }

    void TerminalPage::_RestoreRecentlyClosedWorkspace(size_t recentIndex)
    {
        if (recentIndex >= _workspaceSidebarRecent.size())
        {
            return;
        }
        const auto recent = _workspaceSidebarRecent[recentIndex];

        const auto nativeHistory = std::find_if(
            _previouslyClosedPanesAndTabs.begin(),
            _previouslyClosedPanesAndTabs.end(),
            [&](const auto& entry) {
                return entry.id == recent.nativeHistoryId;
            });
        if (nativeHistory != _previouslyClosedPanesAndTabs.end())
        {
            const auto actions = nativeHistory->actions;
            _previouslyClosedPanesAndTabs.erase(nativeHistory);
            for (const auto& action : actions)
            {
                _actionDispatch->DoAction(action);
            }
        }
        else if (!recent.workspaceId.empty() && !recent.workspaceName.empty())
        {
            _RestoreDeclarativeWorkspace(recent);
        }
        else
        {
            NewTerminalArgs args;
            if (!recent.cwd.empty()) args.StartingDirectory(recent.cwd);
            if (!recent.title.empty()) args.TabTitle(recent.title);
            _OpenNewTab(args);
        }

        _workspaceSidebarRecent.erase(_workspaceSidebarRecent.begin() + recentIndex);
        _SaveWorkspaceSidebarState();
        _RefreshWorkspaceSidebar(true);
    }

    safe_void_coroutine TerminalPage::_ShowWorkspaceComposer()
    try
    {
        auto strong = get_strong();
        const auto focused = _GetFocusedTabImpl();
        auto root = focused ? _WorkspaceSidebarMetadataFor(focused).cwd : winrt::hstring{};
        WorkspaceComposerRoot().Text(root);
        WorkspaceComposerName().Text({});
        WorkspaceComposerPreview().Text({});
        WorkspaceComposerPreview().Visibility(Visibility::Collapsed);
        if (const auto presenter = _dialogPresenter.get())
        {
            const auto result = co_await presenter.ShowDialog(WorkspaceComposerDialog());
            if (result == ContentDialogResult::Primary)
            {
                _CreateWorkspaceFromComposer();
            }
        }
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_PreviewWorkspaceComposer()
    try
    {
        auto strong = get_strong();
        const auto selected = WorkspaceComposerTemplate().SelectedItem().try_as<ComboBoxItem>();
        const auto templateName = selected
                                      ? winrt::unbox_value_or<winrt::hstring>(selected.Tag(), L"feature")
                                      : winrt::hstring{ L"feature" };
        auto name = WorkspaceComposerName().Text();
        if (name.empty())
        {
            name = L"agent-workspace";
        }
        WorkspaceComposerPreview().Text(L"Loading preview…");
        WorkspaceComposerPreview().Visibility(Visibility::Visible);
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const auto args = L"agent-workspace template " + _quoteArg(templateName) +
                          L" --name " + _quoteArg(name) + L" --stdout";
        co_await winrt::resume_background();
        const auto output = ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, args, 8'000);
        co_await wil::resume_foreground(dispatcher);
        WorkspaceComposerPreview().Text(
            output.empty() ? L"Preview failed. Run Workspace diagnostics for details."
                           : winrt::to_hstring(output));
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_CreateWorkspaceFromComposer()
    try
    {
        auto strong = get_strong();
        const auto selected = WorkspaceComposerTemplate().SelectedItem().try_as<ComboBoxItem>();
        const auto templateName = selected
                                      ? winrt::unbox_value_or<winrt::hstring>(selected.Tag(), L"feature")
                                      : winrt::hstring{ L"feature" };
        const auto name = WorkspaceComposerName().Text();
        const auto root = WorkspaceComposerRoot().Text();
        if (name.empty() || root.empty() || !std::filesystem::is_directory(std::filesystem::path{ root.c_str() }))
        {
            _ShowControlNoticeDialog(
                L"Workspace not created",
                L"Enter a workspace name and an existing project directory.");
            co_return;
        }

        const auto manifest = (std::filesystem::path{ root.c_str() } /
                               L".intelligent-terminal" /
                               L"manifests" /
                               (_workspaceSlug(name) + L".yaml"))
                                  .wstring();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const auto templateArgs = L"agent-workspace template " + _quoteArg(templateName) +
                                  L" --name " + _quoteArg(name) +
                                  L" --output " + _quoteArg(winrt::hstring{ manifest });
        const auto applyArgs = L"agent-workspace apply " + _quoteArg(winrt::hstring{ manifest });
        const auto dispatcher = Dispatcher();
        co_await winrt::resume_background();
        const auto rendered = ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, templateArgs, 10'000);
        const auto applied = rendered.empty()
                                 ? std::string{}
                                 : ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, applyArgs, 45'000);
        co_await wil::resume_foreground(dispatcher);
        if (applied.empty())
        {
            _ShowControlNoticeDialog(
                L"Workspace not created",
                L"The manifest could not be written or applied. Existing manifests are never overwritten automatically; run Workspace diagnostics for details.");
            co_return;
        }
        _ShowControlNoticeDialog(
            L"Workspace created",
            L"The native pane layout is open and its runtime, events, and restore state are now tracked by WTA.");
        for (auto& [_, metadata] : _workspaceSidebarMetadata)
        {
            metadata.refreshedAtMs = 0;
        }
        _RefreshWorkspaceSidebar(true);
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_ShowWorkspaceAttentionCenter()
    try
    {
        auto strong = get_strong();
        const auto panel = WorkspaceAttentionItems();
        panel.Children().Clear();

        struct AttentionRow
        {
            winrt::hstring stableId;
            winrt::hstring workspace;
            winrt::hstring target;
            WorkspaceSidebarMetadata::Event event;
        };
        std::vector<AttentionRow> rows;
        for (auto& [stableId, metadata] : _workspaceSidebarMetadata)
        {
            for (const auto& event : metadata.events)
            {
                rows.push_back(AttentionRow{
                    winrt::hstring{ stableId },
                    metadata.workspaceName.empty() ? L"Workspace" : metadata.workspaceName,
                    event.target,
                    event,
                });
                metadata.lastSeenEventMs = std::max(metadata.lastSeenEventMs, event.timestampMs);
            }
            metadata.unread = 0;
            if (const auto tab = _FindTabByStableId(winrt::hstring{ stableId }))
            {
                _workspaceSidebarPersistedMetadata[_WorkspaceSidebarPersistentKey(tab)] = metadata;
            }
        }
        std::stable_sort(rows.begin(), rows.end(), [](const auto& left, const auto& right) {
            return left.event.timestampMs > right.event.timestampMs;
        });
        if (rows.empty())
        {
            TextBlock empty;
            empty.Text(L"No durable workspace events need attention.");
            empty.Opacity(0.66);
            empty.TextWrapping(TextWrapping::Wrap);
            panel.Children().Append(empty);
        }
        for (size_t index = 0; index < std::min<size_t>(rows.size(), 100); ++index)
        {
            const auto row = rows[index];
            Button button;
            button.HorizontalContentAlignment(HorizontalAlignment::Stretch);
            button.Padding(Thickness{ 10, 8, 10, 8 });
            button.Background(_themeBrush(L"SystemControlHighlightListLowBrush", Color{ 0x10, 0xFF, 0xFF, 0xFF }));

            StackPanel content;
            content.Spacing(3);
            TextBlock heading;
            heading.Text(row.workspace + L" · " + row.event.kind);
            heading.FontWeight(FontWeights::SemiBold());
            TextBlock summary;
            summary.Text(row.event.summary);
            summary.TextWrapping(TextWrapping::Wrap);
            TextBlock source;
            source.Text(L"From " + row.event.source +
                        (row.event.target.empty() ? L"" : L" → " + row.event.target));
            source.FontSize(11);
            source.Opacity(0.58);
            content.Children().Append(heading);
            content.Children().Append(summary);
            content.Children().Append(source);
            button.Content(content);
            AutomationProperties::SetName(button, heading.Text() + L", " + summary.Text());
            button.Click([weakThis = get_weak(), stableId = row.stableId, target = row.target](auto&&, auto&&) {
                if (const auto self = weakThis.get())
                {
                    self->_FocusWorkspaceAgent(stableId, target);
                }
            });
            panel.Children().Append(button);
        }
        _SaveWorkspaceSidebarState();
        _RefreshWorkspaceSidebar(false);
        if (const auto presenter = _dialogPresenter.get())
        {
            co_await presenter.ShowDialog(WorkspaceAttentionDialog());
        }
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_ShowWorkspaceFleet()
    try
    {
        auto strong = get_strong();
        _RefreshWorkspaceFleet();
        WorkspaceFleetOverlay().Visibility(Visibility::Visible);
        co_return;
    }
    CATCH_LOG();

    void TerminalPage::_RefreshWorkspaceFleet()
    {
        if (!WorkspaceFleetOverlay())
        {
            return;
        }

        const auto agentsPanel = WorkspaceFleetAgentItems();
        const auto queuedPanel = WorkspaceFleetQueuedTasks();
        const auto runningPanel = WorkspaceFleetRunningTasks();
        const auto completedPanel = WorkspaceFleetCompletedTasks();
        const auto targetPanel = WorkspaceFleetTargetItems();
        const auto jobPanel = WorkspaceFleetJobItems();
        agentsPanel.Children().Clear();
        queuedPanel.Children().Clear();
        runningPanel.Children().Clear();
        completedPanel.Children().Clear();
        targetPanel.Children().Clear();
        jobPanel.Children().Clear();

        const auto focused = _GetFocusedTabImpl();
        if (!focused)
        {
            WorkspaceFleetScopeText().Text(L"No focused workspace");
            WorkspaceFleetAgentCount().Text(L"0");
            WorkspaceFleetRunningCount().Text(L"0");
            WorkspaceFleetAttentionCount().Text(L"0");
            WorkspaceFleetTaskCount().Text(L"0");
            WorkspaceFleetTeamSummary().Text(L"No native team");
            WorkspaceFleetTargetSummary().Text(L"0 targets");
            WorkspaceFleetJobSummary().Text(L"0 jobs");
            return;
        }

        const auto stableId = focused->StableId();
        const auto& metadata = _WorkspaceSidebarMetadataFor(focused);
        const auto workspaceName = metadata.workspaceName.empty() ? focused->Title() : metadata.workspaceName;
        WorkspaceFleetTitleText().Text(L"Agents & Tasks");
        WorkspaceFleetScopeText().Text(
            workspaceName +
            (metadata.branch.empty() ? L"" : L"  ·  " + metadata.branch) +
            (metadata.cwd.empty() ? L"" : L"  ·  " + _compactPath(metadata.cwd)));

        uint32_t runningAgents = 0;
        uint32_t attentionAgents = 0;
        for (const auto& agent : metadata.agents)
        {
            const auto status = _lower(std::wstring{ agent.activity });
            runningAgents += status.starts_with(L"working") || status.starts_with(L"running");
            attentionAgents += status.starts_with(L"stale") ||
                               status.starts_with(L"stopping") ||
                               status.starts_with(L"error") ||
                               status.starts_with(L"attention");

            Button button;
            button.HorizontalAlignment(HorizontalAlignment::Stretch);
            button.HorizontalContentAlignment(HorizontalAlignment::Stretch);
            button.Padding(Thickness{ 10, 9, 10, 9 });
            button.Background(_themeBrush(
                L"SystemControlHighlightListLowBrush",
                Color{ 0x10, 0xFF, 0xFF, 0xFF }));
            button.BorderBrush(_brush(Color{ 0, 0, 0, 0 }));

            Grid row;
            row.ColumnDefinitions().Append(ColumnDefinition{});
            row.ColumnDefinitions().GetAt(0).Width(GridLengthHelper::Auto());
            row.ColumnDefinitions().Append(ColumnDefinition{});
            row.ColumnDefinitions().GetAt(1).Width(
                GridLengthHelper::FromValueAndType(1, GridUnitType::Star));
            row.ColumnDefinitions().Append(ColumnDefinition{});
            row.ColumnDefinitions().GetAt(2).Width(GridLengthHelper::Auto());

            winrt::Windows::UI::Xaml::Shapes::Ellipse dot;
            dot.Width(8);
            dot.Height(8);
            dot.Margin(Thickness{ 0, 0, 9, 0 });
            dot.VerticalAlignment(VerticalAlignment::Center);
            const auto dotColor = status.starts_with(L"working") || status.starts_with(L"running")
                                      ? Color{ 0xFF, 0x4E, 0xCB, 0x71 }
                                  : status.starts_with(L"stale") || status.starts_with(L"error")
                                      ? Color{ 0xFF, 0xE7, 0x48, 0x56 }
                                  : status.starts_with(L"stopping") || status.starts_with(L"attention")
                                      ? Color{ 0xFF, 0xFF, 0xB9, 0x00 }
                                      : Color{ 0xFF, 0x78, 0x78, 0x78 };
            dot.Fill(_brush(dotColor));

            StackPanel identity;
            TextBlock name;
            name.Text((agent.coordinator ? L"★ " : L"") +
                      agent.id +
                      (agent.role.empty() ? L"" : L"  ·  " + agent.role));
            name.FontWeight(FontWeights::SemiBold());
            name.TextTrimming(TextTrimming::CharacterEllipsis);
            TextBlock detail;
            detail.Text(
                (agent.teamName.empty() ? L"Surface agent" : agent.teamName) +
                (agent.currentTaskId.empty() ? L"" : L"  ·  " + agent.currentTaskId) +
                (agent.model.empty() ? L"" : L"  ·  " + agent.model));
            detail.FontSize(10);
            detail.Opacity(0.58);
            detail.TextTrimming(TextTrimming::CharacterEllipsis);
            identity.Children().Append(name);
            identity.Children().Append(detail);
            Grid::SetColumn(identity, 1);

            TextBlock activity;
            activity.Text(agent.activity.empty() ? L"idle" : agent.activity);
            activity.FontSize(10);
            activity.Opacity(0.72);
            activity.VerticalAlignment(VerticalAlignment::Center);
            activity.Margin(Thickness{ 8, 0, 0, 0 });
            Grid::SetColumn(activity, 2);

            row.Children().Append(dot);
            row.Children().Append(identity);
            row.Children().Append(activity);
            button.Content(row);
            AutomationProperties::SetName(button, name.Text() + L", " + activity.Text());
            button.Click([
                weakThis = get_weak(),
                stableId,
                target = agent.sessionId.empty() ? agent.id : L"session:" + agent.sessionId
            ](auto&&, auto&&) {
                if (const auto self = weakThis.get())
                {
                    self->_FocusWorkspaceAgent(stableId, target);
                    self->WorkspaceFleetOverlay().Visibility(Visibility::Collapsed);
                }
            });
            agentsPanel.Children().Append(button);
        }

        if (metadata.agents.empty())
        {
            TextBlock empty;
            empty.Margin(Thickness{ 10, 18, 10, 0 });
            empty.Text(L"No managed agents are bound to this workspace yet.\n"
                       L"Open the Chat Pane for a surface or add a native team worker.");
            empty.TextWrapping(TextWrapping::Wrap);
            empty.Opacity(0.62);
            agentsPanel.Children().Append(empty);
        }

        uint32_t openTasks = 0;
        const auto makeTaskCard = [weakThis = get_weak(), stableId, &metadata](
                                      const WorkspaceSidebarMetadata::Task& task) {
            Border card;
            card.Padding(Thickness{ 9, 8, 9, 8 });
            card.CornerRadius(winrt::Windows::UI::Xaml::CornerRadius{ 5.0 });
            card.BorderThickness(Thickness{ 1 });
            card.BorderBrush(_themeBrush(
                L"SystemControlForegroundBaseLowBrush",
                Color{ 0x24, 0xFF, 0xFF, 0xFF }));
            card.Background(_themeBrush(
                L"SystemControlHighlightListLowBrush",
                Color{ 0x0C, 0xFF, 0xFF, 0xFF }));

            StackPanel body;
            body.Spacing(4);
            TextBlock title;
            title.Text(task.title.empty() ? task.id : task.title);
            title.FontSize(11);
            title.FontWeight(FontWeights::SemiBold());
            title.TextWrapping(TextWrapping::WrapWholeWords);
            body.Children().Append(title);

            TextBlock detail;
            detail.Text(
                task.teamName +
                (task.owner.empty() ? L"" : L"  ·  " + task.owner) +
                winrt::to_hstring(fmt::format("  ·  {}/{}", task.attempts, task.maxAttempts)));
            detail.FontSize(9);
            detail.Opacity(0.55);
            detail.TextTrimming(TextTrimming::CharacterEllipsis);
            body.Children().Append(detail);

            const auto outcome = !task.error.empty() ? task.error : task.result;
            if (!outcome.empty())
            {
                TextBlock summary;
                summary.Text(outcome);
                summary.FontSize(9);
                summary.Opacity(0.68);
                summary.MaxLines(3);
                summary.TextWrapping(TextWrapping::WrapWholeWords);
                body.Children().Append(summary);
            }

            StackPanel actions;
            actions.Orientation(Orientation::Horizontal);
            actions.Spacing(4);
            const auto status = _lower(std::wstring{ task.status });
            if (!task.owner.empty())
            {
                Button focus;
                focus.Content(winrt::box_value(L"Focus"));
                focus.Padding(Thickness{ 7, 2, 7, 2 });
                focus.FontSize(9);
                focus.Click([
                    weakThis,
                    stableId,
                    team = task.teamName,
                    worker = task.owner
                ](auto&&, auto&&) {
                    if (const auto self = weakThis.get())
                    {
                        self->_RunWorkspaceTeamCommand(
                            stableId,
                            team,
                            winrt::hstring{
                                std::wstring{ L"focus --worker " } + _quoteArg(worker) },
                            L"Focused worker " + worker);
                        self->WorkspaceFleetOverlay().Visibility(Visibility::Collapsed);
                    }
                });
                actions.Children().Append(focus);
            }
            if (status == L"failed" && task.attempts < task.maxAttempts)
            {
                Button retry;
                retry.Content(winrt::box_value(L"Retry"));
                retry.Padding(Thickness{ 7, 2, 7, 2 });
                retry.FontSize(9);
                retry.Click([
                    weakThis,
                    stableId,
                    team = task.teamName,
                    taskId = task.id
                ](auto&&, auto&&) {
                    if (const auto self = weakThis.get())
                    {
                        self->_RunWorkspaceTeamCommand(
                            stableId,
                            team,
                            winrt::hstring{
                                std::wstring{ L"retry --task " } + _quoteArg(taskId) },
                            L"Task returned to the queue");
                    }
                });
                actions.Children().Append(retry);
            }
            if (status == L"pending" || status == L"assigned" || status == L"running")
            {
                Button cancel;
                cancel.Content(winrt::box_value(L"Cancel"));
                cancel.Padding(Thickness{ 7, 2, 7, 2 });
                cancel.FontSize(9);
                cancel.Click([
                    weakThis,
                    stableId,
                    team = task.teamName,
                    taskId = task.id
                ](auto&&, auto&&) {
                    if (const auto self = weakThis.get())
                    {
                        self->_RunWorkspaceTeamCommand(
                            stableId,
                            team,
                            winrt::hstring{
                                std::wstring{ L"cancel --task " } + _quoteArg(taskId) +
                                L" --reason " + _quoteArg(L"Cancelled from Agents & Tasks") },
                            L"Task cancelled");
                    }
                });
                actions.Children().Append(cancel);
            }
            if (actions.Children().Size() > 0)
            {
                body.Children().Append(actions);
            }

            card.Child(body);
            AutomationProperties::SetName(
                card,
                title.Text() + L", " + task.status +
                    (task.owner.empty() ? L"" : L", owner " + task.owner));
            return card;
        };

        for (const auto& task : metadata.tasks)
        {
            const auto status = _lower(std::wstring{ task.status });
            if (status == L"pending" || status == L"assigned")
            {
                ++openTasks;
                queuedPanel.Children().Append(makeTaskCard(task));
            }
            else if (status == L"running")
            {
                ++openTasks;
                runningPanel.Children().Append(makeTaskCard(task));
            }
            else
            {
                completedPanel.Children().Append(makeTaskCard(task));
            }
        }

        const auto addEmptyColumn = [](const StackPanel& panel, const winrt::hstring& text) {
            if (panel.Children().Size() == 0)
            {
                TextBlock empty;
                empty.Margin(Thickness{ 7, 14, 7, 0 });
                empty.Text(text);
                empty.FontSize(10);
                empty.Opacity(0.42);
                empty.TextWrapping(TextWrapping::Wrap);
                panel.Children().Append(empty);
            }
        };
        addEmptyColumn(queuedPanel, L"No queued tasks");
        addEmptyColumn(runningPanel, L"No active tasks");
        addEmptyColumn(completedPanel, L"No completed tasks");

        WorkspaceFleetAgentCount().Text(winrt::to_hstring(metadata.agents.size()));
        WorkspaceFleetRunningCount().Text(winrt::to_hstring(runningAgents));
        WorkspaceFleetAttentionCount().Text(winrt::to_hstring(attentionAgents));
        WorkspaceFleetTaskCount().Text(winrt::to_hstring(openTasks));
        WorkspaceFleetTeamSummary().Text(
            metadata.teams.empty()
                ? L"No native team"
                : winrt::to_hstring(fmt::format(
                      "{} team{} · {} task{}",
                      metadata.teams.size(),
                      metadata.teams.size() == 1 ? "" : "s",
                      metadata.tasks.size(),
                      metadata.tasks.size() == 1 ? "" : "s")));

        uint32_t healthyTargets = 0;
        for (const auto& target : metadata.computeTargets)
        {
            const auto health = _lower(std::wstring{ target.health });
            healthyTargets += health == L"healthy" && !target.disabled;

            Border card;
            card.Padding(Thickness{ 9, 7, 9, 7 });
            card.CornerRadius(winrt::Windows::UI::Xaml::CornerRadius{ 5.0 });
            card.Background(_themeBrush(
                L"SystemControlHighlightListLowBrush",
                Color{ 0x0C, 0xFF, 0xFF, 0xFF }));

            Grid row;
            row.ColumnDefinitions().Append(ColumnDefinition{});
            row.ColumnDefinitions().GetAt(0).Width(
                GridLengthHelper::FromValueAndType(1, GridUnitType::Star));
            row.ColumnDefinitions().Append(ColumnDefinition{});
            row.ColumnDefinitions().GetAt(1).Width(GridLengthHelper::Auto());

            StackPanel identity;
            TextBlock name;
            name.Text(target.name.empty() ? target.id : target.name);
            name.FontSize(11);
            name.FontWeight(FontWeights::SemiBold());
            TextBlock detail;
            detail.Text(
                target.provider + L"  ·  " + target.os + L"/" + target.arch +
                L"  ·  " + target.trust +
                winrt::to_hstring(fmt::format(
                    "  ·  {} agent / {} job slots",
                    target.agentSlots,
                    target.buildSlots)));
            detail.FontSize(9);
            detail.Opacity(0.58);
            detail.TextTrimming(TextTrimming::CharacterEllipsis);
            identity.Children().Append(name);
            identity.Children().Append(detail);

            StackPanel actions;
            actions.Orientation(Orientation::Horizontal);
            actions.Spacing(4);
            TextBlock status;
            status.Text(target.disabled ? L"disabled" : target.health);
            status.FontSize(9);
            status.Opacity(0.72);
            status.VerticalAlignment(VerticalAlignment::Center);
            actions.Children().Append(status);

            if (target.provider == L"ssh" || target.provider == L"azure")
            {
                Button connect;
                connect.Content(winrt::box_value(target.disabled ? L"Trust & connect" : L"Remote workspace"));
                connect.FontSize(9);
                connect.Padding(Thickness{ 6, 2, 6, 2 });
                connect.Click([
                    weakThis = get_weak(),
                    id = target.id
                ](auto&&, auto&&) {
                    if (const auto self = weakThis.get())
                    {
                        self->_OpenRemoteWorkspace(id);
                    }
                });
                actions.Children().Append(connect);

                Button files;
                files.Content(winrt::box_value(L"Files"));
                files.FontSize(9);
                files.Padding(Thickness{ 6, 2, 6, 2 });
                files.IsEnabled(!target.disabled && health == L"healthy");
                files.Click([
                    weakThis = get_weak(),
                    id = target.id
                ](auto&&, auto&&) {
                    if (const auto self = weakThis.get())
                    {
                        self->_ShowRemoteFileExplorer(id);
                    }
                });
                actions.Children().Append(files);
            }

            Button createAgent;
            createAgent.Content(winrt::box_value(L"New Codex"));
            createAgent.FontSize(9);
            createAgent.Padding(Thickness{ 6, 2, 6, 2 });
            createAgent.IsEnabled(!target.disabled && health == L"healthy");
            createAgent.Click([
                weakThis = get_weak(),
                stableId,
                id = target.id
            ](auto&&, auto&&) {
                if (const auto self = weakThis.get())
                {
                    if (const auto tab = self->_FindTabByStableId(stableId))
                    {
                        self->_OpenManagedAgentSurface(
                            tab->GetActivePane(),
                            id,
                            L"codex");
                    }
                }
            });
            actions.Children().Append(createAgent);

            Button probe;
            probe.Content(winrt::box_value(L"Probe"));
            probe.FontSize(9);
            probe.Padding(Thickness{ 6, 2, 6, 2 });
            probe.Click([
                weakThis = get_weak(),
                stableId,
                id = target.id
            ](auto&&, auto&&) {
                if (const auto self = weakThis.get())
                {
                    self->_RunWorkspaceComputeCommand(
                        stableId,
                        winrt::hstring{ std::wstring{ L"target probe " } + _quoteArg(id) },
                        winrt::hstring{ std::wstring{ L"Probing " } + id.c_str() + L"…" },
                        L"Target probe completed");
                }
            });
            actions.Children().Append(probe);

            Button toggle;
            toggle.Content(winrt::box_value(target.disabled ? L"Enable" : L"Disable"));
            toggle.FontSize(9);
            toggle.Padding(Thickness{ 6, 2, 6, 2 });
            toggle.Click([
                weakThis = get_weak(),
                stableId,
                id = target.id,
                enable = target.disabled
            ](auto&&, auto&&) {
                if (const auto self = weakThis.get())
                {
                    self->_RunWorkspaceComputeCommand(
                        stableId,
                        winrt::hstring{
                            std::wstring{ enable ? L"target enable " : L"target disable " } + _quoteArg(id)
                        },
                        winrt::hstring{
                            std::wstring{ enable ? L"Enabling " : L"Disabling " } + id.c_str() + L"…"
                        },
                        enable ? L"Target enabled" : L"Target disabled");
                }
            });
            actions.Children().Append(toggle);
            Grid::SetColumn(actions, 1);
            row.Children().Append(identity);
            row.Children().Append(actions);
            card.Child(row);
            AutomationProperties::SetName(
                card,
                name.Text() + L", " + status.Text() + L", " + target.provider);
            targetPanel.Children().Append(card);
        }
        if (metadata.computeTargets.empty())
        {
            TextBlock empty;
            empty.Margin(Thickness{ 8, 12, 8, 0 });
            empty.Text(metadata.computeError.empty()
                           ? L"No compute targets. Run target discovery from Add."
                           : L"Compute store unavailable: " + metadata.computeError);
            empty.FontSize(10);
            empty.Opacity(0.58);
            empty.TextWrapping(TextWrapping::Wrap);
            targetPanel.Children().Append(empty);
        }
        WorkspaceFleetTargetSummary().Text(winrt::to_hstring(fmt::format(
            "{} healthy · {} total",
            healthyTargets,
            metadata.computeTargets.size())));

        uint32_t activeRemoteRuntimes = 0;
        for (const auto& environment : metadata.environments)
        {
            const auto connection = std::find_if(
                metadata.connections.begin(),
                metadata.connections.end(),
                [&](const auto& candidate) {
                    return candidate.environmentId == environment.id;
                });
            const auto connectionState =
                connection == metadata.connections.end() ? winrt::hstring{ L"disconnected" } :
                                                           connection->state;

            Border card;
            card.Padding(Thickness{ 9, 7, 9, 7 });
            card.CornerRadius(winrt::Windows::UI::Xaml::CornerRadius{ 5.0 });
            card.Background(_themeBrush(
                L"SystemControlHighlightListLowBrush",
                Color{ 0x0C, 0xFF, 0xFF, 0xFF }));
            StackPanel body;
            TextBlock title;
            title.Text(L"Environment  ·  " + environment.targetId);
            title.FontSize(11);
            title.FontWeight(FontWeights::SemiBold());
            TextBlock detail;
            detail.Text(
                connectionState + L"  ·  " + environment.runtimeVersion +
                winrt::to_hstring(fmt::format(
                    "  ·  protocol {}  ·  {}",
                    environment.protocolVersion,
                    winrt::to_string(environment.launchMethod))));
            detail.FontSize(9);
            detail.Opacity(0.58);
            detail.TextTrimming(TextTrimming::CharacterEllipsis);
            body.Children().Append(title);
            body.Children().Append(detail);
            if (connection != metadata.connections.end() && !connection->lastError.empty())
            {
                TextBlock error;
                error.Text(connection->lastError);
                error.FontSize(9);
                error.Foreground(_brush(Color{ 0xFF, 0xE7, 0x48, 0x56 }));
                error.TextWrapping(TextWrapping::Wrap);
                error.MaxLines(2);
                body.Children().Append(error);
            }
            card.Child(body);
            AutomationProperties::SetName(
                card,
                title.Text() + L", " + connectionState + L", " + environment.state);
            jobPanel.Children().Append(card);
        }
        bool hasRemoteAgentRuntimes = false;
        for (const auto& binding : metadata.surfaceBindings)
        {
            if (binding.kind != L"managed_agent" || binding.remoteSessionId.empty())
            {
                continue;
            }
            hasRemoteAgentRuntimes = true;

            const auto state = _lower(std::wstring{ binding.state });
            activeRemoteRuntimes +=
                state != L"closed" && state != L"failed" && state != L"stopped";

            Border card;
            card.Padding(Thickness{ 9, 7, 9, 7 });
            card.CornerRadius(winrt::Windows::UI::Xaml::CornerRadius{ 5.0 });
            card.Background(_themeBrush(
                L"SystemControlHighlightListLowBrush",
                Color{ 0x0C, 0xFF, 0xFF, 0xFF }));

            Grid row;
            row.ColumnDefinitions().Append(ColumnDefinition{});
            row.ColumnDefinitions().GetAt(0).Width(
                GridLengthHelper::FromValueAndType(1, GridUnitType::Star));
            row.ColumnDefinitions().Append(ColumnDefinition{});
            row.ColumnDefinitions().GetAt(1).Width(GridLengthHelper::Auto());

            StackPanel identity;
            TextBlock title;
            title.Text(
                (binding.agentId.empty() ? L"Managed agent" : binding.agentId) +
                L"  ·  " + binding.homeTargetId);
            title.FontSize(11);
            title.FontWeight(FontWeights::SemiBold());
            TextBlock detail;
            detail.Text(
                binding.state + L"  ·  session " + binding.remoteSessionId +
                (binding.adapterKind.empty() ? L"" : L"  ·  " + binding.adapterKind));
            detail.FontSize(9);
            detail.Opacity(0.58);
            detail.TextTrimming(TextTrimming::CharacterEllipsis);
            identity.Children().Append(title);
            identity.Children().Append(detail);

            StackPanel actions;
            actions.Orientation(Orientation::Horizontal);
            actions.Spacing(4);

            Button metrics;
            metrics.Content(winrt::box_value(L"Metrics"));
            metrics.FontSize(9);
            metrics.Padding(Thickness{ 6, 2, 6, 2 });
            metrics.IsEnabled(!binding.homeTargetId.empty());
            metrics.Click([
                weakThis = get_weak(),
                target = binding.homeTargetId,
                session = binding.remoteSessionId
            ](auto&&, auto&&) {
                if (const auto self = weakThis.get())
                {
                    self->_ShowRemoteRuntimeMetrics(target, session);
                }
            });
            actions.Children().Append(metrics);

            if (!binding.surfaceId.empty())
            {
                Button focus;
                focus.Content(winrt::box_value(L"Focus"));
                focus.FontSize(9);
                focus.Padding(Thickness{ 6, 2, 6, 2 });
                focus.Click([
                    weakThis = get_weak(),
                    stableId,
                    surface = binding.surfaceId
                ](auto&&, auto&&) {
                    if (const auto self = weakThis.get())
                    {
                        self->_FocusWorkspaceAgent(stableId, surface);
                        self->WorkspaceFleetOverlay().Visibility(Visibility::Collapsed);
                    }
                });
                actions.Children().Append(focus);
            }

            Grid::SetColumn(actions, 1);
            row.Children().Append(identity);
            row.Children().Append(actions);
            card.Child(row);
            AutomationProperties::SetName(
                card,
                title.Text() + L", " + binding.state + L", remote session " +
                    binding.remoteSessionId);
            jobPanel.Children().Append(card);
        }

        for (const auto& remote : metadata.remoteWorkspaces)
        {
            const auto state = _lower(std::wstring{ remote.state });
            activeRemoteRuntimes += state != L"closed" && state != L"failed";
            Border card;
            card.Padding(Thickness{ 9, 7, 9, 7 });
            card.CornerRadius(winrt::Windows::UI::Xaml::CornerRadius{ 5.0 });
            card.Background(_themeBrush(
                L"SystemControlHighlightListLowBrush",
                Color{ 0x0C, 0xFF, 0xFF, 0xFF }));
            StackPanel body;
            TextBlock title;
            title.Text(L"Remote workspace  ·  " + remote.targetId);
            title.FontSize(11);
            title.FontWeight(FontWeights::SemiBold());
            TextBlock detail;
            detail.Text(
                remote.state +
                winrt::to_hstring(fmt::format(
                    "  ·  reconnect attempt {}",
                    remote.reconnectAttempt)));
            detail.FontSize(9);
            detail.Opacity(0.58);
            body.Children().Append(title);
            body.Children().Append(detail);
            if (!remote.lastError.empty())
            {
                TextBlock error;
                error.Text(remote.lastError);
                error.FontSize(9);
                error.Foreground(_brush(Color{ 0xFF, 0xE7, 0x48, 0x56 }));
                error.TextWrapping(TextWrapping::Wrap);
                error.MaxLines(2);
                body.Children().Append(error);
            }
            card.Child(body);
            jobPanel.Children().Append(card);
        }

        for (const auto& browser : metadata.browsers)
        {
            const auto state = _lower(std::wstring{ browser.state });
            activeRemoteRuntimes += state != L"closed" && state != L"failed";
            Border card;
            card.Padding(Thickness{ 9, 7, 9, 7 });
            card.CornerRadius(winrt::Windows::UI::Xaml::CornerRadius{ 5.0 });
            card.Background(_themeBrush(
                L"SystemControlHighlightListLowBrush",
                Color{ 0x0C, 0xFF, 0xFF, 0xFF }));
            Grid row;
            row.ColumnDefinitions().Append(ColumnDefinition{});
            row.ColumnDefinitions().GetAt(0).Width(
                GridLengthHelper::FromValueAndType(1, GridUnitType::Star));
            row.ColumnDefinitions().Append(ColumnDefinition{});
            row.ColumnDefinitions().GetAt(1).Width(GridLengthHelper::Auto());
            StackPanel body;
            TextBlock title;
            title.Text(L"Browser  ·  " + browser.targetId);
            title.FontSize(11);
            title.FontWeight(FontWeights::SemiBold());
            TextBlock detail;
            detail.Text(browser.state + L"  ·  " + browser.url);
            detail.FontSize(9);
            detail.Opacity(0.58);
            detail.TextTrimming(TextTrimming::CharacterEllipsis);
            body.Children().Append(title);
            body.Children().Append(detail);
            if (!browser.lastError.empty())
            {
                TextBlock error;
                error.Text(browser.lastError);
                error.FontSize(9);
                error.Foreground(_brush(Color{ 0xFF, 0xE7, 0x48, 0x56 }));
                error.TextWrapping(TextWrapping::Wrap);
                error.MaxLines(2);
                body.Children().Append(error);
            }
            row.Children().Append(body);
            if (!browser.surfaceId.empty())
            {
                Button focus;
                focus.Content(winrt::box_value(L"Focus"));
                focus.FontSize(9);
                focus.Padding(Thickness{ 6, 2, 6, 2 });
                focus.Click([
                    weakThis = get_weak(),
                    stableId,
                    surface = browser.surfaceId
                ](auto&&, auto&&) {
                    if (const auto self = weakThis.get())
                    {
                        self->_FocusWorkspaceAgent(stableId, surface);
                    }
                });
                Grid::SetColumn(focus, 1);
                row.Children().Append(focus);
            }
            card.Child(row);
            jobPanel.Children().Append(card);
        }

        uint32_t activeJobs = 0;
        for (const auto& job : metadata.computeJobs)
        {
            const auto state = _lower(std::wstring{ job.state });
            const bool active = state == L"queued" || state == L"staging" ||
                                state == L"running" || state == L"cancelling";
            activeJobs += active;

            Border card;
            card.Padding(Thickness{ 9, 7, 9, 7 });
            card.CornerRadius(winrt::Windows::UI::Xaml::CornerRadius{ 5.0 });
            card.Background(_themeBrush(
                L"SystemControlHighlightListLowBrush",
                Color{ 0x0C, 0xFF, 0xFF, 0xFF }));
            Grid row;
            row.ColumnDefinitions().Append(ColumnDefinition{});
            row.ColumnDefinitions().GetAt(0).Width(
                GridLengthHelper::FromValueAndType(1, GridUnitType::Star));
            row.ColumnDefinitions().Append(ColumnDefinition{});
            row.ColumnDefinitions().GetAt(1).Width(GridLengthHelper::Auto());
            StackPanel identity;
            TextBlock title;
            title.Text(job.workload.empty() ? job.id : job.workload + L"  ·  " + job.id);
            title.FontSize(11);
            title.FontWeight(FontWeights::SemiBold());
            TextBlock detail;
            detail.Text(
                job.targetId +
                (job.snapshotId.empty() ? L"" : L"  ·  " + job.snapshotId) +
                winrt::to_hstring(fmt::format("  ·  attempt {}", job.attempt)));
            detail.FontSize(9);
            detail.Opacity(0.58);
            detail.TextTrimming(TextTrimming::CharacterEllipsis);
            identity.Children().Append(title);
            identity.Children().Append(detail);
            TextBlock status;
            status.Text(job.state);
            status.FontSize(9);
            status.Opacity(0.72);
            status.VerticalAlignment(VerticalAlignment::Center);
            Grid::SetColumn(status, 1);
            row.Children().Append(identity);
            row.Children().Append(status);
            card.Child(row);
            AutomationProperties::SetName(card, title.Text() + L", " + status.Text());
            jobPanel.Children().Append(card);
        }
        uint32_t activeTransfers = 0;
        for (const auto& transfer : metadata.fileTransfers)
        {
            const auto state = _lower(std::wstring{ transfer.state });
            const bool active = state == L"preparing" ||
                                state == L"uploading" ||
                                state == L"verifying" ||
                                state == L"cancelling";
            activeTransfers += active;

            Border card;
            card.Padding(Thickness{ 9, 7, 9, 7 });
            card.CornerRadius(winrt::Windows::UI::Xaml::CornerRadius{ 5.0 });
            card.Background(_themeBrush(
                L"SystemControlHighlightListLowBrush",
                Color{ 0x0C, 0xFF, 0xFF, 0xFF }));

            Grid row;
            row.ColumnDefinitions().Append(ColumnDefinition{});
            row.ColumnDefinitions().GetAt(0).Width(
                GridLengthHelper::FromValueAndType(1, GridUnitType::Star));
            row.ColumnDefinitions().Append(ColumnDefinition{});
            row.ColumnDefinitions().GetAt(1).Width(GridLengthHelper::Auto());

            StackPanel identity;
            TextBlock title;
            title.Text(L"Transfer  ·  " +
                       (transfer.displayName.empty() ? transfer.id : transfer.displayName));
            title.FontSize(11);
            title.FontWeight(FontWeights::SemiBold());

            const auto percent =
                transfer.sizeBytes == 0
                    ? 0.0
                    : (100.0 * static_cast<double>(transfer.bytesTransferred) /
                       static_cast<double>(transfer.sizeBytes));
            TextBlock detail;
            detail.Text(
                transfer.targetId +
                winrt::to_hstring(fmt::format(
                    "  ·  {:.0f}%  ·  {:.1f}/{:.1f} MiB",
                    std::min(100.0, percent),
                    static_cast<double>(transfer.bytesTransferred) / (1024.0 * 1024.0),
                    static_cast<double>(transfer.sizeBytes) / (1024.0 * 1024.0))));
            detail.FontSize(9);
            detail.Opacity(0.58);
            detail.TextTrimming(TextTrimming::CharacterEllipsis);
            identity.Children().Append(title);
            identity.Children().Append(detail);

            if (!transfer.error.empty())
            {
                TextBlock error;
                error.Text(transfer.error);
                error.FontSize(9);
                error.Foreground(_brush(Color{ 0xFF, 0xE7, 0x48, 0x56 }));
                error.MaxLines(2);
                error.TextWrapping(TextWrapping::WrapWholeWords);
                identity.Children().Append(error);
            }

            StackPanel actions;
            actions.Orientation(Orientation::Horizontal);
            actions.Spacing(4);
            TextBlock status;
            status.Text(transfer.state);
            status.FontSize(9);
            status.Opacity(0.72);
            status.VerticalAlignment(VerticalAlignment::Center);
            actions.Children().Append(status);

            if (active && state != L"cancelling")
            {
                Button cancel;
                cancel.Content(winrt::box_value(L"Cancel"));
                cancel.FontSize(9);
                cancel.Padding(Thickness{ 6, 2, 6, 2 });
                cancel.Click([
                    weakThis = get_weak(),
                    stableId,
                    id = transfer.id
                ](auto&&, auto&&) {
                    if (const auto self = weakThis.get())
                    {
                        self->_RunWorkspaceComputeCommand(
                            stableId,
                            winrt::hstring{
                                std::wstring{ L"transfer cancel " } + _quoteArg(id)
                            },
                            L"Cancelling transfer…",
                            L"Transfer cancellation requested");
                    }
                });
                actions.Children().Append(cancel);
            }
            else if (state == L"failed" || state == L"cancelled")
            {
                Button retry;
                retry.Content(winrt::box_value(L"Retry"));
                retry.FontSize(9);
                retry.Padding(Thickness{ 6, 2, 6, 2 });
                retry.Click([
                    weakThis = get_weak(),
                    stableId,
                    id = transfer.id
                ](auto&&, auto&&) {
                    if (const auto self = weakThis.get())
                    {
                        self->_RunWorkspaceComputeCommand(
                            stableId,
                            winrt::hstring{
                                std::wstring{ L"transfer retry " } + _quoteArg(id)
                            },
                            L"Retrying verified transfer…",
                            L"Transfer retry completed");
                    }
                });
                actions.Children().Append(retry);
            }

            Grid::SetColumn(actions, 1);
            row.Children().Append(identity);
            row.Children().Append(actions);
            card.Child(row);
            AutomationProperties::SetName(card, title.Text() + L", " + transfer.state);
            jobPanel.Children().Append(card);
        }
        if (metadata.computeJobs.empty() &&
            metadata.fileTransfers.empty() &&
            metadata.remoteWorkspaces.empty() &&
            metadata.browsers.empty() &&
            metadata.environments.empty() &&
            !hasRemoteAgentRuntimes)
        {
            TextBlock empty;
            empty.Margin(Thickness{ 8, 12, 8, 0 });
            empty.Text(
                L"No routed jobs or verified transfers. "
                L"PTY commands remain local unless explicitly submitted.");
            empty.FontSize(10);
            empty.Opacity(0.58);
            empty.TextWrapping(TextWrapping::Wrap);
            jobPanel.Children().Append(empty);
        }
        WorkspaceFleetJobSummary().Text(winrt::to_hstring(fmt::format(
            "{} jobs · {} transfers · {} remote",
            activeJobs,
            activeTransfers,
            activeRemoteRuntimes)));
    }

    safe_void_coroutine TerminalPage::_ShowRemoteFileExplorer(winrt::hstring targetId)
    try
    {
        auto strong = get_strong();
        if (targetId.empty())
        {
            co_return;
        }
        const auto focused = _GetFocusedTabImpl();
        if (!focused)
        {
            co_return;
        }
        _remoteFileTargetId = targetId;
        _remoteFileWorkspaceId = focused->StableId();
        _remoteFileRoot = {};
        _remoteFileRootLabel = {};
        _remoteFilePath = {};
        _remoteFileSelectedPath = {};
        _remoteFileSelectedName = {};
        _remoteFileSelectedDirectory = false;
        _remoteFileRootWritable = false;
        _remoteFileRootDeletable = false;
        WorkspaceRemoteFilesDialog().Title(winrt::box_value(
            winrt::hstring{ std::wstring{ L"Remote files · " } + targetId.c_str() }));
        WorkspaceRemoteFilePath().Text({});
        WorkspaceRemoteFilePreview().Text({});
        WorkspaceRemoteFileName().Text({});
        WorkspaceRemoteFileMutationConsent().IsChecked(false);
        WorkspaceRemoteFileRootSetup().Visibility(Visibility::Collapsed);
        WorkspaceRemoteFileRootPath().Text({});
        WorkspaceRemoteFileRootLabel().Text(L"Project");
        WorkspaceRemoteFileRootSource().SelectedIndex(0);
        WorkspaceRemoteFileRootWriteConsent().IsChecked(false);
        WorkspaceRemoteFileRootDeleteConsent().IsChecked(false);
        WorkspaceRemoteFileWideScopeConsent().IsChecked(false);
        WorkspaceRemoteFileItems().Children().Clear();
        WorkspaceRemoteFileStatus().Text(L"Connecting to the verified remote node…");
        _RefreshRemoteFileExplorer();
        if (const auto presenter = _dialogPresenter.get())
        {
            co_await presenter.ShowDialog(WorkspaceRemoteFilesDialog());
        }
        ++_remoteFileRefreshGeneration;
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_RefreshRemoteFileExplorer()
    try
    {
        auto strong = get_strong();
        const auto generation = ++_remoteFileRefreshGeneration;
        const auto target = _remoteFileTargetId;
        const auto workspace = _remoteFileWorkspaceId;
        auto root = _remoteFileRoot;
        auto rootLabel = _remoteFileRootLabel;
        auto rootWritable = _remoteFileRootWritable;
        auto rootDeletable = _remoteFileRootDeletable;
        const auto path = _remoteFilePath;
        if (target.empty() || workspace.empty())
        {
            co_return;
        }
        WorkspaceRemoteFileStatus().Text(L"Loading remote files…");
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        co_await winrt::resume_background();

        if (root.empty())
        {
            const auto rootsOutput =
                ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                    wtaPath,
                    L"compute file roots --target " + _quoteArg(target) +
                        L" --workspace " + _quoteArg(workspace),
                    12'000);
            Json::Value roots;
            Json::CharReaderBuilder reader;
            std::string errors;
            std::istringstream rootsInput{ rootsOutput };
            if (!rootsOutput.empty() &&
                Json::parseFromStream(reader, rootsInput, &roots, &errors) &&
                roots["roots"].isArray() &&
                !roots["roots"].empty())
            {
                root = _jsonString(roots["roots"][0], "id");
                rootLabel = _jsonString(roots["roots"][0], "label");
                rootWritable = roots["roots"][0].get("writable", false).asBool();
                rootDeletable = roots["roots"][0].get("deletable", false).asBool();
            }
        }

        Json::Value listing;
        if (!root.empty())
        {
            const auto listOutput =
                ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                    wtaPath,
                    L"compute file list --target " + _quoteArg(target) +
                        L" --workspace " + _quoteArg(workspace) +
                        L" --root " + _quoteArg(root) +
                        L" --path " + _quoteArg(path) +
                        L" --limit 500",
                    20'000);
            Json::CharReaderBuilder reader;
            std::string errors;
            std::istringstream input{ listOutput };
            if (!listOutput.empty())
            {
                Json::parseFromStream(reader, input, &listing, &errors);
            }
        }

        co_await wil::resume_foreground(dispatcher);
        if (generation != _remoteFileRefreshGeneration ||
            target != _remoteFileTargetId)
        {
            co_return;
        }
        _remoteFileRoot = root;
        _remoteFileRootLabel = rootLabel;
        _remoteFileRootWritable = rootWritable;
        _remoteFileRootDeletable = rootDeletable;
        WorkspaceRemoteFileItems().Children().Clear();
        WorkspaceRemoteFilePath().Text(
            root.empty() ?
                winrt::hstring{} :
                rootLabel + (path.empty() ? L"" : L" / " + path));
        WorkspaceRemoteFileRootSetup().Visibility(
            root.empty() ? Visibility::Visible : Visibility::Collapsed);
        WorkspaceRemoteFileMutationConsent().IsEnabled(rootWritable || rootDeletable);
        WorkspaceRemoteFileNewFolderButton().IsEnabled(rootWritable);
        WorkspaceRemoteFileRenameButton().IsEnabled(rootWritable);
        WorkspaceRemoteFileDeleteButton().IsEnabled(rootDeletable);
        WorkspaceRemoteFileDownloadButton().IsEnabled(!root.empty());
        if (root.empty() || !listing.isObject() || !listing["entries"].isArray())
        {
            WorkspaceRemoteFileStatus().Text(
                L"No policy-scoped root is available for this workspace. "
                L"Authorize an explicit project/worktree root below; HOME is never exposed automatically.");
            co_return;
        }

        const auto& entries = listing["entries"];
        WorkspaceRemoteFileStatus().Text(winrt::to_hstring(fmt::format(
            "{} item{}{}",
            entries.size(),
            entries.size() == 1 ? "" : "s",
            listing.get("has_more", false).asBool() ? " · more available" : "")));
        for (const auto& entry : entries)
        {
            const auto name = _jsonString(entry, "name");
            const auto relative = _jsonString(entry, "path");
            const auto kind = _jsonString(entry, "kind");
            const auto isDirectory = kind == L"directory";

            Button row;
            row.HorizontalContentAlignment(HorizontalAlignment::Stretch);
            row.Padding(Thickness{ 8, 5, 8, 5 });
            row.Background(Brush{ nullptr });
            row.BorderBrush(Brush{ nullptr });
            Windows::Foundation::Collections::ValueSet tag;
            tag.Insert(L"path", winrt::box_value(relative));
            tag.Insert(L"name", winrt::box_value(name));
            tag.Insert(L"kind", winrt::box_value(kind));
            row.Tag(tag);
            row.Click({ this, &TerminalPage::_WorkspaceRemoteFileEntryClicked });

            Grid content;
            content.ColumnDefinitions().Append(ColumnDefinition{});
            content.ColumnDefinitions().GetAt(0).Width(GridLengthHelper::Auto());
            content.ColumnDefinitions().Append(ColumnDefinition{});
            content.ColumnDefinitions().GetAt(1).Width(
                GridLengthHelper::FromValueAndType(1, GridUnitType::Star));
            content.ColumnDefinitions().Append(ColumnDefinition{});
            content.ColumnDefinitions().GetAt(2).Width(GridLengthHelper::Auto());
            FontIcon icon;
            icon.Glyph(isDirectory ? L"\xE8B7" : L"\xE8A5");
            icon.FontFamily(
                winrt::Windows::UI::Xaml::Media::FontFamily{
                    L"Segoe Fluent Icons, Segoe MDL2 Assets" });
            icon.FontSize(13);
            icon.Margin(Thickness{ 0, 0, 8, 0 });
            TextBlock label;
            label.Text(name);
            label.FontSize(11);
            label.TextTrimming(TextTrimming::CharacterEllipsis);
            Grid::SetColumn(label, 1);
            TextBlock detail;
            detail.Text(
                isDirectory ? L"folder" :
                              winrt::to_hstring(fmt::format(
                                  "{:.1f} KiB",
                                  entry.get("size", Json::UInt64{ 0 }).asDouble() / 1024.0)));
            detail.FontSize(9);
            detail.Opacity(0.52);
            Grid::SetColumn(detail, 2);
            content.Children().Append(icon);
            content.Children().Append(label);
            content.Children().Append(detail);
            row.Content(content);
            AutomationProperties::SetName(
                row,
                name + (isDirectory ? L", folder" : L", file"));
            WorkspaceRemoteFileItems().Children().Append(row);
        }
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_OpenRemoteFilePreview(
        winrt::hstring relativePath,
        winrt::hstring displayName)
    try
    {
        auto strong = get_strong();
        const auto generation = _remoteFileRefreshGeneration;
        const auto target = _remoteFileTargetId;
        const auto workspace = _remoteFileWorkspaceId;
        const auto root = _remoteFileRoot;
        WorkspaceRemoteFilePreview().Text(L"Loading " + displayName + L"…");
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        co_await winrt::resume_background();
        const auto output =
            ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                wtaPath,
                L"compute file read --target " + _quoteArg(target) +
                    L" --workspace " + _quoteArg(workspace) +
                    L" --root " + _quoteArg(root) +
                    L" --path " + _quoteArg(relativePath),
                20'000);
        Json::Value preview;
        Json::CharReaderBuilder reader;
        std::string errors;
        std::istringstream input{ output };
        if (!output.empty())
        {
            Json::parseFromStream(reader, input, &preview, &errors);
        }
        co_await wil::resume_foreground(dispatcher);
        if (generation != _remoteFileRefreshGeneration ||
            relativePath != _remoteFileSelectedPath)
        {
            co_return;
        }
        if (preview.isObject() && preview["text"].isString())
        {
            auto text = winrt::to_hstring(preview["text"].asString());
            if (preview.get("truncated", false).asBool())
            {
                std::wstring expanded{ text };
                expanded += L"\r\n\r\n[Preview truncated]";
                text = winrt::hstring{ expanded };
            }
            WorkspaceRemoteFilePreview().Text(text);
        }
        else
        {
            WorkspaceRemoteFilePreview().Text(
                L"Preview unavailable. The file may be binary, too large, missing, or outside the scoped root.");
        }
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_RunRemoteFileMutation(
        winrt::hstring operation,
        winrt::hstring sourcePath,
        winrt::hstring destinationPath)
    try
    {
        auto strong = get_strong();
        const auto target = _remoteFileTargetId;
        const auto workspace = _remoteFileWorkspaceId;
        const auto root = _remoteFileRoot;
        if (target.empty() || workspace.empty() || root.empty())
        {
            co_return;
        }

        std::wstring arguments;
        DWORD timeout = 30'000;
        if (operation == L"download")
        {
            if (sourcePath.empty() || _remoteFileSelectedDirectory)
            {
                WorkspaceRemoteFileStatus().Text(L"Select a regular file to download.");
                co_return;
            }
            wil::unique_cotaskmem_string downloads;
            if (FAILED(SHGetKnownFolderPath(
                    FOLDERID_Downloads,
                    KF_FLAG_DEFAULT,
                    nullptr,
                    &downloads)) ||
                !downloads)
            {
                WorkspaceRemoteFileStatus().Text(L"The local Downloads folder is unavailable.");
                co_return;
            }
            const auto localName = winrt::to_hstring(fmt::format(
                "remote-{}-{}",
                _nowMs(),
                winrt::to_string(_remoteFileSelectedName)));
            const auto destination = std::filesystem::path{ downloads.get() } /
                                     std::wstring_view{ localName };
            arguments =
                L"compute file download --target " + _quoteArg(target) +
                L" --workspace " + _quoteArg(workspace) +
                L" --root " + _quoteArg(root) +
                L" --path " + _quoteArg(sourcePath) +
                L" --destination " + _quoteArg(winrt::hstring{ destination.wstring() });
            timeout = 10 * 60 * 1000;
        }
        else
        {
            if (!WorkspaceRemoteFileMutationConsent().IsChecked().Value())
            {
                WorkspaceRemoteFileStatus().Text(
                    L"Enable “Allow changes” before mutating the remote workspace.");
                co_return;
            }
            if (operation == L"mkdir")
            {
                arguments =
                    L"compute file mkdir --target " + _quoteArg(target) +
                    L" --workspace " + _quoteArg(workspace) +
                    L" --root " + _quoteArg(root) +
                    L" --path " + _quoteArg(destinationPath) +
                    L" --recursive --allow-destructive";
            }
            else if (operation == L"rename")
            {
                arguments =
                    L"compute file rename --target " + _quoteArg(target) +
                    L" --workspace " + _quoteArg(workspace) +
                    L" --root " + _quoteArg(root) +
                    L" --source " + _quoteArg(sourcePath) +
                    L" --destination " + _quoteArg(destinationPath) +
                    L" --allow-destructive";
            }
            else if (operation == L"remove")
            {
                arguments =
                    L"compute file remove --target " + _quoteArg(target) +
                    L" --workspace " + _quoteArg(workspace) +
                    L" --root " + _quoteArg(root) +
                    L" --path " + _quoteArg(sourcePath) +
                    (_remoteFileSelectedDirectory ? L" --recursive" : L"") +
                    L" --allow-destructive";
            }
            else
            {
                co_return;
            }
        }

        WorkspaceRemoteFileStatus().Text(
            operation == L"download" ? L"Downloading and verifying file…" :
                                       L"Applying scoped remote file change…");
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        co_await winrt::resume_background();
        const auto output =
            ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                wtaPath,
                arguments,
                timeout);
        co_await wil::resume_foreground(dispatcher);
        if (output.empty())
        {
            WorkspaceRemoteFileStatus().Text(
                L"The operation failed closed; no successful WTA result was returned.");
            co_return;
        }
        WorkspaceRemoteFileMutationConsent().IsChecked(false);
        WorkspaceRemoteFileStatus().Text(
            operation == L"download" ?
                L"Download completed and verified in the local Downloads folder." :
                L"Remote file change completed.");
        if (operation != L"download")
        {
            _remoteFileSelectedPath = {};
            _remoteFileSelectedName = {};
            WorkspaceRemoteFilePreview().Text({});
            _RefreshRemoteFileExplorer();
        }
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_AuthorizeRemoteFileRoot(winrt::hstring label,
                                                                winrt::hstring path,
                                                                winrt::hstring source,
                                                                bool writable,
                                                                bool deletable,
                                                                bool acknowledgeWideScope)
    try
    {
        auto strong = get_strong();
        const auto target = _remoteFileTargetId;
        const auto workspace = _remoteFileWorkspaceId;
        if (target.empty() || workspace.empty() || label.empty() || path.empty())
        {
            WorkspaceRemoteFileStatus().Text(
                L"Target, workspace, label and remote path are required.");
            co_return;
        }
        if (deletable && !writable)
        {
            WorkspaceRemoteFileStatus().Text(
                L"Delete capability requires create/rename capability.");
            co_return;
        }
        if ((source == L"explicit_home" || source == L"admin") && !acknowledgeWideScope)
        {
            WorkspaceRemoteFileStatus().Text(
                L"Broad HOME/admin access requires the explicit acknowledgement.");
            co_return;
        }

        std::wstring arguments =
            L"compute file authorize --target " + _quoteArg(target) +
            L" --workspace " + _quoteArg(workspace) +
            L" --label " + _quoteArg(label) +
            L" --path " + _quoteArg(path) +
            L" --source " + _quoteArg(source);
        if (writable)
        {
            arguments += L" --writable";
        }
        if (deletable)
        {
            arguments += L" --deletable";
        }
        if (acknowledgeWideScope)
        {
            arguments += L" --acknowledge-wide-scope";
        }

        WorkspaceRemoteFileStatus().Text(L"Authorizing the explicit root policy…");
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        co_await winrt::resume_background();
        const auto output = ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
            wtaPath,
            arguments,
            20'000);
        co_await wil::resume_foreground(dispatcher);
        if (output.empty())
        {
            WorkspaceRemoteFileStatus().Text(
                L"Root authorization failed closed. Check target health, capabilities and the remote path.");
            co_return;
        }

        _remoteFileRoot = {};
        _remoteFileRootLabel = {};
        _remoteFilePath = {};
        _remoteFileRootWritable = false;
        _remoteFileRootDeletable = false;
        WorkspaceRemoteFileRootWriteConsent().IsChecked(false);
        WorkspaceRemoteFileRootDeleteConsent().IsChecked(false);
        WorkspaceRemoteFileWideScopeConsent().IsChecked(false);
        _RefreshRemoteFileExplorer();
    }
    CATCH_LOG();

    void TerminalPage::_WorkspaceRemoteFilesRefreshClicked(const IInspectable&, const RoutedEventArgs&)
    {
        _RefreshRemoteFileExplorer();
    }

    void TerminalPage::_WorkspaceRemoteFilesUpClicked(const IInspectable&, const RoutedEventArgs&)
    {
        _remoteFilePath = _remoteParent(_remoteFilePath);
        _remoteFileSelectedPath = {};
        _remoteFileSelectedName = {};
        WorkspaceRemoteFilePreview().Text({});
        _RefreshRemoteFileExplorer();
    }

    void TerminalPage::_WorkspaceRemoteFileEntryClicked(const IInspectable& sender, const RoutedEventArgs&)
    {
        const auto row = sender.try_as<Button>();
        const auto tag = row ? row.Tag().try_as<Windows::Foundation::Collections::ValueSet>() : nullptr;
        if (!tag)
        {
            return;
        }
        const auto lookup = [&](const wchar_t* key) {
            return winrt::unbox_value_or<winrt::hstring>(tag.TryLookup(key), {});
        };
        _remoteFileSelectedPath = lookup(L"path");
        _remoteFileSelectedName = lookup(L"name");
        _remoteFileSelectedDirectory = lookup(L"kind") == L"directory";
        WorkspaceRemoteFileName().Text(_remoteFileSelectedName);
        if (_remoteFileSelectedDirectory)
        {
            _remoteFilePath = _remoteFileSelectedPath;
            _remoteFileSelectedPath = {};
            _remoteFileSelectedName = {};
            WorkspaceRemoteFilePreview().Text({});
            _RefreshRemoteFileExplorer();
        }
        else
        {
            _OpenRemoteFilePreview(_remoteFileSelectedPath, _remoteFileSelectedName);
        }
    }

    void TerminalPage::_WorkspaceRemoteFileNewFolderClicked(const IInspectable&, const RoutedEventArgs&)
    {
        const auto name = WorkspaceRemoteFileName().Text();
        if (name.empty() || name == L"." || name == L".." ||
            std::wstring_view{ name }.find_first_of(L"/\\") != std::wstring_view::npos)
        {
            WorkspaceRemoteFileStatus().Text(L"Enter one valid folder name without path separators.");
            return;
        }
        _RunRemoteFileMutation(L"mkdir", {}, _remoteJoin(_remoteFilePath, name));
    }

    void TerminalPage::_WorkspaceRemoteFileRenameClicked(const IInspectable&, const RoutedEventArgs&)
    {
        const auto name = WorkspaceRemoteFileName().Text();
        if (_remoteFileSelectedPath.empty() ||
            name.empty() || name == L"." || name == L".." ||
            std::wstring_view{ name }.find_first_of(L"/\\") != std::wstring_view::npos)
        {
            WorkspaceRemoteFileStatus().Text(L"Select an entry and enter one valid new name.");
            return;
        }
        _RunRemoteFileMutation(
            L"rename",
            _remoteFileSelectedPath,
            _remoteJoin(_remoteParent(_remoteFileSelectedPath), name));
    }

    void TerminalPage::_WorkspaceRemoteFileDeleteClicked(const IInspectable&, const RoutedEventArgs&)
    {
        if (_remoteFileSelectedPath.empty())
        {
            WorkspaceRemoteFileStatus().Text(L"Select a file or folder to delete.");
            return;
        }
        _RunRemoteFileMutation(L"remove", _remoteFileSelectedPath);
    }

    void TerminalPage::_WorkspaceRemoteFileDownloadClicked(const IInspectable&, const RoutedEventArgs&)
    {
        _RunRemoteFileMutation(L"download", _remoteFileSelectedPath);
    }

    void TerminalPage::_WorkspaceRemoteFileAuthorizeRootClicked(const IInspectable&,
                                                                const RoutedEventArgs&)
    {
        const auto label = WorkspaceRemoteFileRootLabel().Text();
        const auto path = WorkspaceRemoteFileRootPath().Text();
        const auto selected = WorkspaceRemoteFileRootSource().SelectedItem().try_as<ComboBoxItem>();
        const auto source = selected ?
                                winrt::unbox_value_or<winrt::hstring>(selected.Tag(), L"project") :
                                winrt::hstring{ L"project" };
        const auto writable = WorkspaceRemoteFileRootWriteConsent().IsChecked().Value();
        const auto deletable = WorkspaceRemoteFileRootDeleteConsent().IsChecked().Value();
        const auto broad = WorkspaceRemoteFileWideScopeConsent().IsChecked().Value();
        _AuthorizeRemoteFileRoot(label, path, source, writable, deletable, broad);
    }

    safe_void_coroutine TerminalPage::_ShowWorkspaceTeamComposer()
    try
    {
        auto strong = get_strong();
        const auto focused = _GetFocusedTabImpl();
        if (!focused)
        {
            co_return;
        }
        const auto& metadata = _WorkspaceSidebarMetadataFor(focused);
        const auto defaultTeam =
            metadata.teams.empty() ? winrt::hstring{} : metadata.teams.front().name;
        WorkspaceTeamWorkerTeam().Text(defaultTeam);
        WorkspaceTeamTaskTeam().Text(defaultTeam);
        WorkspaceTeamCreateName().Text({});
        WorkspaceTeamWorkerId().Text({});
        WorkspaceTeamWorkerRole().Text({});
        WorkspaceTeamWorkerModel().Text({});
        WorkspaceTeamTaskId().Text({});
        WorkspaceTeamTaskTitle().Text({});
        WorkspaceTeamTaskPrompt().Text({});
        WorkspaceTeamComposerOperation().SelectedIndex(metadata.teams.empty() ? 0 : 1);
        if (const auto presenter = _dialogPresenter.get())
        {
            const auto result = co_await presenter.ShowDialog(WorkspaceTeamComposerDialog());
            if (result == ContentDialogResult::Primary)
            {
                _CreateWorkspaceTeamEntity();
            }
        }
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_CreateWorkspaceTeamEntity()
    try
    {
        auto strong = get_strong();
        const auto focused = _GetFocusedTabImpl();
        if (!focused)
        {
            co_return;
        }
        const auto metadata = _WorkspaceSidebarMetadataFor(focused);
        if (metadata.cwd.empty())
        {
            _ShowControlNoticeDialog(
                L"Agents & Tasks",
                L"The focused workspace does not expose a working directory.");
            co_return;
        }

        const auto selected =
            WorkspaceTeamComposerOperation().SelectedItem().try_as<ComboBoxItem>();
        const auto operation =
            selected ? winrt::unbox_value_or<winrt::hstring>(selected.Tag(), L"team") : L"team";
        std::wstring arguments;
        if (operation == L"team")
        {
            const auto name = WorkspaceTeamCreateName().Text();
            const auto leader = WorkspaceTeamCreateLeader().Text();
            if (name.empty() || leader.empty())
            {
                _ShowControlNoticeDialog(
                    L"Create native team",
                    L"Team name and leader identity are required.");
                co_return;
            }
            arguments = L"team create --root " + _quoteArg(metadata.cwd) +
                        L" --name " + _quoteArg(name) +
                        L" --leader " + _quoteArg(leader) +
                        L" --workspace-id " + _quoteArg(focused->StableId());
        }
        else if (operation == L"worker")
        {
            const auto team = WorkspaceTeamWorkerTeam().Text();
            const auto worker = WorkspaceTeamWorkerId().Text();
            const auto role = WorkspaceTeamWorkerRole().Text();
            const auto agent =
                WorkspaceTeamWorkerAgent().Text().empty() ? L"codex" : WorkspaceTeamWorkerAgent().Text();
            const auto model = WorkspaceTeamWorkerModel().Text();
            if (team.empty() || worker.empty() || role.empty())
            {
                _ShowControlNoticeDialog(
                    L"Add agent worker",
                    L"Team name, worker ID and role are required.");
                co_return;
            }
            arguments = L"team add-worker --root " + _quoteArg(metadata.cwd) +
                        L" --name " + _quoteArg(team) +
                        L" --worker " + _quoteArg(worker) +
                        L" --role " + _quoteArg(role) +
                        L" --agent " + _quoteArg(agent) +
                        L" --cwd " + _quoteArg(metadata.cwd);
            if (!model.empty())
            {
                arguments += L" --model " + _quoteArg(model);
            }
        }
        else
        {
            const auto team = WorkspaceTeamTaskTeam().Text();
            const auto taskId = WorkspaceTeamTaskId().Text();
            const auto title = WorkspaceTeamTaskTitle().Text();
            const auto prompt = WorkspaceTeamTaskPrompt().Text();
            if (team.empty() || taskId.empty() || title.empty() || prompt.empty())
            {
                _ShowControlNoticeDialog(
                    L"Add team task",
                    L"Team name, task ID, title and instructions are required.");
                co_return;
            }
            arguments = L"team add-task --root " + _quoteArg(metadata.cwd) +
                        L" --name " + _quoteArg(team) +
                        L" --id " + _quoteArg(taskId) +
                        L" --title " + _quoteArg(title) +
                        L" --prompt " + _quoteArg(prompt);
        }

        WorkspaceFleetScopeText().Text(L"Creating workspace-scoped entity…");
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        co_await winrt::resume_background();
        const auto output =
            ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, arguments, 30'000);
        co_await wil::resume_foreground(dispatcher);
        if (output.empty())
        {
            _ShowControlNoticeDialog(
                L"Agents & Tasks",
                L"WTA rejected the operation or did not return a result. No local state was edited by the UI.");
            co_return;
        }
        WorkspaceFleetScopeText().Text(
            operation == L"team" ? L"Native team created"
            : operation == L"worker" ? L"Agent worker launched in a terminal pane"
                                      : L"Durable task added");
        _RequestWorkspaceSidebarMetadata(focused, true);
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_RunWorkspaceTeamCommand(winrt::hstring stableId,
                                                               winrt::hstring teamName,
                                                               winrt::hstring arguments,
                                                               winrt::hstring successMessage)
    try
    {
        auto strong = get_strong();
        const auto tab = _FindTabByStableId(stableId);
        if (!tab)
        {
            co_return;
        }
        const auto metadata = _WorkspaceSidebarMetadataFor(tab);
        if (metadata.cwd.empty() || teamName.empty())
        {
            _ShowControlNoticeDialog(
                L"Agents & Tasks",
                L"This action requires a workspace-scoped native team.");
            co_return;
        }

        WorkspaceFleetScopeText().Text(L"Applying team action…");
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const std::wstring command =
            std::wstring{ L"team " } + std::wstring{ arguments } +
            L" --root " + _quoteArg(metadata.cwd) +
            L" --name " + _quoteArg(teamName);
        co_await winrt::resume_background();
        const auto output =
            ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, command, 12'000);
        co_await wil::resume_foreground(dispatcher);
        if (output.empty())
        {
            WorkspaceFleetScopeText().Text(L"Team action failed; no response from WTA");
            co_return;
        }
        WorkspaceFleetScopeText().Text(successMessage);
        _RequestWorkspaceSidebarMetadata(tab, true);
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_RunWorkspaceComputeCommand(winrt::hstring stableId,
                                                                  winrt::hstring arguments,
                                                                  winrt::hstring progressMessage,
                                                                  winrt::hstring successMessage)
    try
    {
        auto strong = get_strong();
        const auto tab = _FindTabByStableId(stableId);
        if (!tab)
        {
            co_return;
        }

        WorkspaceFleetScopeText().Text(progressMessage);
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const std::wstring command = std::wstring{ L"compute " } + std::wstring{ arguments };
        co_await winrt::resume_background();
        const auto output =
            ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, command, 45'000);
        co_await wil::resume_foreground(dispatcher);
        if (output.empty())
        {
            WorkspaceFleetScopeText().Text(L"Compute action failed; WTA returned no result");
            co_return;
        }
        WorkspaceFleetScopeText().Text(successMessage);
        _RequestWorkspaceSidebarMetadata(tab, true);
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_ShowRemoteRuntimeMetrics(winrt::hstring targetId,
                                                                winrt::hstring remoteSessionId)
    try
    {
        auto strong = get_strong();
        if (targetId.empty() || remoteSessionId.empty())
        {
            _ShowControlNoticeDialog(
                L"Remote runtime",
                L"This managed surface does not expose a target and persistent session ID.");
            co_return;
        }

        WorkspaceFleetScopeText().Text(L"Reading live remote process metrics…");
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const std::wstring command =
            L"compute node pty-status " + _quoteArg(targetId) +
            L" --session " + _quoteArg(remoteSessionId);
        co_await winrt::resume_background();
        const auto output =
            ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, command, 20'000);

        Json::Value status;
        bool parsed = false;
        if (!output.empty())
        {
            Json::CharReaderBuilder reader;
            std::string errors;
            std::istringstream input{ output };
            parsed = Json::parseFromStream(reader, input, &status, &errors) && status.isObject();
        }

        winrt::hstring message;
        if (parsed)
        {
            const auto metrics = status["metrics"];
            const auto state = _jsonString(status, "state");
            const auto pid = status.get("pid", Json::UInt{ 0 }).asUInt();
            const auto attachments = status.get("attachments", Json::UInt{ 0 }).asUInt();
            const auto cols = status.get("effective_cols", Json::UInt{ 0 }).asUInt();
            const auto rows = status.get("effective_rows", Json::UInt{ 0 }).asUInt();
            const auto rssBytes =
                metrics.isObject() ? metrics.get("rss_bytes", Json::UInt64{ 0 }).asUInt64() : 0;
            const auto userCpuMs =
                metrics.isObject() ? metrics.get("user_cpu_ms", Json::UInt64{ 0 }).asUInt64() : 0;
            const auto systemCpuMs =
                metrics.isObject() ? metrics.get("system_cpu_ms", Json::UInt64{ 0 }).asUInt64() : 0;

            message = winrt::to_hstring(fmt::format(
                "Target: {}\nSession: {}\nState: {}\nPID: {}\nAttachments: {}\n"
                "Effective terminal: {} × {}\nMemory: {:.1f} MiB\n"
                "CPU time: {:.2f}s user + {:.2f}s system",
                winrt::to_string(targetId),
                winrt::to_string(remoteSessionId),
                winrt::to_string(state),
                pid,
                attachments,
                cols,
                rows,
                static_cast<double>(rssBytes) / (1024.0 * 1024.0),
                static_cast<double>(userCpuMs) / 1000.0,
                static_cast<double>(systemCpuMs) / 1000.0));
        }

        co_await wil::resume_foreground(dispatcher);
        if (!parsed)
        {
            WorkspaceFleetScopeText().Text(L"Remote metrics unavailable");
            _ShowControlNoticeDialog(
                L"Remote runtime",
                L"The verified node did not return a valid PTY status. The session may be offline or reconnecting.");
            co_return;
        }

        WorkspaceFleetScopeText().Text(L"Remote metrics refreshed");
        _ShowControlNoticeDialog(L"Remote runtime metrics", message);
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_FocusWorkspaceAgent(winrt::hstring stableId,
                                                           winrt::hstring target)
    try
    {
        auto strong = get_strong();
        const auto tab = _FindTabByStableId(stableId);
        if (!tab)
        {
            co_return;
        }
        uint32_t index = 0;
        if (_tabs.IndexOf(*tab, index))
        {
            _SelectTab(index);
        }
        const auto metadata = _WorkspaceSidebarMetadataFor(tab);
        if (target.empty() || target == L"*")
        {
            co_return;
        }
        const auto targetView = std::wstring_view{ target };
        if (targetView.starts_with(L"session:"))
        {
            const auto sessionId = winrt::hstring{ targetView.substr(8) };
            if (sessionId.empty())
            {
                co_return;
            }
            const auto args = L"focus --target " + _quoteArg(sessionId);
            const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
            co_await winrt::resume_background();
            ::Microsoft::Terminal::WtaProcess::RunWtaAndWait(wtaPath, args, 8'000);
            co_return;
        }
        if (!metadata.persisted)
        {
            co_return;
        }
        const auto args = L"agent-workspace focus --root " + _quoteArg(metadata.cwd) +
                          L" --name " + _quoteArg(metadata.workspaceName) +
                          L" --target " + _quoteArg(target);
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        co_await winrt::resume_background();
        ::Microsoft::Terminal::WtaProcess::RunWtaAndWait(wtaPath, args, 8'000);
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_ShowWorkspaceGit()
    try
    {
        auto strong = get_strong();
        const auto focused = _GetFocusedTabImpl();
        if (!focused)
        {
            co_return;
        }
        const auto metadata = _WorkspaceSidebarMetadataFor(focused);
        if (metadata.cwd.empty())
        {
            _ShowControlNoticeDialog(L"Git workspace", L"The active tab does not expose a working directory.");
            co_return;
        }
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const auto args = L"agent-workspace inspect-git --root " + _quoteArg(metadata.cwd);
        co_await winrt::resume_background();
        const auto output = ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, args, 10'000);
        Json::Value result;
        if (!output.empty())
        {
            Json::CharReaderBuilder reader;
            std::string errors;
            std::istringstream input{ output };
            Json::parseFromStream(reader, input, &result, &errors);
        }
        co_await wil::resume_foreground(dispatcher);
        if (!result.isObject())
        {
            _ShowControlNoticeDialog(L"Git workspace", L"Git inspection failed or this directory is not a repository.");
            co_return;
        }
        const auto& summary = result["summary"];
        WorkspaceGitSummary().Text(
            summary.isObject()
                ? winrt::to_hstring(fmt::format(
                      "{} · {} changed · ↑{} ↓{}{}",
                      summary.get("branch", "").asString(),
                      summary.get("changed_files", 0).asUInt(),
                      summary.get("ahead", 0).asUInt(),
                      summary.get("behind", 0).asUInt(),
                      result.get("truncated", false).asBool() ? " · output truncated" : ""))
                : L"Not a Git repository");
        auto text = result.get("status", "").asString();
        const auto diff = result.get("diff", "").asString();
        if (!diff.empty())
        {
            text += "\n\n--- WORKTREE DIFF ---\n";
            text += diff;
        }
        WorkspaceGitOutput().Text(winrt::to_hstring(text));
        if (const auto presenter = _dialogPresenter.get())
        {
            co_await presenter.ShowDialog(WorkspaceGitDialog());
        }
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_ShowWorkspaceDoctor()
    try
    {
        auto strong = get_strong();
        const auto focused = _GetFocusedTabImpl();
        const auto root = focused ? _WorkspaceSidebarMetadataFor(focused).cwd : winrt::hstring{ L"." };
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const auto args = L"agent-workspace doctor --root " + _quoteArg(root.empty() ? L"." : root);
        co_await winrt::resume_background();
        const auto output = ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, args, 12'000);
        co_await wil::resume_foreground(dispatcher);
        WorkspaceDoctorOutput().Text(
            output.empty()
                ? L"WTA diagnostics could not run. Verify that wta.exe is packaged beside the terminal."
                : winrt::to_hstring(output));
        if (const auto presenter = _dialogPresenter.get())
        {
            co_await presenter.ShowDialog(WorkspaceDoctorDialog());
        }
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_SnapshotDeclarativeWorkspace(winrt::hstring root,
                                                                    winrt::hstring name)
    try
    {
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const auto args = L"agent-workspace snapshot --root " + _quoteArg(root) +
                          L" --name " + _quoteArg(name);
        co_await winrt::resume_background();
        ::Microsoft::Terminal::WtaProcess::RunWtaAndWait(wtaPath, args, 8'000);
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_VerifyWorkspace(winrt::hstring stableId)
    try
    {
        auto strong = get_strong();
        const auto tab = _FindTabByStableId(stableId);
        if (!tab)
        {
            co_return;
        }
        const auto metadata = _WorkspaceSidebarMetadataFor(tab);
        if (!metadata.persisted || metadata.manifestPath.empty())
        {
            co_return;
        }
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const auto args = L"agent-workspace verify " + _quoteArg(metadata.manifestPath);
        co_await winrt::resume_background();
        const auto output = ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(wtaPath, args, 70'000);
        co_await wil::resume_foreground(dispatcher);
        _ShowControlNoticeDialog(
            output.empty() ? L"Workspace verification failed" : L"Workspace verification passed",
            output.empty()
                ? L"The manifest's bounded verifier returned a failure or timed out. No approval or merge action was performed."
                : winrt::to_hstring(output));
    }
    CATCH_LOG();

    safe_void_coroutine TerminalPage::_RestoreDeclarativeWorkspace(RecentlyClosedWorkspace recent)
    try
    {
        auto strong = get_strong();
        const auto dispatcher = Dispatcher();
        const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        const auto args = L"agent-workspace restore --root " + _quoteArg(recent.cwd) +
                          L" --name " + _quoteArg(recent.workspaceName);
        co_await winrt::resume_background();
        const auto restored = ::Microsoft::Terminal::WtaProcess::RunWtaAndWait(wtaPath, args, 45'000);
        co_await wil::resume_foreground(dispatcher);
        if (!restored)
        {
            _ShowControlNoticeDialog(
                L"Workspace restore failed",
                L"The persisted manifest or Terminal control plane was unavailable. The original worktrees were left untouched.");
            co_return;
        }
        _RefreshWorkspaceSidebar(true);
    }
    CATCH_LOG();

    void TerminalPage::_WorkspaceSidebarNewClicked(const IInspectable&,
                                                    const winrt::Microsoft::UI::Xaml::Controls::SplitButtonClickEventArgs&)
    {
        // Execute the same path as the native split button, including default
        // profile resolution, elevation policy and telemetry.
        _OpenNewTerminalViaDropdown(NewTerminalArgs{});
    }

    void TerminalPage::_WorkspaceSidebarComposerClicked(const IInspectable&,
                                                         const RoutedEventArgs&)
    {
        _ShowWorkspaceComposer();
    }

    void TerminalPage::_WorkspaceSidebarAttentionClicked(const IInspectable&,
                                                          const RoutedEventArgs&)
    {
        _ShowWorkspaceAttentionCenter();
    }

    void TerminalPage::_WorkspaceSidebarFleetClicked(const IInspectable&,
                                                      const RoutedEventArgs&)
    {
        if (WorkspaceFleetOverlay().Visibility() == Visibility::Visible)
        {
            WorkspaceFleetOverlay().Visibility(Visibility::Collapsed);
        }
        else
        {
            _ShowWorkspaceFleet();
        }
    }

    void TerminalPage::_WorkspaceFleetCloseClicked(const IInspectable&,
                                                    const RoutedEventArgs&)
    {
        WorkspaceFleetOverlay().Visibility(Visibility::Collapsed);
        if (const auto control = _GetActiveControl())
        {
            control.Focus(FocusState::Programmatic);
        }
    }

    void TerminalPage::_WorkspaceFleetRefreshClicked(const IInspectable&,
                                                      const RoutedEventArgs&)
    {
        if (const auto focused = _GetFocusedTabImpl())
        {
            WorkspaceFleetScopeText().Text(L"Refreshing workspace control plane…");
            _RequestWorkspaceSidebarMetadata(focused, true);
        }
    }

    void TerminalPage::_WorkspaceFleetDiscoverTargetsClicked(const IInspectable&,
                                                              const RoutedEventArgs&)
    {
        if (const auto focused = _GetFocusedTabImpl())
        {
            _RunWorkspaceComputeCommand(
                focused->StableId(),
                L"target discover --save",
                L"Discovering local, WSL, and concrete SSH targets…",
                L"Target discovery completed");
        }
    }

    void TerminalPage::_WorkspaceFleetAddClicked(const IInspectable&,
                                                  const RoutedEventArgs&)
    {
        _ShowWorkspaceTeamComposer();
    }

    void TerminalPage::_WorkspaceTeamComposerOperationChanged(
        const IInspectable&,
        const SelectionChangedEventArgs&)
    {
        if (!WorkspaceTeamComposerOperation() ||
            !WorkspaceTeamCreateFields() ||
            !WorkspaceTeamWorkerFields() ||
            !WorkspaceTeamTaskFields())
        {
            return;
        }
        const auto selected =
            WorkspaceTeamComposerOperation().SelectedItem().try_as<ComboBoxItem>();
        const auto operation =
            selected ? winrt::unbox_value_or<winrt::hstring>(selected.Tag(), L"team") : L"team";
        WorkspaceTeamCreateFields().Visibility(
            operation == L"team" ? Visibility::Visible : Visibility::Collapsed);
        WorkspaceTeamWorkerFields().Visibility(
            operation == L"worker" ? Visibility::Visible : Visibility::Collapsed);
        WorkspaceTeamTaskFields().Visibility(
            operation == L"task" ? Visibility::Visible : Visibility::Collapsed);
    }

    void TerminalPage::_WorkspaceSidebarGitClicked(const IInspectable&,
                                                    const RoutedEventArgs&)
    {
        _ShowWorkspaceGit();
    }

    void TerminalPage::_WorkspaceSidebarDoctorClicked(const IInspectable&,
                                                       const RoutedEventArgs&)
    {
        _ShowWorkspaceDoctor();
    }

    void TerminalPage::_WorkspaceComposerPreviewClicked(const IInspectable&,
                                                         const RoutedEventArgs&)
    {
        _PreviewWorkspaceComposer();
    }

    void TerminalPage::_WorkspaceSidebarToggleClicked(const IInspectable&,
                                                       const RoutedEventArgs&)
    {
        _workspaceSidebarVisible = !_workspaceSidebarVisible;
        _ApplyWorkspaceSidebarVisibility();
        _SaveWorkspaceSidebarState();
        if (_workspaceSidebarVisible)
        {
            _RefreshWorkspaceSidebar(true);
        }
    }

    void TerminalPage::_WorkspaceSidebarRecentClicked(const IInspectable&,
                                                       const RoutedEventArgs&)
    {
        _workspaceSidebarShowRecent = !_workspaceSidebarShowRecent;
        WorkspaceSidebarRecentLabel().Text(_workspaceSidebarShowRecent ? L"Hide recently closed" : L"Recently closed");
        _RefreshWorkspaceSidebar(false);
    }

    void TerminalPage::_WorkspaceSidebarRefreshClicked(const IInspectable&,
                                                        const RoutedEventArgs&)
    {
        for (auto& [_, metadata] : _workspaceSidebarMetadata)
        {
            metadata.refreshedAtMs = 0;
        }
        _RefreshWorkspaceSidebar(true);
    }

    void TerminalPage::_WorkspaceSidebarSearchChanged(const IInspectable&,
                                                       const TextChangedEventArgs&)
    {
        _RefreshWorkspaceSidebar(false);
    }

    void TerminalPage::_WorkspaceSidebarSearchClicked(const IInspectable&,
                                                       const RoutedEventArgs&)
    {
        _workspaceSidebarSearchVisible = !_workspaceSidebarSearchVisible;
        if (_workspaceSidebarSearchVisible)
        {
            WorkspaceSidebarSearch().Visibility(Visibility::Visible);
            WorkspaceSidebarSearch().Focus(FocusState::Programmatic);
            AutomationProperties::SetName(WorkspaceSidebarSearchButton(), L"Hide workspace search");
        }
        else
        {
            WorkspaceSidebarSearch().Text({});
            WorkspaceSidebarSearch().Visibility(Visibility::Collapsed);
            AutomationProperties::SetName(WorkspaceSidebarSearchButton(), L"Find a workspace");
        }
    }

    void TerminalPage::_WorkspaceSidebarAcceleratorInvoked(
        const winrt::Windows::UI::Xaml::Input::KeyboardAccelerator&,
        const winrt::Windows::UI::Xaml::Input::KeyboardAcceleratorInvokedEventArgs& args)
    {
        _workspaceSidebarVisible = !_workspaceSidebarVisible;
        _ApplyWorkspaceSidebarVisibility();
        _SaveWorkspaceSidebarState();
        if (_workspaceSidebarVisible)
        {
            _RefreshWorkspaceSidebar(true);
        }
        args.Handled(true);
    }

    void TerminalPage::_WorkspaceSidebarResizeDelta(const IInspectable&,
                                                     const DragDeltaEventArgs& args)
    {
        const auto wasCompact = _workspaceSidebarWidth < 252.0;
        _workspaceSidebarWidth = std::round(std::clamp(_workspaceSidebarWidth + args.HorizontalChange(),
                                                       SidebarMinWidth,
                                                       SidebarMaxWidth));
        WorkspaceSidebarColumn().Width(GridLengthHelper::FromValueAndType(
            _workspaceSidebarWidth,
            GridUnitType::Pixel));
        if (wasCompact != (_workspaceSidebarWidth < 252.0))
        {
            _RefreshWorkspaceSidebar(false);
        }
    }

    void TerminalPage::_WorkspaceSidebarResizeCompleted(const IInspectable&,
                                                         const DragCompletedEventArgs&)
    {
        _SaveWorkspaceSidebarState();
    }
}
