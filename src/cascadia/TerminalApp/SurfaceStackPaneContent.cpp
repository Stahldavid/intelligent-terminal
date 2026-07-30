// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#include "pch.h"
#include "SurfaceStackPaneContent.h"
#include "BrowserPaneContent.h"
#include "TerminalPaneContent.h"
#include "../inc/AgentRegistry.h"
#include "../inc/WtaProcess.h"
#include <json/json.h>
#include <sstream>

#include "SurfaceStackPaneContent.g.cpp"

using namespace winrt;
using namespace winrt::Windows::Foundation;
using namespace winrt::Windows::UI;
using namespace winrt::Windows::UI::Text;
using namespace winrt::Windows::System;
using namespace winrt::Windows::UI::Xaml;
using namespace winrt::Windows::UI::Xaml::Automation;
using namespace winrt::Windows::UI::Xaml::Controls;
using namespace winrt::Windows::UI::Xaml::Input;
using namespace winrt::Windows::UI::Xaml::Media;
using namespace winrt::Microsoft::Terminal::Settings::Model;
using namespace winrt::Microsoft::Terminal;

namespace winrt::TerminalApp::implementation
{
    template<typename TCallback>
    static void _dispatchSurfaceUi(
        const Windows::UI::Core::CoreDispatcher& dispatcher,
        TCallback&& callback)
    {
        if (dispatcher.HasThreadAccess())
        {
            callback();
            return;
        }

        // ConPTY output, connection-state and notification events may arrive
        // on worker threads. SurfaceStack owns XAML chrome, so every forwarded
        // event must cross the same UI dispatcher before touching its model or
        // raising an event consumed by Pane/Tab UI. Ignoring the returned
        // action is intentional: the weak target makes teardown safe and the
        // producer must never block the terminal output thread.
        (void)dispatcher.RunAsync(
            Windows::UI::Core::CoreDispatcherPriority::Normal,
            std::forward<TCallback>(callback));
    }

    SurfaceStackPaneContent::SurfaceStackPaneContent(const TerminalApp::IPaneContent& initialSurface)
    {
        _initializeVisualTree();
        if (initialSurface)
        {
            AddSurface(initialSurface);
        }
    }

    void SurfaceStackPaneContent::_initializeVisualTree()
    {
        _root.RowDefinitions().Append(RowDefinition{});
        _root.RowDefinitions().GetAt(0).Height(GridLengthHelper::Auto());
        _root.RowDefinitions().Append(RowDefinition{});
        _root.RowDefinitions().GetAt(1).Height(GridLengthHelper::FromValueAndType(1, GridUnitType::Star));

        _chrome.ColumnDefinitions().Append(ColumnDefinition{});
        _chrome.ColumnDefinitions().GetAt(0).Width(GridLengthHelper::FromValueAndType(1, GridUnitType::Star));
        _chrome.ColumnDefinitions().Append(ColumnDefinition{});
        _chrome.ColumnDefinitions().GetAt(1).Width(GridLengthHelper::Auto());
        _chrome.MinHeight(28);
        _chrome.Background(SolidColorBrush{ Color{ 0xFF, 0x19, 0x19, 0x19 } });
        _chromeFrame.BorderThickness(Thickness{ 0, 0, 0, 1 });
        _chromeFrame.BorderBrush(SolidColorBrush{ Color{ 0x28, 0xFF, 0xFF, 0xFF } });
        _chromeFrame.Child(_chrome);

        _tabStrip.Orientation(Orientation::Horizontal);
        _tabStrip.Spacing(2);
        _tabStrip.Margin(Thickness{ 3, 2, 3, 2 });

        ScrollViewer scroller;
        scroller.HorizontalScrollMode(ScrollMode::Enabled);
        scroller.HorizontalScrollBarVisibility(ScrollBarVisibility::Hidden);
        scroller.VerticalScrollMode(ScrollMode::Disabled);
        scroller.Content(_tabStrip);
        Grid::SetColumn(scroller, 0);

        // Do not squeeze the secondary hit target out of the SplitButton.
        // A 48px fixed width was smaller than the control template's primary
        // + secondary columns at common DPI/text scales, leaving only the `+`
        // visibly/clickably exposed. Keep two deterministic mouse targets:
        // 36px duplicate-current primary and 28px profile-picker chevron.
        _newSurfaceButton.MinWidth(64);
        _newSurfaceButton.MaxWidth(64);
        _newSurfaceButton.Width(64);
        _newSurfaceButton.Height(24);
        _newSurfaceButton.Padding(Thickness{ 0 });
        _newSurfaceButton.Margin(Thickness{ 0, 2, 3, 2 });
        _newSurfaceButton.Background(SolidColorBrush{ Colors::Transparent() });
        _newSurfaceButton.BorderBrush(SolidColorBrush{ Colors::Transparent() });
        _newSurfaceButton.Resources().Insert(
            box_value(L"SplitButtonPrimaryButtonSize"),
            box_value(36.0));
        _newSurfaceButton.Resources().Insert(
            box_value(L"SplitButtonSecondaryButtonSize"),
            box_value(28.0));
        _newSurfaceButton.Resources().Insert(
            box_value(L"SplitButtonForeground"),
            SolidColorBrush{ Colors::White() });
        _newSurfaceButton.Resources().Insert(
            box_value(L"SplitButtonForegroundSecondary"),
            SolidColorBrush{ Colors::White() });
        const auto newSurfaceLabel = RS_(L"SurfaceNewTabToolTip");
        ToolTipService::SetToolTip(_newSurfaceButton, box_value(newSurfaceLabel));
        AutomationProperties::SetName(_newSurfaceButton, newSurfaceLabel);
        AutomationProperties::SetHelpText(_newSurfaceButton, RS_(L"SurfaceNewTabWithProfileHelpText"));
        FontIcon plus;
        plus.FontFamily(FontFamily{ L"Segoe Fluent Icons" });
        plus.FontSize(11);
        plus.Glyph(L"\xE710");
        _newSurfaceButton.Content(plus);
        _newSurfaceButton.Click([weakThis = get_weak()](auto&&, auto&&) {
            if (const auto self = weakThis.get())
            {
                // Primary click intentionally preserves the fast path: clone
                // the active surface, including its profile and cwd.
                self->NewSurfaceRequested.raise(*self, nullptr);
            }
        });
        Grid::SetColumn(_newSurfaceButton, 1);

        KeyboardAccelerator newSurface;
        newSurface.Key(VirtualKey::T);
        newSurface.Modifiers(VirtualKeyModifiers::Control | VirtualKeyModifiers::Menu);
        newSurface.Invoked([weakThis = get_weak()](auto&&, const KeyboardAcceleratorInvokedEventArgs& args) {
            if (const auto self = weakThis.get())
            {
                self->NewSurfaceRequested.raise(*self, nullptr);
                args.Handled(true);
            }
        });
        _root.KeyboardAccelerators().Append(newSurface);

        KeyboardAccelerator newSurfaceWithProfile;
        newSurfaceWithProfile.Key(VirtualKey::T);
        newSurfaceWithProfile.Modifiers(VirtualKeyModifiers::Control | VirtualKeyModifiers::Menu | VirtualKeyModifiers::Shift);
        newSurfaceWithProfile.Invoked([weakThis = get_weak()](auto&&, const KeyboardAcceleratorInvokedEventArgs& args) {
            if (const auto self = weakThis.get())
            {
                if (const auto flyout = self->_newSurfaceButton.Flyout())
                {
                    flyout.ShowAt(self->_newSurfaceButton);
                    args.Handled(true);
                }
            }
        });
        _root.KeyboardAccelerators().Append(newSurfaceWithProfile);

        KeyboardAccelerator previousSurface;
        previousSurface.Key(VirtualKey::Left);
        previousSurface.Modifiers(VirtualKeyModifiers::Control | VirtualKeyModifiers::Menu);
        previousSurface.Invoked([weakThis = get_weak()](auto&&, const KeyboardAcceleratorInvokedEventArgs& args) {
            if (const auto self = weakThis.get())
            {
                args.Handled(self->_activateRelative(-1));
            }
        });
        _root.KeyboardAccelerators().Append(previousSurface);

        KeyboardAccelerator nextSurface;
        nextSurface.Key(VirtualKey::Right);
        nextSurface.Modifiers(VirtualKeyModifiers::Control | VirtualKeyModifiers::Menu);
        nextSurface.Invoked([weakThis = get_weak()](auto&&, const KeyboardAcceleratorInvokedEventArgs& args) {
            if (const auto self = weakThis.get())
            {
                args.Handled(self->_activateRelative(1));
            }
        });
        _root.KeyboardAccelerators().Append(nextSurface);

        _chrome.Children().Append(scroller);
        _chrome.Children().Append(_newSurfaceButton);
        Grid::SetRow(_chromeFrame, 0);
        Grid::SetRow(_contentHost, 1);
        _root.Children().Append(_chromeFrame);
        _root.Children().Append(_contentHost);
    }

    SurfaceStackPaneContent::Surface* SurfaceStackPaneContent::_find(const uint32_t surfaceId) noexcept
    {
        const auto it = std::find_if(_surfaces.begin(), _surfaces.end(), [surfaceId](const auto& surface) {
            return surface.id == surfaceId;
        });
        return it == _surfaces.end() ? nullptr : &*it;
    }

    const SurfaceStackPaneContent::Surface* SurfaceStackPaneContent::_find(const uint32_t surfaceId) const noexcept
    {
        const auto it = std::find_if(_surfaces.cbegin(), _surfaces.cend(), [surfaceId](const auto& surface) {
            return surface.id == surfaceId;
        });
        return it == _surfaces.cend() ? nullptr : &*it;
    }

    SurfaceStackPaneContent::Surface* SurfaceStackPaneContent::_active() noexcept
    {
        return _find(_activeSurfaceId);
    }

    const SurfaceStackPaneContent::Surface* SurfaceStackPaneContent::_active() const noexcept
    {
        return _find(_activeSurfaceId);
    }

    uint32_t SurfaceStackPaneContent::AddSurface(const TerminalApp::IPaneContent& content)
    {
        if (!content || _closed)
        {
            return 0;
        }

        Surface surface;
        surface.id = _nextSurfaceId++;
        surface.content = content;
        _surfaces.emplace_back(std::move(surface));
        _wireSurface(_surfaces.back());
        _activeSurfaceId = _surfaces.back().id;
        _rebuildTabStrip();
        _showActiveSurface(true);
        _raiseSurfaceChanged(L"created", _activeSurfaceId, gsl::narrow_cast<uint32_t>(_surfaces.size() - 1), content);
        return _activeSurfaceId;
    }

    void SurfaceStackPaneContent::_wireSurface(Surface& surface)
    {
        const auto id = surface.id;
        const auto content = surface.content;
        auto weakThis = get_weak();
        const auto dispatcher = _root.Dispatcher();

        surface.events.CloseRequested = content.CloseRequested(auto_revoke, [weakThis, dispatcher, id](auto&&, auto&&) {
            _dispatchSurfaceUi(dispatcher, [weakThis, id] {
                if (const auto self = weakThis.get())
                {
                    self->CloseSurface(id);
                }
            });
        });
        surface.events.TitleChanged = content.TitleChanged(auto_revoke, [weakThis, dispatcher, id](auto&&, auto&&) {
            _dispatchSurfaceUi(dispatcher, [weakThis, id] {
                if (const auto self = weakThis.get())
                {
                    self->_rebuildTabStrip();
                    if (self->_activeSurfaceId == id)
                    {
                        self->TitleChanged.raise(*self, nullptr);
                    }
                }
            });
        });
        surface.events.TabColorChanged = content.TabColorChanged(auto_revoke, [weakThis, dispatcher, id](auto&&, auto&&) {
            _dispatchSurfaceUi(dispatcher, [weakThis, id] {
                if (const auto self = weakThis.get())
                {
                    self->_rebuildTabStrip();
                    if (self->_activeSurfaceId == id)
                    {
                        self->TabColorChanged.raise(*self, nullptr);
                    }
                }
            });
        });
        surface.events.TaskbarProgressChanged = content.TaskbarProgressChanged(auto_revoke, [weakThis, dispatcher](auto&&, auto&&) {
            _dispatchSurfaceUi(dispatcher, [weakThis] {
                if (const auto self = weakThis.get())
                {
                    self->TaskbarProgressChanged.raise(*self, nullptr);
                }
            });
        });
        surface.events.ReadOnlyChanged = content.ReadOnlyChanged(auto_revoke, [weakThis, dispatcher, id](auto&&, auto&&) {
            _dispatchSurfaceUi(dispatcher, [weakThis, id] {
                if (const auto self = weakThis.get(); self && self->_activeSurfaceId == id)
                {
                    self->ReadOnlyChanged.raise(*self, nullptr);
                }
            });
        });
        surface.events.ConnectionStateChanged = content.ConnectionStateChanged(auto_revoke, [weakThis, dispatcher, id](auto&&, auto&&) {
            _dispatchSurfaceUi(dispatcher, [weakThis, id] {
                if (const auto self = weakThis.get(); self && self->_activeSurfaceId == id)
                {
                    self->ConnectionStateChanged.raise(nullptr, nullptr);
                }
            });
        });
        surface.events.FocusRequested = content.FocusRequested(auto_revoke, [weakThis, dispatcher, id](auto&&, auto&&) {
            _dispatchSurfaceUi(dispatcher, [weakThis, id] {
                if (const auto self = weakThis.get(); self && self->_activeSurfaceId == id)
                {
                    self->FocusRequested.raise(*self, nullptr);
                }
            });
        });
        surface.events.BellRequested = content.BellRequested(auto_revoke, [weakThis, dispatcher, id](auto&&, const TerminalApp::BellEventArgs& args) {
            const auto flashTaskbar = args.FlashTaskbar();
            const auto sendNotification = args.SendNotification();
            _dispatchSurfaceUi(dispatcher, [weakThis, id, flashTaskbar, sendNotification] {
                if (const auto self = weakThis.get())
                {
                    if (const auto surface = self->_find(id); surface && id != self->_activeSurfaceId)
                    {
                        ++surface->unreadCount;
                        self->_rebuildTabStrip();
                    }
                    const auto forwardedArgs = winrt::make<winrt::TerminalApp::implementation::BellEventArgs>(
                        flashTaskbar,
                        sendNotification);
                    self->BellRequested.raise(*self, forwardedArgs);
                }
            });
        });
        surface.events.NotificationRequested = content.NotificationRequested(auto_revoke, [weakThis, dispatcher, id](auto&&, const TerminalApp::NotificationEventArgs& args) {
            const auto title = args.Title();
            const auto body = args.Body();
            _dispatchSurfaceUi(dispatcher, [weakThis, id, title, body] {
                if (const auto self = weakThis.get())
                {
                    if (const auto surface = self->_find(id); surface && id != self->_activeSurfaceId)
                    {
                        ++surface->unreadCount;
                        self->_rebuildTabStrip();
                    }
                    const auto forwardedArgs = winrt::make<winrt::TerminalApp::implementation::NotificationEventArgs>(
                        title,
                        body);
                    self->NotificationRequested.raise(*self, forwardedArgs);
                }
            });
        });
    }

    void SurfaceStackPaneContent::_rebuildTabStrip()
    {
        _tabStrip.Children().Clear();
        for (const auto& surface : _surfaces)
        {
            Button tab;
            tab.MinWidth(72);
            tab.MaxWidth(220);
            tab.Height(24);
            tab.Padding(Thickness{ 8, 1, 8, 1 });
            tab.BorderThickness(Thickness{ 0 });
            auto background = surface.id == _activeSurfaceId ?
                                  Color{ 0x38, 0xFF, 0xFF, 0xFF } :
                                  Colors::Transparent();
            if (const auto customColor = surface.content.TabColor())
            {
                background = customColor.Value();
                background.A = surface.id == _activeSurfaceId ? 0xFF : 0x66;
            }
            tab.Background(SolidColorBrush{ background });

            TextBlock label;
            auto title = surface.content.Title().empty() ? hstring{ L"Terminal" } : surface.content.Title();
            if (surface.unreadCount > 0)
            {
                title = hstring{ fmt::format(FMT_COMPILE(L"{}  • {}"), title, surface.unreadCount) };
            }
            label.Text(title);
            label.FontSize(11);
            label.TextTrimming(TextTrimming::CharacterEllipsis);
            tab.Content(label);
            ToolTipService::SetToolTip(tab, box_value(label.Text()));
            AutomationProperties::SetName(tab, label.Text());

            const auto id = surface.id;
            tab.Click([weakThis = get_weak(), id](auto&&, auto&&) {
            if (const auto self = weakThis.get())
            {
                if (const auto surface = self->_find(id))
                {
                    surface->unreadCount = 0;
                }
                self->ActivateSurface(id);
            }
            });

            MenuFlyout menu;
            MenuFlyoutItem moveLeft;
            moveLeft.Text(RS_(L"SurfaceMoveLeft"));
            moveLeft.Click([weakThis = get_weak(), id](auto&&, auto&&) {
                if (const auto self = weakThis.get()) self->MoveSurface(id, -1);
            });
            MenuFlyoutItem moveRight;
            moveRight.Text(RS_(L"SurfaceMoveRight"));
            moveRight.Click([weakThis = get_weak(), id](auto&&, auto&&) {
                if (const auto self = weakThis.get()) self->MoveSurface(id, 1);
            });
            MenuFlyoutItem closeOthers;
            closeOthers.Text(RS_(L"SurfaceCloseOther"));
            closeOthers.Click([weakThis = get_weak(), id](auto&&, auto&&) {
                if (const auto self = weakThis.get()) self->CloseOtherSurfaces(id);
            });
            MenuFlyoutItem close;
            close.Text(RS_(L"SurfaceClose"));
            close.Click([weakThis = get_weak(), id](auto&&, auto&&) {
                if (const auto self = weakThis.get()) self->CloseSurface(id);
            });
            menu.Items().Append(moveLeft);
            menu.Items().Append(moveRight);
            menu.Items().Append(MenuFlyoutSeparator{});
            menu.Items().Append(closeOthers);
            menu.Items().Append(close);
            tab.ContextFlyout(menu);
            _tabStrip.Children().Append(tab);
        }
    }

    IconElement SurfaceStackPaneContent::_createFlyoutIcon(const hstring& iconPath)
    {
        if (iconPath.empty())
        {
            return nullptr;
        }
        if (const auto icon = UI::IconPathConverter::IconWUX(iconPath))
        {
            AutomationProperties::SetAccessibilityView(
                icon,
                winrt::Windows::UI::Xaml::Automation::Peers::AccessibilityView::Raw);
            return icon;
        }
        return nullptr;
    }

    MenuFlyoutItem SurfaceStackPaneContent::_createNewSurfaceFlyoutProfile(
        const CascadiaSettings& settings,
        const Profile& profile,
        const int32_t profileIndex,
        const hstring& iconPathOverride)
    {
        MenuFlyoutItem item;
        item.Text(profile.Name());

        if (profile.Guid() == settings.GlobalSettings().DefaultProfile())
        {
            item.FontWeight(FontWeights::Bold());
        }

        const auto iconPath = iconPathOverride.empty() ? profile.Icon().Resolved() : iconPathOverride;
        item.Icon(_createFlyoutIcon(iconPath));

        const auto id = fmt::format(FMT_COMPILE(L"Terminal.OpenNewTabProfile{}"), profileIndex);
        if (const auto chord = settings.ActionMap().GetKeyBindingForAction(id))
        {
            item.KeyboardAcceleratorTextOverride(KeyChordSerialization::ToString(chord));
        }

        item.Click([weakThis = get_weak(), profileIndex](auto&&, auto&&) {
            if (const auto self = weakThis.get())
            {
                // Profile identity remains an INewContentArgs. TerminalPage is
                // still the sole factory for profile, SSH, WSL and cloud
                // terminal connections.
                self->NewSurfaceRequested.raise(*self, NewTerminalArgs{ profileIndex });
            }
        });
        return item;
    }

    void SurfaceStackPaneContent::_dispatchFlyoutAction(const ActionAndArgs& action)
    {
        if (!action)
        {
            return;
        }

        // A "newTab" entry inside the surface tab strip means "new surface
        // in this workspace". All other commands retain their native action
        // semantics through TerminalPage's existing ShortcutActionDispatch.
        if (action.Action() == ShortcutAction::NewTab)
        {
            const auto args = action.Args().try_as<NewTabArgs>();
            NewSurfaceRequested.raise(*this, args ? args.ContentArgs() : nullptr);
            return;
        }

        ActionRequested.raise(*this, action);
    }

    MenuFlyoutItem SurfaceStackPaneContent::_createNewSurfaceFlyoutAction(
        const CascadiaSettings& settings,
        const hstring& actionId,
        const hstring& iconPathOverride)
    {
        MenuFlyoutItem item;
        const auto action = settings.ActionMap().GetActionByID(actionId);
        if (!action)
        {
            item.IsEnabled(false);
            return item;
        }

        item.Text(action.Name());
        const auto iconPath = iconPathOverride.empty() ? action.Icon().Resolved() : iconPathOverride;
        item.Icon(_createFlyoutIcon(iconPath));
        if (const auto chord = settings.ActionMap().GetKeyBindingForAction(actionId))
        {
            item.KeyboardAcceleratorTextOverride(KeyChordSerialization::ToString(chord));
        }

        item.Click([weakThis = get_weak(), action](auto&&, auto&&) {
            if (const auto self = weakThis.get())
            {
                self->_dispatchFlyoutAction(action.ActionAndArgs());
            }
        });
        return item;
    }

    std::vector<MenuFlyoutItemBase> SurfaceStackPaneContent::_createNewSurfaceFlyoutItems(
        const CascadiaSettings& settings,
        const Windows::Foundation::Collections::IVector<NewTabMenuEntry>& entries)
    {
        std::vector<MenuFlyoutItemBase> items;
        if (!entries)
        {
            return items;
        }

        for (const auto& entry : entries)
        {
            if (!entry)
            {
                continue;
            }

            switch (entry.Type())
            {
            case NewTabMenuEntryType::Separator:
                items.emplace_back(MenuFlyoutSeparator{});
                break;
            case NewTabMenuEntryType::Folder:
            {
                const auto folder = entry.as<FolderEntry>();
                const auto children = _createNewSurfaceFlyoutItems(settings, folder.Entries());
                if (children.empty() && (!folder.AllowEmpty() || folder.Inlining() == FolderEntryInlining::Auto))
                {
                    break;
                }
                if (folder.Inlining() == FolderEntryInlining::Auto && children.size() == 1)
                {
                    items.emplace_back(children.front());
                    break;
                }

                MenuFlyoutSubItem folderItem;
                folderItem.Text(folder.Name());
                folderItem.Icon(_createFlyoutIcon(folder.Icon().Resolved()));
                for (const auto& child : children)
                {
                    folderItem.Items().Append(child);
                }
                if (children.empty())
                {
                    MenuFlyoutItem empty;
                    empty.Text(RS_(L"NewTabMenuFolderEmpty"));
                    empty.IsEnabled(false);
                    folderItem.Items().Append(empty);
                }
                items.emplace_back(folderItem);
                break;
            }
            case NewTabMenuEntryType::RemainingProfiles:
            case NewTabMenuEntryType::MatchProfiles:
            {
                const auto collection = entry.as<ProfileCollectionEntry>();
                if (const auto profiles = collection.Profiles())
                {
                    for (auto&& [profileIndex, profile] : profiles)
                    {
                        items.emplace_back(_createNewSurfaceFlyoutProfile(settings, profile, profileIndex, {}));
                    }
                }
                break;
            }
            case NewTabMenuEntryType::Profile:
            {
                const auto profileEntry = entry.as<ProfileEntry>();
                if (const auto profile = profileEntry.Profile())
                {
                    items.emplace_back(_createNewSurfaceFlyoutProfile(
                        settings,
                        profile,
                        profileEntry.ProfileIndex(),
                        profileEntry.Icon().Resolved()));
                }
                break;
            }
            case NewTabMenuEntryType::Action:
            {
                const auto actionEntry = entry.as<ActionEntry>();
                if (settings.ActionMap().GetActionByID(actionEntry.ActionId()))
                {
                    items.emplace_back(_createNewSurfaceFlyoutAction(
                        settings,
                        actionEntry.ActionId(),
                        actionEntry.Icon().Resolved()));
                }
                break;
            }
            default:
                break;
            }
        }
        return items;
    }

    void SurfaceStackPaneContent::_rebuildNewSurfaceFlyout(const CascadiaSettings& settings)
    {
        MenuFlyout flyout;
        const auto items = _createNewSurfaceFlyoutItems(settings, settings.GlobalSettings().NewTabMenu());
        if (!items.empty())
        {
            MenuFlyoutItem destination;
            destination.Text(RS_(L"SurfaceNewWithProfileHeader"));
            destination.IsEnabled(false);
            flyout.Items().Append(destination);
            flyout.Items().Append(MenuFlyoutSeparator{});
        }
        for (const auto& item : items)
        {
            flyout.Items().Append(item);
        }

        // Managed surfaces use the same canonical ComputeTarget store as the
        // CLI and Agents & Tasks view. The marker is intercepted by
        // TerminalPage before any shell is spawned; it is never executed as a
        // command line.
        try
        {
            const auto wtaPath = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
            const auto output = ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                wtaPath,
                L"compute target list",
                2'000);
            Json::Value targets;
            Json::CharReaderBuilder reader;
            std::string errors;
            std::istringstream input{ output };
            if (!output.empty() &&
                Json::parseFromStream(reader, input, &targets, &errors) &&
                targets.isArray())
            {
                MenuFlyoutSubItem managed;
                managed.Text(L"New managed agent surface");
                for (const auto& target : targets)
                {
                    if (target.get("disabled", false).asBool() ||
                        target.get("trust_tier", "").asString() == "production")
                    {
                        continue;
                    }
                    const auto targetId = winrt::to_hstring(target.get("id", "").asString());
                    if (targetId.empty())
                    {
                        continue;
                    }
                    MenuFlyoutSubItem destination;
                    const auto display = winrt::to_hstring(target.get("display_name", "").asString());
                    destination.Text(display.empty() ? targetId : display);
                    for (const auto& agent : ::Microsoft::Terminal::Settings::Model::AgentRegistry::FilteredAcpAgents())
                    {
                        MenuFlyoutItem item;
                        item.Text(winrt::hstring{ agent.displayName });
                        item.Click([
                            weakThis = get_weak(),
                            targetId,
                            agentId = winrt::hstring{ agent.id }
                        ](auto&&, auto&&) {
                            if (const auto self = weakThis.get())
                            {
                                NewTerminalArgs marker;
                                std::wstring markerCommandline{ L"__intellterm_managed_surface_v1__|" };
                                markerCommandline.append(targetId);
                                markerCommandline.push_back(L'|');
                                markerCommandline.append(agentId);
                                marker.Commandline(winrt::hstring{ markerCommandline });
                                self->NewSurfaceRequested.raise(*self, marker);
                            }
                        });
                        destination.Items().Append(item);
                    }
                    managed.Items().Append(destination);
                }
                if (managed.Items().Size() > 0)
                {
                    flyout.Items().Append(MenuFlyoutSeparator{});
                    flyout.Items().Append(managed);
                }
            }

            const auto workspaceOutput = ::Microsoft::Terminal::WtaProcess::RunWtaCaptureStdout(
                wtaPath,
                L"compute remote-workspace list",
                2'000);
            Json::Value remoteWorkspaces;
            std::istringstream workspaceInput{ workspaceOutput };
            if (!workspaceOutput.empty() &&
                Json::parseFromStream(reader, workspaceInput, &remoteWorkspaces, &errors) &&
                remoteWorkspaces.isArray())
            {
                MenuFlyoutSubItem browser;
                browser.Text(L"New remote browser surface");
                for (const auto& workspace : remoteWorkspaces)
                {
                    if (workspace.get("state", "").asString() != "ready")
                    {
                        continue;
                    }
                    const auto remoteWorkspaceId =
                        winrt::to_hstring(workspace.get("remote_workspace_id", "").asString());
                    if (remoteWorkspaceId.empty())
                    {
                        continue;
                    }
                    MenuFlyoutItem item;
                    const auto targetId =
                        winrt::to_hstring(workspace.get("target_id", "").asString());
                    item.Text(targetId.empty() ? remoteWorkspaceId : targetId);
                    item.Click([weakThis = get_weak(), remoteWorkspaceId](auto&&, auto&&) {
                        if (const auto self = weakThis.get())
                        {
                            NewTerminalArgs marker;
                            std::wstring markerCommandline{ L"__intellterm_browser_surface_v1__|" };
                            markerCommandline.append(remoteWorkspaceId);
                            markerCommandline.append(L"|https://example.com");
                            marker.Commandline(winrt::hstring{ markerCommandline });
                            self->NewSurfaceRequested.raise(*self, marker);
                        }
                    });
                    browser.Items().Append(item);
                }
                if (browser.Items().Size() > 0)
                {
                    flyout.Items().Append(MenuFlyoutSeparator{});
                    flyout.Items().Append(browser);
                }
            }
        }
        catch (...)
        {
            LOG_CAUGHT_EXCEPTION();
        }

        // Keep the canonical static commands at the bottom just like the
        // window-level new-tab dropdown. They are dispatched through the
        // same ActionMap, so keybindings and policy remain single-sourced.
        constexpr std::array staticActionIds{
            L"Terminal.OpenSettingsUI",
            L"Terminal.ToggleCommandPalette",
            L"Terminal.OpenAboutDialog",
        };
        bool appendedStaticSeparator = false;
        for (const auto actionId : staticActionIds)
        {
            if (settings.ActionMap().GetActionByID(actionId))
            {
                if (!appendedStaticSeparator && flyout.Items().Size() > 0)
                {
                    flyout.Items().Append(MenuFlyoutSeparator{});
                    appendedStaticSeparator = true;
                }
                flyout.Items().Append(_createNewSurfaceFlyoutAction(settings, actionId, {}));
            }
        }

        if (flyout.Items().Size() == 0)
        {
            MenuFlyoutItem unavailable;
            unavailable.Text(RS_(L"SurfaceNoProfilesAvailable"));
            unavailable.IsEnabled(false);
            flyout.Items().Append(unavailable);
        }

        _newSurfaceButton.Flyout(flyout);
    }

    hstring SurfaceStackPaneContent::_surfaceSessionId(const TerminalApp::IPaneContent& content)
    {
        TerminalApp::TerminalPaneContent terminal{ nullptr };
        if (content)
        {
            terminal = content.try_as<TerminalApp::TerminalPaneContent>();
            if (!terminal)
            {
                if (const auto agent = content.try_as<TerminalApp::AgentPaneContent>())
                {
                    terminal = agent.GetTerminalContent();
                }
            }
        }
        if (terminal)
        {
            if (const auto control = terminal.GetTermControl())
            {
                if (const auto connection = control.Connection())
                {
                    return winrt::to_hstring(connection.SessionId());
                }
            }
        }
        if (const auto browser = content.try_as<TerminalApp::BrowserPaneContent>())
        {
            return browser.SurfaceSessionId();
        }
        return {};
    }

    void SurfaceStackPaneContent::_raiseSurfaceChanged(
        const std::wstring_view kind,
        const uint32_t surfaceId,
        const uint32_t index,
        const TerminalApp::IPaneContent& content,
        const hstring& capturedSessionId)
    {
        _lastSurfaceChangeKind = kind;
        _lastChangedSurfaceId = surfaceId;
        _lastChangedSurfaceIndex = index;
        _lastChangedSurfaceSessionId = capturedSessionId.empty() ? _surfaceSessionId(content) : capturedSessionId;
        Windows::Foundation::Collections::ValueSet change;
        change.Insert(L"kind", box_value(_lastSurfaceChangeKind));
        change.Insert(L"surface_local_id", box_value(_lastChangedSurfaceId));
        change.Insert(L"surface_index", box_value(_lastChangedSurfaceIndex));
        change.Insert(L"surface_count", box_value(SurfaceCount()));
        change.Insert(L"surface_id", box_value(_lastChangedSurfaceSessionId));
        SurfaceCollectionChanged.raise(*this, change);
    }

    void SurfaceStackPaneContent::_showActiveSurface(const bool focus)
    {
        _contentHost.Children().Clear();
        if (const auto active = _active())
        {
            const auto root = active->content.GetRoot();
            if (root)
            {
                _contentHost.Children().Append(root);
            }
            if (focus)
            {
                active->content.Focus(FocusState::Programmatic);
            }
        }
    }

    void SurfaceStackPaneContent::_raiseActiveSurfacePropertiesChanged()
    {
        TitleChanged.raise(*this, nullptr);
        TabColorChanged.raise(*this, nullptr);
        TaskbarProgressChanged.raise(*this, nullptr);
        ReadOnlyChanged.raise(*this, nullptr);
        ConnectionStateChanged.raise(nullptr, nullptr);
    }

    bool SurfaceStackPaneContent::ActivateSurface(const uint32_t surfaceId)
    {
        if (!_find(surfaceId) || _activeSurfaceId == surfaceId)
        {
            if (const auto surface = _find(surfaceId); surface && surface->unreadCount)
            {
                surface->unreadCount = 0;
                _rebuildTabStrip();
            }
            return _activeSurfaceId == surfaceId;
        }
        _find(surfaceId)->unreadCount = 0;
        _activeSurfaceId = surfaceId;
        _rebuildTabStrip();
        _showActiveSurface(true);
        _raiseActiveSurfacePropertiesChanged();
        const auto it = std::find_if(_surfaces.cbegin(), _surfaces.cend(), [surfaceId](const auto& surface) {
            return surface.id == surfaceId;
        });
        _raiseSurfaceChanged(
            L"activated",
            surfaceId,
            gsl::narrow_cast<uint32_t>(std::distance(_surfaces.cbegin(), it)),
            it->content);
        return true;
    }

    bool SurfaceStackPaneContent::_activateRelative(const int32_t delta)
    {
        if (_surfaces.size() < 2)
        {
            return false;
        }
        const auto it = std::find_if(_surfaces.begin(), _surfaces.end(), [this](const auto& surface) {
            return surface.id == _activeSurfaceId;
        });
        if (it == _surfaces.end())
        {
            return false;
        }
        const auto current = std::distance(_surfaces.begin(), it);
        const auto count = gsl::narrow_cast<int64_t>(_surfaces.size());
        const auto next = (current + delta + count) % count;
        return ActivateSurface(_surfaces[gsl::narrow_cast<size_t>(next)].id);
    }

    void SurfaceStackPaneContent::_closeSurfaceAt(const size_t index, const bool closeContent)
    {
        if (index >= _surfaces.size()) return;
        auto surface = std::move(_surfaces[index]);
        _surfaces.erase(_surfaces.begin() + index);
        surface.events = {};
        if (closeContent && surface.content)
        {
            surface.content.Close();
        }
    }

    bool SurfaceStackPaneContent::CloseSurface(const uint32_t surfaceId)
    {
        const auto it = std::find_if(_surfaces.begin(), _surfaces.end(), [surfaceId](const auto& surface) {
            return surface.id == surfaceId;
        });
        if (it == _surfaces.end()) return false;
        const auto index = std::distance(_surfaces.begin(), it);
        const auto closedContent = it->content;
        const auto closedSessionId = _surfaceSessionId(closedContent);
        _closeSurfaceAt(index, true);
        if (_surfaces.empty())
        {
            _raiseSurfaceChanged(L"closed", surfaceId, gsl::narrow_cast<uint32_t>(index), closedContent, closedSessionId);
            CloseRequested.raise(*this, nullptr);
            return true;
        }
        if (_activeSurfaceId == surfaceId)
        {
            _activeSurfaceId = _surfaces[std::min<size_t>(index, _surfaces.size() - 1)].id;
        }
        _rebuildTabStrip();
        _showActiveSurface(true);
        _raiseActiveSurfacePropertiesChanged();
        _raiseSurfaceChanged(L"closed", surfaceId, gsl::narrow_cast<uint32_t>(index), closedContent, closedSessionId);
        return true;
    }

    bool SurfaceStackPaneContent::CloseOtherSurfaces(const uint32_t surfaceId)
    {
        if (!_find(surfaceId)) return false;
        struct ClosedSurface
        {
            uint32_t id;
            uint32_t index;
            TerminalApp::IPaneContent content;
            hstring sessionId;
        };
        std::vector<ClosedSurface> closed;
        for (size_t i = _surfaces.size(); i-- > 0;)
        {
            if (_surfaces[i].id != surfaceId)
            {
                closed.emplace_back(ClosedSurface{
                    _surfaces[i].id,
                    gsl::narrow_cast<uint32_t>(i),
                    _surfaces[i].content,
                    _surfaceSessionId(_surfaces[i].content),
                });
                _closeSurfaceAt(i, true);
            }
        }
        _activeSurfaceId = surfaceId;
        _rebuildTabStrip();
        _showActiveSurface(true);
        _raiseActiveSurfacePropertiesChanged();
        for (const auto& item : closed)
        {
            _raiseSurfaceChanged(L"closed", item.id, item.index, item.content, item.sessionId);
        }
        return true;
    }

    bool SurfaceStackPaneContent::MoveSurface(const uint32_t surfaceId, const int32_t delta)
    {
        const auto it = std::find_if(_surfaces.begin(), _surfaces.end(), [surfaceId](const auto& surface) {
            return surface.id == surfaceId;
        });
        if (it == _surfaces.end()) return false;
        const auto from = std::distance(_surfaces.begin(), it);
        const auto to = std::clamp<int64_t>(static_cast<int64_t>(from) + delta, 0, static_cast<int64_t>(_surfaces.size()) - 1);
        if (from == to) return false;
        std::iter_swap(_surfaces.begin() + from, _surfaces.begin() + to);
        _rebuildTabStrip();
        _raiseSurfaceChanged(
            L"moved",
            surfaceId,
            gsl::narrow_cast<uint32_t>(to),
            _surfaces[gsl::narrow_cast<size_t>(to)].content);
        return true;
    }

    TerminalApp::IPaneContent SurfaceStackPaneContent::DetachActiveSurface()
    {
        const auto it = std::find_if(_surfaces.begin(), _surfaces.end(), [this](const auto& surface) {
            return surface.id == _activeSurfaceId;
        });
        if (it == _surfaces.end()) return nullptr;
        auto content = it->content;
        const auto detachedId = it->id;
        const auto detachedSessionId = _surfaceSessionId(content);
        const auto index = std::distance(_surfaces.begin(), it);
        _closeSurfaceAt(index, false);
        _activeSurfaceId = _surfaces.empty() ? 0 : _surfaces[std::min<size_t>(index, _surfaces.size() - 1)].id;
        _rebuildTabStrip();
        _showActiveSurface(false);
        _raiseSurfaceChanged(L"detached", detachedId, gsl::narrow_cast<uint32_t>(index), content, detachedSessionId);
        return content;
    }

    FrameworkElement SurfaceStackPaneContent::GetRoot() { return _root; }
    Size SurfaceStackPaneContent::MinimumSize()
    {
        if (const auto active = _active())
        {
            auto size = active->content.MinimumSize();
            size.Height += 28;
            return size;
        }
        return {};
    }
    hstring SurfaceStackPaneContent::Title() { if (const auto active = _active()) return active->content.Title(); return {}; }
    uint64_t SurfaceStackPaneContent::TaskbarState()
    {
        uint64_t state = 0;
        for (const auto& surface : _surfaces) state = std::max(state, surface.content.TaskbarState());
        return state;
    }
    uint64_t SurfaceStackPaneContent::TaskbarProgress()
    {
        if (const auto active = _active()) return active->content.TaskbarProgress();
        return 0;
    }
    bool SurfaceStackPaneContent::ReadOnly() { if (const auto active = _active()) return active->content.ReadOnly(); return true; }
    hstring SurfaceStackPaneContent::Icon() { if (const auto active = _active()) return active->content.Icon(); return {}; }
    IReference<Color> SurfaceStackPaneContent::TabColor() { if (const auto active = _active()) return active->content.TabColor(); return nullptr; }
    Brush SurfaceStackPaneContent::BackgroundBrush() { if (const auto active = _active()) return active->content.BackgroundBrush(); return nullptr; }
    INewContentArgs SurfaceStackPaneContent::GetNewTerminalArgs(const BuildStartupKind kind)
    {
        if (const auto active = _active()) return active->content.GetNewTerminalArgs(kind);
        return nullptr;
    }
    void SurfaceStackPaneContent::Focus(const FocusState reason) { if (const auto active = _active()) active->content.Focus(reason); }
    void SurfaceStackPaneContent::UpdateSettings(const CascadiaSettings& settings)
    {
        _rebuildNewSurfaceFlyout(settings);
        for (const auto& surface : _surfaces) surface.content.UpdateSettings(settings);
    }
    void SurfaceStackPaneContent::Close()
    {
        if (std::exchange(_closed, true)) return;
        _contentHost.Children().Clear();
        for (auto& surface : _surfaces)
        {
            surface.events = {};
            if (surface.content) surface.content.Close();
        }
        _surfaces.clear();
        _tabStrip.Children().Clear();
    }
    uint32_t SurfaceStackPaneContent::SurfaceCount() const noexcept { return gsl::narrow_cast<uint32_t>(_surfaces.size()); }
    uint32_t SurfaceStackPaneContent::ActiveSurfaceId() const noexcept { return _activeSurfaceId; }
    hstring SurfaceStackPaneContent::LastSurfaceChangeKind() const noexcept { return _lastSurfaceChangeKind; }
    uint32_t SurfaceStackPaneContent::LastChangedSurfaceId() const noexcept { return _lastChangedSurfaceId; }
    uint32_t SurfaceStackPaneContent::LastChangedSurfaceIndex() const noexcept { return _lastChangedSurfaceIndex; }
    hstring SurfaceStackPaneContent::LastChangedSurfaceSessionId() const noexcept { return _lastChangedSurfaceSessionId; }
    uint32_t SurfaceStackPaneContent::SurfaceIdAt(const uint32_t index) const noexcept
    {
        return index < _surfaces.size() ? _surfaces[index].id : 0;
    }
    TerminalApp::IPaneContent SurfaceStackPaneContent::SurfaceAt(const uint32_t index) const noexcept
    {
        return index < _surfaces.size() ? _surfaces[index].content : nullptr;
    }
    TerminalApp::IPaneContent SurfaceStackPaneContent::ActiveSurface() const noexcept
    {
        if (const auto active = _active()) return active->content;
        return nullptr;
    }
}
