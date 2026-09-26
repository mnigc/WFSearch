[中文](benchmarks.md) | [English](benchmarks.en.md)

# Benchmarks

Two verification levels with different data sources — don't mix them up:

| | What it measures | How to run | Where the data lives |
|---|---|---|---|
| **Synthetic 1M entries** (this page) | The query engine itself: parse → parallel scan → merge → slice | `cargo bench -p wfs-core --bench scan` | tables on this page |
| **Real C: drive** (README target table) | End to end: full-build time, memory/file, change visibility, warm start | elevated terminal: `powershell -ExecutionPolicy Bypass -File scripts\acceptance.ps1` | script output (see the end of this page) |

Synthetic index entry names rotate over 4 patterns (`report_0000000.docx` /
`项目文件夹_0000000.txt` / `IMG_202600000.jpg` / `notes-0000000.md`), with 1/16 marked as
directories. Building the index itself (1M `insert`s) takes about **0.13 s**.

## Query latency (criterion, 1M entries, release)

30 samples per group; values are means:

| Case | Query | Mean | Notes |
|---|---|---|---|
| `match-all` | empty | **0.44 ms** | floor of a full scan (no match-decision cost) |
| `substr-ascii-common` | `report` | **1.30 ms** | 250k hits, `memchr` ASCII fast path |
| `substr-ascii-rare` | `report_0099` | **1.63 ms** | upper bound for sparse hits |
| `substr-cjk` | `文件夹` | **2.14 ms** | Unicode folding path, ~1.6× slower than ASCII |
| `multi-term` | `report 2026` | **1.54 ms** | two-term AND (cheapest term judged first) |
| `wildcard` | `*.docx` | **4.25 ms** | wildcard backtracking — the priciest name match |
| `wildcard-cjk` | `项目文件夹_000*` | **2.97 ms** | zero-alloc `match_chars` |
| `sort-name` | `report` + `sort=name` | **8.07 ms** | must collect all 250k hits, then sort |
| `path-term` | `文件夹_0000\0` | **11.77 ms** | a term containing `\` matches on path: materializes a path per candidate |
| `match-path-flag` | `report` + `match_path=true` | **10.76 ms** | same, but all 250k hits get materialized (worst case) |
| `sort-path` | `report` + `sort=path` | **26.01 ms** | path sort: collect + materialize each + sort (`sort_by_cached_key`, one materialization per entry) |

Takeaways:

- **The default path (no sort, no path matching) sits in the 1–5 ms range**, far below the
  README's 30 ms (P99) target.
- **Path-related features (path matching, path sort) cost 5–10×** because each candidate
  needs one full path string built. This is an explicit design trade-off (see README known
  limits), not a regression.
- CJK costs only ~1.6×; wildcards are the priciest — everything stays in millisecond territory.

## Index publish cost (the RwLock vs ArcSwap decision)

A design review proposed replacing `parking_lot::RwLock` with `ArcSwap` to make queries
lock-free. The premise is that **publishing a snapshot must be cheap**: every incremental
batch would clone the whole `VolumeIndex` (the `Vec<Node>` + the `frn_index` and
`children` hash maps); otherwise `ArcSwap` buys nothing.

| Case | Mean |
|---|---|
| `clone-1m-volume-index` (cost of one publish) | **17.97 ms** |
| `rwlock-read-1m` (the read lock being replaced) | **≈ 0 (sub-microsecond)** |

**Conclusion: keep `RwLock`.** Publishing is 4–5 orders of magnitude more expensive than
the read lock: cloning 18 ms per event batch would eat the whole journal poll interval
(poll_ms=100), while the read lock's own cost sits under measurement noise. This decision
only needs revisiting if the index becomes a shareable immutable structure (e.g. arena +
copy-metadata-only).

## Reproducing

```powershell
# All benchmarks (release build; criterion takes a few minutes)
cargo bench -p wfs-core --bench scan

# One short warm-up/measurement round (quick before/after comparisons)
cargo bench -p wfs-core --bench scan -- --warm-up-time 1 --measurement-time 3 --sample-size 20
```

`--bench scan` must be explicit: without a target, cargo passes the arguments to the lib
test harness too, which fails with `Unrecognized option` on criterion's flags.

Criterion's historical reports live in `target/criterion/<group>/<case>/report/index.html`
(base/new comparisons).

## Real-disk acceptance

What synthetic indexes cannot measure — MFT enumeration speed, USN journal delivery
latency, snapshot restore time — is covered by
[`scripts/acceptance.ps1`](../scripts/acceptance.ps1) run once in an **elevated** terminal. It:

1. cold-starts (full MFT build) → records time to `phase=ready`, indexed file count, memory/file;
2. runs 200 rounds × 3 query shapes → reports `query_ms` P50/P99 (engine) and HTTP round-trip P50/P99;
3. creates/deletes a uniquely named file in `%TEMP%` → records index visibility latency;
4. restarts after a `quit` snapshot → records warm-start time;
5. compares each result against the README's 5 targets as PASS/FAIL.

The script only writes a temporary data dir under `%TEMP%` — it never touches
`%ProgramData%` and never installs the service. Output is pure ASCII, pasteable into issues.

### Measured results (2026-09-26, C: with 1.246M files)

| Gate | Target | Measured | Result |
|---|---|---|---|
| Cold start (full MFT build) | < 15 s | 2.2 s (1,246,080 files, 103.2 MB) | PASS |
| Query P99 (200 rounds × 3 shapes) | < 30 ms | engine 10.0 ms; HTTP round trip 14.7 ms | PASS |
| Memory per file | ≤ 100 B | 87 B | PASS |
| Change visibility (create / delete) | ≤ 1 s | 72 ms / 75 ms | PASS |
| Warm start (77.3 MB snapshot) | ≤ 3 s | 0.5 s | PASS |

Query detail (engine `query_ms` / HTTP round trip, P50 / P99): `*.dll` 7.0/10.0 ms
(round trip 9.3/14.7, 46,699 hits); `*.log` 7.0/8.0 ms (round trip 9.2/10.7, 9,205 hits);
`kernel32` 2.0/2.0 ms (round trip 4.0/5.8, 183 hits).

> Numbers from one run on one machine; hardware and workload will shift them. Rerun the
> script on your own machine to reproduce.

> The tables on this page are measured values; **the README target table remains
> "unverified targets" on any machine that has not run the script** (see the README's
> "Verification status").
