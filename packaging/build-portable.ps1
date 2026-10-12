# UNTESTED sketch (never run on Windows): assemble a portable Local AI Hub folder.
# Usage, from any directory:
#   powershell -ExecutionPolicy Bypass -File packaging\build-portable.ps1 -ColibriRelease C:\dl\colibri-v2.0.0-windows-x86_64.zip `
#     -ColibriSha256 <hash you verified> -Python C:\dl\python-embed -Out dist
# Always rebuilds hubd.exe and stops if the build fails, so an older binary is never shipped. -Out must be a new folder
# or one this script built before; it is emptied first, so a re-run never nests copies or keeps stale files.
# The bundle's hub.cmd runs hubd.exe with the bundled Python and Colibri (HUB_PYTHON / HUB_COLIBRI_DIR).
param([Parameter(Mandatory)][string]$ColibriRelease, [Parameter(Mandatory)][string]$ColibriSha256,
      [Parameter(Mandatory)][string]$Python, [string]$Out = "dist")
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Out = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Out)
$Marker = Join-Path $Out '.local-ai-hub-portable'

$h = (Get-FileHash -Algorithm SHA256 $ColibriRelease).Hash
if ($h -ne $ColibriSha256.ToUpper()) { throw "Colibri archive hash mismatch: $h" }
if (-not (Test-Path -PathType Leaf (Join-Path $Python 'python.exe'))) { throw "no python.exe in $Python" }

# Native commands ignore $ErrorActionPreference: check the exit code explicitly.
cargo build --release -p hubd --manifest-path (Join-Path $Root 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit code $LASTEXITCODE); nothing was packaged" }

if (Test-Path $Out) {
    if (@(Get-ChildItem -Force $Out).Count -gt 0 -and -not (Test-Path $Marker)) {
        throw "$Out is not empty and was not built by this script; choose another -Out folder"
    }
    Remove-Item -Recurse -Force $Out
}
New-Item -ItemType Directory $Out | Out-Null
New-Item -ItemType File $Marker | Out-Null
# The destinations below do not exist yet, so Copy-Item creates them instead of copying into them.
Copy-Item (Join-Path $Root 'target\release\hubd.exe') $Out
Copy-Item -Recurse (Join-Path $Root 'scripts') (Join-Path $Out 'scripts')
Copy-Item -Recurse $Python (Join-Path $Out 'python')
Expand-Archive $ColibriRelease (Join-Path $Out 'colibri')
# The archive may or may not wrap everything in one top-level folder.
$colibri = 'colibri'
if (-not (Test-Path (Join-Path $Out 'colibri\c\openai_server.py'))) {
    $inner = @(Get-ChildItem -Directory (Join-Path $Out 'colibri') |
        Where-Object { Test-Path (Join-Path $_.FullName 'c\openai_server.py') })
    if ($inner.Count -ne 1) { throw "c\openai_server.py not found in $ColibriRelease" }
    $colibri = "colibri\$($inner[0].Name)"
}
Copy-Item (Join-Path $Root 'LICENSE'), (Join-Path $Root 'NOTICE'), (Join-Path $Root 'docs\UPSTREAM.md') $Out
@"
@echo off
rem Runs hubd.exe with the Python and Colibri bundled in this folder; --python / --colibri-dir still override.
setlocal
set "HUB_PYTHON=%~dp0python\python.exe"
set "HUB_COLIBRI_DIR=%~dp0$colibri"
"%~dp0hubd.exe" %*
exit /b %ERRORLEVEL%
"@ -replace "`r?`n", "`r`n" | Set-Content -Encoding Ascii (Join-Path $Out 'hub.cmd')   # cmd.exe wants CRLF
"Built $Out (unsigned). Run: $Out\hub.cmd --help"
