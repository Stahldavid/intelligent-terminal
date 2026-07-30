@echo off
cd /d "%~dp0"

rem Reuse the canonical bootstrap. It selects the v145-capable VS 18 toolset
rem required by the repository's vcpkg overlay triplets.
if defined MSBUILD if not exist "%MSBUILD%" set "MSBUILD="
if not defined MSBUILD for %%X in (msbuild.exe) do set "MSBUILD=%%~$PATH:X"
if not defined MSBUILD call tools\razzle.cmd
if not defined MSBUILD exit /b 1
rem vcpkg otherwise auto-selects the newest installed Visual Studio, which can
rem produce STL-incompatible .lib files for the MSBuild/toolset selected above.
set "VCPKG_VISUAL_STUDIO_PATH=%MSBUILD:\MSBuild\Current\Bin\amd64\MSBuild.exe=%"
if "%VCPKG_VISUAL_STUDIO_PATH%"=="%MSBUILD%" set "VCPKG_VISUAL_STUDIO_PATH=%MSBUILD:\MSBuild\Current\Bin\MSBuild.exe=%"
if "%VCPKG_VISUAL_STUDIO_PATH%"=="%MSBUILD%" (
    echo Could not derive the Visual Studio installation from MSBuild: %MSBUILD%
    exit /b 1
)
rem A standalone, pinned vcpkg can be newer than the copy bundled with Visual
rem Studio (notably for VS 2026/v145 discovery). When VCPKG_ROOT is supplied,
rem make MSBuild import that exact integration instead of silently falling back
rem to the Visual Studio bundled executable and targets.
set "VCPKG_ROOT_ARG="
if defined VCPKG_ROOT (
    if not exist "%VCPKG_ROOT%\vcpkg.exe" (
        echo VCPKG_ROOT does not contain vcpkg.exe: %VCPKG_ROOT%
        exit /b 1
    )
    set "VCPKG_ROOT_ARG=/p:VcpkgRoot=%VCPKG_ROOT%\"
)

set SOLUTION_DIR=%CD%\
rem This project has several very large C++/WinRT PCHs. Building more than one
rem compiler at a time can exhaust the default Windows page file (C3859/1455)
rem on normal developer workstations, so keep the local packaging path serial.
set COMMON=/p:Platform=x64 /p:Configuration=Release /p:WindowsTerminalBranding=Dev /p:SolutionDir=%SOLUTION_DIR% %VCPKG_ROOT_ARG% /p:MultiProcessorCompilation=false /p:CL_MPCount=1 /m:1 /nodeReuse:false /nologo

rem Wipe the wapproj's Release intermediates so glob-based Content items
rem (like wt-agent-hooks\**) get re-evaluated. Without this, an incremental
rem MSIX build keeps the cached file list and silently drops freshly-added
rem files from the package.
if exist "src\cascadia\CascadiaPackage\obj\x64\Release" rmdir /s /q "src\cascadia\CascadiaPackage\obj\x64\Release"
if exist "src\cascadia\CascadiaPackage\bin\x64\Release\AppX" rmdir /s /q "src\cascadia\CascadiaPackage\bin\x64\Release\AppX"

rem Generate ITerminalHandoff.h and ITerminalProtocol.h before any consumer
rem graph can compile TerminalConnection in parallel.
> _build_msix_x64.log echo MSBuild=%MSBUILD%
>> _build_msix_x64.log echo VCPKG_ROOT=%VCPKG_ROOT%
>> _build_msix_x64.log echo VCPKG_VISUAL_STUDIO_PATH=%VCPKG_VISUAL_STUDIO_PATH%
"%MSBUILD%" src\host\proxy\Host.Proxy.vcxproj %COMMON% >> _build_msix_x64.log 2>&1
if %ERRORLEVEL% NEQ 0 (
    echo OpenConsoleProxy build failed: %ERRORLEVEL%
    exit /b %ERRORLEVEL%
)

rem Build Settings Model first. Its winmd is the source-of-truth for the
rem Profile / Globals WinRT projection. If we don't pin its build ahead
rem of consumer projects, cppwinrt can scan a stale older winmd elsewhere
rem and generate consumer projections missing newer members (e.g.
rem DragDropDelimiter), producing C2039 in TerminalSettingsAppAdapterLib.
"%MSBUILD%" src\cascadia\TerminalSettingsModel\Microsoft.Terminal.Settings.ModelLib.vcxproj %COMMON% >> _build_msix_x64.log 2>&1
if %ERRORLEVEL% NEQ 0 (
    echo Settings Model build failed: %ERRORLEVEL%
    exit /b %ERRORLEVEL%
)

rem Build Settings Editor next (generates XBF files)
"%MSBUILD%" src\cascadia\TerminalSettingsEditor\Microsoft.Terminal.Settings.Editor.vcxproj %COMMON% >> _build_msix_x64.log 2>&1
if %ERRORLEVEL% NEQ 0 (
    echo Settings Editor build failed: %ERRORLEVEL%
    exit /b %ERRORLEVEL%
)

rem Now build the full package
"%MSBUILD%" src\cascadia\CascadiaPackage\CascadiaPackage.wapproj %COMMON% /p:GenerateAppxPackageOnBuild=true /p:AppxBundle=Never >> _build_msix_x64.log 2>&1
if errorlevel 1 (
    echo CascadiaPackage build failed.
    exit /b 1
)
echo Exit code: 0
exit /b 0
