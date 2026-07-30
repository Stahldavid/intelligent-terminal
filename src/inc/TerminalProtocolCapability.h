// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#pragma once

#include <Windows.h>
#include <bcrypt.h>

#include <array>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cwchar>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#pragma comment(lib, "bcrypt.lib")

namespace Microsoft::Terminal::Protocol::Capability
{
    enum class Scope
    {
        Invalid,
        Surface,
        Workspace,
    };

    enum class Operation : uint64_t
    {
        GetCapabilities = 1ull << 0,
        GetActivePane = 1ull << 1,
        ListWindows = 1ull << 2,
        ListTabs = 1ull << 3,
        ListPanes = 1ull << 4,
        ReadPaneOutput = 1ull << 5,
        GetProcessStatus = 1ull << 6,
        GetSessionVariable = 1ull << 7,
        GetSettings = 1ull << 8,
        CreateTab = 1ull << 9,
        SplitPane = 1ull << 10,
        ClosePane = 1ull << 11,
        SendInput = 1ull << 12,
        FocusPane = 1ull << 13,
        SetSessionVariable = 1ull << 14,
        Subscribe = 1ull << 15,
        Unsubscribe = 1ull << 16,
        SendEvent = 1ull << 17,
        CreateSurface = 1ull << 18,
    };

    constexpr uint64_t ToMask(const Operation operation) noexcept
    {
        return static_cast<uint64_t>(operation);
    }

    constexpr uint64_t SurfaceOperations =
        ToMask(Operation::GetCapabilities) |
        ToMask(Operation::GetActivePane) |
        ToMask(Operation::ListWindows) |
        ToMask(Operation::ListTabs) |
        ToMask(Operation::ListPanes) |
        ToMask(Operation::ReadPaneOutput) |
        ToMask(Operation::GetProcessStatus) |
        ToMask(Operation::GetSessionVariable) |
        ToMask(Operation::ClosePane) |
        ToMask(Operation::SendInput) |
        ToMask(Operation::FocusPane) |
        ToMask(Operation::SetSessionVariable) |
        ToMask(Operation::Subscribe) |
        ToMask(Operation::Unsubscribe) |
        ToMask(Operation::SendEvent) |
        // A process running in a surface may create a sibling surface only
        // inside its own pane. TerminalProtocolComServer still requires the
        // signed surface session ID to match the requested target, so this
        // does not grant workspace-wide creation or cross-pane access.
        ToMask(Operation::CreateSurface);

    constexpr uint64_t WorkspaceOperations =
        SurfaceOperations |
        ToMask(Operation::GetSettings) |
        ToMask(Operation::SplitPane);

    struct Claims
    {
        Scope scope{ Scope::Invalid };
        std::wstring subject;
        std::wstring workspaceId;
        std::wstring surfaceId;
        uint64_t operations{ 0 };
        uint64_t expiresAtUnixSeconds{ 0 };
        std::wstring nonce;

        bool Has(const Operation operation) const noexcept
        {
            return (operations & ToMask(operation)) != 0;
        }

        bool IsExpired() const noexcept
        {
            const auto now = static_cast<uint64_t>(
                std::chrono::duration_cast<std::chrono::seconds>(
                    std::chrono::system_clock::now().time_since_epoch())
                    .count());
            return expiresAtUnixSeconds <= now;
        }
    };

    namespace details
    {
        inline uint64_t _nowUnixSeconds() noexcept
        {
            return static_cast<uint64_t>(
                std::chrono::duration_cast<std::chrono::seconds>(
                    std::chrono::system_clock::now().time_since_epoch())
                    .count());
        }

        inline std::wstring _lower(std::wstring value)
        {
            for (auto& ch : value)
            {
                if (ch >= L'A' && ch <= L'Z')
                {
                    ch = static_cast<wchar_t>(ch - L'A' + L'a');
                }
            }
            return value;
        }

        // Capability identities are compared with Terminal Protocol
        // workspace/session identifiers, which accept GUIDs both with and
        // without enclosing braces. Persist one canonical representation in
        // the signed payload and in validated claims so a workspace minted
        // from Tab::StableId ("{guid}") authorizes an event whose tab_id was
        // normalized to "guid" by the COM server.
        inline std::wstring _canonicalIdentifier(std::wstring value)
        {
            value = _lower(std::move(value));
            if (value.size() > 2 && value.front() == L'{' && value.back() == L'}')
            {
                value = value.substr(1, value.size() - 2);
            }
            return value;
        }

        inline std::wstring _guid()
        {
            GUID guid{};
            if (FAILED(CoCreateGuid(&guid)))
            {
                return {};
            }

            wchar_t buffer[40]{};
            if (::StringFromGUID2(guid, buffer, ARRAYSIZE(buffer)) == 0)
            {
                return {};
            }

            std::wstring value{ buffer };
            if (value.size() > 2 && value.front() == L'{' && value.back() == L'}')
            {
                value = value.substr(1, value.size() - 2);
            }
            return _lower(std::move(value));
        }

        inline std::wstring _hex(const std::span<const uint8_t> bytes)
        {
            static constexpr wchar_t alphabet[] = L"0123456789abcdef";
            std::wstring result;
            result.resize(bytes.size() * 2);
            for (size_t i = 0; i < bytes.size(); ++i)
            {
                result[i * 2] = alphabet[bytes[i] >> 4];
                result[i * 2 + 1] = alphabet[bytes[i] & 0x0f];
            }
            return result;
        }

        inline std::optional<std::array<uint8_t, 32>> _hmacSha256(
            const std::wstring_view secret,
            const std::wstring_view payload) noexcept
        {
            BCRYPT_ALG_HANDLE algorithm{};
            BCRYPT_HASH_HANDLE hash{};
            std::vector<uint8_t> hashObject;
            std::array<uint8_t, 32> digest{};

            const auto close = [&]() noexcept {
                if (hash)
                {
                    BCryptDestroyHash(hash);
                }
                if (algorithm)
                {
                    BCryptCloseAlgorithmProvider(algorithm, 0);
                }
            };

            if (!BCRYPT_SUCCESS(BCryptOpenAlgorithmProvider(
                    &algorithm,
                    BCRYPT_SHA256_ALGORITHM,
                    nullptr,
                    BCRYPT_ALG_HANDLE_HMAC_FLAG)))
            {
                close();
                return std::nullopt;
            }

            DWORD objectLength{};
            DWORD resultLength{};
            if (!BCRYPT_SUCCESS(BCryptGetProperty(
                    algorithm,
                    BCRYPT_OBJECT_LENGTH,
                    reinterpret_cast<PUCHAR>(&objectLength),
                    sizeof(objectLength),
                    &resultLength,
                    0)))
            {
                close();
                return std::nullopt;
            }
            hashObject.resize(objectLength);

            if (!BCRYPT_SUCCESS(BCryptCreateHash(
                    algorithm,
                    &hash,
                    hashObject.data(),
                    static_cast<ULONG>(hashObject.size()),
                    reinterpret_cast<PUCHAR>(const_cast<wchar_t*>(secret.data())),
                    static_cast<ULONG>(secret.size() * sizeof(wchar_t)),
                    0)) ||
                !BCRYPT_SUCCESS(BCryptHashData(
                    hash,
                    reinterpret_cast<PUCHAR>(const_cast<wchar_t*>(payload.data())),
                    static_cast<ULONG>(payload.size() * sizeof(wchar_t)),
                    0)) ||
                !BCRYPT_SUCCESS(BCryptFinishHash(
                    hash,
                    digest.data(),
                    static_cast<ULONG>(digest.size()),
                    0)))
            {
                close();
                return std::nullopt;
            }

            close();
            return digest;
        }

        inline std::vector<std::wstring_view> _split(const std::wstring_view value)
        {
            std::vector<std::wstring_view> fields;
            size_t start = 0;
            while (start <= value.size())
            {
                const auto end = value.find(L'|', start);
                fields.emplace_back(value.substr(
                    start,
                    end == std::wstring_view::npos ? value.size() - start : end - start));
                if (end == std::wstring_view::npos)
                {
                    break;
                }
                start = end + 1;
            }
            return fields;
        }

        inline bool _constantTimeEqual(const std::wstring_view left, const std::wstring_view right) noexcept
        {
            if (left.size() != right.size())
            {
                return false;
            }
            wchar_t difference = 0;
            for (size_t i = 0; i < left.size(); ++i)
            {
                difference |= left[i] ^ right[i];
            }
            return difference == 0;
        }

        inline std::optional<uint64_t> _parseUnsigned(const std::wstring_view value, const int base) noexcept
        {
            if (value.empty())
            {
                return std::nullopt;
            }
            const std::wstring text{ value };
            wchar_t* end{};
            errno = 0;
            const auto result = _wcstoui64(text.c_str(), &end, base);
            if (errno == ERANGE || end != text.c_str() + text.size())
            {
                return std::nullopt;
            }
            return static_cast<uint64_t>(result);
        }
    }

    inline std::optional<std::wstring> Mint(
        const std::wstring_view hostSecret,
        const Scope scope,
        const std::wstring_view workspaceId,
        const std::wstring_view surfaceId,
        const uint64_t operations,
        const std::chrono::seconds lifetime = std::chrono::hours{ 24 * 7 })
    {
        if (hostSecret.empty() || scope == Scope::Invalid || operations == 0 || lifetime.count() <= 0 ||
            workspaceId.find(L'|') != std::wstring_view::npos ||
            surfaceId.find(L'|') != std::wstring_view::npos ||
            (scope == Scope::Surface && surfaceId.empty()) ||
            (scope == Scope::Workspace && workspaceId.empty()))
        {
            return std::nullopt;
        }

        const auto nonce = details::_guid();
        if (nonce.empty())
        {
            return std::nullopt;
        }

        const auto scopeName = scope == Scope::Surface ? L"surface" : L"workspace";
        const auto subject = scope == Scope::Surface ? L"conpty" : L"wta-helper";
        const auto expires = details::_nowUnixSeconds() + static_cast<uint64_t>(lifetime.count());

        wchar_t operationsText[17]{};
        _ui64tow_s(operations, operationsText, ARRAYSIZE(operationsText), 16);

        std::wstring payload = L"itcap1|intelligent-terminal|";
        payload.append(subject)
            .append(L"|")
            .append(scopeName)
            .append(L"|")
            .append(details::_canonicalIdentifier(std::wstring{ workspaceId }))
            .append(L"|")
            .append(details::_canonicalIdentifier(std::wstring{ surfaceId }))
            .append(L"|")
            .append(operationsText)
            .append(L"|")
            .append(std::to_wstring(expires))
            .append(L"|")
            .append(nonce);

        const auto digest = details::_hmacSha256(hostSecret, payload);
        if (!digest)
        {
            return std::nullopt;
        }
        return payload + L"|" + details::_hex(*digest);
    }

    inline std::optional<Claims> Validate(
        const std::wstring_view hostSecret,
        const std::wstring_view token,
        const uint64_t nowUnixSeconds = details::_nowUnixSeconds()) noexcept
    {
        try
        {
            const auto fields = details::_split(token);
            if (fields.size() != 10 ||
                fields[0] != L"itcap1" ||
                fields[1] != L"intelligent-terminal")
            {
                return std::nullopt;
            }

            const auto separator = token.rfind(L'|');
            if (separator == std::wstring_view::npos)
            {
                return std::nullopt;
            }
            const auto payload = token.substr(0, separator);
            const auto expected = details::_hmacSha256(hostSecret, payload);
            if (!expected ||
                !details::_constantTimeEqual(details::_hex(*expected), details::_lower(std::wstring{ fields[9] })))
            {
                return std::nullopt;
            }

            Claims claims;
            claims.subject = fields[2];
            if (fields[3] == L"surface")
            {
                claims.scope = Scope::Surface;
            }
            else if (fields[3] == L"workspace")
            {
                claims.scope = Scope::Workspace;
            }
            else
            {
                return std::nullopt;
            }

            // Canonicalize after MAC validation as well so capabilities
            // minted by older builds (whose signed workspace field retained
            // GUID braces) remain valid and compare consistently.
            claims.workspaceId = details::_canonicalIdentifier(std::wstring{ fields[4] });
            claims.surfaceId = details::_canonicalIdentifier(std::wstring{ fields[5] });
            const auto operations = details::_parseUnsigned(fields[6], 16);
            const auto expires = details::_parseUnsigned(fields[7], 10);
            if (!operations || !expires || *operations == 0 || *expires <= nowUnixSeconds)
            {
                return std::nullopt;
            }
            claims.operations = *operations;
            claims.expiresAtUnixSeconds = *expires;
            claims.nonce = details::_lower(std::wstring{ fields[8] });

            if (claims.nonce.empty() ||
                (claims.scope == Scope::Surface && claims.surfaceId.empty()) ||
                (claims.scope == Scope::Workspace && claims.workspaceId.empty()))
            {
                return std::nullopt;
            }
            return claims;
        }
        catch (...)
        {
            return std::nullopt;
        }
    }
}
