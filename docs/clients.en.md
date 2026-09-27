[中文](clients.md) | [English](clients.en.md)

# Client Integration Examples

Every transport carries JSON payloads (see [protocol.en.md](protocol.en.md)). Prefer the
Named Pipe (lowest latency); use HTTP for web/script scenarios.

## Python — Named Pipe (runnable)

```python
import json
import msvcrt
import struct
from ctypes import windll

PIPE = r"\\.\pipe\wfs-engine-v1"

def connect():
    GENERIC_READ_WRITE = 0xC0000000
    OPEN_EXISTING = 3
    h = windll.kernel32.CreateFileW(PIPE, GENERIC_READ_WRITE, 0, None, OPEN_EXISTING, 0, None)
    if h == -1:
        raise OSError("cannot open pipe (is wfs-server running?)")
    return msvcrt.open_osfhandle(h, 0)

def call(f, req: dict) -> dict:
    body = json.dumps(req).encode()
    f.write(struct.pack("<I", len(body)))
    f.write(body)
    f.flush()
    (n,) = struct.unpack("<I", f.read(4))
    return json.loads(f.read(n))

if __name__ == "__main__":
    f = connect()
    print(call(f, {"op": "ping"}))
    r = call(f, {"op": "search", "args": {"q": "*.py", "limit": 10}})
    print(f"{r['total_matched']} matches in {r['query_ms']} ms")
    for item in r["results"]:
        print(" ", item["path"])
```

## Python — HTTP

A loopback connection carries no user identity, so every request must send the contents of
`http.token` in the data directory as `x-wfs-token`. The token is re-issued at each engine
start — do not cache it.

```python
import json, os, urllib.parse, urllib.request

TOKEN_PATH = os.path.join(os.environ["ProgramData"], "WFSearch", "http.token")

def token() -> str:
    with open(TOKEN_PATH, encoding="utf-8") as f:
        return f.read().strip()

def search(q: str, limit: int = 20):
    url = "http://127.0.0.1:15100/api/v1/search?" + urllib.parse.urlencode({"q": q, "limit": limit})
    req = urllib.request.Request(url, headers={"x-wfs-token": token()})
    with urllib.request.urlopen(req) as resp:
        return json.load(resp)
```

## C# — NamedPipeClientStream

```csharp
using System.IO;
using System.Text;
using System.Text.Json;

static async Task<JsonElement> CallPipeAsync(object req)
{
    await using var pipe = new NamedPipeClientStream(".", "wfs-engine-v1",
        PipeDirection.InOut, PipeOptions.Asynchronous);
    await pipe.ConnectAsync(3000);

    byte[] body = JsonSerializer.SerializeToUtf8Bytes(req);
    byte[] header = BitConverter.GetBytes((uint)body.Length); // little-endian
    await pipe.WriteAsync(header);
    await pipe.WriteAsync(body);
    await pipe.FlushAsync();

    byte[] lenBuf = new byte[4];
    await pipe.ReadExactlyAsync(lenBuf);
    byte[] resp = new byte[BitConverter.ToUInt32(lenBuf)];
    await pipe.ReadExactlyAsync(resp);
    return JsonDocument.Parse(resp).RootElement;
}

// Usage:
var resp = await CallPipeAsync(new {
    op = "search",
    args = new { q = "*.docx", limit = 20, offset = 0, sort = "none" }
});
Console.WriteLine(resp.GetProperty("data").GetProperty("total_matched"));
```

## C# — HTTP

```csharp
var token = (await File.ReadAllTextAsync(
    Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.CommonApplicationData),
        "WFSearch", "http.token"))).Trim();
using var http = new HttpClient();
http.DefaultRequestHeaders.Add("x-wfs-token", token);
var url = "http://127.0.0.1:15100/api/v1/search?q=" + Uri.EscapeDataString("*.docx") + "&limit=20";
var resp = await JsonSerializer.DeserializeAsync<JsonElement>(await http.GetStreamAsync(url));
```

## Rust (wfs-client crate)

```rust
use wfs_client::Client;
use wfs_proto::SearchReq;

let mut c = Client::connect_default().unwrap();
let r = c.search(&SearchReq { q: "report".into(), ..Default::default() }).unwrap();
for f in &r.results { println!("{}", f.path); }

// Match on path (replaces filename matching with whole-path matching; 5–10× slower)
let r = c.search(&SearchReq { q: r"projects\2026".into(), match_path: true, ..Default::default() }).unwrap();

// HTTP transport: load_token() reads <ProgramData>\WFSearch\http.token
let token = wfs_client::load_token().unwrap();
let r = wfs_client::search_http(&SearchReq { q: "*.md".into(), ..Default::default() }, 15100, &token).unwrap();
```

Every `SearchReq` field except `q` has a default: `limit=100`, `offset=0`,
`sort=SortKind::None`, `match_path=false`.

## Integration notes

1. **Reuse connections**: a pipe connection can carry many requests (one frame each) —
   avoid reconnecting per call.
2. **Paging**: `total_matched` is the full hit count; fetch pages with `offset`/`limit`.
   Results default to index order (fastest).
3. **Readiness**: the server listens as soon as it starts; clients should check
   `status` for `phase == "ready"` on the relevant volume before concluding "no results",
   so an unfinished index is not mistaken for an empty one.
4. **Errors**: `{"type":"err","data":{"code":1|2|3,"message":...}}`; HTTP maps these to
   4xx/5xx. `code: 4` (missing or wrong token → `401`) exists on HTTP only.
5. **Path matching**: terms containing `\` or `/` match on the path automatically; to put
   **all** terms on the path set `match_path=true` (Named Pipe) or `match_path=true` (HTTP
   query parameter). Keep it off for filename-only searches — it costs 5–10×.
6. **Permissions**: `acl = "restricted"` tightens **both** channels — the pipe then accepts
   only SYSTEM + Administrators (everyone else gets "access denied"), and `http.token` is
   readable only by them, so HTTP yields 401 for the rest. Under the default `open`, anyone
   signed in at the console can use both.
