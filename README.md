<div align="center">

# 🔍 WFSearch

**Windows Flash Search**

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
- 📄 **Document content search** — `content:` terms scan candidate files live: plain text (UTF-8/UTF-16) plus Office/ODF XML containers, snippets back per hit, zero index on disk
- 🗂️ **Full-volume index in seconds** — reads the NTFS MFT directly; 1.25 M files in ~2 s
- 🔄 **Live updates** — USN Journal polled every 100 ms, changes visible in < 100 ms; automatic full rebuild on journal wrap
- 🪶 **Tiny footprint** — ≈ 87 bytes per file (~103 MB for 1.25 M entries), ~0% idle CPU
- 🔌 **Two transports, one protocol** — Named Pipe for lowest latency, loopback HTTP for scripts & web; identical JSON payloads
- 🩺 **Self-diagnosing** — `doctor` probes every ioctl step by step; an acceptance script measures all targets on real hardware

> **Scope notes:** content search ships as a query-time live scan (`content:` terms — no
> background indexing, nothing on disk; PDF/legacy binary Office/ANSI encodings
> unsupported). It reads files **as the service account**, so locally signed-in users can
> read snippets their own token cannot open — see the security boundary in
> [docs/deploy.en.md](docs/deploy.en.md). Still out of scope: a background full-text
> index, pinyin matching, and ReFS/network drives.

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
cargo run --release -p wfs-client -- status --http     # HTTP channel (loads the token itself)
cargo run --release -p wfs-client -- search "src\core" --match-path   # match on path
cargo run --release -p wfs-client -- search "*.md content:deploy"     # search inside documents
$tok = Get-Content "$env:ProgramData\WFSearch\http.token"
curl "http://127.0.0.1:15100/api/v1/search?q=report&limit=10" -H "x-wfs-token: $tok"

# 3️⃣ Register as a Windows service (elevated). Registration belongs to the
#    deployer — the exe no longer installs itself. binPath must be absolute.
$exe = 'C:\Program Files\WFSearch\wfs-server.exe'
sc.exe create WFSearch binPath= "`"$exe`" run" obj= LocalSystem start= auto
sc.exe start WFSearch

# 4️⃣ Real-disk acceptance — one run, every metric (elevated)
powershell -ExecutionPolicy Bypass -File scripts\acceptance.ps1
```

**REPL** — type a query, or `status` / `quit`. **Query syntax** — `report *.docx C:`:
terms are ANDed; wildcards and drive filters work; a term containing `\` or `/` matches on
the path automatically, or force it with `--match-path`; a `content:term` scans the
*contents* of whatever the other terms let through (narrow by filename first — see
[docs/protocol.en.md](docs/protocol.en.md)).

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

## 📄 Document content search (v0.2)

A `content:term` turns any query into a document-content search:

```powershell
wfs-cli search "*.md content:deploy"     # markdown files containing "deploy"
wfs-cli search "report content:预算 C:"   # ANDs freely with name / path / drive terms
```

- **🔎 Live scan, zero index** — the name-level terms first narrow candidates to a window
  (default 2 000 files), then each candidate is opened and matched in real time. Nothing
  is written to disk, no background CPU, and results are always current with the index.
- **📝 Formats** — plain text (UTF-8 / UTF-16 via BOM; NUL-byte sniffing skips binaries)
  and OOXML/ODF containers (`.docx` `.docm` `.xlsx` `.xlsm` `.pptx` `.pptm` `.odt` `.ods`
  `.odp`): the document XML is unzipped and stripped to text. PDF and legacy binary
  Office are not supported.
- **🧾 Rich hits** — each result carries a `snippet` around the first hit plus a per-file
  match count; the response summarizes the scan (scanned / skipped-by-size / binary /
  errors, with `truncated` and `timed_out` flags for partial coverage).
- **⏱️ Bounded by design** — per-file size cap (8 MiB), per-scan time budget (10 s), and
  a small reader pool that never competes with the name search's rayon threads. Tune it
  all in the `[content]` config section; `enabled = false` switches it off entirely.
- **🔐 Security** — files are opened **as the service account** (LocalSystem for the
  service), so a query can read snippets the calling user could not open themselves. The
  engine logs a loud startup warning while content search is on under `acl = open`; the
  boundary and both knobs are documented in [docs/deploy.en.md](docs/deploy.en.md).

## ✅ Verification status

| Area | Status | Evidence |
|---|---|---|
| Query engine (matching, sorting, path matching) | ✅ Verified | 28 unit tests + criterion benchmarks (synthetic 1M) |
| Protocol JSON contract (pipe & HTTP payloads) | ✅ Verified | contract tests + pipe/HTTP end-to-end tests |
| Document content scan (text + OOXML/ODF, budgets, truncation) | ✅ Verified | 16 wfs-content tests + real-file end-to-end in server tests; live on a 1.8 M-file machine |
| Snapshot format (write / validate / reject) | ✅ Verified | round-trip + corrupt/foreign-version rejection tests |
| Full MFT enum, USN journal incrementals | ✅ Verified | real-disk acceptance: build + change visibility < 100 ms; byte-level parsing tests (incl. FRN sequence-bit regression) |
| Performance & memory targets (5 of 6) | ✅ Verified (one machine) | acceptance script: five gates PASS (table above); idle CPU not covered |
| Service / SCM lifecycle | ✅ Verified (one machine) | `sc create` + in-place exe swap (stop → rename → drop in → start) run on real hardware |
| Background full-text index, pinyin, ReFS/network drives | ❌ Not in scope | content search is query-time scanning; an inverted index remains out |

## 📁 Repository layout

```
crates/
├── wfs-proto/    protocol types (JSON serde) shared by both transports — 7 contract tests
├── wfs-core/     in-memory index + query engine (pure logic) — 28 unit tests + criterion bench
├── wfs-fs/       MFT enum, USN Journal (hand-written kernel32 FFI, byte-level parsing) — 23 unit tests
├── wfs-content/  document-content extraction & matching (BOM text, OOXML/ODF, snippets) — 16 tests
├── wfs-server/   service binary: console/service modes, pipe+http, snapshots, SCM, content scan — 34 tests
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
acl = "open"               # who may reach the engine: open (default) | restricted

[content]                  # document-content search (`content:` terms)
enabled = true             # false rejects content: queries outright
max_candidates = 2000      # candidate window per scan
max_file_bytes = 8388608   # per-file cap (8 MiB)
timeout_ms = 10000         # per-scan time budget
max_concurrency = 4        # concurrent file readers
```

> 💡 `acl = "restricted"` limits **both** channels to SYSTEM + Administrators. `open` means
> anyone signed in at the console. An invalid value falls back to `open` with a loud warning —
> never a silent downgrade. The pipe authenticates through its DACL (the OS does it for free);
> HTTP authenticates with a bearer token published to `%ProgramData%\WFSearch\http.token`,
> because a loopback connection carries no user identity. Reading that file *is* the
> credential, and `acl` is what gates it.

## 📚 Documentation

| | English | 中文 |
|---|---|---|
| 📡 Wire protocol (pipe framing + HTTP API) | [protocol.en.md](docs/protocol.en.md) | [protocol.md](docs/protocol.md) |
| 🛠️ Deployment, config, troubleshooting | [deploy.en.md](docs/deploy.en.md) | [deploy.md](docs/deploy.md) |
| 🧩 Client examples (Python / C# / Rust) | [clients.en.md](docs/clients.en.md) | [clients.md](docs/clients.md) |
| 📈 Benchmarks & acceptance results | [benchmarks.en.md](docs/benchmarks.en.md) | [benchmarks.md](docs/benchmarks.md) |

## ⚠️ Known limits

- 🔐 Requires elevation (LocalSystem has it; console mode needs an elevated terminal).
- 💾 NTFS fixed disks only — USN Journal limitation; ReFS/network drives unsupported.
- 🐢 **Path matching** (`match_path`, or `\` / `/` inside a term) and `sort=name|path`
  materialize the full path per candidate — 5–10× slower than name matching; name matching
  is the fast path.
- 🔎 **Content search** covers plain text + OOXML/ODF only — no PDF, legacy binary
  Office (`.doc`/`.xls`/`.ppt`), ANSI/GBK encodings, or encrypted containers. A query
  with no name-level terms scans an arbitrary index-order window: narrow by filename
  first. It reads files as the service account — see the security note above.
- 📏 File size / mtime are not indexed (raw MFT record parsing could add them later).
