[中文](protocol.md) | [English](protocol.en.md)

# WFSearch 线上协议 v2

> v2 变更:`q` 新增 `content:` 词触发文档内容扫描;`SearchResp` 新增可选 `content`、
> `FileResult` 新增可选 `snippet`/`content_matches`。全部是**可选字段**——不带 `content:`
> 的查询 wire 形状与 v1 完全一致,v1 客户端不受影响。新客户端可探测
> `status.protocol >= 2` 判断服务端能力。

两种传输通道,载荷完全一致(JSON,UTF-8):

| 通道 | 地址 | 帧格式 |
|---|---|---|
| Named Pipe(推荐) | `\\.\pipe\wfs-engine-v1` | 4 字节小端长度 + JSON;请求/响应同构;连接可复用 |
| HTTP(辅助) | `http://127.0.0.1:15100` | 标准 REST,见下 |

单帧上限 4MB。鉴权按通道各有一套,由配置项 `acl` 统一选择(见 [deploy.md](deploy.md)):

* **Named Pipe**:凭 DACL,操作系统在连接时就完成鉴权。`open`(默认)放行本机登录用户,
  `restricted` 仅 SYSTEM + Administrators。
* **HTTP**:凭 bearer token。回环连接不携带用户身份,所以每个请求都要带
  `x-wfs-token`,值取自数据目录下的 `http.token`;该文件的 DACL 跟着 `acl` 走 —— 读得到
  文件就等于有凭证。

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

`q` 语法:空白分隔多词 AND;每词为子串(大小写不敏感、Unicode 折叠)或含 `*`/`?` 的通配符;`c:` 单独成词时过滤盘符;`content:词`(前缀大小写不敏感)触发文档内容扫描,见下节。

`sort`:`none`(默认,索引序)| `name` | `path`(后两者需收集全部命中,大结果集较慢)。

`match_path`:把**文件名匹配**改成**整条路径匹配**(`args.match_path = true` 时所有词都按路径判)。
另外,**查询词里只要含 `\` 或 `/`,该词自动按路径匹配**,与 `match_path` 无关——所以
`{"q": "src\\core"}` 不需要额外标志就能按路径搜。

路径匹配要求对每个候选物化完整路径,比名字匹配慢 5~10×(实测见
[benchmarks.md](benchmarks.md));能只搜文件名时就不要用它。

### 内容检索:`content:` 词

`q` 里出现 `content:词` 即在名字搜索之上追加**查询时文档内容扫描**:

* **流程**:先用其余词(文件名/路径/盘符)把候选收敛到候选窗口(默认 2000 个,可配),
  再实时读取候选文件内容匹配。**没有文件名约束时候选就是全部索引条目**,会被窗口截断
  (`truncated: true`)——内容检索应当先用文件名缩小范围。
* **内容词**:大小写折叠子串(与文件名词一致,CJK 可用),不支持通配符;多个 `content:`
  词 AND;每个词是单独一个空白分隔 token,无短语语法。
* **覆盖格式**:纯文本(txt/md/log/csv/源码等;BOM 探测 UTF-8/UTF-16,无 BOM 按 UTF-8
  解,内容含 NUL 字节判二进制跳过)与 OOXML/ODF 容器
  (.docx/.docm/.xlsx/.xlsm/.pptx/.pptm/.odt/.ods/.odp,解 zip 提取主文档 XML 的文本)。
  **不支持**:PDF、旧版二进制 Office(.doc/.xls/.ppt)、GBK/ANSI 编码、加密容器。
* **语义变化**:此时 `total_matched` = **内容匹配**的文件数;`limit`/`offset` 作用于内容
  匹配之后的分页;`query_ms` 含扫描耗时(可达秒级,传输层已把它放在阻塞线程池)。

```jsonc
{"type": "search", "data": {
    "total_matched": 3,
    "query_ms": 812,
    "limit": 100, "offset": 0,
    "results": [{"name": "a.docx", "path": "C:\\work\\a.docx", "is_dir": false,
                 "snippet": "…Q3 预算方案…", "content_matches": 4}],
    "content": {"scanned": 42, "skipped_size": 1, "skipped_binary": 3,
                "errors": 0, "truncated": false, "timed_out": false}
}}
```

`content` 各计数字段把扫过的候选一一归类:`scanned` 读完并匹配的;`skipped_size` 超过
单文件上限(默认 8 MiB);`skipped_binary` 判二进制;`errors` 打不开或解析失败(文件
刚被删/改名、被独占锁定等,属常态而非故障)。`truncated` 表示候选窗口截断(缩小文件名
条件可得全覆盖);`timed_out` 表示扫描预算(默认 10 s)用尽,结果是部分的。`snippet`
是首个命中点的上下文(前后各约 64 字符,空白折叠,`…` 表示省略),`content_matches`
是该文件内的命中总数。

护栏与开关(整体禁用、窗口、大小上限、预算、并发)见 [deploy.md](deploy.md) 的
`[content]` 配置段;引擎自身数据目录下的文件永不扫描。

### 响应

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

`phase`:`building` | `ready` | `failed`。

错误码:`1` 请求非法,`2` 未就绪,`3` 内部错误,`4` token 缺失或不对(仅 HTTP:
pipe 的连接本身带着 Windows 身份,`4` 不会出现在 pipe 上)。

## HTTP API(仅 127.0.0.1)

```
GET /                          # 同源的演示 UI(浏览器需带 token,见下)
GET /api/v1/search?q=<query>&limit=100&offset=0&sort=none&match_path=false
GET /api/v1/status
POST /api/v1/snapshot          # 手动落盘快照
```

每个请求都要带 `x-wfs-token: <http.token 的内容>`。地址栏没法填请求头,所以打开演示 UI
时用查询参数:`http://127.0.0.1:15100/?token=…`;页面加载后,它自己的 `fetch` 会改回头部。
token 在服务启动时随机生成并写入数据目录的 `http.token`(权限按 `acl` 收紧)。

`match_path` 接受 `1` / `true`(大小写不敏感),其余值视为 `false`。

响应即上表 `data` 部分(SearchResp / StatusResp JSON),错误以对应 HTTP 状态码 + `ErrorPayload` 返回
(空查询 `q` → `400` + code 1,token 不对 → `401` + code 4)。

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
