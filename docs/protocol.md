# WFSearch 线上协议 v1

两种传输通道,载荷完全一致(JSON,UTF-8):

| 通道 | 地址 | 帧格式 |
|---|---|---|
| Named Pipe(推荐) | `\\.\pipe\wfs-engine-v1` | 4 字节小端长度 + JSON;请求/响应同构;连接可复用 |
| HTTP(辅助) | `http://127.0.0.1:15100` | 标准 REST,见下 |

单帧上限 4MB。管道 DACL 默认沿用 Windows 默认值(本机任意用户可连);配置
`pipe_acl = "restricted"` 可收紧为仅 SYSTEM + Administrators(见 [deploy.md](deploy.md))。

## Named Pipe

### 请求

```jsonc
// 搜索
{"op": "search", "args": {"q": "report *.docx", "limit": 100, "offset": 0, "sort": "none", "match_path": false}}
// 状态
{"op": "status"}
// 存活探测
{"op": "ping"}
```

除 `q` 外全部字段可省略;`limit` 缺省 100,`offset` 缺省 0,`sort` 缺省 `none`,`match_path` 缺省 `false`。

`q` 语法:空白分隔多词 AND;每词为子串(大小写不敏感、Unicode 折叠)或含 `*`/`?` 的通配符;`c:` 单独成词时过滤盘符。

`sort`:`none`(默认,索引序)| `name` | `path`(后两者需收集全部命中,大结果集较慢)。

`match_path`:把**文件名匹配**改成**整条路径匹配**(`args.match_path = true` 时所有词都按路径判)。
另外,**查询词里只要含 `\` 或 `/`,该词自动按路径匹配**,与 `match_path` 无关——所以
`{"q": "src\\core"}` 不需要额外标志就能按路径搜。

路径匹配要求对每个候选物化完整路径,比名字匹配慢 5~10×(实测见
[benchmarks.md](benchmarks.md));能只搜文件名时就不要用它。

### 响应

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

`phase`:`building` | `ready` | `failed`。

错误码:`1` 请求非法,`2` 未就绪,`3` 内部错误。

## HTTP API(仅 127.0.0.1)

```
GET /api/v1/search?q=<query>&limit=100&offset=0&sort=none&match_path=false
GET /api/v1/status
POST /api/v1/snapshot          # 手动落盘快照
```

`match_path` 接受 `1` / `true`(大小写不敏感),其余值视为 `false`。

响应即上表 `data` 部分(SearchResp / StatusResp JSON),错误以对应 HTTP 状态码 + `ErrorPayload` 返回
(空查询 `q` → `400` + code 1)。

## 帧示例(Python)

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

完整可运行示例(含 C#)见 [clients.md](clients.md)。
