# UNTESTED sketch (never run on Windows): assemble a portable Local AI Hub folder.
# Usage, from any directory:
#   powershell -ExecutionPolicy Bypass -File packaging\build-portable.ps1 -ColibriRelease C:\dl\colibri-v2.0.0-windows-x86_64.zip `
#     -ColibriSha256 <hash you verified> -Python C:\dl\python-embed -Out dist
# Always rebuilds hubd.exe and packages the exact file cargo reports for that build (CARGO_TARGET_DIR, build.target-dir
# and build.target are honoured), so an older binary is never shipped. -Out must be a new or empty folder, or one this
# script built before. The new bundle is assembled next to it and replaces it only once complete: a failed run leaves
# the previous bundle as it was. Every path is taken literally ([ and ] are not wildcards).
# The bundle's hub.cmd runs hubd.exe with the bundled Python and Colibri (HUB_PYTHON / HUB_COLIBRI_DIR).
param([Parameter(Mandatory)][string]$ColibriRelease, [Parameter(Mandatory)][string]$ColibriSha256,
      [Parameter(Mandatory)][string]$Python, [string]$Out = "dist")
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Out = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Out).TrimEnd('\', '/')
$MarkerName = '.local-ai-hub-portable'
$Stage = "$Out.partial"

# A folder this script may empty: missing, empty, or carrying its marker.
function Assert-Ours($dir) {
    if (-not (Test-Path -LiteralPath $dir)) { return }
    if (-not (Test-Path -LiteralPath $dir -PathType Container)) { throw "$dir exists and is not a folder; choose another -Out" }
    if (@(Get-ChildItem -LiteralPath $dir -Force).Count -gt 0 -and
        -not (Test-Path -LiteralPath (Join-Path $dir $MarkerName) -PathType Leaf)) {
        throw "$dir is not empty and was not built by this script; choose another -Out folder"
    }
}
# Everything is checked before the build and before anything is deleted.
Assert-Ours $Out
Assert-Ours $Stage
$h = (Get-FileHash -Algorithm SHA256 -LiteralPath $ColibriRelease).Hash
if ($h -ne $ColibriSha256.ToUpper()) { throw "Colibri archive hash mismatch: $h" }
if (-not (Test-Path -LiteralPath (Join-Path $Python 'python.exe') -PathType Leaf)) { throw "no python.exe in $Python" }

# Native commands ignore $ErrorActionPreference: check the exit code explicitly. The JSON messages on stdout name
# the executable cargo actually produced; diagnostics still go to the console.
$Exe = $null
cargo build --release -p hubd --manifest-path (Join-Path $Root 'Cargo.toml') --message-format=json-render-diagnostics |
    ForEach-Object {
        $m = $null
        try { $m = $_ | ConvertFrom-Json } catch { }
        if ($m -and $m.reason -eq 'compiler-artifact' -and $m.target.name -eq 'hubd' -and
            @($m.target.kind) -contains 'bin' -and $m.executable) { $Exe = $m.executable }
    }
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit code $LASTEXITCODE); nothing was packaged" }
if (-not $Exe -or -not (Test-Path -LiteralPath $Exe -PathType Leaf)) { throw "cargo reported no hubd.exe; nothing was packaged" }
& $Exe --help | Out-Null
if ($LASTEXITCODE -ne 0) { throw "$Exe --help failed (exit code $LASTEXITCODE); nothing was packaged" }

# Assemble in $Stage; $Out is untouched until the bundle is complete.
if (Test-Path -LiteralPath $Stage) { Remove-Item -LiteralPath $Stage -Recurse -Force }
[System.IO.Directory]::CreateDirectory($Stage) | Out-Null
[System.IO.File]::WriteAllText((Join-Path $Stage $MarkerName), '')
# The destinations below do not exist yet, so Copy-Item creates them instead of copying into them.
Copy-Item -LiteralPath $Exe -Destination (Join-Path $Stage 'hubd.exe')
Copy-Item -LiteralPath (Join-Path $Root 'scripts') -Destination (Join-Path $Stage 'scripts') -Recurse
Copy-Item -LiteralPath $Python -Destination (Join-Path $Stage 'python') -Recurse
Expand-Archive -LiteralPath $ColibriRelease -DestinationPath (Join-Path $Stage 'colibri')
# The official release packs openai_server.py flat at the archive root (a source tree keeps it
# in c\); the archive may or may not wrap everything in one top-level folder.
function Test-Colibri($dir) {
    (Test-Path -LiteralPath (Join-Path $dir 'openai_server.py')) -or (Test-Path -LiteralPath (Join-Path $dir 'c\openai_server.py'))
}
$colibri = 'colibri'
if (-not (Test-Colibri (Join-Path $Stage 'colibri'))) {
    $inner = @(Get-ChildItem -LiteralPath (Join-Path $Stage 'colibri') -Directory | Where-Object { Test-Colibri $_.FullName })
    if ($inner.Count -ne 1) { throw "openai_server.py not found in $ColibriRelease" }
    $colibri = "colibri\$($inner[0].Name)"
}
Copy-Item -LiteralPath (Join-Path $Root 'LICENSE'), (Join-Path $Root 'NOTICE'), (Join-Path $Root 'docs\UPSTREAM.md') -Destination $Stage
@"
@echo off
rem Runs hubd.exe with the Python and Colibri bundled in this folder; --python / --colibri-dir still override.
setlocal
set "HUB_PYTHON=%~dp0python\python.exe"
set "HUB_COLIBRI_DIR=%~dp0$colibri"
"%~dp0hubd.exe" %*
exit /b %ERRORLEVEL%
"@ -replace "`r?`n", "`r`n" | Set-Content -Encoding Ascii -LiteralPath (Join-Path $Stage 'hub.cmd')   # cmd.exe wants CRLF

# Swap: only now is the previous bundle removed.
if (Test-Path -LiteralPath $Out) { Remove-Item -LiteralPath $Out -Recurse -Force }
[System.IO.Directory]::Move($Stage, $Out)
"Built $Out from $Exe (unsigned). Run: $(Join-Path $Out 'hub.cmd') --help"
