# Windows PowerShell 5.1 / 7. Set $env:HUB_BASE_URL, $env:HUB_API_KEY, $env:HUB_MODEL first.
$base = $env:HUB_BASE_URL.TrimEnd('/')
$h = @{ Authorization = "Bearer $($env:HUB_API_KEY)" }

# 1) models
Invoke-RestMethod -Uri "$base/models" -Headers $h -TimeoutSec 30 | ConvertTo-Json -Depth 5

# 2) chat (non-streaming)
$body = @{ model = $env:HUB_MODEL; max_tokens = 64; messages = @(@{ role = 'user'; content = 'Say hello.' }) } | ConvertTo-Json -Depth 5
Invoke-RestMethod -Method Post -Uri "$base/chat/completions" -Headers $h -ContentType 'application/json' -Body $body -TimeoutSec 600 | ConvertTo-Json -Depth 6

# 3) streaming with curl.exe (-N disables buffering; --max-time is the explicit timeout)
$sbody = @{ model = $env:HUB_MODEL; stream = $true; max_tokens = 64; messages = @(@{ role = 'user'; content = 'Say hello.' }) } | ConvertTo-Json -Depth 5 -Compress
$sbody | Out-File -Encoding ascii -NoNewline "$env:TEMP\hub-body.json"
curl.exe -sN --max-time 600 -H "Authorization: Bearer $($env:HUB_API_KEY)" -H "Content-Type: application/json" --data-binary "@$env:TEMP\hub-body.json" "$base/chat/completions"
