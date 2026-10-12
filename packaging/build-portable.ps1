# UNTESTED sketch: assemble a portable Local AI Hub folder on Windows.
# Usage: .\build-portable.ps1 -ColibriRelease C:\dl\colibri-v2.0.0-windows-x86_64.zip -ColibriSha256 <hash you verified> -Python C:\dl\python-embed -Out dist
param([Parameter(Mandatory)][string]$ColibriRelease, [Parameter(Mandatory)][string]$ColibriSha256,
      [Parameter(Mandatory)][string]$Python, [string]$Out = "dist")
$ErrorActionPreference = 'Stop'
$h = (Get-FileHash -Algorithm SHA256 $ColibriRelease).Hash
if ($h -ne $ColibriSha256.ToUpper()) { throw "Colibri archive hash mismatch: $h" }
cargo build --release -p hubd
New-Item -ItemType Directory -Force $Out | Out-Null
Copy-Item target\release\hubd.exe $Out
Copy-Item -Recurse -Force scripts $Out\scripts
Copy-Item -Recurse -Force $Python $Out\python
Expand-Archive -Force $ColibriRelease $Out\colibri
Copy-Item LICENSE, NOTICE, docs\UPSTREAM.md $Out
"Built $Out (unsigned). Run: $Out\hubd.exe --help"
