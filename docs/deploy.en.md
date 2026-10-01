[中文](deploy.md) | [English](deploy.en.md)

# WFSearch Deployment Guide

## Build

```powershell
cargo build --release
# Artifacts:
#   target\release\wfs-server.exe
#   target\release\wfs-cli.exe
```

## Development & debugging (console mode)

MFT/USN access requires elevation — **use an elevated terminal**:

```powershell
wfs-server.exe console                 # all fixed drives
wfs-server.exe --drives C,D console    # specific drives
wfs-server.exe --http-port 15200 console
```

REPL: type a query to search; `status` shows index progress; `quit` exits and flushes a
snapshot.

## Production deployment (Windows service)

Registration belongs to the **deployer** (or to the host application embedding the engine):
`wfs-server.exe` has no `install`/`uninstall` subcommand — a service only ever sees the string
in its `binPath`, and this binary should not decide on someone else's behalf where it lands or
how many times it is registered.

```powershell
# Elevated terminal. binPath must be absolute; the trailing `run` is the entry
# subcommand the SCM invokes.
$exe = 'C:\Program Files\WFSearch\wfs-server.exe'
sc.exe create WFSearch binPath= "`"$exe`" run" obj= LocalSystem start= auto DisplayName= "WFSearch File Search Engine"
sc.exe description WFSearch "Fast NTFS filename search engine (MFT index + USN journal). Query via named pipe \\.\pipe\wfs-engine-v1 or http://127.0.0.1:15100."
sc.exe start WFSearch
wfs-cli.exe status                     # verify

sc.exe stop WFSearch                   # stop (saves a snapshot automatically)
sc.exe delete WFSearch                 # remove the registration (stop it first)
```

`obj= LocalSystem` is a hard requirement for MFT access. Update versions with the
"stop → rename → drop in → start" sequence below — do **not** delete and recreate the service.

The service log **appends** to `%ProgramData%\WFSearch\wfs.log` (rotated to `wfs.log.old`
at startup past 16 MB) — a service process has no console, and without a log file you see
nothing at all. For debugging, reproduce in `console` mode first (logs go to stderr), or
set `RUST_LOG=debug`.

## Embedding as a component (host app updates the exe)

The release channel is GitHub Releases: push a `v*` tag and CI attaches both exes, both
`.sha256` files and a `latest.json` manifest. Hosts follow exactly one entry point — it needs
no API token, so it is not subject to the 60 requests/hour anonymous limit that
`/repos/.../releases/latest` imposes:

```
https://github.com/mnigc/WFSearch/releases/latest/download/latest.json
```

```jsonc
{
  "version": "0.2.0", "tag": "v0.2.0", "protocol": 2,
  "released_at": "2026-10-01T00:00:00Z",
  "assets": {
    "wfs-server.exe": { "url": "…/releases/download/v0.2.0/wfs-server.exe",
                        "sha256": "9408…f8b6", "size": 3483136 }
  }
}
```

`url` pins one version (use it if you want a staged rollout); `protocol` is what a host checks
for capabilities (protocol 2 adds `content:` document search; its additions are optional
fields, backward-compatible with v1 clients). The exe is also always reachable as
`releases/latest/download/wfs-server.exe` for whoever wants the newest build.

### Replacing the file: stop → rename → drop in → start

A service's `binPath` is baked in, so **the file name must stay the same** — which makes an
update these four steps:

1. **Check**: `GET latest.json`, compare `version` with what is installed; stop if equal.
2. **Download**: fetch to a temporary file and **verify the SHA-256** before using it. A file
   that arrived over HTTP carries a Zone.Identifier stream; leave it and the first run gets
   SmartScreen'd:
   `Remove-Item -LiteralPath $exe -Stream Zone.Identifier -ErrorAction SilentlyContinue`.
3. **Stop + swap**: `sc stop WFSearch`, then poll `sc query` until STOPPED (for a child
   process: `TerminateProcess` and **wait for it to actually exit** — .NET's `Kill()` is
   asynchronous, and not waiting hits the file lock). Rename the old file to
   `wfs-server.exe.old` — Windows allows renaming a **running** exe, so this finishes even if
   the stop above did not take, and the `.old` doubles as the rollback copy — then put the new
   exe at the original path. `binPath` is unchanged, so there is **no** `sc config` and no
   reinstall.
4. **Start + decide**: `sc start WFSearch`, wait until every volume in `/api/v1/status`
   reaches `phase: ready`. Only then delete `.old`; if the new build fails, rename `.old`
   back and start once more. Snapshots make the restart warm, so success or failure is known
   within 1–2 seconds without reindexing.

Delete a leftover `.old` **on the host's next launch** (a renamed file cannot be removed while
the old process still holds it).

### What the integration relies on

Breaking any of these three invalidates "replace one file":

- **Single exe, zero external DLLs**: never add a native dependency that must ship alongside.
- **Data directory fixed at `%ProgramData%\WFSearch\`**: independent of the exe's path and
  name, so moving the exe never loses the index.
- **Backward-compatible HTTP protocol and snapshot format**: a host may stay on an older exe;
  fields may be added, never redefined.

A host on HTTP must also know: **the token is re-issued at every start**, so do not cache it —
re-read `%ProgramData%\WFSearch\http.token` after (re)launching the engine. Or move to the
named pipe, where the DACL authenticates and there is no credential to manage.

## Real-disk acceptance

Run once in an elevated terminal to get cold start / query latency / change visibility /
warm start / memory, each checked against the README targets:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\acceptance.ps1
```

The script only writes a temporary data dir under `%TEMP%` and starts one console-mode
process — **it installs no service and never touches `%ProgramData%`**. The flow: volume
self-check (`doctor`) → cold start (full MFT build) → 200 rounds × 3 query shapes →
create/delete visibility in `%TEMP%` → warm start after a `quit` snapshot. Output is pure
ASCII, pasteable as-is; the first step includes the `doctor` result, and any later failure
stops the engine first, then prints the engine-log lines matching
`volume/ERROR/WARN/journal/panic` (the engine runs with
`RUST_LOG=info,wfs_server=debug,wfs_fs=debug`, so the log also carries the journal's
records→events counts).

## Volume self-check (doctor)

When an index refuses to build, don't guess — `doctor` runs each step of `run_volume`
separately and prints what the kernel answered:

```powershell
wfs-server.exe doctor C
```

```
volume C:
  open \\.\C:            ok
  FSCTL_QUERY_USN_JOURNAL: ok - id 0x1d95f206cae361d, next_usn 49521681376, lowest_valid 0, max 64 MB, record versions (2, 4)
  ensure USN journal      : ok - id 0x1d95f206cae361d
  FSCTL_ENUM_USN_DATA     : ok - 1433097 records (24-byte input, first entry "System Volume Information")
  FSCTL_READ_USN_JOURNAL  : ok - 0 pending record(s), 0 event(s), 0 unparsable, next_usn 49521681376
  journal history         : usn 49521681376..49521681376 - 0 record(s), 0 event(s), 0 unparsable [0 create, 0 delete, 0 rename]
```

Any failing line prints `FAILED - <reason>` and the process exits with code 1 (script
friendly). If `open` fails with `access denied`, the process is not elevated;
`32-byte input` means the kernel only accepts the versioned `MFT_ENUM_DATA_V1` (possible on
newer Windows; the engine switches automatically); `0 records` means the kernel judged the
enumeration empty on the spot (historically caused by `HighUsn` set to `u64::MAX`, i.e. -1
in the signed USN domain).

The last two lines are the diagnostic for "new files never show up" — note the difference
between `record(s)` and `event(s)`: `journal history` replays from the oldest record the
volume still holds (NTFS keeps only a recent window, typically a few hundred thousand
records here). `records > 0` with `events == 0` means the records the kernel handed over
are not `USN_RECORD_V2` (the indexer cannot read them; the log carries a matching
`not USN_RECORD_V2` warning); `records == 0` means the journal genuinely has nothing to
replay. These two lines are diagnostic only and never affect the exit code.

## Data & configuration

| File | Path | Notes |
|---|---|---|
| Config | `%ProgramData%\WFSearch\config.toml` | all defaults when absent; or `--config <FILE>` |
| Snapshot | `%ProgramData%\WFSearch\index.bin` | index + journal position, written atomically (tmp + rename) |
| Service log | `%ProgramData%\WFSearch\wfs.log` | service mode only; rotated to `.log.old` past 16 MB |
| HTTP token | `%ProgramData%\WFSearch\http.token` | generated at startup; the HTTP credential, its DACL follows `acl` |
| Custom data dir | `data_dir = "D:\\wfsdata"` in config | hosts the snapshot, the log and the token |

First start: a full enumeration per drive (while it runs, `/status` shows `building` plus
progress); a snapshot is written automatically once the build finishes, and every graceful
exit afterwards (service stop / Ctrl-C) refreshes it. On restart, a volume whose journal
has not wrapped recovers in seconds; otherwise it rebuilds automatically.

## Content search (the `[content]` config section)

A `content:` term in `q` triggers a query-time document-content scan (syntax and semantics
in [protocol.en.md](protocol.en.md)). It builds **no index and performs no background IO**:
each query first narrows candidates by name, then reads those files in real time. The
defaults suit interactive use; tighten them for batch or embedded scenarios:

```toml
[content]
enabled = true            # false makes content: queries fail with code 1
max_candidates = 2000     # candidate window: at most this many name matches are scanned
max_file_bytes = 8388608  # per-file cap (bytes); larger files land in skipped_size
timeout_ms = 10000        # per-scan budget; when spent, partial results carry timed_out
max_concurrency = 4       # concurrent readers (deliberately separate from the rayon pool
                          # the name search uses — keep it small)
```

**Security boundary**: content search hands every process that can reach the engine the
extra ability to **read file contents as the service account (SYSTEM)** — a name query
only lists names, a content query opens the files. For local integration (pipe via DACL,
HTTP via token) that is barely more than what the caller could read on its own; but if the
indexed scope holds data the callers cannot otherwise read, combine
`acl = "restricted"` with, where appropriate, `content.enabled = false`. The engine's own
data directory (`config.toml`/`index.bin`/`http.token`) is never scanned.

## Query verification

```powershell
wfs-cli.exe search "*.docx"
wfs-cli.exe search "report C: 2026" --limit 50 --sort name
wfs-cli.exe search "src\core" --match-path         # whole-path matching
wfs-cli.exe search "*.md content:budget" --limit 10 # document-content search (narrow by
                                                    # name first, then a live scan)
wfs-cli.exe search "*.md" --http                     # via the HTTP channel (loads http.token)
wfs-cli.exe search "*.md" --http --token <value>     # when the data dir is not the default
wfs-cli.exe status
```

Access control is the single `acl` key, and it covers **both** channels: `open` (default)
admits anyone signed in at the console, `restricted` admits only SYSTEM + Administrators — in
which case wfs-cli needs an elevated terminal on either transport, and the demo UI is
reachable only by an admin (it cannot read the token). An invalid value falls back to `open`
with a stderr warning.

Demo UI in a browser: `http://127.0.0.1:15100/?token=<Get-Content $env:ProgramData\WFSearch\http.token>`
(an address bar cannot set a header, so `/` accepts `?token=`; the page's own requests use the
header). Without a token you get a page explaining where the token lives, not results.

## Troubleshooting

| Symptom | Cause & handling |
|---|---|
| volume shows `failed` in `status` | Run `wfs-server.exe doctor <drive>` first: it reports which ioctl failed and the Win32 error code. Usual causes: console mode not elevated, drive absent, or the volume's USN journal being unusable |
| New files never show up | Within 1 s is the normal window; if it persists, run `doctor` (see `journal history` above), then start the engine with `RUST_LOG=info,wfs_server=debug,wfs_fs=debug`: the watch loop logs `journal at usn <n> - <polls> poll(s), <records> record(s) read so far` every 15 s and `journal +N records -> M events` on changes. A position that never moves, or `records > 0, events == 0`, pinpoints journal-read vs parsing |
| Pipe connection refused | Service not running, `pipe_name` mismatch, or `acl = "restricted"` with a non-admin client |
| Pipe exits immediately | The name is taken (an instance is already running): change `pipe_name` via `--config`, or stop the old process first |
| HTTP replies 401 / `code:4` | No `x-wfs-token`, or a token that no longer matches `http.token` (a restart re-issues it, `acl` changed, `data_dir` moved) |
| `http.token` is unreadable | Its DACL is set by `acl`; under `restricted` a non-admin process cannot read it — that is the restriction working. Use an elevated terminal or go back to `open` |
| HTTP port conflict | Change `http_port`; it binds 127.0.0.1 only and is never exposed to the LAN |
| Memory growth | Deletes create tombstones; past the threshold (>1024 and >5%) a full rebuild reclaims them automatically. `POST /api/v1/snapshot` plus a manual restart speeds up reclamation |
| Where is the service log | `%ProgramData%\WFSearch\wfs.log` (console mode writes to stderr instead) |
