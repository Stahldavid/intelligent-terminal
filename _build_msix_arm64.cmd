@echo off
cd /d "%~dp0"

rem Reuse the canonical bootstrap. It selects the v145-capable VS 18 toolset
rem required by the repository's vcpkg overlay triplets.
if defined MSBUILD if not exist "%MSBUILD%" set "MSBUILD="
if not defined MSBUILD for %%X in (msbuild.exe) do set "MSBUILD=%%~$PATH:X"
if not defined MSBUILD call tools\razzle.cmd
if not defined MSBUILD exit /b 1
rem Keep vcpkg's compiler aligned with MSBuild; otherwise it selects the
rem newest installed VS and can emit libraries that the chosen STL cannot link.
set "VCPKG_VISUAL_STUDIO_PATH=%MSBUILD:\MSBuild\Current\Bin\amd64\MSBuild.exe=%"
if "%VCPKG_VISUAL_STUDIO_PATH%"=="%MSBUILD%" set "VCPKG_VISUAL_STUDIO_PATH=%MSBUILD:\MSBuild\Current\Bin\MSBuild.exe=%"
if "%VCPKG_VISUAL_STUDIO_PATH%"=="%MSBUILD%" (
    echo Could not derive the Visual Studio installation from MSBuild: %MSBUILD%
    exit /b 1
)
rem See the x64 driver: CI builders may carry a pinned standalone vcpkg with
rem newer Visual Studio/toolset discovery than the copy bundled with VS.
set "VCPKG_ROOT_ARG="
if defined VCPKG_ROOT (
    if not exist "%VCPKG_ROOT%\vcpkg.exe" (
        echo VCPKG_ROOT does not contain vcpkg.exe: %VCPKG_ROOT%
        exit /b 1
    )
    set "VCPKG_ROOT_ARG=/p:VcpkgRoot=%VCPKG_ROOT%\"
)

set SOLUTION_DIR=%CD%\
rem Keep the project graph and compiler serial. Parallel ARM64 package builds
rem can race while resolving freshly linked WinMD implementation DLLs
rem (MSB3272: file in use), and they also multiply the large C++/WinRT PCH
rem memory footprint.
set COMMON=/p:Platform=ARM64 /p:Configuration=Release /p:WindowsTerminalBranding=Dev /p:SolutionDir=%SOLUTION_DIR% %VCPKG_ROOT_ARG% /p:MultiProcessorCompilation=false /p:CL_MPCount=1 /m:1 /nodeReuse:false /nologo

rem Wipe ARM64 Release intermediates so glob Content items (wt-agent-hooks\**)
rem get re-evaluated; otherwise an incremental MSIX build silently drops
rem freshly-added files. See _build_msix_x64.cmd for the long-form note.
if exist "src\cascadia\CascadiaPackage\obj\ARM64\Release" rmdir /s /q "src\cascadia\CascadiaPackage\obj\ARM64\Release"
if exist "src\cascadia\CascadiaPackage\bin\ARM64\Release\AppX" rmdir /s /q "src\cascadia\CascadiaPackage\bin\ARM64\Release\AppX"

> _build_msix_arm64.log echo MSBuild=%MSBUILD%
>> _build_msix_arm64.log echo VCPKG_ROOT=%VCPKG_ROOT%
>> _build_msix_arm64.log echo VCPKG_VISUAL_STUDIO_PATH=%VCPKG_VISUAL_STUDIO_PATH%
"%MSBUILD%" src\host\proxy\Host.Proxy.vcxproj %COMMON% >> _build_msix_arm64.log 2>&1
if %ERRORLEVEL% NEQ 0 (
    echo OpenConsoleProxy build failed: %ERRORLEVEL%
    exit /b %ERRORLEVEL%
)

"%MSBUILD%" src\cascadia\TerminalSettingsModel\Microsoft.Terminal.Settings.ModelLib.vcxproj %COMMON% >> _build_msix_arm64.log 2>&1
if %ERRORLEVEL% NEQ 0 (
    echo Settings Model build failed: %ERRORLEVEL%
    exit /b %ERRORLEVEL%
)

"%MSBUILD%" src\cascadia\TerminalSettingsEditor\Microsoft.Terminal.Settings.Editor.vcxproj %COMMON% >> _build_msix_arm64.log 2>&1
if %ERRORLEVEL% NEQ 0 (
    echo Settings Editor build failed: %ERRORLEVEL%
    exit /b %ERRORLEVEL%
)

"%MSBUILD%" src\cascadia\CascadiaPackage\CascadiaPackage.wapproj %COMMON% /p:GenerateAppxPackageOnBuild=true /p:AppxBundle=Never >> _build_msix_arm64.log 2>&1
if errorlevel 1 (
    echo CascadiaPackage build failed.
    exit /b 1
)
echo Exit code: 0
exit /b 0
