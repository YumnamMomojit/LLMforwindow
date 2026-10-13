#!/bin/sh
# SEMDOK_BASE_URL, SEMDOK_API_KEY, SEMDOK_MODEL must be set. -N: no output buffering, --max-time: explicit timeout.
curl -sS --max-time 30  -H "Authorization: Bearer $SEMDOK_API_KEY" "$SEMDOK_BASE_URL/models"; echo
curl -sSN --max-time 600 -H "Authorization: Bearer $SEMDOK_API_KEY" -H 'Content-Type: application/json' \
  "$SEMDOK_BASE_URL/chat/completions" \
  -d "{\"model\":\"$SEMDOK_MODEL\",\"stream\":true,\"max_tokens\":64,\"messages\":[{\"role\":\"user\",\"content\":\"Say hello.\"}]}"
