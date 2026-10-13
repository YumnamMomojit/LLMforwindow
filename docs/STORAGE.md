# Storage layout

```
%LOCALAPPDATA%\LocalAIHub\        (HUB_DATA_DIR / --data-dir)
  hub.db (+ -wal/-shm)            keys (verifiers), models catalog, settings, request log, workspaces, files, conversations
  secrets\                        DPAPI-protected blobs (dev: 0600 files)
  workspaces\<16-hex id>\...      user files (one folder per workspace); .tmp-uploads cleaned at start
  workspaces.lock                 held by each `hubd workspace`/`backup` command, so they run one at a time (the start-up
                                  clean-up would otherwise delete the upload of a `workspace put` still running elsewhere)
  exports\  cache\  logs\         reserved areas (created by `hubd init`)
<models root, user-chosen>        model weights are referenced in place, never copied or deleted
```
SQLite: WAL, `PRAGMA user_version` migrations (v3 now; v3 adds an explicit `revoked` flag so revocation does not depend on the clock); a newer DB than the app refuses to open. Binary files are on disk, never blobs.
Clock assumption of the v3 upgrade: the first command of the new version that opens a v2 `hub.db` flags every key whose `revoked_at`
is at or before *the system clock at that moment* as revoked, for good. If the clock runs fast then (a dual-boot PC before its time
sync, say), an old key still inside a `key rotate` grace period loses the rest of that period, up to the clock error, even after the
clock is corrected; its replacement key is unaffected. A slow clock instead leaves an ended grace period unflagged, and that key
works only until the clock is right again (its `revoked_at` is then in the past).
If keys are in a rotation grace period, check that the clock is right (Windows: Settings, Time, Sync now) before the first command.
Quota per workspace and a free-space reserve are enforced during streaming writes. Export = zip (`metadata.json` + `files/...`, no models, no secrets).
Database backup = `VACUUM INTO` snapshot with `api_keys`/`request_log` removed. No automatic/cloud/encrypted backup and no secure erase (SSD) in v1.
Removing a model from the catalog never deletes user files. Destructive workspace operations require an explicit `confirmed=true`
(`--yes` on the CLI); `workspace get` and `workspace export` never replace an existing file without `--overwrite`.
Inference keys cannot reach any storage function: the gateway has no such route.
Not built yet: storage UI, configurable roots in a config file, drive/quota settings screen.
