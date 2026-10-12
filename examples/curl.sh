#!/bin/sh
# HUB_BASE_URL, HUB_API_KEY, HUB_MODEL must be set. -N: no output buffering, --max-time: explicit timeout.
curl -sS --max-time 30  -H "Authorization: Bearer $HUB_API_KEY" "$HUB_BASE_URL/models"; echo
curl -sSN --max-time 600 -H "Authorization: Bearer $HUB_API_KEY" -H 'Content-Type: application/json' \
  "$HUB_BASE_URL/chat/completions" \
  -d "{\"model\":\"$HUB_MODEL\",\"stream\":true,\"max_tokens\":64,\"messages\":[{\"role\":\"user\",\"content\":\"Say hello.\"}]}"
