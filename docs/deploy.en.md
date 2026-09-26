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

```powershell
# Elevated terminal
wfs-server.exe install                 # registers the service (LocalSystem, auto-start)
sc start WFSearch                      # start
wfs-cli.exe status                     # verify

sc stop WFSearch                       # stop (saves a snapshot automatically)
wfs-server.exe uninstall               # remove registration
```

The service log **appends** to `%ProgramData%\WFSearch\wfs.log` (rotated to `wfs.log.old`
at startup past 16 MB) — a service process has no console, and without a log file you see
nothing at all. For debugging, reproduce in `console` mode first (logs go to stderr), or
set `RUST_LOG=debug`.

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
| Custom data dir | `data_dir = "D:\\wfsdata"` in config | hosts both snapshot and log |

First start: a full enumeration per drive (while it runs, `/status` shows `building` plus
progress); a snapshot is written automatically once the build finishes, and every graceful
exit afterwards (service stop / Ctrl-C) refreshes it. On restart, a volume whose journal
has not wrapped recovers in seconds; otherwise it rebuilds automatically.

## Query verification

```powershell
wfs-cli.exe search "*.docx"
wfs-cli.exe search "report C: 2026" --limit 50 --sort name
wfs-cli.exe search "src\core" --match-path         # whole-path matching
wfs-cli.exe search "*.md" --http                     # via the HTTP channel
wfs-cli.exe status
```

`pipe_acl = "restricted"` (SYSTEM + Administrators only) affects every pipe client:
in that mode wfs-cli must run elevated (the HTTP channel is unaffected). An invalid value
falls back to `open` with a stderr warning.

## Troubleshooting

| Symptom | Cause & handling |
|---|---|
| volume shows `failed` in `status` | Run `wfs-server.exe doctor <drive>` first: it reports which ioctl failed and the Win32 error code. Usual causes: console mode not elevated, drive absent, or the volume's USN journal being unusable |
| New files never show up | Within 1 s is the normal window; if it persists, run `doctor` (see `journal history` above), then start the engine with `RUST_LOG=info,wfs_server=debug,wfs_fs=debug`: the watch loop logs `journal at usn <n> - <polls> poll(s), <records> record(s) read so far` every 15 s and `journal +N records -> M events` on changes. A position that never moves, or `records > 0, events == 0`, pinpoints journal-read vs parsing |
| Pipe connection refused | Service not running, `pipe_name` mismatch, or `pipe_acl = "restricted"` with a non-admin client |
| Pipe exits immediately | The name is taken (an instance is already running): change `pipe_name` via `--config`, or stop the old process first |
| HTTP port conflict | Change `http_port`; it binds 127.0.0.1 only and is never exposed to the LAN |
| Memory growth | Deletes create tombstones; past the threshold (>1024 and >5%) a full rebuild reclaims them automatically. `POST /api/v1/snapshot` plus a manual restart speeds up reclamation |
| Where is the service log | `%ProgramData%\WFSearch\wfs.log` (console mode writes to stderr instead) |
