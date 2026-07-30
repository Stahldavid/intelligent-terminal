// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#include "precomp.h"

#include "../../inc/TerminalProtocolCapability.h"
#include "../TerminalProtocol/ProtocolParsing.h"

using namespace WEX::Logging;
using namespace WEX::TestExecution;
using namespace Microsoft::Terminal::Protocol::Capability;

namespace TerminalAppUnitTests
{
    class TerminalProtocolCapabilityTests
    {
        TEST_CLASS(TerminalProtocolCapabilityTests);

        TEST_METHOD(SurfaceClaimsRoundTrip);
        TEST_METHOD(WorkspaceClaimsRoundTrip);
        TEST_METHOD(TamperAndWrongIssuerSecretFailClosed);
        TEST_METHOD(ExpiryIsCheckedOnEveryValidation);
        TEST_METHOD(NonceIsUnique);
        TEST_METHOD(DelimitedIdentityFailsClosed);
        TEST_METHOD(NativeChatSnapshotUsesDirectRoute);
        TEST_METHOD(DirectRoutePreservesSchemaForPageValidation);
        TEST_METHOD(RemoteRelayEventRequiresScopedPayload);
    };

    void TerminalProtocolCapabilityTests::SurfaceClaimsRoundTrip()
    {
        const auto token = Mint(
            L"host-secret",
            Scope::Surface,
            {},
            L"11111111-2222-3333-4444-555555555555",
            SurfaceOperations);
        VERIFY_IS_TRUE(token.has_value());

        const auto claims = Validate(L"host-secret", *token);
        VERIFY_IS_TRUE(claims.has_value());
        VERIFY_IS_TRUE(claims->scope == Scope::Surface);
        VERIFY_ARE_EQUAL(std::wstring{ L"conpty" }, claims->subject);
        VERIFY_ARE_EQUAL(std::wstring{ L"11111111-2222-3333-4444-555555555555" }, claims->surfaceId);
        VERIFY_IS_TRUE(claims->Has(Operation::SendInput));
        VERIFY_IS_TRUE(claims->Has(Operation::CreateSurface));
        VERIFY_IS_FALSE(claims->Has(Operation::CreateTab));
        VERIFY_IS_FALSE(claims->Has(Operation::SplitPane));
    }

    void TerminalProtocolCapabilityTests::WorkspaceClaimsRoundTrip()
    {
        const auto token = Mint(
            L"host-secret",
            Scope::Workspace,
            // Tab::StableId uses the braced GUID spelling. The resulting
            // claims must use the same canonical spelling as the COM event
            // authorization path or every workspace event is rejected.
            L"{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}",
            L"11111111-2222-3333-4444-555555555555",
            WorkspaceOperations);
        VERIFY_IS_TRUE(token.has_value());

        const auto claims = Validate(L"host-secret", *token);
        VERIFY_IS_TRUE(claims.has_value());
        VERIFY_IS_TRUE(claims->scope == Scope::Workspace);
        VERIFY_ARE_EQUAL(std::wstring{ L"wta-helper" }, claims->subject);
        VERIFY_ARE_EQUAL(std::wstring{ L"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee" }, claims->workspaceId);
        VERIFY_IS_TRUE(claims->Has(Operation::SplitPane));
        VERIFY_IS_TRUE(claims->Has(Operation::CreateSurface));
        VERIFY_IS_TRUE(claims->Has(Operation::Subscribe));
        VERIFY_IS_FALSE(claims->Has(Operation::CreateTab));
    }

    void TerminalProtocolCapabilityTests::TamperAndWrongIssuerSecretFailClosed()
    {
        const auto token = Mint(
            L"host-secret",
            Scope::Surface,
            {},
            L"11111111-2222-3333-4444-555555555555",
            SurfaceOperations);
        VERIFY_IS_TRUE(token.has_value());

        auto tampered = *token;
        tampered[tampered.size() - 1] = tampered.back() == L'0' ? L'1' : L'0';
        VERIFY_IS_FALSE(Validate(L"host-secret", tampered).has_value());
        VERIFY_IS_FALSE(Validate(L"different-secret", *token).has_value());
    }

    void TerminalProtocolCapabilityTests::ExpiryIsCheckedOnEveryValidation()
    {
        const auto token = Mint(
            L"host-secret",
            Scope::Surface,
            {},
            L"11111111-2222-3333-4444-555555555555",
            SurfaceOperations,
            std::chrono::seconds{ 30 });
        VERIFY_IS_TRUE(token.has_value());

        const auto claims = Validate(L"host-secret", *token);
        VERIFY_IS_TRUE(claims.has_value());
        VERIFY_IS_FALSE(Validate(L"host-secret", *token, claims->expiresAtUnixSeconds).has_value());
        VERIFY_IS_FALSE(Mint(
                            L"host-secret",
                            Scope::Surface,
                            {},
                            L"11111111-2222-3333-4444-555555555555",
                            SurfaceOperations,
                            std::chrono::seconds{ 0 })
                            .has_value());
    }

    void TerminalProtocolCapabilityTests::NonceIsUnique()
    {
        const auto first = Mint(
            L"host-secret",
            Scope::Surface,
            {},
            L"11111111-2222-3333-4444-555555555555",
            SurfaceOperations);
        const auto second = Mint(
            L"host-secret",
            Scope::Surface,
            {},
            L"11111111-2222-3333-4444-555555555555",
            SurfaceOperations);
        VERIFY_IS_TRUE(first.has_value());
        VERIFY_IS_TRUE(second.has_value());

        const auto firstClaims = Validate(L"host-secret", *first);
        const auto secondClaims = Validate(L"host-secret", *second);
        VERIFY_IS_TRUE(firstClaims.has_value());
        VERIFY_IS_TRUE(secondClaims.has_value());
        VERIFY_ARE_NOT_EQUAL(firstClaims->nonce, secondClaims->nonce);
    }

    void TerminalProtocolCapabilityTests::DelimitedIdentityFailsClosed()
    {
        VERIFY_IS_FALSE(Mint(
                            L"host-secret",
                            Scope::Workspace,
                            L"workspace|forged-field",
                            L"11111111-2222-3333-4444-555555555555",
                            WorkspaceOperations)
                            .has_value());
        VERIFY_IS_FALSE(Mint(
                            L"host-secret",
                            Scope::Surface,
                            {},
                            L"surface|forged-field",
                            SurfaceOperations)
                            .has_value());
    }

    void TerminalProtocolCapabilityTests::NativeChatSnapshotUsesDirectRoute()
    {
        Json::Value parsed;
        const auto route = Microsoft::Terminal::Protocol::Parsing::ClassifySendEvent(
            R"({"type":"event","method":"native_chat_snapshot","params":{"protocol_version":1,"workspace_id":"workspace-1","scope_key":"workspace-1::surface::surface-1","sequence":1}})",
            parsed);

        VERIFY_IS_TRUE(route == Microsoft::Terminal::Protocol::Parsing::SendEventRoute::NativeChatSnapshot);
        VERIFY_ARE_EQUAL(std::string{ "workspace-1" }, parsed["params"]["workspace_id"].asString());
    }

    void TerminalProtocolCapabilityTests::DirectRoutePreservesSchemaForPageValidation()
    {
        Json::Value parsed;
        // The pure classifier intentionally only selects a direct dispatch
        // route. TerminalPage::OnNativeChatSnapshot and
        // AgentPaneContent::ApplyNativeChatSnapshot then validate the params
        // schema and reject this missing-workspace/missing-sequence payload.
        const auto route = Microsoft::Terminal::Protocol::Parsing::ClassifySendEvent(
            R"({"type":"event","method":"native_chat_snapshot","params":{}})",
            parsed);
        VERIFY_IS_TRUE(route == Microsoft::Terminal::Protocol::Parsing::SendEventRoute::NativeChatSnapshot);
        VERIFY_IS_FALSE(parsed["params"].isMember("workspace_id"));
        VERIFY_IS_FALSE(parsed["params"].isMember("sequence"));
    }

    void TerminalProtocolCapabilityTests::RemoteRelayEventRequiresScopedPayload()
    {
        Json::Value parsed;
        const auto valid = Microsoft::Terminal::Protocol::Parsing::ClassifySendEvent(
            R"({"type":"event","method":"remote_relay_event","params":{"workspace_id":"workspace-1","surface_id":"surface-1","kind":"notify","payload":{"title":"Done","body":"Build finished"}}})",
            parsed);
        VERIFY_IS_TRUE(valid == Microsoft::Terminal::Protocol::Parsing::SendEventRoute::RemoteRelayEvent);

        const auto missingScope = Microsoft::Terminal::Protocol::Parsing::ClassifySendEvent(
            R"({"type":"event","method":"remote_relay_event","params":{"kind":"notify","payload":{}}})",
            parsed);
        VERIFY_IS_TRUE(missingScope == Microsoft::Terminal::Protocol::Parsing::SendEventRoute::Invalid);

        const auto nonObjectPayload = Microsoft::Terminal::Protocol::Parsing::ClassifySendEvent(
            R"({"type":"event","method":"remote_relay_event","params":{"workspace_id":"workspace-1","kind":"notify","payload":"bad"}})",
            parsed);
        VERIFY_IS_TRUE(nonObjectPayload == Microsoft::Terminal::Protocol::Parsing::SendEventRoute::Invalid);
    }
}
