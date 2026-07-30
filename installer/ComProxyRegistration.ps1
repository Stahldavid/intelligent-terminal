Set-StrictMode -Version 2

$script:TerminalProtocolInterfaceIds = @(
    '{3D8F4B26-5C7E-4A9B-B1D0-2F5A7C9E1B4D}',
    '{9C7E2A14-3B5D-4F8A-A2C9-1E4F6B8D0A3C}'
)

function Initialize-PerUserComRegistrationNativeMethods {
    if ('IntelligentTerminal.PerUserComRegistration' -as [type]) {
        return
    }

    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32;

namespace IntelligentTerminal
{
    public static class PerUserComRegistration
    {
        private static readonly IntPtr HKEY_CLASSES_ROOT =
            new IntPtr(unchecked((int)0x80000000u));

        [DllImport("advapi32.dll")]
        private static extern int RegOverridePredefKey(IntPtr hKey, IntPtr hNewHKey);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr LoadLibraryW(string lpFileName);

        [DllImport("kernel32.dll", CharSet = CharSet.Ansi, SetLastError = true)]
        private static extern IntPtr GetProcAddress(IntPtr hModule, string lpProcName);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool FreeLibrary(IntPtr hModule);

        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        private delegate int RegistrationEntryPoint();

        public static int Invoke(string proxyPath, string entryPoint)
        {
            if (String.IsNullOrWhiteSpace(proxyPath))
            {
                throw new ArgumentException("A proxy path is required.", "proxyPath");
            }

            using (RegistryKey classes = Registry.CurrentUser.CreateSubKey(
                @"Software\Classes",
                RegistryKeyPermissionCheck.ReadWriteSubTree))
            {
                if (classes == null)
                {
                    throw new InvalidOperationException(
                        @"Could not open HKCU\Software\Classes for per-user COM registration.");
                }

                int overrideResult = RegOverridePredefKey(
                    HKEY_CLASSES_ROOT,
                    classes.Handle.DangerousGetHandle());
                if (overrideResult != 0)
                {
                    throw new Win32Exception(
                        overrideResult,
                        "RegOverridePredefKey(HKEY_CLASSES_ROOT) failed.");
                }

                IntPtr module = IntPtr.Zero;
                try
                {
                    module = LoadLibraryW(proxyPath);
                    if (module == IntPtr.Zero)
                    {
                        throw new Win32Exception(
                            Marshal.GetLastWin32Error(),
                            "Loading the COM proxy failed.");
                    }

                    IntPtr procedure = GetProcAddress(module, entryPoint);
                    if (procedure == IntPtr.Zero)
                    {
                        throw new Win32Exception(
                            Marshal.GetLastWin32Error(),
                            "The COM proxy does not export " + entryPoint + ".");
                    }

                    RegistrationEntryPoint registration =
                        (RegistrationEntryPoint)Marshal.GetDelegateForFunctionPointer(
                            procedure,
                            typeof(RegistrationEntryPoint));
                    int result = registration();
                    if (result < 0)
                    {
                        Marshal.ThrowExceptionForHR(result);
                    }

                    return result;
                }
                finally
                {
                    if (module != IntPtr.Zero)
                    {
                        FreeLibrary(module);
                    }

                    int restoreResult = RegOverridePredefKey(HKEY_CLASSES_ROOT, IntPtr.Zero);
                    if (restoreResult != 0)
                    {
                        throw new Win32Exception(
                            restoreResult,
                            "Restoring HKEY_CLASSES_ROOT after COM registration failed.");
                    }
                }
            }
        }
    }
}
'@
}

function Get-PerUserComProxyRegistration {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$InterfaceIds
    )

    $registrations = @()
    foreach ($interfaceId in $InterfaceIds) {
        $interfaceKeyPath = "Software\Classes\Interface\$interfaceId"
        $interfaceKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($interfaceKeyPath)
        if ($null -eq $interfaceKey) {
            $registrations += [pscustomobject]@{
                InterfaceId = $interfaceId
                Registered = $false
                ProxyClsid = $null
                ProxyPath = $null
            }
            continue
        }

        try {
            $proxyKey = $interfaceKey.OpenSubKey('ProxyStubClsid32')
            try {
                $proxyClsid = if ($null -ne $proxyKey) {
                    [string]$proxyKey.GetValue($null, $null)
                } else {
                    $null
                }
            }
            finally {
                if ($null -ne $proxyKey) {
                    $proxyKey.Dispose()
                }
            }
        }
        finally {
            $interfaceKey.Dispose()
        }

        $proxyPath = $null
        if (-not [string]::IsNullOrWhiteSpace($proxyClsid)) {
            $serverKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey(
                "Software\Classes\CLSID\$proxyClsid\InprocServer32")
            try {
                if ($null -ne $serverKey) {
                    $proxyPath = [string]$serverKey.GetValue($null, $null)
                }
            }
            finally {
                if ($null -ne $serverKey) {
                    $serverKey.Dispose()
                }
            }
        }

        $registrations += [pscustomobject]@{
            InterfaceId = $interfaceId
            Registered = -not [string]::IsNullOrWhiteSpace($proxyClsid)
            ProxyClsid = $proxyClsid
            ProxyPath = $proxyPath
        }
    }

    return @($registrations)
}

function Register-PerUserComProxy {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [string]$ProxyPath,

        [string[]]$InterfaceIds = $script:TerminalProtocolInterfaceIds
    )

    $resolvedProxyPath = (Resolve-Path -LiteralPath $ProxyPath -ErrorAction Stop).Path
    Initialize-PerUserComRegistrationNativeMethods
    try {
        [void][IntelligentTerminal.PerUserComRegistration]::Invoke(
            $resolvedProxyPath,
            'DllRegisterServer')
    }
    catch {
        throw "Per-user COM proxy registration failed for '$resolvedProxyPath': $($_.Exception.Message)"
    }

    $registrations = @(Get-PerUserComProxyRegistration -InterfaceIds $InterfaceIds)
    $invalid = @(
        $registrations | Where-Object {
            -not $_.Registered -or
            [string]::IsNullOrWhiteSpace($_.ProxyPath) -or
            -not [System.StringComparer]::OrdinalIgnoreCase.Equals(
                [System.IO.Path]::GetFullPath($_.ProxyPath),
                [System.IO.Path]::GetFullPath($resolvedProxyPath))
        }
    )
    if ($invalid.Count -gt 0) {
        $summary = ($invalid | ForEach-Object {
            "$($_.InterfaceId) => '$($_.ProxyPath)'"
        }) -join '; '
        throw "Per-user COM proxy verification failed: $summary"
    }

    return $registrations
}

function Unregister-PerUserComProxy {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [string]$ProxyPath,

        [string[]]$InterfaceIds = $script:TerminalProtocolInterfaceIds
    )

    if (-not (Test-Path -LiteralPath $ProxyPath -PathType Leaf)) {
        return
    }

    $resolvedProxyPath = (Resolve-Path -LiteralPath $ProxyPath -ErrorAction Stop).Path
    Initialize-PerUserComRegistrationNativeMethods
    try {
        [void][IntelligentTerminal.PerUserComRegistration]::Invoke(
            $resolvedProxyPath,
            'DllUnregisterServer')
    }
    catch {
        throw "Per-user COM proxy unregistration failed for '$resolvedProxyPath': $($_.Exception.Message)"
    }

    $remaining = @(
        Get-PerUserComProxyRegistration -InterfaceIds $InterfaceIds |
            Where-Object {
                $_.Registered -and
                -not [string]::IsNullOrWhiteSpace($_.ProxyPath) -and
                [System.StringComparer]::OrdinalIgnoreCase.Equals(
                    [System.IO.Path]::GetFullPath($_.ProxyPath),
                    [System.IO.Path]::GetFullPath($resolvedProxyPath))
            }
    )
    if ($remaining.Count -gt 0) {
        throw "Per-user COM proxy unregistration did not remove all Intelligent Terminal interfaces."
    }
}
