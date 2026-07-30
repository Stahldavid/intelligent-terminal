// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#pragma once

#include "BrowserPaneContent.g.h"
#include "BasicPaneEvents.h"

#include <WebView2.h>
#include <wrl.h>

namespace winrt::TerminalApp::implementation
{
    struct BrowserPaneContent :
        BrowserPaneContentT<BrowserPaneContent>,
        BasicPaneEvents
    {
        BrowserPaneContent();
        ~BrowserPaneContent();

        void Initialize(uint64_t ownerHwnd,
                        const winrt::hstring& browserRecordId,
                        const winrt::hstring& surfaceSessionId,
                        const winrt::hstring& userDataFolder,
                        uint16_t proxyPort,
                        const winrt::hstring& initialUrl);

        winrt::hstring BrowserRecordId() const noexcept { return _browserRecordId; }
        winrt::hstring SurfaceSessionId() const noexcept { return _surfaceSessionId; }

#pragma region IPaneContent
        Windows::UI::Xaml::FrameworkElement GetRoot() { return *this; }
        void UpdateSettings(const Microsoft::Terminal::Settings::Model::CascadiaSettings&) {}
        Windows::Foundation::Size MinimumSize() { return { 120, 80 }; }
        void Focus(Windows::UI::Xaml::FocusState reason = Windows::UI::Xaml::FocusState::Programmatic);
        void Close();
        Microsoft::Terminal::Settings::Model::INewContentArgs GetNewTerminalArgs(BuildStartupKind kind) const;
        winrt::hstring Title() { return _title.empty() ? winrt::hstring{ L"Browser" } : _title; }
        uint64_t TaskbarState() { return 0; }
        uint64_t TaskbarProgress() { return 0; }
        bool ReadOnly() { return false; }
        winrt::hstring Icon() const { return L"\xE774"; }
        Windows::Foundation::IReference<Windows::UI::Color> TabColor() const noexcept { return nullptr; }
        Windows::UI::Xaml::Media::Brush BackgroundBrush() { return Background(); }
#pragma endregion

    private:
        HWND _ownerHwnd{};
        HWND _browserHostHwnd{};
        winrt::hstring _browserRecordId;
        winrt::hstring _surfaceSessionId;
        winrt::hstring _userDataFolder;
        winrt::hstring _initialUrl;
        winrt::hstring _title{ L"Browser" };
        uint16_t _proxyPort{};
        bool _initialized{};
        bool _closing{};

        ::Microsoft::WRL::ComPtr<ICoreWebView2Environment> _environment;
        ::Microsoft::WRL::ComPtr<ICoreWebView2Controller> _controller;
        ::Microsoft::WRL::ComPtr<ICoreWebView2> _webView;
        ::Microsoft::WRL::ComPtr<ICoreWebView2_4> _webView4;
        EventRegistrationToken _navigationStartingToken{};
        EventRegistrationToken _navigationCompletedToken{};
        EventRegistrationToken _sourceChangedToken{};
        EventRegistrationToken _documentTitleChangedToken{};
        EventRegistrationToken _newWindowToken{};
        EventRegistrationToken _permissionToken{};
        EventRegistrationToken _downloadToken{};

        Windows::UI::Xaml::FrameworkElement::Loaded_revoker _loadedRevoker;
        Windows::UI::Xaml::FrameworkElement::Unloaded_revoker _unloadedRevoker;
        Windows::UI::Xaml::FrameworkElement::LayoutUpdated_revoker _layoutRevoker;
        Windows::UI::Xaml::Controls::Primitives::ButtonBase::Click_revoker _backRevoker;
        Windows::UI::Xaml::Controls::Primitives::ButtonBase::Click_revoker _forwardRevoker;
        Windows::UI::Xaml::Controls::Primitives::ButtonBase::Click_revoker _reloadRevoker;
        Windows::UI::Xaml::Controls::TextBox::KeyDown_revoker _addressKeyRevoker;

        void _start();
        void _createHostWindow();
        void _updateBounds();
        void _setStatus(std::wstring_view message, bool failed);
        void _navigate(std::wstring_view url);
        void _reportState(std::wstring_view state, std::wstring_view error = {});
        void _closeWebView() noexcept;
        bool _isAllowedUri(std::wstring_view uri) const noexcept;
    };
}

namespace winrt::TerminalApp::factory_implementation
{
    BASIC_FACTORY(BrowserPaneContent);
}
