// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#include "pch.h"
#include "AgentPaneContent.h"
#include "AgentPaneContent.g.cpp"

#include <algorithm>
#include <cwctype>
#include <sstream>
#include <winrt/Windows.UI.Xaml.Media.h>

using namespace winrt::Windows::UI;
using namespace winrt::Windows::UI::Xaml;
using namespace winrt::Windows::UI::Xaml::Controls;
using namespace winrt::Windows::UI::Xaml::Media;
using namespace winrt::Windows::UI::Xaml::Input;
using namespace winrt::Microsoft::Terminal::Control;
using namespace winrt::Microsoft::Terminal::Settings::Model;

namespace winrt::TerminalApp::implementation
{
    namespace
    {
        enum class AgentLogoKind
        {
            Copilot,
            Claude,
            Gemini,
            Codex,
            OpenCode,
        };

        // Map the agent's display name (case-insensitive substring) to its
        // XAML path. Unknown agents fall back to Copilot.
        AgentLogoKind _logoForAgent(const winrt::hstring& name)
        {
            std::wstring lower{ name };
            std::transform(lower.begin(), lower.end(), lower.begin(),
                           [](wchar_t c) { return static_cast<wchar_t>(std::towlower(c)); });
            if (lower.find(L"claude") != std::wstring::npos) return AgentLogoKind::Claude;
            if (lower.find(L"codex") != std::wstring::npos) return AgentLogoKind::Codex;
            if (lower.find(L"openai") != std::wstring::npos) return AgentLogoKind::Codex;
            if (lower.find(L"gpt") != std::wstring::npos) return AgentLogoKind::Codex;
            if (lower.find(L"gemini") != std::wstring::npos) return AgentLogoKind::Gemini;
            if (lower.find(L"opencode") != std::wstring::npos) return AgentLogoKind::OpenCode;
            return AgentLogoKind::Copilot;
        }

    }

    AgentPaneContent::AgentPaneContent(const winrt::TerminalApp::TerminalPaneContent& inner) :
        _inner{ inner }
    {
        InitializeComponent();

        // The wta TermControl is owned by the inner TerminalPaneContent.
        // Its GetRoot() returns the TermControl itself; pin it into our row 1.
        if (_inner)
        {
            InnerContent().Content(_inner.GetRoot());
        }

        _wireInnerEvents();

        // Default label + logo until wta sends an agent_status event.
        _refreshLabel();
        _refreshLogo();
    }

    winrt::TerminalApp::TerminalPaneContent AgentPaneContent::GetTerminalContent()
    {
        return _inner;
    }

    winrt::Microsoft::Terminal::Control::TermControl AgentPaneContent::GetTermControl()
    {
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            return impl->GetTermControl();
        }
        return nullptr;
    }

    void AgentPaneContent::UpdateAgentStatus(const winrt::hstring& name,
                                             const winrt::hstring& version,
                                             const winrt::hstring& model,
                                             const winrt::hstring& state,
                                             const winrt::hstring& backend)
    {
        const bool nameChanged = _agentName != name;
        _agentName = name;
        _agentVersion = version;
        _agentModel = model;
        _agentState = state;
        _agentBackend = backend;
        _refreshLabel();
        if (nameChanged)
        {
            _refreshLogo();
        }
        // Match `SetSessionsView` and `ApplyAutofixState`: any bottom-bar-
        // affecting state mutation on AgentPaneContent must raise
        // `StateChanged` so subscribers (TerminalPage's bar-refresh
        // handler) can pick up the change without polling. The bar does
        // not currently render the agent name itself, but the cross-
        // window-drag fix path in `TabManagement.cpp` relies on
        // `_UpdateBottomBarState` running once after the wire-up to
        // reflect any cached state the helper pushed before the wire
        // was in place — without the raise here, that catch-up wouldn't
        // observe agent_status arriving in the same race window. Also
        // future-proofs the bar against ever displaying agent name.
        StateChanged.raise(*this, nullptr);
    }

    // Swap the bar between two modes:
    //   * chat / connecting / etc. (active=false) — agent logo + "<name> <version>"
    //   * session management view (active=true)  — no logo, "Agent sessions"
    // Idempotent so callers don't need to dedupe.
    void AgentPaneContent::SetSessionsView(bool active)
    {
        if (_isSessionsView == active)
        {
            return;
        }
        _isSessionsView = active;
        _refreshLabel();
        _refreshLogo();
        StateChanged.raise(*this, nullptr);
    }

    void AgentPaneContent::ApplyAutofixState(AutofixState state,
                                             const winrt::hstring& paneId,
                                             const winrt::hstring& summary,
                                             const winrt::hstring& fixPreview,
                                             const winrt::hstring& hotkeyHint,
                                             const winrt::hstring& suggestionTitle)
    {
        _autofixState = state;
        if (state == AutofixState::Idle)
        {
            // Clear ALL cached fields on idle, including `_hotkeyHint`.
            // The bottom bar reads these directly, so a leftover hint
            // from a prior Detected/Pending transition would otherwise
            // hang around after the bar should have gone quiet.
            _lastErrorPaneId = {};
            _fixPreview = {};
            _suggestionTitle = {};
            _detectedSummary = {};
            _hotkeyHint = {};
        }
        else
        {
            if (!paneId.empty())
            {
                _lastErrorPaneId = paneId;
            }
            if (!summary.empty())
            {
                _detectedSummary = summary;
            }
            if (!fixPreview.empty())
            {
                _fixPreview = fixPreview;
            }
            if (!hotkeyHint.empty())
            {
                _hotkeyHint = hotkeyHint;
            }
            if (!suggestionTitle.empty())
            {
                _suggestionTitle = suggestionTitle;
            }
        }
        StateChanged.raise(*this, nullptr);
    }

    void AgentPaneContent::SetAgentPanePosition(const winrt::hstring& position)
    {
        if (_agentPanePosition == position)
        {
            return;
        }
        _agentPanePosition = position;
        StateChanged.raise(*this, nullptr);
    }

    void AgentPaneContent::UpdateFocusContext(const winrt::hstring& workspace,
                                               const winrt::hstring& pane,
                                               const winrt::hstring& surface,
                                               const winrt::hstring& profile,
                                               const winrt::hstring& cwd)
    {
        std::wstring breadcrumb;
        const auto append = [&breadcrumb](const winrt::hstring& value) {
            if (value.empty())
            {
                return;
            }
            if (!breadcrumb.empty())
            {
                breadcrumb += L"  \u203A  ";
            }
            breadcrumb += value;
        };
        append(workspace);
        append(pane);
        append(surface);
        append(profile);
        if (!cwd.empty())
        {
            append(cwd);
        }
        std::wstring visibleContext{ RS_(L"AgentFollowingContextPrefix") };
        visibleContext += L": ";
        if (!profile.empty())
        {
            visibleContext += profile;
        }
        else if (!workspace.empty())
        {
            visibleContext += workspace;
        }
        if (!cwd.empty())
        {
            if (!profile.empty() || !workspace.empty())
            {
                visibleContext += L"  \u00B7  ";
            }
            visibleContext += cwd;
        }
        AgentContextText().Text(winrt::hstring{ visibleContext });
        winrt::Windows::UI::Xaml::Automation::AutomationProperties::SetName(
            AgentContextText(),
            winrt::hstring{ RS_fmt(L"AgentActiveContextAutomationName", breadcrumb) });
    }

    void AgentPaneContent::_raiseNativeChatAction(
        const std::string_view action,
        const Json::Value& additional)
    {
        if (_nativeWorkspaceId.empty() || _nativeScopeKey.empty())
        {
            return;
        }
        Json::Value params{ Json::objectValue };
        params["action"] = std::string{ action };
        params["workspace_id"] = winrt::to_string(_nativeWorkspaceId);
        params["scope_key"] = winrt::to_string(_nativeScopeKey);
        if (additional.isObject())
        {
            for (const auto& name : additional.getMemberNames())
            {
                params[name] = additional[name];
            }
        }
        Json::StreamWriterBuilder writer;
        writer["indentation"] = "";
        NativeChatAction.raise(
            *this,
            winrt::to_hstring(Json::writeString(writer, params)));
    }

    void AgentPaneContent::_NativeChatSendClicked(
        const winrt::Windows::Foundation::IInspectable&,
        const winrt::Windows::UI::Xaml::RoutedEventArgs&)
    {
        _submitNativeChatComposer();
    }

    void AgentPaneContent::_submitNativeChatComposer()
    {
        if (!_nativeCanSubmit)
        {
            return;
        }
        const auto text = NativeChatComposer().Text();
        if (text.empty())
        {
            return;
        }
        Json::Value additional{ Json::objectValue };
        additional["text"] = winrt::to_string(text);
        _raiseNativeChatAction("submit", additional);
        NativeChatComposer().Text(L"");
    }

    void AgentPaneContent::_NativeChatCancelClicked(
        const winrt::Windows::Foundation::IInspectable&,
        const winrt::Windows::UI::Xaml::RoutedEventArgs&)
    {
        _raiseNativeChatAction("cancel");
    }

    void AgentPaneContent::_NativeChatRetryClicked(
        const winrt::Windows::Foundation::IInspectable&,
        const winrt::Windows::UI::Xaml::RoutedEventArgs&)
    {
        _raiseNativeChatAction("retry");
    }

    void AgentPaneContent::_NativeChatComposerKeyDown(
        const winrt::Windows::Foundation::IInspectable&,
        const winrt::Windows::UI::Xaml::Input::KeyRoutedEventArgs& args)
    {
        if (args.Key() != winrt::Windows::System::VirtualKey::Enter)
        {
            return;
        }
        const auto coreWindow = winrt::Windows::UI::Core::CoreWindow::GetForCurrentThread();
        const auto shift = coreWindow ?
                               coreWindow.GetKeyState(winrt::Windows::System::VirtualKey::Shift) :
                               winrt::Windows::UI::Core::CoreVirtualKeyStates::None;
        if (WI_IsFlagSet(shift, winrt::Windows::UI::Core::CoreVirtualKeyStates::Down))
        {
            return;
        }
        args.Handled(true);
        _submitNativeChatComposer();
    }

    void AgentPaneContent::_renderNativeChatMessage(const Json::Value& message)
    {
        if (!message.isObject())
        {
            return;
        }
        const auto kind = message["kind"].asString();
        auto card = Border{};
        card.Padding(Thickness{ 10, 8, 10, 8 });
        card.CornerRadius(winrt::Windows::UI::Xaml::CornerRadius{ 6.0 });
        card.MaxWidth(760);
        card.HorizontalAlignment(kind == "user" ? HorizontalAlignment::Right :
                                                  HorizontalAlignment::Stretch);

        const auto brush = [](const Color color) {
            return SolidColorBrush{ color };
        };
        if (kind == "user")
        {
            card.Background(brush(ColorHelper::FromArgb(110, 0, 120, 212)));
        }
        else if (kind == "error")
        {
            card.Background(brush(ColorHelper::FromArgb(70, 232, 17, 35)));
        }
        else if (kind == "tool")
        {
            card.Background(brush(ColorHelper::FromArgb(38, 255, 185, 0)));
        }
        else if (kind == "disclaimer" || kind == "system" || kind == "event")
        {
            card.Background(brush(ColorHelper::FromArgb(24, 255, 255, 255)));
        }
        else
        {
            card.Background(brush(ColorHelper::FromArgb(14, 255, 255, 255)));
        }

        if (kind == "plan" && message["entries"].isArray())
        {
            auto panel = StackPanel{};
            panel.Spacing(4);
            auto heading = TextBlock{};
            heading.Text(RS_(L"NativeChatPlanLabel"));
            heading.FontWeight(winrt::Windows::UI::Text::FontWeights::SemiBold());
            panel.Children().Append(heading);
            for (const auto& entry : message["entries"])
            {
                const auto status = entry["status"].asString();
                const auto marker = status == "completed" ? L"\u2713 " :
                                    status == "in_progress" ? L"\u25CF " :
                                                              L"\u25CB ";
                auto line = TextBlock{};
                std::wstring lineText{ marker };
                lineText += winrt::to_hstring(entry["content"].asString());
                line.Text(lineText);
                line.TextWrapping(TextWrapping::WrapWholeWords);
                line.IsTextSelectionEnabled(true);
                panel.Children().Append(line);
            }
            card.Child(panel);
        }
        else
        {
            auto panel = StackPanel{};
            panel.Spacing(3);
            if (kind == "tool")
            {
                auto title = TextBlock{};
                title.Text(winrt::to_hstring(message["title"].asString()));
                title.FontWeight(winrt::Windows::UI::Text::FontWeights::SemiBold());
                title.TextWrapping(TextWrapping::WrapWholeWords);
                panel.Children().Append(title);
                auto status = TextBlock{};
                status.Text(winrt::to_hstring(message["status"].asString()));
                status.Opacity(0.7);
                panel.Children().Append(status);
            }
            else
            {
                auto label = TextBlock{};
                if (kind == "user")
                {
                    label.Text(RS_(L"NativeChatYouLabel"));
                }
                else if (kind == "agent")
                {
                    label.Text(_agentName.empty() ? RS_(L"NativeChatAgentLabel") : _agentName);
                }
                else if (kind == "turn")
                {
                    label.Text(RS_(L"NativeChatCompletedTurnLabel"));
                }
                else if (kind == "error")
                {
                    label.Text(RS_(L"NativeChatErrorLabel"));
                }
                if (!label.Text().empty())
                {
                    label.FontWeight(winrt::Windows::UI::Text::FontWeights::SemiBold());
                    label.FontSize(11);
                    label.Opacity(0.78);
                    panel.Children().Append(label);
                }
                auto body = TextBlock{};
                body.Text(winrt::to_hstring(message["text"].asString()));
                body.TextWrapping(TextWrapping::WrapWholeWords);
                body.IsTextSelectionEnabled(true);
                panel.Children().Append(body);
                if (message.isMember("trailing_marker") && message["trailing_marker"].isString())
                {
                    auto marker = TextBlock{};
                    marker.Text(winrt::to_hstring(message["trailing_marker"].asString()));
                    marker.Opacity(0.65);
                    marker.FontStyle(winrt::Windows::UI::Text::FontStyle::Italic);
                    panel.Children().Append(marker);
                }
            }
            card.Child(panel);
        }
        NativeChatMessages().Children().Append(card);
    }

    void AgentPaneContent::ApplyNativeChatSnapshot(const winrt::hstring& eventJson)
    {
        Json::Value event;
        Json::CharReaderBuilder reader;
        std::string errors;
        std::istringstream stream{ winrt::to_string(eventJson) };
        if (!Json::parseFromStream(reader, stream, &event, &errors))
        {
            return;
        }
        const auto& params = event["params"];
        if (!params.isObject() || params["protocol_version"].asUInt() != 1)
        {
            return;
        }
        const auto sequence = params["sequence"].asUInt64();
        if (sequence == 0 || sequence <= _nativeSnapshotSequence)
        {
            return;
        }
        _nativeSnapshotSequence = sequence;
        _nativeWorkspaceId = winrt::to_hstring(params["workspace_id"].asString());
        _nativeScopeKey = winrt::to_hstring(params["scope_key"].asString());

        const auto nativeChat = params["view"].asString() == "chat" &&
                                params["mode"].asString() == "chat";
        NativeChatRoot().Visibility(nativeChat ? Visibility::Visible : Visibility::Collapsed);
        InnerContent().Visibility(nativeChat ? Visibility::Collapsed : Visibility::Visible);
        if (!nativeChat)
        {
            return;
        }

        const auto connection = params["connection"].asString();
        const auto agent = params["agent"].asString();
        const auto model = params["model"].asString();
        const auto session = params["session_id"].asString();
        const auto cwd = params["cwd"].asString();
        const auto permissionProfile = params["permission_profile"].asString();
        std::wstring status;
        const auto appendStatus = [&status](const std::string_view label, const std::string& value) {
            if (value.empty())
            {
                return;
            }
            if (!status.empty())
            {
                status += L"  \u00B7  ";
            }
            status += winrt::to_hstring(label);
            status += L": ";
            status += winrt::to_hstring(value);
        };
        appendStatus(winrt::to_string(RS_(L"NativeChatStatusConnectionLabel")), connection);
        appendStatus(winrt::to_string(RS_(L"NativeChatStatusAgentLabel")), agent);
        appendStatus(winrt::to_string(RS_(L"NativeChatStatusModelLabel")), model);
        appendStatus(winrt::to_string(RS_(L"NativeChatStatusSessionLabel")), session);
        appendStatus(winrt::to_string(RS_(L"NativeChatStatusCwdLabel")), cwd);
        appendStatus(winrt::to_string(RS_(L"NativeChatStatusPermissionsLabel")), permissionProfile);
        NativeChatStatusText().Text(winrt::hstring{ status });

        _nativeCanSubmit = params["can_submit"].asBool();
        const auto busy = params["busy"].asBool();
        NativeChatComposer().IsEnabled(_nativeCanSubmit);
        NativeChatSendButton().IsEnabled(_nativeCanSubmit);
        NativeChatCancelButton().Visibility(busy ? Visibility::Visible : Visibility::Collapsed);
        NativeChatRetryButton().Visibility(
            connection == "failed" || connection == "disconnected" ?
                Visibility::Visible :
                Visibility::Collapsed);

        NativeChatMessages().Children().Clear();
        if (params["messages"].isArray())
        {
            for (const auto& message : params["messages"])
            {
                _renderNativeChatMessage(message);
            }
        }
        if (params["active_tools"].isArray())
        {
            for (const auto& tool : params["active_tools"])
            {
                Json::Value message{ Json::objectValue };
                message["kind"] = "tool";
                message["title"] = tool["title"];
                message["status"] = tool["status"];
                _renderNativeChatMessage(message);
            }
        }
        if (params["streaming"].isString() && !params["streaming"].asString().empty())
        {
            Json::Value streaming{ Json::objectValue };
            streaming["kind"] = "agent";
            streaming["text"] = params["streaming"];
            _renderNativeChatMessage(streaming);
        }

        NativePermissionButtons().Children().Clear();
        const auto& permission = params["permission"];
        const auto hasPermission = permission.isObject() &&
                                   permission["options"].isArray() &&
                                   !permission["options"].empty();
        NativePermissionCard().Visibility(
            hasPermission ? Visibility::Visible : Visibility::Collapsed);
        if (hasPermission)
        {
            NativePermissionDescription().Text(
                winrt::to_hstring(permission["description"].asString()));
            for (const auto& option : permission["options"])
            {
                auto button = Button{};
                button.Content(box_value(winrt::to_hstring(option["name"].asString())));
                const auto optionId = option["id"].asString();
                const auto weakThis = get_weak();
                button.Click([weakThis, optionId](auto&&, auto&&) {
                    if (const auto self = weakThis.get())
                    {
                        Json::Value additional{ Json::objectValue };
                        additional["option_id"] = optionId;
                        self->_raiseNativeChatAction("permission", additional);
                    }
                });
                NativePermissionButtons().Children().Append(button);
            }
        }

        NativeChatScroll().ChangeView(
            nullptr,
            box_value(NativeChatScroll().ScrollableHeight())
                .as<winrt::Windows::Foundation::IReference<double>>(),
            nullptr,
            true);
    }

    // Apply the supplied colors to the agent-pane top bar (#348). The vector
    // logo paths bind to the label's foreground, so both take the same tint.
    // The 1px bottom hairline uses the foreground color at ~15% alpha, so it
    // reads as a soft separator (like the original #26FFFFFF) — consistent
    // with the text but not a hard full-white/black line.
    void AgentPaneContent::ApplyThemeColors(const Media::Brush& background,
                                            const Media::Brush& foreground)
    {
        if (const auto barRoot = AgentBarRoot())
        {
            barRoot.Background(background);
            if (const auto fgSolid = foreground.try_as<Media::SolidColorBrush>())
            {
                auto c = fgSolid.Color();
                c.A = 0x26;
                barRoot.BorderBrush(Media::SolidColorBrush{ c });
            }
            else
            {
                barRoot.BorderBrush(foreground);
            }
        }
        if (const auto label = AgentLabelText())
        {
            label.Foreground(foreground);
        }
    }

    void AgentPaneContent::_refreshLabel()
    {
        // Session-management view takes over the bar — the wta TUI below no
        // longer renders its own "Agent sessions" header, so this is where
        // that title lives.
        if (_isSessionsView)
        {
            const auto text = _agentName.empty() ?
                                  std::wstring{ RS_(L"AgentPane_SessionsTitle") } :
                                  RS_fmt(L"AgentPane_SessionsTitleFormat", std::wstring{ _agentName });
            AgentLabelText().Text(winrt::hstring{ text });
            return;
        }

        std::wstring text;
        if (_agentName.empty())
        {
            text = L"";
        }
        else
        {
            text = std::wstring{ _agentName };
            if (!_agentBackend.empty())
            {
                text += L" \u00B7 ";
                text += _agentBackend;
            }
            if (!_agentVersion.empty())
            {
                text += L" ";
                text += _agentVersion;
            }
            else if (!_agentModel.empty())
            {
                text += L" ";
                text += _agentModel;
            }
        }
        AgentLabelText().Text(winrt::hstring{ text });
    }

    void AgentPaneContent::_refreshLogo()
    {
        if (_isSessionsView)
        {
            AgentLogo().Visibility(Visibility::Collapsed);
            return;
        }

        if (_agentName.empty())
        {
            AgentLogo().Visibility(Visibility::Collapsed);
            return;
        }

        const auto logo = _logoForAgent(_agentName);
        CopilotLogo().Visibility(logo == AgentLogoKind::Copilot ? Visibility::Visible : Visibility::Collapsed);
        ClaudeLogo().Visibility(logo == AgentLogoKind::Claude ? Visibility::Visible : Visibility::Collapsed);
        GeminiLogo().Visibility(logo == AgentLogoKind::Gemini ? Visibility::Visible : Visibility::Collapsed);
        CodexLogo().Visibility(logo == AgentLogoKind::Codex ? Visibility::Visible : Visibility::Collapsed);
        OpenCodeLogo().Visibility(logo == AgentLogoKind::OpenCode ? Visibility::Visible : Visibility::Collapsed);
        AgentLogo().Visibility(Visibility::Visible);
    }

#pragma region IPaneContent forwarding
    winrt::Windows::UI::Xaml::FrameworkElement AgentPaneContent::GetRoot()
    {
        return *this;
    }

    void AgentPaneContent::UpdateSettings(const CascadiaSettings& settings)
    {
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            impl->UpdateSettings(settings);
        }
        // Re-pick up the pane position in case settings changed it.
        SetAgentPanePosition(settings.GlobalSettings().AgentPanePosition());
    }

    winrt::Windows::Foundation::Size AgentPaneContent::MinimumSize()
    {
        // Reserve 52px (scope + context header) on top of the inner control's minimum.
        // The bottom bar is window-level chrome now, so it isn't part of
        // this pane's minimum size.
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            const auto inner = impl->MinimumSize();
            return { inner.Width, inner.Height + 52.0f };
        }
        return { 1, 52.0f };
    }

    void AgentPaneContent::Focus(winrt::Windows::UI::Xaml::FocusState reason)
    {
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            impl->Focus(reason);
        }
    }

    void AgentPaneContent::Close()
    {
        _unwireInnerEvents();
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            impl->Close();
        }
    }

    INewContentArgs AgentPaneContent::GetNewTerminalArgs(BuildStartupKind kind) const
    {
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            return impl->GetNewTerminalArgs(kind);
        }
        return nullptr;
    }

    winrt::hstring AgentPaneContent::Title()
    {
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            return impl->Title();
        }
        return L"Agent";
    }

    uint64_t AgentPaneContent::TaskbarState()
    {
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            return impl->TaskbarState();
        }
        return 0;
    }

    uint64_t AgentPaneContent::TaskbarProgress()
    {
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            return impl->TaskbarProgress();
        }
        return 0;
    }

    bool AgentPaneContent::ReadOnly()
    {
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            return impl->ReadOnly();
        }
        return false;
    }

    winrt::hstring AgentPaneContent::Icon() const
    {
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            return impl->Icon();
        }
        return {};
    }

    Windows::Foundation::IReference<winrt::Windows::UI::Color> AgentPaneContent::TabColor() const noexcept
    {
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            return impl->TabColor();
        }
        return nullptr;
    }

    winrt::Windows::UI::Xaml::Media::Brush AgentPaneContent::BackgroundBrush()
    {
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            return impl->BackgroundBrush();
        }
        return nullptr;
    }
#pragma endregion

#pragma region ISnappable
    float AgentPaneContent::SnapDownToGrid(const TerminalApp::PaneSnapDirection direction, const float sizeToSnap)
    {
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            // Snapping is computed against the terminal grid; account for the
            // 36px we steal off the top before delegating, then add it back.
            if (direction == TerminalApp::PaneSnapDirection::Height)
            {
                const auto adjusted = std::max(0.0f, sizeToSnap - 52.0f);
                return impl->SnapDownToGrid(direction, adjusted) + 52.0f;
            }
            return impl->SnapDownToGrid(direction, sizeToSnap);
        }
        return sizeToSnap;
    }

    Windows::Foundation::Size AgentPaneContent::GridUnitSize()
    {
        if (const auto& impl = winrt::get_self<implementation::TerminalPaneContent>(_inner))
        {
            return impl->GridUnitSize();
        }
        return { 1, 1 };
    }
#pragma endregion

#pragma region inner event forwarding
    void AgentPaneContent::_wireInnerEvents()
    {
        if (!_inner)
        {
            return;
        }

        // Forward each inner IPaneContent event up to our own subscribers so
        // Tab / TerminalPage can stay agnostic to the wrapper.
        const auto self = get_strong();

        _innerCloseRequested = _inner.CloseRequested(
            [self](const winrt::TerminalApp::IPaneContent& /*sender*/, const auto& args) {
                self->CloseRequested.raise(*self, args);
            });

        _innerConnectionStateChanged = _inner.ConnectionStateChanged(
            [self](const auto& sender, const auto& args) {
                self->ConnectionStateChanged.raise(sender, args);
            });

        _innerBellRequested = _inner.BellRequested(
            [self](const winrt::TerminalApp::IPaneContent& /*sender*/, const winrt::TerminalApp::BellEventArgs& args) {
                self->BellRequested.raise(*self, args);
            });

        _innerTitleChanged = _inner.TitleChanged(
            [self](const winrt::TerminalApp::IPaneContent& /*sender*/, const auto& args) {
                self->TitleChanged.raise(*self, args);
            });

        _innerTabColorChanged = _inner.TabColorChanged(
            [self](const winrt::TerminalApp::IPaneContent& /*sender*/, const auto& args) {
                self->TabColorChanged.raise(*self, args);
            });

        _innerTaskbarProgressChanged = _inner.TaskbarProgressChanged(
            [self](const winrt::TerminalApp::IPaneContent& /*sender*/, const auto& args) {
                self->TaskbarProgressChanged.raise(*self, args);
            });

        _innerReadOnlyChanged = _inner.ReadOnlyChanged(
            [self](const winrt::TerminalApp::IPaneContent& /*sender*/, const auto& args) {
                self->ReadOnlyChanged.raise(*self, args);
            });

        _innerFocusRequested = _inner.FocusRequested(
            [self](const winrt::TerminalApp::IPaneContent& /*sender*/, const auto& args) {
                self->FocusRequested.raise(*self, args);
            });
    }

    void AgentPaneContent::_unwireInnerEvents()
    {
        if (!_inner)
        {
            return;
        }
        _inner.CloseRequested(_innerCloseRequested);
        _inner.ConnectionStateChanged(_innerConnectionStateChanged);
        _inner.BellRequested(_innerBellRequested);
        _inner.TitleChanged(_innerTitleChanged);
        _inner.TabColorChanged(_innerTabColorChanged);
        _inner.TaskbarProgressChanged(_innerTaskbarProgressChanged);
        _inner.ReadOnlyChanged(_innerReadOnlyChanged);
        _inner.FocusRequested(_innerFocusRequested);
    }
#pragma endregion
}
