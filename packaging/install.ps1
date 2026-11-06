param(
    [Parameter(Mandatory = $true)][string]$Prefix,
    [string]$Python = "python"
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ($Prefix -notmatch '^(?:[A-Za-z]:[\\/]|\\\\[^\\/]+[\\/][^\\/]+(?:[\\/]|$))') {
    throw "Choose a new absolute installation directory with -Prefix."
}
$Prefix = [System.IO.Path]::GetFullPath($Prefix)
if (Test-Path -LiteralPath $Prefix) {
    throw "Installation directory already exists; choose a new prefix."
}
$null = Get-Command $Python -ErrorAction Stop
$pythonVersion = (& $Python --version 2>&1 | Out-String).Trim()
$versionMatch = [regex]::Match($pythonVersion, '^Python\s+(\d+)\.(\d+)')
if ($LASTEXITCODE -ne 0 -or -not $versionMatch.Success) {
    throw "Python 3.10+ with venv is required for Semgrep."
}
$major = [int]$versionMatch.Groups[1].Value
$minor = [int]$versionMatch.Groups[2].Value
if ($major -lt 3 -or ($major -eq 3 -and $minor -lt 10)) {
    throw "Python 3.10+ with venv is required for Semgrep."
}
foreach ($binary in @("project-check.exe", "project-docs.exe")) {
    if (-not (Test-Path -LiteralPath (Join-Path $PSScriptRoot "bin/$binary") -PathType Leaf)) {
        throw "Run this installer from an extracted native project-check bundle."
    }
}
$null = New-Item -ItemType Directory -Path $Prefix
foreach ($directory in @("bin", "share/project-check", "lib")) {
    $null = New-Item -ItemType Directory -Path (Join-Path $Prefix $directory)
}
# Install fail-closed metadata before any runnable binary can be discovered.
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "share/project-check/tools.json") -Destination (Join-Path $Prefix "share/project-check/tools.json")
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "bin/project-check.exe") -Destination (Join-Path $Prefix "bin")
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "bin/project-docs.exe") -Destination (Join-Path $Prefix "bin")
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "share/project-check/rules") -Destination (Join-Path $Prefix "share/project-check") -Recurse
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "LICENSE") -Destination $Prefix
& $Python -m venv (Join-Path $Prefix "lib/semgrep")
if ($LASTEXITCODE -ne 0) { throw "Could not create the Semgrep environment; Python 3.10+ with venv is required." }
& (Join-Path $Prefix "lib/semgrep/Scripts/python.exe") -m pip install --disable-pip-version-check --only-binary=:all: --require-hashes -r (Join-Path $PSScriptRoot "semgrep-requirements.txt")
if ($LASTEXITCODE -ne 0) { throw "Could not install the pinned Semgrep dependencies." }
$oldVersionCheck = $env:SEMGREP_ENABLE_VERSION_CHECK
$oldMetrics = $env:SEMGREP_SEND_METRICS
try {
    $env:SEMGREP_ENABLE_VERSION_CHECK = "0"
    $env:SEMGREP_SEND_METRICS = "off"
    & (Join-Path $Prefix "lib/semgrep/Scripts/semgrep.exe") --version
    if ($LASTEXITCODE -ne 0) { throw "The Semgrep scanner is unavailable on this platform." }
} finally {
    $env:SEMGREP_ENABLE_VERSION_CHECK = $oldVersionCheck
    $env:SEMGREP_SEND_METRICS = $oldMetrics
}
$tools = '{"version":2,"rules":"share/project-check/rules","scanner":"lib/semgrep/Scripts/semgrep.exe","docs":"bin/project-docs.exe","ca":null}'
[System.IO.File]::WriteAllText((Join-Path $Prefix "share/project-check/tools.json"), $tools, [System.Text.UTF8Encoding]::new($false))
& (Join-Path $Prefix "bin/project-check.exe") --help
if ($LASTEXITCODE -ne 0) { throw "The installed runner could not start." }
Write-Host "Installed project-check in $Prefix"
Write-Host "Add $(Join-Path $Prefix 'bin') to PATH."
Write-Host "Keep this installation at its chosen prefix; rerun the installer into a new prefix to move it."
