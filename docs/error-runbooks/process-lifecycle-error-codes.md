# Process lifecycle error codes (BOOT-04, PROC-02, API-SERVER-01)

> Added 2026-10-05 (audit M3, the last three uncoded error lines that had no
> fitting code). All three are **log-sink only**: none is in a CloudWatch
> metric filter, so none pages on its own. Each one is already noticed by
> something else (the start watchdog, the liveness alarm, `/health` going
> dark); the code makes the log line findable and triageable. Adding a page
> for any of them needs a dated row in the Dhan noise lock first.

## BOOT-04 — the frame log could not be opened at boot

**What it means.** At boot the app opens the durable frame log (the WAL that
keeps every received frame before it is parsed). Opening it failed, so boot
**halts on purpose**: running without it would let frames be lost silently
when the pipeline backs up. The line carries `source = "ws_frame_spill_init"`,
the error and the directory.

**Severity:** High. The triage rule escalates it; it is never auto-actioned.

**What to do.**
1. Read the error on the line: it is almost always free disk space or folder
   permissions on the WAL directory it names.
2. `df -h` on the data volume; free space if it is full (never delete WAL
   segments that have no verified copy in the cold bucket).
3. Check the folder exists and is writable by the service user.
4. Restart the service. Boot continues only once the log opens.

## PROC-02 — the process panicked

**What it means.** The panic hook logged where the process panicked
(`panic_location`), the message (`panic_payload`) and how the last WAL drain
went (`wal_drain`), then handed over to the default panic handler. The process
exits (`panic = "abort"` in release) and systemd restarts it. The line carries
`source = "panic_hook"`.

**Severity:** High. The triage rule escalates it; it is never auto-actioned.

**What to do.**
1. Find the line in `errors.jsonl` / CloudWatch Logs; the location names the
   file and line that panicked.
2. Check that the restart came up (`make doctor`, the boot heartbeat).
3. A panic is a bug: open a fix with a regression test that reproduces it.
4. If `wal_drain` reports frames not written, they are re-read from the WAL at
   the next boot; confirm the replay counters after the restart.

## API-SERVER-01 — the HTTP API server stopped

**What it means.** The axum server task returned an error. `/health`,
`/api/feeds`, `/api/quote/*` and the MCP read endpoints stop answering until
the process restarts. The feed, the candles and persistence keep running.
The line carries `source = "axum_serve"` and the error.

**Severity:** High.

**What to do.**
1. Read the error: a closed or exhausted listener socket is the usual cause.
2. Check the port is still bound (`ss -ltnp`) and nothing else took it.
3. Restart the service outside market hours, or during market hours only if
   the operator needs the API (the feed does not depend on it).
