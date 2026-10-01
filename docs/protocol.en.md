[中文](protocol.md) | [English](protocol.en.md)

# WFSearch Wire Protocol v2

> What changed in v2: a `content:` term in `q` triggers a document-content
> scan; `SearchResp` gains optional `content`, and `FileResult` gains optional
> `snippet`/`content_matches`. Everything added is an **optional field** — the
> wire shape of a query without `content:` is byte-identical to v1, so v1
> clients are unaffected. New clients can probe `status.protocol >= 2`.

Two transports carry identical payloads (JSON, UTF-8):

| Channel | Address | Framing |
|---|---|---|
| Named Pipe (preferred) | `\\.\pipe\wfs-engine-v1` | 4-byte little-endian length + JSON; requests/responses are symmetric; the connection is reusable |
| HTTP (auxiliary) | `http://127.0.0.1:15100` | plain REST, see below |

Per-frame cap: 4 MB. Authentication differs per channel and is selected by the one config
key `acl` (see [deploy.en.md](deploy.en.md)):

* **Named Pipe**: the DACL does the work, and the OS enforces it at connect time. `open`
  (default) admits anyone signed in at the console; `restricted` admits only SYSTEM +
  Administrators.
* **HTTP**: a bearer token. A loopback connection carries no user identity, so every request
  must send `x-wfs-token` with the contents of `http.token` in the data directory. That
  file's DACL follows `acl` — reading it *is* holding the credential.

## Named Pipe

### Requests

```jsonc
// search
{"op": "search", "args": {"q": "report *.docx", "limit": 100, "offset": 0, "sort": "none", "match_path": false}}
// status
{"op": "status"}
// liveness probe
{"op": "ping"}
```

Every field except `q` is optional; `limit` defaults to 100, `offset` to 0, `sort` to
`none`, `match_path` to `false`.

`q` syntax: whitespace-separated terms ANDed; each term is a substring
(case-insensitive, Unicode-folded) or a `*`/`?` wildcard; a standalone `c:` filters by drive;
a `content:term` prefix (case-insensitive) triggers a document-content scan, see below.

`sort`: `none` (default, index order) | `name` | `path` (the latter two must collect every
hit first — slower on large result sets).

`match_path`: switches **filename matching** to **whole-path matching** (when
`args.match_path = true`, every term is judged against the path). Additionally, **any term
containing `\` or `/` matches on the path automatically**, regardless of `match_path` — so
`{"q": "src\\core"}` searches by path with no extra flags.

Path matching requires materializing the full path for every candidate — 5–10× slower than
name matching (measured in [benchmarks.en.md](benchmarks.en.md)); avoid it when a filename
search suffices.

### Content search: `content:` terms

A `content:term` in `q` adds a **query-time document-content scan** on top of the name
search:

* **Flow**: the remaining terms (name / path / drive) first narrow the candidates down to
  the candidate window (default 2 000, configurable); the scan then reads those files'
  contents in real time. **With no filename constraints the candidate set is the whole
  index**, which the window truncates (`truncated: true`) — narrow by filename first.
* **Content terms**: foldcase substrings (same semantics as name terms, CJK included), no
  wildcards; multiple `content:` terms AND; each term is a single whitespace-separated
  token, no phrase syntax.
* **Formats covered**: plain text (txt/md/log/csv/source code; BOM-detected UTF-8/UTF-16,
  no BOM means UTF-8, a NUL byte anywhere means binary → skipped) and OOXML/ODF containers
  (.docx/.docm/.xlsx/.xlsm/.pptx/.pptm/.odt/.ods/.odp — the document XML members are
  unzipped and stripped to text). **Not supported**: PDF, legacy binary Office
  (.doc/.xls/.ppt), GBK/ANSI encodings, encrypted containers.
* **Semantic change**: `total_matched` now counts **content matches**; `limit`/`offset`
  page over the content matches; `query_ms` includes the scan (which can take seconds —
  the transports already run it off their reactor threads).

```jsonc
{"type": "search", "data": {
    "total_matched": 3,
    "query_ms": 812,
    "limit": 100, "offset": 0,
    "results": [{"name": "a.docx", "path": "C:\\work\\a.docx", "is_dir": false,
                 "snippet": "…Q3 budget plan…", "content_matches": 4}],
    "content": {"scanned": 42, "skipped_size": 1, "skipped_binary": 3,
                "errors": 0, "truncated": false, "timed_out": false}
}}
```

Every scanned candidate lands in exactly one `content` counter: `scanned` (read and
matched), `skipped_size` (over the per-file cap, default 8 MiB), `skipped_binary`
(rejected as binary), `errors` (unreadable or unparseable — deleted/renamed between
indexing and opening, exclusively locked; routine, not a fault). `truncated` means the
candidate window cut the scan short (narrow the name terms for full coverage);
`timed_out` means the scan budget (default 10 s) ran out and results are partial.
`snippet` is the context around the first hit (~64 chars each side, whitespace collapsed,
`…` for elided text); `content_matches` is the total hit count in that file.

Guards and switches (disable entirely, window, size cap, budget, concurrency) live in the
`[content]` section of [deploy.en.md](deploy.en.md); files under the engine's own data
directory are never scanned.

### Responses

```jsonc
{"type": "search", "data": {
    "total_matched": 1234,
    "query_ms": 12,
    "limit": 100, "offset": 0,
    "results": [{"name": "a.docx", "path": "C:\\work\\2026\\a.docx", "is_dir": false}]
}}
{"type": "status", "data": {
    "version": "0.2.0", "protocol": 2, "uptime_ms": 54321,
    "approx_memory_bytes": 98000000,
    "volumes": [{"drive": "C", "phase": "ready", "files": 1234567,
                 "deleted": 12, "journal": true, "last_update_ms_ago": 350}]}
}}
{"type": "pong"}
{"type": "err", "data": {"code": 1, "message": "query must not be empty"}}
```

`phase`: `building` | `ready` | `failed`.

Error codes: `1` invalid request, `2` not ready, `3` internal error, `4` missing or wrong
token (HTTP only — a pipe connection already carries the client's Windows identity, so `4`
never appears on the pipe).

## HTTP API (127.0.0.1 only)

```
GET /                          # the same-origin demo UI (needs the token, see below)
GET /api/v1/search?q=<query>&limit=100&offset=0&sort=none&match_path=false
GET /api/v1/status
POST /api/v1/snapshot          # flush a snapshot manually
```

Every request must send `x-wfs-token: <contents of http.token>`. An address bar cannot set a
header, so open the demo UI with the query form instead: `http://127.0.0.1:15100/?token=…` —
once loaded, the page's own `fetch` calls switch back to the header. The token is generated at
startup and written to `http.token` in the data directory, under a DACL that follows `acl`.

`match_path` accepts `1` / `true` (case-insensitive); anything else counts as `false`.

Responses are the `data` objects from the table above (SearchResp / StatusResp JSON);
errors return the matching HTTP status code + `ErrorPayload` (empty `q` → `400` + code 1,
bad token → `401` + code 4).

## Frame example (Python)

```python
import json, struct
from ctypes import *

pipe = windll.kernel32.CreateFileW(r"\\.\pipe\wfs-engine-v1", 0xC0000000, 0, None, 3, 0, None)
f = msvcrt.open_osfhandle(pipe, 0)          # import msvcrt
req = json.dumps({"op": "search", "args": {"q": "*.md"}}).encode()
f.write(struct.pack("<I", len(req))); f.write(req)
n = struct.unpack("<I", f.read(4))[0]
print(json.loads(f.read(n)))
```

For complete runnable examples (including C#) see [clients.en.md](clients.en.md).
