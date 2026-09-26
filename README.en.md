[中文](README.md) | [English](README.en.md)

# WFSearch — Low-level Windows File Search Service

An Everything-style **filename search engine**: it reads the NTFS MFT directly to build a
full in-memory index, keeps it live with the USN Journal, and serves queries to any
application over a **Named Pipe + local HTTP**. The goal is **extreme speed at minimal
cost**:

| Metric | Target | Measured¹ |
|---|---|---|
| Full index (1M files, SSD) | < 15 s | **2.2 s** (1.246M files) |
| Single query (1M files) | < 30 ms (P99) | **10.0 ms** (engine) / 14.7 ms (HTTP round trip) |
| Idle CPU | ≈ 0% (one journal poll per 100 ms, single syscall) | — (not covered by the script) |
| Memory | ≤ ~100 MB / 1M files | **87 B / file** (~103 MB) |
| Change visibility | ≤ 1 s | **create 72 ms / delete 75 ms** |
| Restart with valid snapshot | ≤ 3 s | **0.5 s** |

> ¹ Measured on 2026-09-26 by a real-disk acceptance run (C:, 1.246M files, elevated
> console, [`scripts/acceptance.ps1`](scripts/acceptance.ps1)); all five gates PASS.
> Rerun the script on your own machine and trust its output; synthetic query-engine
> benchmarks live in [docs/benchmarks.md](docs/benchmarks.md).

> v1 scope: **filename/path search only** (substrings + `*` `?` wildcards, case-insensitive,
> Unicode/CJK aware). Full-text content search, pinyin matching, and ReFS/network drives are
> out of scope for v1.

## Verification status

| Area | Status | Evidence |
|---|---|---|
| Query engine (match semantics, sorting, path matching) | **Verified** | 20 unit tests + criterion benchmarks (synthetic 1M entries) |
| Protocol JSON contract (pipe/HTTP payloads) | **Verified** | wfs-proto contract tests + pipe/HTTP end-to-end tests (real pipe & socket) |
| Snapshot format (write/validate/reject corrupt images) | **Verified** | Round-trip test + corrupt/foreign-version rejection tests |
| Full MFT enumeration, USN journal incrementals | **Verified** | Real-disk acceptance: full build + create/delete visibility < 100 ms; byte-level record parsing unit tests (incl. FRN sequence-bit regression) |
| Performance/memory targets (5 of 6) | **Verified (single machine)** | acceptance.ps1: five gates PASS (see table above); idle CPU is not covered by the script |
| Service mode install / SCM lifecycle | **Implemented, unverified** | Code complete; needs an on-machine `install` + `sc start` test |
| Full-text content search, pinyin, ReFS/network drives | **Not implemented** | Out of v1 scope |


## How it works

```
App ──Named Pipe──┐
                  ▼
Web ──127.0.0.1 HTTP──► wfs-server
                          │  query: in-memory linear scan (memchr/SIMD + rayon)
                          ▼
                      wfs-core index (one per volume: name arena + 24B/file node tree)
                          ▲
              FSCTL_ENUM_USN_DATA full walk │ FSCTL_READ_USN_JOURNAL incrementals
                          │
                        NTFS volume
```

- **Full build**: enumerates every MFT file record (FRN / parent FRN / name) from the
  volume handle in one pass — seconds per million files. File reference numbers are
  normalized at parse time by stripping the high 16-bit NTFS sequence counter (keeping the
  48-bit record number); without this, parent anchoring misses on every entry.
- **Live updates**: the USN Journal is read every ~100 ms and create/delete/rename events
  are applied in batches; a wrapped or deleted journal triggers an automatic full rebuild.
- **Cold start**: the index + journal position serialize to `index.bin`; on restart the
  snapshot resumes directly if the journal has not wrapped, otherwise the volume rebuilds.
- **Delete policy**: tombstone marking (subtree BFS), with an automatic rebuild to reclaim
  memory once the tombstone ratio crosses a threshold.

## Quick start

```powershell
# 1. Developer console mode (needs elevation: MFT access requires an elevated token)
cargo run --release -p wfs-server -- console

# 2. Query from another terminal
cargo run --release -p wfs-client -- search "*.rs"     # named pipe
cargo run --release -p wfs-client -- status --http     # HTTP channel
cargo run --release -p wfs-client -- search "src\core" --match-path   # match on path
curl "http://127.0.0.1:15100/api/v1/search?q=%E9%A1%B9%E7%9B%AE&limit=10"

# 3. Install as a Windows service (elevated)
wfs-server.exe install
sc start WFSearch

# 4. Real-disk acceptance (elevated terminal; one run yields every metric)
powershell -ExecutionPolicy Bypass -File scripts\acceptance.ps1
```

REPL commands: `type a query`, `status`, `quit`. Query syntax: `report *.docx C:`
(multi-term AND, wildcards, drive filter); a term containing `\` or `/` matches on the
path automatically (`src\core`), and `--match-path` forces every term onto the path.

If a volume fails to come up, run the `doctor` self-check first (elevated terminal; prints
each ioctl's result step by step, exit code 1 on failure):

```powershell
wfs-server.exe doctor C        # open volume / query journal / full enum / read journal
wfs-server.exe doctor          # no letters: probes every configured (or detected) drive
```

## Repository layout

```
crates/
├── wfs-proto/    protocol types (JSON serde) shared by both transports (6 contract tests)
├── wfs-core/     in-memory index + query engine (pure logic, 20 unit tests + criterion bench)
├── wfs-fs/       MFT enum, USN Journal (hand-written kernel32 FFI + byte-level parsing, 17 unit tests)
├── wfs-server/   service binary: console/service modes, pipe+http, snapshots, SCM lifecycle
│                 (11 tests: incl. pipe/HTTP end-to-end, snapshot round-trip)
└── wfs-client/   Rust SDK (reference client) + wfs-cli debug tool (3 tests: URL encoding + CLI args)
scripts/
└── acceptance.ps1  real-disk acceptance (one elevated run: doctor self-check + cold/warm start,
                    latency, visibility — per-gate PASS/FAIL)
docs/
├── protocol.md        wire protocol reference (Chinese: protocol.md)
├── deploy.md          deployment / config / service management (Chinese: deploy.md)
├── clients.md         Python / C# client examples (Chinese: clients.md)
└── benchmarks.md      benchmark data and the RwLock/ArcSwap decision (Chinese: benchmarks.md)
```

CI (GitHub Actions, windows-latest) gates `cargo fmt --check`, `cargo clippy -D warnings`,
`cargo test`, and the release build; the bench job in `.github/workflows/ci.yml` is
manual-dispatch only.

## Configuration

`%ProgramData%\WFSearch\config.toml` (defaults apply when absent; or `--config <FILE>`):

```toml
drives = ["auto"]          # or ["C", "D"]
http_port = 15100          # binds 127.0.0.1 only
pipe_name = "\\\\.\\pipe\\wfs-engine-v1"
poll_ms = 100              # journal poll interval
max_limit = 1000           # per-query limit cap
pipe_acl = "open"          # pipe DACL: open (default, any local user) | restricted (SYSTEM + admins)
```

`pipe_acl = "restricted"` fits "only specific services/admins may query"; an invalid value
falls back to `open` with a loud stderr warning (never a silent downgrade). Note the pipe
ACL does **not** constrain the HTTP channel: HTTP always binds the loopback only, but any
local user can reach it — tightening access fully means considering both channels.

## Documentation

- [Protocol reference](docs/protocol.en.md) ([中文](docs/protocol.md)) — Named Pipe framing + HTTP API
- [Deployment guide](docs/deploy.en.md) ([中文](docs/deploy.md)) — service install, config, troubleshooting
- [Client examples](docs/clients.en.md) ([中文](docs/clients.md)) — Python / C# / Rust code
- [Benchmarks](docs/benchmarks.en.md) ([中文](docs/benchmarks.md)) — measured data, reproduction, real-disk acceptance

## Known limits (v1)

- Requires elevation (the LocalSystem service has it; console mode needs an elevated terminal).
- NTFS fixed disks only (USN Journal limitation); ReFS/network drives unsupported.
- **Path matching** (the `match_path` flag, or writing `\` / `/` inside a term) and
  `sort=name|path` must materialize the full path for every candidate — 5–10× slower than
  name matching (measured in [benchmarks.md](docs/benchmarks.en.md)); name matching is the
  fast path.
- File size / modification time are not indexed (the enum interface lacks them; raw MFT
  record parsing could add them later).
