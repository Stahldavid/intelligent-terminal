@echo off
cd /d "%~dp0"
set "CERT_PATH=cert\IntelligentTerminalDev.pfx"
for /f "usebackq delims=" %%i in (`powershell -NoProfile -Command "([xml](Get-Content 'src/cascadia/CascadiaPackage/Package-Dev.appxmanifest')).Package.Identity.Version"`) do set "PACKAGE_VERSION=%%i"
for /f "usebackq delims=" %%i in (`powershell -NoProfile -Command "$root = [Environment]::GetFolderPath('ProgramFilesX86'); Get-ChildItem (Join-Path $root 'Windows Kits\10\bin\*\x64\signtool.exe') | Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName"`) do set "SIGNTOOL=%%i"
if not defined PACKAGE_VERSION (
    echo Could not read Package-Dev.appxmanifest version.
    exit /b 1
)
if not exist "%SIGNTOOL%" (
    echo Could not locate signtool.exe in the Windows 10 SDK.
    exit /b 1
)
if not exist "%CERT_PATH%" (
    echo Missing signing credential: "%CERT_PATH%".
    echo Provision or generate the development certificate explicitly before signing.
    exit /b 1
)
"%SIGNTOOL%" sign /fd SHA256 /p "" /f "%CERT_PATH%" "src\cascadia\CascadiaPackage\AppPackages\CascadiaPackage_%PACKAGE_VERSION%_x64_Test\CascadiaPackage_%PACKAGE_VERSION%_x64.msix"
if %ERRORLEVEL% NEQ 0 exit /b %ERRORLEVEL%
"%SIGNTOOL%" sign /fd SHA256 /p "" /f "%CERT_PATH%" "src\cascadia\CascadiaPackage\AppPackages\CascadiaPackage_%PACKAGE_VERSION%_ARM64_Test\CascadiaPackage_%PACKAGE_VERSION%_ARM64.msix"
echo Exit code: %ERRORLEVEL%
