[中文](protocol.md) | [English](protocol.en.md)

# WFSearch Wire Protocol v1

Two transports carry identical payloads (JSON, UTF-8):

| Channel | Address | Framing |
|---|---|---|
| Named Pipe (preferred) | `\\.\pipe\wfs-engine-v1` | 4-byte little-endian length + JSON; requests/responses are symmetric; the connection is reusable |
| HTTP (auxiliary) | `http://127.0.0.1:15100` | plain REST, see below |

Per-frame cap: 4 MB. The pipe DACL defaults to the Windows default (any local user may
connect); `pipe_acl = "restricted"` tightens it to SYSTEM + Administrators (see
[deploy.en.md](deploy.en.md)).

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
(case-insensitive, Unicode-folded) or a `*`/`?` wildcard; a standalone `c:` filters by drive.

`sort`: `none` (default, index order) | `name` | `path` (the latter two must collect every
hit first — slower on large result sets).

`match_path`: switches **filename matching** to **whole-path matching** (when
`args.match_path = true`, every term is judged against the path). Additionally, **any term
containing `\` or `/` matches on the path automatically**, regardless of `match_path` — so
`{"q": "src\\core"}` searches by path with no extra flags.

Path matching requires materializing the full path for every candidate — 5–10× slower than
name matching (measured in [benchmarks.en.md](benchmarks.en.md)); avoid it when a filename
search suffices.

### Responses

```jsonc
{"type": "search", "data": {
    "total_matched": 1234,
    "query_ms": 12,
    "limit": 100, "offset": 0,
    "results": [{"name": "a.docx", "path": "C:\\work\\2026\\a.docx", "is_dir": false}]
}}
{"type": "status", "data": {
    "version": "0.1.0", "protocol": 1, "uptime_ms": 54321,
    "approx_memory_bytes": 98000000,
    "volumes": [{"drive": "C", "phase": "ready", "files": 1234567,
                 "deleted": 12, "journal": true, "last_update_ms_ago": 350}]}
}}
{"type": "pong"}
{"type": "err", "data": {"code": 1, "message": "query must not be empty"}}
```

`phase`: `building` | `ready` | `failed`.

Error codes: `1` invalid request, `2` not ready, `3` internal error.

## HTTP API (127.0.0.1 only)

```
GET /api/v1/search?q=<query>&limit=100&offset=0&sort=none&match_path=false
GET /api/v1/status
POST /api/v1/snapshot          # flush a snapshot manually
```

`match_path` accepts `1` / `true` (case-insensitive); anything else counts as `false`.

Responses are the `data` objects from the table above (SearchResp / StatusResp JSON);
errors return the matching HTTP status code + `ErrorPayload` (empty `q` → `400` + code 1).

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
