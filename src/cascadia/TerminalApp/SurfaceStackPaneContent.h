// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#pragma once

#include "SurfaceStackPaneContent.g.h"
#include "BasicPaneEvents.h"

namespace winrt::TerminalApp::implementation
{
    struct SurfaceStackPaneContent :
        SurfaceStackPaneContentT<SurfaceStackPaneContent>,
        BasicPaneEvents
    {
        SurfaceStackPaneContent(const TerminalApp::IPaneContent& initialSurface);

        Windows::UI::Xaml::FrameworkElement GetRoot();
        void UpdateSettings(const Microsoft::Terminal::Settings::Model::CascadiaSettings& settings);
        Windows::Foundation::Size MinimumSize();
        winrt::hstring Title();
        uint64_t TaskbarState();
        uint64_t TaskbarProgress();
        bool ReadOnly();
        winrt::hstring Icon();
        Windows::Foundation::IReference<Windows::UI::Color> TabColor();
        Windows::UI::Xaml::Media::Brush BackgroundBrush();
        Microsoft::Terminal::Settings::Model::INewContentArgs GetNewTerminalArgs(BuildStartupKind kind);
        void Focus(Windows::UI::Xaml::FocusState reason);
        void Close();

        uint32_t SurfaceCount() const noexcept;
        uint32_t ActiveSurfaceId() const noexcept;
        TerminalApp::IPaneContent ActiveSurface() const noexcept;
        uint32_t SurfaceIdAt(uint32_t index) const noexcept;
        TerminalApp::IPaneContent SurfaceAt(uint32_t index) const noexcept;
        uint32_t AddSurface(const TerminalApp::IPaneContent& content);
        TerminalApp::IPaneContent DetachActiveSurface();
        bool ActivateSurface(uint32_t surfaceId);
        bool CloseSurface(uint32_t surfaceId);
        bool CloseOtherSurfaces(uint32_t surfaceId);
        bool MoveSurface(uint32_t surfaceId, int32_t delta);
        winrt::hstring LastSurfaceChangeKind() const noexcept;
        uint32_t LastChangedSurfaceId() const noexcept;
        uint32_t LastChangedSurfaceIndex() const noexcept;
        winrt::hstring LastChangedSurfaceSessionId() const noexcept;

        til::typed_event<TerminalApp::SurfaceStackPaneContent, Windows::Foundation::IInspectable> NewSurfaceRequested;
        til::typed_event<TerminalApp::SurfaceStackPaneContent, Windows::Foundation::IInspectable> ActionRequested;
        til::typed_event<TerminalApp::SurfaceStackPaneContent, Windows::Foundation::IInspectable> SurfaceCollectionChanged;

    private:
        struct SurfaceEventTokens
        {
            TerminalApp::IPaneContent::ConnectionStateChanged_revoker ConnectionStateChanged;
            TerminalApp::IPaneContent::CloseRequested_revoker CloseRequested;
            TerminalApp::IPaneContent::BellRequested_revoker BellRequested;
            TerminalApp::IPaneContent::TitleChanged_revoker TitleChanged;
            TerminalApp::IPaneContent::TabColorChanged_revoker TabColorChanged;
            TerminalApp::IPaneContent::TaskbarProgressChanged_revoker TaskbarProgressChanged;
            TerminalApp::IPaneContent::ReadOnlyChanged_revoker ReadOnlyChanged;
            TerminalApp::IPaneContent::FocusRequested_revoker FocusRequested;
            TerminalApp::IPaneContent::NotificationRequested_revoker NotificationRequested;
        };

        struct Surface
        {
            uint32_t id{};
            TerminalApp::IPaneContent content{ nullptr };
            SurfaceEventTokens events{};
            uint32_t unreadCount{};
        };

        Windows::UI::Xaml::Controls::Grid _root;
        Windows::UI::Xaml::Controls::Border _chromeFrame;
        Windows::UI::Xaml::Controls::Grid _chrome;
        Windows::UI::Xaml::Controls::StackPanel _tabStrip;
        Windows::UI::Xaml::Controls::Grid _contentHost;
        Microsoft::UI::Xaml::Controls::SplitButton _newSurfaceButton;
        std::vector<Surface> _surfaces;
        uint32_t _activeSurfaceId{};
        uint32_t _nextSurfaceId{ 1 };
        bool _closed{ false };
        winrt::hstring _lastSurfaceChangeKind{};
        uint32_t _lastChangedSurfaceId{};
        uint32_t _lastChangedSurfaceIndex{};
        winrt::hstring _lastChangedSurfaceSessionId{};

        Surface* _find(uint32_t surfaceId) noexcept;
        const Surface* _find(uint32_t surfaceId) const noexcept;
        Surface* _active() noexcept;
        const Surface* _active() const noexcept;
        void _initializeVisualTree();
        void _wireSurface(Surface& surface);
        void _rebuildTabStrip();
        void _rebuildNewSurfaceFlyout(const Microsoft::Terminal::Settings::Model::CascadiaSettings& settings);
        std::vector<Windows::UI::Xaml::Controls::MenuFlyoutItemBase> _createNewSurfaceFlyoutItems(
            const Microsoft::Terminal::Settings::Model::CascadiaSettings& settings,
            const Windows::Foundation::Collections::IVector<Microsoft::Terminal::Settings::Model::NewTabMenuEntry>& entries);
        Windows::UI::Xaml::Controls::MenuFlyoutItem _createNewSurfaceFlyoutProfile(
            const Microsoft::Terminal::Settings::Model::CascadiaSettings& settings,
            const Microsoft::Terminal::Settings::Model::Profile& profile,
            int32_t profileIndex,
            const winrt::hstring& iconPathOverride);
        Windows::UI::Xaml::Controls::MenuFlyoutItem _createNewSurfaceFlyoutAction(
            const Microsoft::Terminal::Settings::Model::CascadiaSettings& settings,
            const winrt::hstring& actionId,
            const winrt::hstring& iconPathOverride);
        Windows::UI::Xaml::Controls::IconElement _createFlyoutIcon(const winrt::hstring& iconPath);
        void _dispatchFlyoutAction(const Microsoft::Terminal::Settings::Model::ActionAndArgs& action);
        void _raiseSurfaceChanged(
            std::wstring_view kind,
            uint32_t surfaceId,
            uint32_t index,
            const TerminalApp::IPaneContent& content,
            const winrt::hstring& capturedSessionId = {});
        static winrt::hstring _surfaceSessionId(const TerminalApp::IPaneContent& content);
        void _showActiveSurface(bool focus);
        void _raiseActiveSurfacePropertiesChanged();
        void _closeSurfaceAt(size_t index, bool closeContent);
        bool _activateRelative(int32_t delta);
    };
}

namespace winrt::TerminalApp::factory_implementation
{
    BASIC_FACTORY(SurfaceStackPaneContent);
}
