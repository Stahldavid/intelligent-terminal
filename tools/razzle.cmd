@echo off

echo Setting up dev environment...

rem Open Console build environment setup
rem Adds msbuild to your path, and adds the open\tools directory as well
rem This recreates what it's like to be an actual windows developer!

rem skip the setup if we're already ready.
if not "%OpenConBuild%" == "" goto :END

rem Add Opencon build scripts to path
set PATH=%PATH%;%~dp0;

rem add some helper envvars - The Opencon root, and also the processor arch, for output paths
set OPENCON_TOOLS=%~dp0
rem The opencon root is at ...\open\tools\, without the last 7 chars ('\tools\')
set OPENCON=%OPENCON_TOOLS:~0,-7%

rem Add nuget to PATH
set PATH=%OPENCON%\dep\nuget;%PATH%

rem Run nuget restore so you can use vswhere
nuget restore %OPENCON%\OpenConsole.slnx -Verbosity quiet
nuget restore %OPENCON%\dep\nuget\packages.config -Verbosity quiet

:FIND_MSBUILD
rem Honor an explicit MSBUILD override. This is both useful for constrained
rem build hosts and consistent with the diagnostic below that asks callers to
rem set MSBUILD when automatic discovery fails.
if not defined MSBUILD goto :SEARCH_PATH_MSBUILD
if exist "%MSBUILD%" goto :FOUND_ENV_MSBUILD
echo Ignoring invalid MSBUILD override: "%MSBUILD%"
set MSBUILD=

:SEARCH_PATH_MSBUILD
rem GH#1313: If msbuild is already on the path, we don't need to look for it.
for %%X in (msbuild.exe) do (set MSBUILD=%%~$PATH:X)
if defined MSBUILD goto :FOUND_PATH_MSBUILD

rem Find vswhere
rem from https://github.com/microsoft/vs-setup-samples/blob/master/tools/vswhere.cmd
for /f "usebackq delims=" %%I in (`dir /b /aD /o-N /s "%~dp0..\packages\vswhere*" 2^>nul`) do (
    for /f "usebackq delims=" %%J in (`where /r "%%I" vswhere.exe 2^>nul`) do (
        set VSWHERE=%%J
    )
)

if not defined VSWHERE (
    echo Could not find vswhere on your machine. Please set the VSWHERE variable to the location of vswhere.exe and run razzle again.
    exit /b 1
)

rem Add path to MSBuild Binaries
rem
rem Prefer VS 18 because the repository's vcpkg overlay triplets require the
rem v145 toolset. The loose ResourceDictionary runtime classes prevent the
rem former SDK 26100 WMC9999 null-ClassFullName failure. Fall back to VS 2022
rem only when VS 18 is unavailable; callers can still select MSBuild explicitly.
rem
rem Keep each range outside the FOR command so its closing parenthesis cannot
rem terminate the block during cmd's first parse. `call` is also required:
rem FOR /F executes its command through cmd /c, whose quote stripping otherwise
rem breaks an executable path such as "C:\Program Files\...\vswhere.exe".
set "VS_VERSION_RANGE=[18.0,19.0)"
for /f "usebackq tokens=*" %%B in (`call "%VSWHERE%" -latest -prerelease -products * -requires Microsoft.Component.MSBuild -version "%VS_VERSION_RANGE%" -find MSBuild\**\Bin\MSBuild.exe 2^>nul`) do (
    set MSBUILD=%%B
)

if not defined MSBUILD (
    set "VS_VERSION_RANGE=[17.0,18.0)"
    for /f "usebackq tokens=*" %%B in (`call "%VSWHERE%" -latest -prerelease -products * -requires Microsoft.Component.MSBuild -version "%VS_VERSION_RANGE%" -find MSBuild\**\Bin\MSBuild.exe 2^>nul`) do (
        set MSBUILD=%%B
    )
)

if not defined MSBUILD (
    echo Could not find MSBuild on your machine. Please set the MSBUILD variable to the location of MSBuild.exe and run razzle again.
    exit /b 1
)

:FOUND_ENV_MSBUILD
echo Using MSBuild at %MSBUILD% from the environment.
goto :FOUND_MSBUILD

:FOUND_PATH_MSBUILD
echo Using MSBuild at %MSBUILD% which was already on the path.
goto :FOUND_MSBUILD

:FOUND_MSBUILD

rem Guard: make sure we actually resolved a real MSBuild.exe. Without this, a
rem chained command like `razzle && bcz` would run bcz with an empty MSBUILD/
rem PLATFORM/CONFIGURATION and fail cryptically with '""' is not recognized.
if exist "%MSBUILD%" goto :MSBUILD_READY
echo Could not find a usable MSBuild.exe ^(resolved: "%MSBUILD%"^).
echo Open a "Developer PowerShell/Command Prompt for VS", or run
echo   Import-Module .\tools\OpenConsole.psm1; Set-MsbuildDevEnvironment
echo in your shell before razzle, then try again.
exit /b 1

:MSBUILD_READY
rem Add MSBuild's own directory to PATH, with a proper ; separator.
set "MSBUILD_BIN=%MSBUILD:MSBuild.exe=%"
set "PATH=%PATH%;%MSBUILD_BIN%"

if "%PROCESSOR_ARCHITECTURE%" == "AMD64" (
    set ARCH=x64
    set PLATFORM=x64
) else (
    set ARCH=x86
    set PLATFORM=Win32
)
set DEFAULT_CONFIGURATION=Debug

rem call .razzlerc - for your generic razzle environment stuff
if exist "%OPENCON_TOOLS%\.razzlerc.cmd" (
    call %OPENCON_TOOLS%\.razzlerc.cmd
)   else (
    (
        echo @echo off
        echo.
        echo rem This is your razzlerc file. It can be used for default dev environment setup.
    ) > %OPENCON_TOOLS%\.razzlerc.cmd
)

rem if there are args, run them. This can be used for additional env. customization,
rem    especially on a per shortcut basis.
:ARGS_LOOP
if (%1) == () goto :POST_ARGS_LOOP
if (%1) == (dbg) (
    set DEFAULT_CONFIGURATION=Debug
    shift
    goto :ARGS_LOOP
)
if (%1) == (rel) (
    set DEFAULT_CONFIGURATION=Release
    shift
    goto :ARGS_LOOP
)
if (%1) == (x86) (
    set ARCH=x86
    set PLATFORM=Win32
    shift
    goto :ARGS_LOOP
)
if exist %1 (
    call %1
) else (
    echo Could not locate "%1"
)
shift
goto :ARGS_LOOP

:POST_ARGS_LOOP
set TAEF=%OPENCON%\packages\Microsoft.Taef.10.100.251104001\build\Binaries\%ARCH%\TE.exe
rem Set this envvar so setup won't repeat itself
set OpenConBuild=true

:END
echo The dev environment is ready to go!
exit /b 0
