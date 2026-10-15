# Real-Windows end-to-end check of the shipped Semdok Edge bundle (PowerShell 7, run by CI on
# windows-latest). It follows packaging/README-FIRST.txt with the official Colibri v2.0.0 Windows
# release and upstream's random-weight MiMo test model (its replies are noise: this checks the
# plumbing, not answer quality), from a folder whose path has a space and a non-ASCII letter.
#   pwsh -File scripts/windows_e2e.ps1 -BundleZip <SemdokEdge-*-windows-x64.zip> `
#        -ColibriZip <colibri-v2.0.0-windows-x86_64.zip> -ColibriSrc <colibri v2.0.0 checkout>
param(
    [Parameter(Mandatory)][string]$BundleZip,
    [Parameter(Mandatory)][string]$ColibriZip,
    [Parameter(Mandatory)][string]$ColibriSrc,
    [string]$Work = (Join-Path ([IO.Path]::GetTempPath()) "semdok e2e Jos$([char]0xE9)")
)
$ErrorActionPreference = 'Stop'
$Port = 18787
function Step([string]$m) { Write-Host "`n== $m" }
function Fail([string]$m) {
    Write-Host "FAIL: $m"
    foreach ($f in 'serve.out', 'serve.err') {
        $p = Join-Path $Work $f
        if (Test-Path -LiteralPath $p) { Write-Host "--- $f"; Get-Content -LiteralPath $p -Tail 60 | Write-Host }
    }
    exit 1
}
# Runs semdok.cmd like a user at a Command Prompt; returns its output, fails on a nonzero exit.
function Semdok([string[]]$a, [switch]$MayFail) {
    $out = & $script:Cmd @a 2>&1 | ForEach-Object { "$_" }
    $code = $LASTEXITCODE
    $out | Write-Host
    if (-not $MayFail -and $code -ne 0) { Fail "semdok.cmd $($a -join ' ') exited $code" }
    return ($out -join "`n")
}

Step "unpack the bundle into '$Work'"
if (Test-Path -LiteralPath $Work) { Remove-Item -LiteralPath $Work -Recurse -Force }
New-Item -ItemType Directory -Path $Work | Out-Null
Expand-Archive -LiteralPath $BundleZip -DestinationPath $Work
$Root = (Get-ChildItem -LiteralPath $Work -Directory | Where-Object Name -like 'SemdokEdge-*').FullName
if (-not $Root) { Fail 'no SemdokEdge-* folder in the zip' }
$script:Cmd = Join-Path $Root 'semdok.cmd'
$env:SEMDOK_DATA_DIR = Join-Path $Work 'data'
Remove-Item Env:SEMDOK_API_KEY, Env:HUB_API_KEY, Env:SEMDOK_PYTHON, Env:SEMDOK_COLIBRI_DIR -ErrorAction SilentlyContinue

Step 'semdok.cmd check before Colibri is added (must explain, exit 1)'
$o = Semdok @('check') -MayFail
if ($LASTEXITCODE -eq 0 -or $o -notmatch 'Colibri  : NOT FOUND') { Fail 'check did not report the missing Colibri' }

Step 'add the official Colibri Windows release (README-FIRST step 2)'
Expand-Archive -LiteralPath $ColibriZip -DestinationPath (Join-Path $Root 'colibri')
$o = Semdok @('check')
if ($o -notmatch 'Ready\.') { Fail 'check did not say Ready.' }

Step 'make the tiny test model with upstream tools'
$Tiny = Join-Path $Work 'mimo_tiny'
$Model = Join-Path $Work "model $([char]0xFC)"
python (Join-Path $ColibriSrc 'c\tools\make_mimo_tiny.py') --output $Tiny --force --vision
if ($LASTEXITCODE -ne 0) { Fail 'make_mimo_tiny.py failed' }
$env:MIMO_TINY = $Tiny
$py = "import sys, shutil; sys.path.insert(0, r'$ColibriSrc\c\tests'); import mimo_serve_fixture as f; shutil.copytree(f.served_fixture(), sys.argv[1])"
python -c $py $Model
if ($LASTEXITCODE -ne 0) { Fail 'could not make the served copy of the fixture' }

Step 'init, model validate/add, key create (README-FIRST steps 3-4)'
Semdok @('init') | Out-Null
Semdok @('model', 'validate', $Model) | Out-Null
Semdok @('model', 'add', $Model, '--id', 'fixture') | Out-Null
$o = Semdok @('key', 'create', 'e2e')
$Key = ($o -split "`n" | Where-Object { $_ -match '^sem_' } | Select-Object -First 1).Trim()
if (-not $Key) { Fail 'key create printed no sem_ key' }

Step 'serve (the engine runs under a Job Object owned by semdok-edge.exe)'
$exe = Join-Path $Root 'semdok-edge.exe'
$serve = Start-Process -FilePath $exe -ArgumentList @('serve', '--model', 'fixture', '--port', "$Port") `
    -RedirectStandardOutput (Join-Path $Work 'serve.out') -RedirectStandardError (Join-Path $Work 'serve.err') `
    -NoNewWindow -PassThru
$deadline = (Get-Date).AddSeconds(240)
while ($true) {
    $log = Get-Content -LiteralPath (Join-Path $Work 'serve.out') -Raw -ErrorAction SilentlyContinue
    if ($log -match 'local API ready') { break }
    if ($serve.HasExited) { Fail "serve exited early with code $($serve.ExitCode)" }
    if ((Get-Date) -gt $deadline) { Fail 'serve was not ready after 240 s' }
    Start-Sleep -Seconds 1
}
Write-Host $log

Step 'local API from PowerShell (README-FIRST step 6)'
$base = "http://127.0.0.1:$Port/v1"
$h = @{ Authorization = "Bearer $Key" }
$m = Invoke-RestMethod -Uri "$base/models" -Headers $h -TimeoutSec 30
if ($m.data[0].id -ne 'fixture') { Fail "models listed: $($m | ConvertTo-Json -Compress)" }
$body = @{ model = 'fixture'; max_tokens = 4; messages = @(@{ role = 'user'; content = 'hi' }) } | ConvertTo-Json -Depth 5
$r = Invoke-RestMethod -Method Post -Uri "$base/chat/completions" -Headers $h -ContentType 'application/json' -Body $body -TimeoutSec 600
if ($r.usage.completion_tokens -lt 1) { Fail "no completion tokens: $($r | ConvertTo-Json -Compress -Depth 6)" }
try { Invoke-RestMethod -Uri "$base/models" -TimeoutSec 30 | Out-Null; Fail 'a request without a key was answered' }
catch { if ($_.Exception.Response.StatusCode.value__ -ne 401) { Fail "keyless request: $($_.Exception.Message)" } }

Step 'streaming with curl.exe'
$sbody = Join-Path $Work 'stream.json'
@{ model = 'fixture'; stream = $true; max_tokens = 4; messages = @(@{ role = 'user'; content = 'hi' }) } | ConvertTo-Json -Depth 5 -Compress |
    Set-Content -LiteralPath $sbody -Encoding ascii -NoNewline
$s = curl.exe -sN --max-time 120 -H "Authorization: Bearer $Key" -H 'Content-Type: application/json' --data-binary "@$sbody" "$base/chat/completions"
if (($s -join "`n") -notmatch 'data: \[DONE\]') { Fail "stream had no [DONE]: $s" }

Step 'semdok.cmd chat (README-FIRST step 5)'
$env:SEMDOK_API_KEY = $Key
$o = ("hi`n/exit`n" | & $script:Cmd chat --url $base --max-tokens 4 2>&1 | ForEach-Object { "$_" }) -join "`n"
$code = $LASTEXITCODE
Write-Host $o
if ($code -ne 0) { Fail "chat exited $code" }
Remove-Item Env:SEMDOK_API_KEY
$o = Semdok @('history', 'list')
if ($o -notmatch 'fixture') { Fail 'the chat was not saved in history' }

Step 'workspaces (README-FIRST step 7)'
$src = Join-Path $Work "note $([char]0xE4).txt"
Set-Content -LiteralPath $src -Value 'hello from Windows' -Encoding utf8
Semdok @('workspace', 'create', 'Project A') | Out-Null
Semdok @('workspace', 'put', 'Project A', $src) | Out-Null
$copy = Join-Path $Work 'copy.txt'
Semdok @('workspace', 'get', 'Project A', (Split-Path -Leaf $src), $copy) | Out-Null
if ((Get-Content -LiteralPath $copy -Raw).Trim() -ne 'hello from Windows') { Fail 'workspace get returned different content' }
Semdok @('workspace', 'export', 'Project A', (Join-Path $Work 'project-a.zip')) | Out-Null

Step 'hard-stop semdok-edge.exe: the Job Object must take the engine with it'
$engine = Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($Model) }
Write-Host "engine processes: $($engine | ForEach-Object { "$($_.ProcessId) $($_.Name)" })"
if (-not $engine) { Fail 'no engine process found while serving' }
Stop-Process -Id $serve.Id -Force
Start-Sleep -Seconds 3
$left = Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($Model) }
if ($left) { Fail "engine processes survived: $($left | ForEach-Object { "$($_.ProcessId) $($_.Name)" })" }

Write-Host "`nPASS: Windows end-to-end (bundle, official Colibri release, check, model, keys, API, streaming, chat, history, workspaces, Job Object cleanup)"
