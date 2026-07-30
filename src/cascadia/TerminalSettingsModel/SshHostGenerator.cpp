// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#include "pch.h"

#include "SshHostGenerator.h"
#include "../../inc/DefaultSettings.h"

#include "DynamicProfileUtils.h"
#include <set>

static constexpr std::wstring_view SshHostGeneratorNamespace{ L"Windows.Terminal.SSH" };

static constexpr std::wstring_view PROFILE_TITLE_PREFIX = L"SSH - ";
static constexpr std::wstring_view PROFILE_ICON_PATH = L"\uE977"; // PC1
static constexpr std::wstring_view GENERATOR_ICON_PATH = L"\uE969"; // StorageNetworkWireless

// OpenSSH is installed under System32 when installed via Optional Features
static constexpr std::wstring_view SSH_EXE_PATH1 = L"%SystemRoot%\\System32\\OpenSSH\\ssh.exe";

// OpenSSH (x86/x64) is installed under Program Files when installed via MSI
static constexpr std::wstring_view SSH_EXE_PATH2 = L"%ProgramFiles%\\OpenSSH\\ssh.exe";

// OpenSSH (x86) is installed under Program Files x86 when installed via MSI on x64 machine
static constexpr std::wstring_view SSH_EXE_PATH3 = L"%ProgramFiles(x86)%\\OpenSSH\\ssh.exe";

static constexpr std::wstring_view SSH_SYSTEM_CONFIG_PATH = L"%ProgramData%\\ssh\\ssh_config";
static constexpr std::wstring_view SSH_USER_CONFIG_PATH = L"%UserProfile%\\.ssh\\config";

static constexpr std::wstring_view SSH_CONFIG_HOST_KEY{ L"Host" };
static constexpr std::wstring_view SSH_CONFIG_INCLUDE_KEY{ L"Include" };

using namespace ::Microsoft::Terminal::Settings::Model;
using namespace winrt::Microsoft::Terminal::Settings::Model;

/*static*/ const std::wregex SshHostGenerator::_configKeyValueRegex{ LR"(^\s*(\w+)\s+([^\s]+.*[^\s])\s*$)" };

winrt::hstring _getProfileName(const std::wstring_view& hostName) noexcept
{
    return til::hstring_format(FMT_COMPILE(L"{0}{1}"), PROFILE_TITLE_PREFIX, hostName);
}

winrt::hstring _getProfileCommandLine(const std::wstring_view& sshExePath, const std::wstring_view& hostName) noexcept
{
    return til::hstring_format(FMT_COMPILE(LR"("{0}" {1})"), sshExePath, hostName);
}

/*static*/ bool SshHostGenerator::_tryFindSshExePath(std::wstring& sshExePath) noexcept
{
    try
    {
        for (const auto& path : { SSH_EXE_PATH1, SSH_EXE_PATH2, SSH_EXE_PATH3 })
        {
            if (std::filesystem::exists(wil::ExpandEnvironmentStringsW<std::wstring>(path.data())))
            {
                sshExePath = path;
                return true;
            }
        }
    }
    CATCH_LOG();

    return false;
}

/*static*/ bool SshHostGenerator::_tryParseConfigKeyValue(const std::wstring_view& line, std::wstring& key, std::wstring& value) noexcept
{
    try
    {
        if (!line.empty() && !line.starts_with(L"#"))
        {
            std::wstring input{ line };
            std::wsmatch match;
            if (std::regex_search(input, match, SshHostGenerator::_configKeyValueRegex))
            {
                key = match[1];
                value = match[2];
                return true;
            }
        }
    }
    CATCH_LOG();

    return false;
}

/*static*/ void SshHostGenerator::_getHostNamesFromConfigFile(const std::wstring_view& configPath, std::vector<std::wstring>& hostNames) noexcept
{
    try
    {
        std::set<std::filesystem::path> visited;
        const auto isConcreteAlias = [](const std::wstring_view value) {
            return !value.empty() &&
                   value.size() <= 128 &&
                   std::all_of(value.begin(), value.end(), [](const wchar_t ch) {
                       return (ch >= L'a' && ch <= L'z') ||
                              (ch >= L'A' && ch <= L'Z') ||
                              (ch >= L'0' && ch <= L'9') ||
                              ch == L'.' || ch == L'_' || ch == L'-';
                   });
        };
        const auto expandInclude = [](std::filesystem::path path,
                                      const std::filesystem::path& parent) {
            auto text = path.wstring();
            if (text.starts_with(L"~/") || text.starts_with(L"~\\"))
            {
                text = wil::ExpandEnvironmentStringsW<std::wstring>(
                    (std::wstring{ L"%UserProfile%\\" } + text.substr(2)).c_str());
                path = text;
            }
            else
            {
                path = wil::ExpandEnvironmentStringsW<std::wstring>(text.c_str());
            }
            if (path.is_relative())
            {
                path = parent / path;
            }
            return path;
        };

        std::function<void(std::filesystem::path, uint32_t)> parse;
        parse = [&](std::filesystem::path resolvedConfigPath, const uint32_t depth) {
            if (depth > 8)
            {
                return;
            }
            std::error_code ec;
            resolvedConfigPath = std::filesystem::weakly_canonical(resolvedConfigPath, ec);
            if (ec || !std::filesystem::is_regular_file(resolvedConfigPath, ec) ||
                !visited.emplace(resolvedConfigPath).second)
            {
                return;
            }
            std::wifstream inputStream(resolvedConfigPath);
            std::wstring line;
            std::wstring key;
            std::wstring value;
            while (std::getline(inputStream, line))
            {
                if (!_tryParseConfigKeyValue(line, key, value))
                {
                    continue;
                }
                if (til::equals_insensitive_ascii(key, SSH_CONFIG_HOST_KEY))
                {
                    std::wistringstream aliases{ value };
                    std::wstring alias;
                    while (aliases >> alias)
                    {
                        if (isConcreteAlias(alias) &&
                            std::find(hostNames.begin(), hostNames.end(), alias) == hostNames.end())
                        {
                            hostNames.emplace_back(std::move(alias));
                        }
                    }
                }
                else if (til::equals_insensitive_ascii(key, SSH_CONFIG_INCLUDE_KEY))
                {
                    std::wistringstream includes{ value };
                    std::wstring token;
                    while (includes >> token)
                    {
                        const auto pattern = expandInclude(
                            token,
                            resolvedConfigPath.parent_path());
                        WIN32_FIND_DATAW data{};
                        wil::unique_hfind handle{ FindFirstFileW(pattern.c_str(), &data) };
                        if (handle)
                        {
                            do
                            {
                                if ((data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) == 0)
                                {
                                    parse(pattern.parent_path() / data.cFileName, depth + 1);
                                }
                            } while (FindNextFileW(handle.get(), &data));
                        }
                        else if (std::filesystem::is_regular_file(pattern, ec))
                        {
                            parse(pattern, depth + 1);
                        }
                    }
                }
            }
        };

        parse(
            std::filesystem::path{
                wil::ExpandEnvironmentStringsW<std::wstring>(configPath.data()) },
            0);
    }
    CATCH_LOG();
}

static bool _isResolvedOpenSshAlias(
    const std::wstring_view sshExePath,
    const std::wstring_view alias) noexcept
{
    try
    {
        // Alias is restricted by _getHostNamesFromConfigFile to
        // [A-Za-z0-9._-], so this diagnostic command has no option/shell
        // injection surface. `ssh -G` remains the canonical evaluator for
        // Include, Match, precedence, ProxyJump and ProxyCommand.
        const auto expandedExe =
            wil::ExpandEnvironmentStringsW<std::wstring>(sshExePath.data());
        const auto command = fmt::format(
            FMT_COMPILE(LR"("{}" -G -- {} 2>NUL)"),
            expandedExe,
            alias);
        auto pipe = _wpopen(command.c_str(), L"rt");
        if (!pipe)
        {
            return false;
        }
        const auto closePipe = wil::scope_exit([&]() noexcept {
            _pclose(pipe);
        });
        wchar_t buffer[1024];
        bool hasHostName = false;
        while (fgetws(buffer, ARRAYSIZE(buffer), pipe))
        {
            const std::wstring_view line{ buffer };
            if (line.starts_with(L"hostname ") && line.size() > 10)
            {
                hasHostName = true;
            }
        }
        return hasHostName;
    }
    CATCH_LOG();
    return false;
}

std::wstring_view SshHostGenerator::GetNamespace() const noexcept
{
    return SshHostGeneratorNamespace;
}

std::wstring_view SshHostGenerator::GetDisplayName() const noexcept
{
    return RS_(L"SshHostGeneratorDisplayName");
}

std::wstring_view SshHostGenerator::GetIcon() const noexcept
{
    return GENERATOR_ICON_PATH;
}

// Method Description:
// - Generate a list of profiles for each detected OpenSSH host.
// Arguments:
// - <none>
// Return Value:
// - <A list of SSH host profiles.>
void SshHostGenerator::GenerateProfiles(std::vector<winrt::com_ptr<implementation::Profile>>& profiles) const
{
    std::wstring sshExePath;
    if (_tryFindSshExePath(sshExePath))
    {
        std::vector<std::wstring> hostNames;

        _getHostNamesFromConfigFile(SSH_SYSTEM_CONFIG_PATH, hostNames);
        _getHostNamesFromConfigFile(SSH_USER_CONFIG_PATH, hostNames);

        for (const auto& hostName : hostNames)
        {
            if (!_isResolvedOpenSshAlias(sshExePath, hostName))
            {
                continue;
            }
            const auto profile{ CreateDynamicProfile(_getProfileName(hostName)) };

            profile->Commandline(_getProfileCommandLine(sshExePath, hostName));
            profile->Icon(winrt::hstring{ PROFILE_ICON_PATH });

            profiles.emplace_back(profile);
        }
    }
}
