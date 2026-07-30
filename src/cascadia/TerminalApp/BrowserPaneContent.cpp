// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#include "pch.h"
#include "BrowserPaneContent.h"
#include "BrowserPaneContent.g.cpp"

#include "../inc/WtaProcess.h"
#include "../WinRTUtils/inc/WtExeUtils.h"
#include <WebView2EnvironmentOptions.h>
#include <wrl/event.h>

using namespace winrt::Windows::System;
using namespace winrt::Windows::UI::Xaml;
using namespace winrt::Windows::UI::Xaml::Controls;
using namespace winrt::Windows::UI::Xaml::Input;
using namespace winrt::Microsoft::Terminal::Settings::Model;
using ::Microsoft::WRL::Callback;
using ::Microsoft::WRL::Make;

namespace
{
    std::wstring _quote(const std::wstring_view value)
    {
        std::wstring quoted;
        QuoteAndEscapeCommandlineArg(value, quoted);
        return quoted;
    }

    void _spawnWta(const std::wstring& arguments)
    {
        const auto wta = ::Microsoft::Terminal::WtaProcess::ResolveWtaExePath();
        ::Microsoft::Terminal::WtaProcess::RunWtaDetached(wta, arguments);
    }
}

namespace winrt::TerminalApp::implementation
{
    BrowserPaneContent::BrowserPaneContent()
    {
        InitializeComponent();
        _loadedRevoker = Loaded(auto_revoke, [this](auto&&, auto&&) {
            _createHostWindow();
            _updateBounds();
            _start();
        });
        _unloadedRevoker = Unloaded(auto_revoke, [this](auto&&, auto&&) {
            if (_browserHostHwnd)
            {
                ShowWindow(_browserHostHwnd, SW_HIDE);
            }
        });
        _layoutRevoker = LayoutUpdated(auto_revoke, [this](auto&&, auto&&) {
            _updateBounds();
        });
        _backRevoker = BackButton().Click(auto_revoke, [this](auto&&, auto&&) {
            BOOL canGoBack = FALSE;
            if (_webView && SUCCEEDED(_webView->get_CanGoBack(&canGoBack)) && canGoBack)
            {
                _webView->GoBack();
                _spawnWta(L"compute browser back " + _quote(_browserRecordId));
            }
        });
        _forwardRevoker = ForwardButton().Click(auto_revoke, [this](auto&&, auto&&) {
            BOOL canGoForward = FALSE;
            if (_webView && SUCCEEDED(_webView->get_CanGoForward(&canGoForward)) && canGoForward)
            {
                _webView->GoForward();
                _spawnWta(L"compute browser forward " + _quote(_browserRecordId));
            }
        });
        _reloadRevoker = ReloadButton().Click(auto_revoke, [this](auto&&, auto&&) {
            if (_webView)
            {
                _webView->Reload();
            }
        });
        _addressKeyRevoker = AddressBar().KeyDown(auto_revoke, [this](auto&&, const KeyRoutedEventArgs& args) {
            if (args.Key() == VirtualKey::Enter)
            {
                _navigate(AddressBar().Text());
                args.Handled(true);
            }
        });
    }

    BrowserPaneContent::~BrowserPaneContent()
    {
        _closeWebView();
    }

    void BrowserPaneContent::Initialize(
        const uint64_t ownerHwnd,
        const winrt::hstring& browserRecordId,
        const winrt::hstring& surfaceSessionId,
        const winrt::hstring& userDataFolder,
        const uint16_t proxyPort,
        const winrt::hstring& initialUrl)
    {
        if (_initialized)
        {
            throw winrt::hresult_illegal_method_call();
        }
        _ownerHwnd = reinterpret_cast<HWND>(ownerHwnd);
        _browserRecordId = browserRecordId;
        _surfaceSessionId = surfaceSessionId;
        _userDataFolder = userDataFolder;
        _proxyPort = proxyPort;
        _initialUrl = initialUrl;
        AddressBar().Text(initialUrl);
        _initialized = true;
        if (IsLoaded())
        {
            _createHostWindow();
            _updateBounds();
            _start();
        }
    }

    void BrowserPaneContent::_createHostWindow()
    {
        if (_browserHostHwnd || !_ownerHwnd || !_initialized || _closing)
        {
            return;
        }
        _browserHostHwnd = CreateWindowExW(
            0,
            L"STATIC",
            L"",
            WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            0,
            0,
            1,
            1,
            _ownerHwnd,
            nullptr,
            GetModuleHandleW(nullptr),
            nullptr);
        if (!_browserHostHwnd)
        {
            _setStatus(L"Could not create the isolated browser host window.", true);
            _reportState(L"failed", L"native browser host window creation failed");
        }
    }

    void BrowserPaneContent::_updateBounds()
    {
        if (!_browserHostHwnd || !_ownerHwnd || !IsLoaded())
        {
            return;
        }
        try
        {
            const auto origin = BrowserHost().TransformToVisual(nullptr).TransformPoint({ 0, 0 });
            const auto dpi = GetDpiForWindow(_ownerHwnd);
            const auto scale = static_cast<double>(dpi ? dpi : 96) / 96.0;
            const auto width = (std::max)(1, static_cast<int>(std::lround(BrowserHost().ActualWidth() * scale)));
            const auto height = (std::max)(1, static_cast<int>(std::lround(BrowserHost().ActualHeight() * scale)));
            SetWindowPos(
                _browserHostHwnd,
                HWND_TOP,
                static_cast<int>(std::lround(origin.X * scale)),
                static_cast<int>(std::lround(origin.Y * scale)),
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW);
            if (_controller)
            {
                const RECT bounds{ 0, 0, width, height };
                _controller->put_Bounds(bounds);
                _controller->put_IsVisible(TRUE);
            }
        }
        catch (...)
        {
            LOG_CAUGHT_EXCEPTION();
        }
    }

    void BrowserPaneContent::_start()
    {
        if (_environment || !_browserHostHwnd || !_initialized || _closing)
        {
            return;
        }
        if (_userDataFolder.empty() || _proxyPort == 0 || !_isAllowedUri(_initialUrl))
        {
            _setStatus(L"Browser isolation contract is incomplete; shared-profile fallback is disabled.", true);
            _reportState(L"failed", L"invalid browser isolation contract");
            return;
        }

        const auto arguments = fmt::format(
            FMT_COMPILE(L"--proxy-server=socks5://127.0.0.1:{} --proxy-bypass-list=<-loopback>"),
            _proxyPort);
        const auto options = Make<CoreWebView2EnvironmentOptions>();
        options->put_AdditionalBrowserArguments(arguments.c_str());
        const auto weakThis = get_weak();
        const auto result = CreateCoreWebView2EnvironmentWithOptions(
            nullptr,
            _userDataFolder.c_str(),
            options.Get(),
            Callback<ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler>(
                [weakThis](HRESULT result, ICoreWebView2Environment* environment) -> HRESULT {
                    const auto self = weakThis.get();
                    if (!self)
                    {
                        return S_OK;
                    }
                    if (FAILED(result) || !environment)
                    {
                        self->_setStatus(L"WebView2 Runtime is unavailable or failed to initialize.", true);
                        self->_reportState(L"failed", L"WebView2 environment initialization failed");
                        return S_OK;
                    }
                    self->_environment = environment;
                    const auto controllerResult = environment->CreateCoreWebView2Controller(
                        self->_browserHostHwnd,
                        Callback<ICoreWebView2CreateCoreWebView2ControllerCompletedHandler>(
                            [weakThis](HRESULT result, ICoreWebView2Controller* controller) -> HRESULT {
                                const auto self = weakThis.get();
                                if (!self)
                                {
                                    return S_OK;
                                }
                                if (FAILED(result) || !controller)
                                {
                                    self->_setStatus(L"WebView2 controller creation failed closed.", true);
                                    self->_reportState(L"failed", L"WebView2 controller creation failed");
                                    return S_OK;
                                }
                                self->_controller = controller;
                                if (FAILED(controller->get_CoreWebView2(
                                        self->_webView.ReleaseAndGetAddressOf())) ||
                                    !self->_webView)
                                {
                                    self->_setStatus(L"WebView2 core is unavailable.", true);
                                    self->_reportState(L"failed", L"WebView2 core unavailable");
                                    return S_OK;
                                }

                                ::Microsoft::WRL::ComPtr<ICoreWebView2Settings> settings;
                                HRESULT policyResult = self->_webView->get_Settings(
                                    settings.ReleaseAndGetAddressOf());
                                if (SUCCEEDED(policyResult) && settings)
                                {
                                    policyResult = settings->put_AreDevToolsEnabled(FALSE);
                                    if (SUCCEEDED(policyResult))
                                        policyResult = settings->put_IsWebMessageEnabled(FALSE);
                                    if (SUCCEEDED(policyResult))
                                        policyResult = settings->put_AreHostObjectsAllowed(FALSE);
                                    if (SUCCEEDED(policyResult))
                                        policyResult = settings->put_AreDefaultScriptDialogsEnabled(FALSE);
                                    if (SUCCEEDED(policyResult))
                                        policyResult = settings->put_IsStatusBarEnabled(TRUE);
                                    if (SUCCEEDED(policyResult))
                                        policyResult = settings->put_IsZoomControlEnabled(TRUE);

                                    ::Microsoft::WRL::ComPtr<ICoreWebView2Settings3> settings3;
                                    if (SUCCEEDED(policyResult))
                                    {
                                        policyResult = settings.As(&settings3);
                                    }
                                    if (SUCCEEDED(policyResult) && settings3)
                                    {
                                        policyResult = settings3->put_AreBrowserAcceleratorKeysEnabled(TRUE);
                                    }

                                    ::Microsoft::WRL::ComPtr<ICoreWebView2Settings4> settings4;
                                    if (SUCCEEDED(policyResult))
                                    {
                                        policyResult = settings.As(&settings4);
                                    }
                                    if (SUCCEEDED(policyResult) && settings4)
                                    {
                                        policyResult = settings4->put_IsPasswordAutosaveEnabled(FALSE);
                                        if (SUCCEEDED(policyResult))
                                        {
                                            policyResult = settings4->put_IsGeneralAutofillEnabled(FALSE);
                                        }
                                    }
                                }
                                if (FAILED(policyResult))
                                {
                                    self->_setStatus(
                                        L"The WebView2 runtime cannot enforce the isolated browser policy.",
                                        true);
                                    self->_reportState(L"failed", L"WebView2 security policy unavailable");
                                    self->_closeWebView();
                                    return S_OK;
                                }

                                self->_webView->add_NavigationStarting(
                                    Callback<ICoreWebView2NavigationStartingEventHandler>(
                                        [weakThis](ICoreWebView2*, ICoreWebView2NavigationStartingEventArgs* args) -> HRESULT {
                                            if (const auto self = weakThis.get())
                                            {
                                                LPWSTR raw{};
                                                if (SUCCEEDED(args->get_Uri(&raw)))
                                                {
                                                    wil::unique_cotaskmem_string uri{ raw };
                                                    if (!self->_isAllowedUri(uri.get()))
                                                    {
                                                        args->put_Cancel(TRUE);
                                                        self->_setStatus(L"Navigation was blocked by the HTTP/HTTPS-only policy.", true);
                                                    }
                                                }
                                            }
                                            return S_OK;
                                        })
                                        .Get(),
                                    &self->_navigationStartingToken);
                                self->_webView->add_NavigationCompleted(
                                    Callback<ICoreWebView2NavigationCompletedEventHandler>(
                                        [weakThis](ICoreWebView2*, ICoreWebView2NavigationCompletedEventArgs* args) -> HRESULT {
                                            if (const auto self = weakThis.get())
                                            {
                                                BOOL success{};
                                                args->get_IsSuccess(&success);
                                                self->LoadingIndicator().IsActive(false);
                                                self->StatusPanel().Visibility(success ? Visibility::Collapsed : Visibility::Visible);
                                                if (!success)
                                                {
                                                    self->StatusText().Text(L"Remote page navigation failed.");
                                                }
                                                else
                                                {
                                                    self->_reportState(L"ready");
                                                }
                                            }
                                            return S_OK;
                                        })
                                        .Get(),
                                    &self->_navigationCompletedToken);
                                self->_webView->add_SourceChanged(
                                    Callback<ICoreWebView2SourceChangedEventHandler>(
                                        [weakThis](ICoreWebView2* sender, ICoreWebView2SourceChangedEventArgs*) -> HRESULT {
                                            if (const auto self = weakThis.get())
                                            {
                                                LPWSTR raw{};
                                                if (SUCCEEDED(sender->get_Source(&raw)))
                                                {
                                                    wil::unique_cotaskmem_string source{ raw };
                                                    self->AddressBar().Text(source.get());
                                                    if (self->_isAllowedUri(source.get()))
                                                    {
                                                        _spawnWta(
                                                            L"compute browser navigate " +
                                                            _quote(self->_browserRecordId) +
                                                            L" " + _quote(source.get()));
                                                    }
                                                }
                                            }
                                            return S_OK;
                                        })
                                        .Get(),
                                    &self->_sourceChangedToken);
                                self->_webView->add_DocumentTitleChanged(
                                    Callback<ICoreWebView2DocumentTitleChangedEventHandler>(
                                        [weakThis](ICoreWebView2* sender, ::IUnknown*) -> HRESULT {
                                            if (const auto self = weakThis.get())
                                            {
                                                LPWSTR raw{};
                                                if (SUCCEEDED(sender->get_DocumentTitle(&raw)))
                                                {
                                                    wil::unique_cotaskmem_string title{ raw };
                                                    self->_title = title.get();
                                                    self->TitleChanged.raise(*self, nullptr);
                                                }
                                            }
                                            return S_OK;
                                        })
                                        .Get(),
                                    &self->_documentTitleChangedToken);
                                self->_webView->add_NewWindowRequested(
                                    Callback<ICoreWebView2NewWindowRequestedEventHandler>(
                                        [](ICoreWebView2*, ICoreWebView2NewWindowRequestedEventArgs* args) -> HRESULT {
                                            args->put_Handled(TRUE);
                                            return S_OK;
                                        })
                                        .Get(),
                                    &self->_newWindowToken);
                                self->_webView->add_PermissionRequested(
                                    Callback<ICoreWebView2PermissionRequestedEventHandler>(
                                        [](ICoreWebView2*, ICoreWebView2PermissionRequestedEventArgs* args) -> HRESULT {
                                            args->put_State(COREWEBVIEW2_PERMISSION_STATE_DENY);
                                            return S_OK;
                                        })
                                        .Get(),
                                    &self->_permissionToken);
                                if (SUCCEEDED(self->_webView.As(&self->_webView4)) &&
                                    self->_webView4)
                                {
                                    self->_webView4->add_DownloadStarting(
                                        Callback<ICoreWebView2DownloadStartingEventHandler>(
                                            [](ICoreWebView2*, ICoreWebView2DownloadStartingEventArgs* args) -> HRESULT {
                                                // Browser downloads are fail-closed. Remote files use the
                                                // verified WTA transfer/explorer path with hashes and audit.
                                                args->put_Cancel(TRUE);
                                                return S_OK;
                                            })
                                            .Get(),
                                        &self->_downloadToken);
                                }

                                self->_updateBounds();
                                self->StatusPanel().Visibility(Visibility::Collapsed);
                                self->_reportState(L"ready");
                                self->_webView->Navigate(self->_initialUrl.c_str());
                                return S_OK;
                            })
                            .Get());
                    if (FAILED(controllerResult))
                    {
                        self->_setStatus(L"WebView2 controller request failed.", true);
                        self->_reportState(L"failed", L"WebView2 controller request failed");
                    }
                    return S_OK;
                })
                .Get());
        if (FAILED(result))
        {
            _setStatus(L"WebView2 Runtime could not be started.", true);
            _reportState(L"failed", L"WebView2 environment request failed");
        }
    }

    void BrowserPaneContent::_navigate(const std::wstring_view url)
    {
        std::wstring normalized{ url };
        while (!normalized.empty() && iswspace(normalized.front()))
        {
            normalized.erase(normalized.begin());
        }
        while (!normalized.empty() && iswspace(normalized.back()))
        {
            normalized.pop_back();
        }
        if (!_isAllowedUri(normalized))
        {
            _setStatus(L"Only HTTP and HTTPS addresses are allowed.", true);
            return;
        }
        AddressBar().Text(normalized);
        LoadingIndicator().IsActive(true);
        StatusPanel().Visibility(Visibility::Visible);
        StatusText().Text(L"Loading through the surface-scoped SSH proxy…");
        _spawnWta(
            L"compute browser navigate " + _quote(_browserRecordId) + L" " + _quote(normalized));
        if (_webView)
        {
            _webView->Navigate(normalized.c_str());
        }
    }

    void BrowserPaneContent::_setStatus(const std::wstring_view message, const bool failed)
    {
        StatusText().Text(winrt::hstring{ message });
        LoadingIndicator().IsActive(!failed);
        StatusPanel().Visibility(Visibility::Visible);
    }

    void BrowserPaneContent::_reportState(const std::wstring_view state, const std::wstring_view error)
    {
        if (_browserRecordId.empty())
        {
            return;
        }
        if (state == L"ready")
        {
            _spawnWta(L"compute browser ready " + _quote(_browserRecordId));
        }
        else if (state == L"failed")
        {
            _spawnWta(
                L"compute browser fail " + _quote(_browserRecordId) +
                L" --error " + _quote(error.empty() ? L"native browser failed" : error));
        }
    }

    bool BrowserPaneContent::_isAllowedUri(const std::wstring_view uri) const noexcept
    {
        const auto hasAllowedScheme =
            (uri.size() >= 8 && _wcsnicmp(uri.data(), L"https://", 8) == 0) ||
            (uri.size() >= 7 && _wcsnicmp(uri.data(), L"http://", 7) == 0);
        if (!hasAllowedScheme ||
            uri.size() > 8192 ||
            uri.find_first_of(L"\r\n\t") != std::wstring_view::npos)
        {
            return false;
        }
        const auto schemeEnd = uri.find(L"://");
        const auto authorityStart = schemeEnd == std::wstring_view::npos ? 0 : schemeEnd + 3;
        const auto authorityEnd = uri.find_first_of(L"/?#", authorityStart);
        const auto authority = uri.substr(
            authorityStart,
            authorityEnd == std::wstring_view::npos ? uri.size() - authorityStart : authorityEnd - authorityStart);
        return !authority.empty() && authority.find(L'@') == std::wstring_view::npos;
    }

    void BrowserPaneContent::Focus(const winrt::Windows::UI::Xaml::FocusState)
    {
        if (_controller)
        {
            _controller->MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
        }
        else
        {
            AddressBar().Focus(winrt::Windows::UI::Xaml::FocusState::Programmatic);
        }
    }

    INewContentArgs BrowserPaneContent::GetNewTerminalArgs(BuildStartupKind) const
    {
        return BaseContentArgs(winrt::hstring{ std::wstring{ L"x-browser:" } + _browserRecordId.c_str() });
    }

    void BrowserPaneContent::Close()
    {
        if (_closing)
        {
            return;
        }
        _closing = true;
        _closeWebView();
        if (!_browserRecordId.empty())
        {
            _spawnWta(L"compute browser close " + _quote(_browserRecordId));
        }
        CloseRequested.raise(*this, nullptr);
    }

    void BrowserPaneContent::_closeWebView() noexcept
    {
        if (_webView)
        {
            if (_navigationStartingToken.value)
                _webView->remove_NavigationStarting(_navigationStartingToken);
            if (_navigationCompletedToken.value)
                _webView->remove_NavigationCompleted(_navigationCompletedToken);
            if (_sourceChangedToken.value)
                _webView->remove_SourceChanged(_sourceChangedToken);
            if (_documentTitleChangedToken.value)
                _webView->remove_DocumentTitleChanged(_documentTitleChangedToken);
            if (_newWindowToken.value)
                _webView->remove_NewWindowRequested(_newWindowToken);
            if (_permissionToken.value)
                _webView->remove_PermissionRequested(_permissionToken);
        }
        if (_webView4 && _downloadToken.value)
        {
            _webView4->remove_DownloadStarting(_downloadToken);
        }
        if (_controller)
        {
            _controller->Close();
        }
        _webView.Reset();
        _webView4.Reset();
        _controller.Reset();
        _environment.Reset();
        if (_browserHostHwnd)
        {
            DestroyWindow(_browserHostHwnd);
            _browserHostHwnd = nullptr;
        }
    }
}
