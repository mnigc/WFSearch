[中文](clients.md) | [English](clients.en.md)

# 应用接入示例

所有通道的载荷都是 JSON(见 [protocol.md](protocol.md))。优先用 Named Pipe(延迟最低);Web/脚本场景用 HTTP。

## Python — Named Pipe(可运行)

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

HTTP 不带用户身份,所以每个请求都要在 `x-wfs-token` 里附上数据目录下 `http.token` 的内容。
token 每次引擎启动换发,别缓存。

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

// 用法:
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

## Rust(wfs-client crate)

```rust
use wfs_client::Client;
use wfs_proto::SearchReq;

let mut c = Client::connect_default().unwrap();
let r = c.search(&SearchReq { q: "报告".into(), ..Default::default() }).unwrap();
for f in &r.results { println!("{}", f.path); }

// 按路径匹配(把文件名匹配换成整条路径匹配;慢 5~10×)
let r = c.search(&SearchReq { q: r"projects\2026".into(), match_path: true, ..Default::default() }).unwrap();

// HTTP 通道:token 由 load_token() 从 <ProgramData>\WFSearch\http.token 读出
let token = wfs_client::load_token().unwrap();
let r = wfs_client::search_http(&SearchReq { q: "*.md".into(), ..Default::default() }, 15100, &token).unwrap();
```

`SearchReq` 除 `q` 外都有默认值:`limit=100`、`offset=0`、`sort=SortKind::None`、`match_path=false`。

## 接入要点

1. **复用连接**:pipe 连接建立后可连续发多个请求(每请求一帧),避免反复连接。
2. **分页**:`total_matched` 是全量命中数,用 `offset`/`limit` 取页;默认按索引序返回(最快)。
3. **就绪判断**:服务启动即监听;客户端应先查 `status` 里对应卷 `phase == "ready"` 再展示"无结果",避免索引未完成时误判。
4. **错误处理**:`{"type":"err","data":{"code":1|2|3,"message":...}}`,HTTP 侧对应 4xx/5xx;
   另有仅 HTTP 出现的 `code: 4`(token 缺失或不对 → `401`)。
5. **路径匹配**:查询词里含 `\` 或 `/` 的词会自动按路径匹配;要按路径匹配**全部**词则设 `match_path=true`(Named Pipe)或 `match_path=true`(HTTP 查询参数)。只搜文件名时不要开,慢 5~10×。
6. **权限**:`acl = "restricted"` 同时收紧两个通道 —— pipe 仅 SYSTEM + Administrators 可连(普通用户收到"拒绝访问"),`http.token` 也只有这几类用户读得到(其他人拿 HTTP 只会吃到 401)。默认 `open` 下本机登录用户两个通道都能用。
