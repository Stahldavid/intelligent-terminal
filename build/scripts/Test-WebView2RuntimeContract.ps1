[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$projectPath = Join-Path $repoRoot 'src\cascadia\TerminalApp\dll\TerminalApp.vcxproj'
$installerPath = Join-Path $repoRoot 'build\scripts\New-WtaLocalInstaller.ps1'

[xml]$project = Get-Content -LiteralPath $projectPath -Raw
$namespace = New-Object System.Xml.XmlNamespaceManager($project.NameTable)
$namespace.AddNamespace('msb', 'http://schemas.microsoft.com/developer/msbuild/2003')

$preferences = @($project.SelectNodes('//msb:WebView2LoaderPreference', $namespace))
if ($preferences.Count -ne 1 -or $preferences[0].InnerText.Trim() -ne 'Static') {
    throw 'TerminalApp.vcxproj must set WebView2LoaderPreference=Static exactly once.'
}

$import = $project.SelectSingleNode(
    "//msb:Import[contains(@Project, 'Microsoft.Web.WebView2.targets')]",
    $namespace)
if (-not $import) {
    throw 'TerminalApp.vcxproj does not import the official Microsoft.Web.WebView2 targets.'
}

$preferenceGroup = $preferences[0].ParentNode
$preferencePosition = 0
$importPosition = 0
for ($index = 0; $index -lt $project.Project.ChildNodes.Count; $index++) {
    $node = $project.Project.ChildNodes[$index]
    if ($node -eq $preferenceGroup) {
        $preferencePosition = $index
    }
    if ($node -eq $import) {
        $importPosition = $index
    }
}
if ($preferencePosition -ge $importPosition) {
    throw 'WebView2LoaderPreference must be set before importing the package targets.'
}

$installer = Get-Content -LiteralPath $installerPath -Raw
foreach ($invariant in @(
    'function Assert-PayloadNativeRuntime',
    '/dependents',
    'WebView2Loader\.dll',
    'Assert-PayloadNativeRuntime -PayloadRoot \$payloadRoot'
)) {
    if ($installer -notmatch $invariant) {
        throw "Installer native dependency gate is missing invariant: $invariant"
    }
}

Write-Host '[webview2-runtime-contract] static loader and payload dependency gate verified.'
