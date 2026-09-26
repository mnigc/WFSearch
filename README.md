<div align="center">

# 🔍 WFSearch

**Blazing-fast filename search for Windows.**

*The whole NTFS MFT indexed in memory — live USN Journal updates — queried over Named Pipe & local HTTP.*

[![CI](https://github.com/mnigc/WFSearch/actions/workflows/ci.yml/badge.svg)](https://github.com/mnigc/WFSearch/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Windows%20NTFS-0078D6)
![Language](https://img.shields.io/badge/language-Rust-dea584)

English · [中文](README.zh.md)

</div>

---

## ✨ Highlights

- ⚡ **Millisecond queries** — substring + `*`/`?` wildcards over a million files at P99 ≈ 10 ms, case-insensitive with Unicode folding (CJK included)
- 🗂️ **Full-volume index in seconds** — reads the NTFS MFT directly; 1.25 M files in ~2 s
- 🔄 **Live updates** — USN Journal polled every 100 ms, changes visible in < 100 ms; automatic full rebuild on journal wrap
- 🪶 **Tiny footprint** — ≈ 87 bytes per file (~103 MB for 1.25 M entries), ~0% idle CPU
- 🔌 **Two transports, one protocol** — Named Pipe for lowest latency, loopback HTTP for scripts & web; identical JSON payloads
- 🩺 **Self-diagnosing** — `doctor` probes every ioctl step by step; an acceptance script measures all targets on real hardware

> **v1 scope:** filename/path search only. Full-text content search, pinyin matching, and ReFS/network drives are out of scope.

## 📊 Performance

| Metric | Target | Measured ¹ |
|---|---|---|
| 🗂️ Full index (1M files, SSD) | < 15 s | **2.2 s** (1.25 M files) |
| ⚡ Single query (1M files, P99) | < 30 ms | **10.0 ms** engine · 14.7 ms HTTP round trip |
| 🪶 Memory | ≤ ~100 MB / 1M files | **87 B / file** (~103 MB) |
| 🔄 Change visibility | ≤ 1 s | **create 72 ms · delete 75 ms** |
| 🔁 Restart with valid snapshot | ≤ 3 s | **0.5 s** |
| 🌙 Idle CPU | ≈ 0% | — (not covered by the script) |

> ¹ One real-disk run (2026-09-26, drive C:, 1.25 M files, elevated console) of
> [`scripts/acceptance.ps1`](scripts/acceptance.ps1) — all five gates PASS. Rerun it on
> your own hardware and trust your numbers. Synthetic query-engine benchmarks:
> [docs/benchmarks.en.md](docs/benchmarks.en.md).

## 🚀 Quick start

```powershell
# 1️⃣ Developer console (elevated — MFT access needs an elevated token)
cargo run --release -p wfs-server -- console

# 2️⃣ Query from another terminal
cargo run --release -p wfs-client -- search "*.rs"     # named pipe
cargo run --release -p wfs-client -- status --http     # HTTP channel
cargo run --release -p wfs-client -- search "src\core" --match-path   # match on path
curl "http://127.0.0.1:15100/api/v1/search?q=report&limit=10"

# 3️⃣ Install as a Windows service (elevated)
wfs-server.exe install
sc start WFSearch

# 4️⃣ Real-disk acceptance — one run, every metric (elevated)
powershell -ExecutionPolicy Bypass -File scripts\acceptance.ps1
```

**REPL** — type a query, or `status` / `quit`. **Query syntax** — `report *.docx C:`:
terms are ANDed; wildcards and drive filters work; a term containing `\` or `/` matches on
the path automatically, or force it with `--match-path`.

**Volume won't come up?** Ask the doctor (elevated; prints each ioctl's result, exit code 1 on failure):

```powershell
wfs-server.exe doctor C        # open volume → query journal → full enum → read journal
wfs-server.exe doctor          # no letters: probes every configured drive
```

## 🧠 How it works

```
App ──Named Pipe──┐
                  ▼
Web ──127.0.0.1 HTTP──► wfs-server
                          │  query: in-memory linear scan (memchr/SIMD + rayon)
                          ▼
                      wfs-core index (one per volume: name arena + 24 B/file node tree)
                          ▲
              FSCTL_ENUM_USN_DATA full walk │ FSCTL_READ_USN_JOURNAL incrementals
                          │
                        NTFS volume
```

- **🗂️ Full build** — every MFT file record (FRN / parent FRN / name) enumerated in one
  pass, seconds per million files. File references are normalized at parse time (the
  16-bit NTFS sequence counter in the high word is stripped) so parent anchoring always resolves.
- **🔄 Live updates** — the USN Journal is read every ~100 ms and create/delete/rename
  events applied in batches; a wrapped or deleted journal triggers an automatic rebuild.
- **🔁 Cold start** — index + journal position serialize to `index.bin`; a snapshot whose
  journal hasn't wrapped resumes in seconds, otherwise the volume rebuilds.
- **🧹 Deletes** — tombstone marking (subtree BFS); a rebuild reclaims memory once the
  tombstone ratio crosses a threshold.

## ✅ Verification status

| Area | Status | Evidence |
|---|---|---|
| Query engine (matching, sorting, path matching) | ✅ Verified | 20 unit tests + criterion benchmarks (synthetic 1M) |
| Protocol JSON contract (pipe & HTTP payloads) | ✅ Verified | contract tests + pipe/HTTP end-to-end tests |
| Snapshot format (write / validate / reject) | ✅ Verified | round-trip + corrupt/foreign-version rejection tests |
| Full MFT enum, USN journal incrementals | ✅ Verified | real-disk acceptance: build + change visibility < 100 ms; byte-level parsing tests (incl. FRN sequence-bit regression) |
| Performance & memory targets (5 of 6) | ✅ Verified (one machine) | acceptance script: five gates PASS (table above); idle CPU not covered |
| Service install / SCM lifecycle | 🚧 Implemented, unverified | needs an on-machine `install` + `sc start` run |
| Full-text, pinyin, ReFS/network drives | ❌ Not in v1 | out of scope |

## 📁 Repository layout

```
crates/
├── wfs-proto/    protocol types (JSON serde) shared by both transports — 6 contract tests
├── wfs-core/     in-memory index + query engine (pure logic) — 20 unit tests + criterion bench
├── wfs-fs/       MFT enum, USN Journal (hand-written kernel32 FFI, byte-level parsing) — 17 unit tests
├── wfs-server/   service binary: console/service modes, pipe+http, snapshots, SCM — 11 tests
└── wfs-client/   Rust SDK (reference client) + wfs-cli debug tool — 3 tests
scripts/
└── acceptance.ps1   real-disk acceptance: doctor + cold/warm start, latency, visibility
docs/                bilingual docs (Chinese primary, *.en.md siblings) — see 📚 below
```

CI (GitHub Actions, `windows-latest`) gates `cargo fmt --check`, `clippy -D warnings`,
`cargo test`, and the release build; the bench job is manual-dispatch only.

## ⚙️ Configuration

`%ProgramData%\WFSearch\config.toml` (defaults apply when absent; or `--config <FILE>`):

```toml
drives = ["auto"]          # or ["C", "D"]
http_port = 15100          # binds 127.0.0.1 only
pipe_name = "\\\\.\\pipe\\wfs-engine-v1"
poll_ms = 100              # journal poll interval
max_limit = 1000           # per-query limit cap
pipe_acl = "open"          # pipe DACL: open (default) | restricted (SYSTEM + admins)
```

> 💡 `pipe_acl = "restricted"` limits *pipe* queries to SYSTEM + Administrators; an invalid
> value falls back to `open` with a loud warning — never a silent downgrade. The pipe ACL
> does **not** constrain the HTTP channel: HTTP binds the loopback only, but any local user
> can reach it, so tightening access fully means considering both channels.

## 📚 Documentation

| | English | 中文 |
|---|---|---|
| 📡 Wire protocol (pipe framing + HTTP API) | [protocol.en.md](docs/protocol.en.md) | [protocol.md](docs/protocol.md) |
| 🛠️ Deployment, config, troubleshooting | [deploy.en.md](docs/deploy.en.md) | [deploy.md](docs/deploy.md) |
| 🧩 Client examples (Python / C# / Rust) | [clients.en.md](docs/clients.en.md) | [clients.md](docs/clients.md) |
| 📈 Benchmarks & acceptance results | [benchmarks.en.md](docs/benchmarks.en.md) | [benchmarks.md](docs/benchmarks.md) |

## ⚠️ Known limits (v1)

- 🔐 Requires elevation (LocalSystem has it; console mode needs an elevated terminal).
- 💾 NTFS fixed disks only — USN Journal limitation; ReFS/network drives unsupported.
- 🐢 **Path matching** (`match_path`, or `\` / `/` inside a term) and `sort=name|path`
  materialize the full path per candidate — 5–10× slower than name matching; name matching
  is the fast path.
- 📏 File size / mtime are not indexed (raw MFT record parsing could add them later).
