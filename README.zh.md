<div align="center">

# 🔍 WFSearch

**Windows Flash Search**

**Windows 上的极速文件名搜索服务。**
 
*整卷 NTFS MFT 常驻内存索引 · USN Journal 实时增量 · Named Pipe + 本地 HTTP 查询。*

[![CI](https://github.com/mnigc/WFSearch/actions/workflows/ci.yml/badge.svg)](https://github.com/mnigc/WFSearch/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Windows%20NTFS-0078D6)
![Language](https://img.shields.io/badge/language-Rust-dea584)

[English](README.md) · 中文

</div>

---

## ✨ 亮点

- ⚡ **毫秒级查询** —— 子串 + `*`/`?` 通配符,百万文件 P99 ≈ 10 ms;大小写不敏感、Unicode 折叠,中文无压力
- 📄 **文档内容检索** —— `content:` 词对候选文件实时扫描:纯文本(UTF-8/UTF-16)与 Office/ODF XML 容器,逐条返回命中上下文,磁盘零索引
- 🗂️ **秒级全量索引** —— 直接读 NTFS MFT,125 万文件约 2 秒
- 🔄 **实时增量** —— 每 100 ms 读一次 USN Journal,变更 < 100 ms 可见;journal 回绕自动全量重建
- 🪶 **占用极低** —— 约 87 字节/文件(125 万条约 103 MB),空闲 CPU ≈ 0%
- 🔌 **两种通道一套协议** —— Named Pipe 延迟最低,回环 HTTP 方便脚本接入;JSON 载荷完全一致
- 🩺 **自诊断** —— `doctor` 逐步探测每个 ioctl;验收脚本在真实磁盘上逐项实测全部指标

> **范围说明**:内容检索以查询时实时扫描形式提供(`content:` 词——不建后台索引、磁盘
> 零占用;不支持 PDF/旧版二进制 Office/ANSI 编码)。它**以服务账户身份读文件**,本机
> 登录用户可借此读到自己权限打不开的内容片段——见 [docs/deploy.md](docs/deploy.md) 的
> 安全边界说明。仍不在范围:后台全文倒排索引、拼音匹配、ReFS/网络盘。

## 📊 性能

| 指标 | 目标 | 实测 ¹ |
|---|---|---|
| 🗂️ 全量索引(百万文件,SSD) | < 15 s | **2.2 s**(125 万文件) |
| ⚡ 单次查询(百万文件,P99) | < 30 ms | **10.0 ms**(引擎)· 14.7 ms(HTTP 往返) |
| 🪶 内存 | ≤ ~100 MB / 百万文件 | **87 B / 文件**(约 103 MB) |
| 🔄 变更可见延迟 | ≤ 1 s | **创建 72 ms · 删除 75 ms** |
| 🔁 带有效快照重启 | ≤ 3 s | **0.5 s** |
| 🌙 空闲 CPU | ≈ 0% | —(脚本未覆盖) |

> ¹ 单次真实盘验收(2026-09-26,C 盘,125 万文件,管理员终端)运行
> [`scripts/acceptance.ps1`](scripts/acceptance.ps1),五项门禁全部 PASS。换机器请重跑,
> 以本机输出为准。查询引擎合成基准见 [docs/benchmarks.md](docs/benchmarks.md)。

## 🚀 快速开始

```powershell
# 1️⃣ 开发者控制台(需管理员权限,MFT 访问要求提升)
cargo run --release -p wfs-server -- console

# 2️⃣ 另开终端查询
cargo run --release -p wfs-client -- search "*.rs"     # named pipe
cargo run --release -p wfs-client -- status --http     # HTTP 通道(自己读 token)
cargo run --release -p wfs-client -- search "src\core" --match-path   # 按路径匹配
cargo run --release -p wfs-client -- search "*.md content:预算"        # 搜文档内容
$tok = Get-Content "$env:ProgramData\WFSearch\http.token"
curl "http://127.0.0.1:15100/api/v1/search?q=report&limit=10" -H "x-wfs-token: $tok"

# 3️⃣ 注册为 Windows 服务(管理员)。注册由部署方负责,exe 自己不再安装;
#    binPath 必须是绝对路径。
$exe = 'C:\Program Files\WFSearch\wfs-server.exe'
sc.exe create WFSearch binPath= "`"$exe`" run" obj= LocalSystem start= auto
sc.exe start WFSearch

# 4️⃣ 真实磁盘验收 —— 跑一次拿到全部指标(管理员)
powershell -ExecutionPolicy Bypass -File scripts\acceptance.ps1
```

**REPL** —— 直接输入查询词,或 `status` / `quit`。**查询语法** —— `report *.docx C:`:
多词 AND、通配符、盘符过滤;词里含 `\` 或 `/` 时自动按路径匹配,也可用 `--match-path` 强制全部按路径;
`content:词` 会对其余词筛出的候选文件做**内容**扫描(先用文件名缩小范围,详见
[docs/protocol.md](docs/protocol.md))。

**卷起不来?** 先问 doctor(管理员终端,逐步打印每个 ioctl 的结果,失败 exit code 1):

```powershell
wfs-server.exe doctor C        # 打开卷 → 查 journal → 全量枚举 → 读 journal
wfs-server.exe doctor          # 不带盘符:探测配置里的所有盘
```

## 🧠 工作原理

```
应用 ──Named Pipe──┐
                   ▼
Web ──127.0.0.1 HTTP──► wfs-server
                          │  查询:内存线性扫描(memchr/SIMD + rayon 并行)
                          ▼
                      wfs-core 索引(每卷一份:名字 arena + 24B/文件节点树)
                          ▲
              FSCTL_ENUM_USN_DATA 全量枚举 │ FSCTL_READ_USN_JOURNAL 增量
                          │
                        NTFS 卷
```

- **🗂️ 全量构建** —— 一次性枚举全部 MFT 文件记录(FRN/父FRN/文件名),百万文件秒级。
  文件引用号在解析时统一归一化(剥掉高位 16 位 NTFS 序列号),父子锚定不会失配。
- **🔄 实时增量** —— 每 ~100 ms 读一次 USN Journal,批量应用 create/delete/rename;
  journal 回绕或删除时自动全量重建。
- **🔁 冷启动** —— 索引 + journal 位置序列化到 `index.bin`;journal 未回绕则秒级续跑,否则重建。
- **🧹 删除策略** —— 墓碑标记(BFS 清子树);墓碑比例超阈值自动重建回收内存。

## 📄 文档内容检索(v0.2)

查询里写一个 `content:词`,整条查询就变成文档内容检索:

```powershell
wfs-cli search "*.md content:部署"      # 内容含"部署"的 markdown
wfs-cli search "report content:预算 C:"  # 与文件名/路径/盘符词自由 AND 组合
```

- **🔎 实时扫描,零索引** —— 先用文件名级条件把候选收敛到窗口(默认 2000 个文件),再
  逐个实时读取匹配。不落盘任何索引、无后台 CPU,结果永远与索引同步新鲜。
- **📝 覆盖格式** —— 纯文本(BOM 探测 UTF-8/UTF-16,NUL 字节判二进制跳过)与
  OOXML/ODF 容器(`.docx` `.docm` `.xlsx` `.xlsm` `.pptx` `.pptm` `.odt` `.ods` `.odp`,
  解 zip 提取文档 XML 剥为文本)。PDF 与旧版二进制 Office 不支持。
- **🧾 命中详情** —— 每条结果带首个命中点的 `snippet` 上下文与文件内命中次数;响应汇
  总本次扫描(扫描数/超限跳过/二进制跳过/失败,以及 `truncated`、`timed_out` 部分覆盖标志)。
- **⏱️ 护栏齐全** —— 单文件大小上限(8 MiB)、单次扫描时间预算(10 s)、独立的小读取
  线程池(不与名字搜索的 rayon 池抢核)。全部在 `[content]` 配置段可调;
  `enabled = false` 整体关闭。
- **🔐 安全** —— 以**服务账户身份**(服务模式即 LocalSystem)打开文件,查询者可借此读
  到自己权限打不开的内容片段。`acl = open` 且内容检索开启时,引擎启动会打一条显眼的
  WARN 日志;边界与闸门详见 [docs/deploy.md](docs/deploy.md) 的安全边界说明。

## ✅ 验证状态

| 项目 | 状态 | 依据 |
|---|---|---|
| 查询引擎(匹配、排序、路径匹配) | ✅ 已验证 | 28 个单测 + criterion 基准(合成 100 万条) |
| 协议 JSON 契约(pipe/HTTP 载荷) | ✅ 已验证 | 契约测试 + pipe/HTTP 端到端测试 |
| 文档内容扫描(文本 + OOXML/ODF、预算、截断) | ✅ 已验证 | wfs-content 16 个测试 + server 端真实文件端到端;已在 180 万文件实机运行 |
| 快照格式(写入/校验/拒绝损坏) | ✅ 已验证 | 往返测试 + 损坏/异版本镜像拒绝测试 |
| MFT 全量枚举、USN journal 增量 | ✅ 已验证 | 真实盘验收:全量构建 + 变更可见 < 100 ms;字节级解析单测(含 FRN 序列号位回归) |
| 性能/内存目标(6 项中 5 项) | ✅ 已验证(单机) | 验收脚本五项 PASS(见上表);空闲 CPU 项未覆盖 |
| 服务 / SCM 生命周期 | ✅ 已验证(单机) | `sc create` + 原地换版本(停 → 改名 → 落新 → 起)实机跑通 |
| 后台全文倒排索引、拼音、ReFS/网络盘 | ❌ 不在范围 | 内容检索是查询时实时扫描;倒排索引仍不做 |

## 📁 仓库结构

```
crates/
├── wfs-proto/    协议类型(JSON serde),两种传输共用 —— 7 个契约测试
├── wfs-core/     内存索引 + 查询引擎(纯逻辑)—— 28 个单测 + criterion 基准
├── wfs-fs/       MFT 枚举、USN Journal(手写 kernel32 FFI + 字节级解析)—— 23 个单测
├── wfs-content/  文档内容提取与匹配(BOM 文本、OOXML/ODF、snippet)—— 16 个测试
├── wfs-server/   服务二进制:console/service 模式、pipe+http、快照、SCM、内容扫描 —— 34 个测试
└── wfs-client/   Rust SDK(参考客户端)+ wfs-cli 调试工具 —— 3 个测试
scripts/
└── acceptance.ps1   真实磁盘验收:doctor 自检 + 冷/热启动、延迟、可见性
docs/                双语文档(中文为主,英文为 *.en.md)—— 见 📚 文档
```

CI(GitHub Actions,`windows-latest`)对 `cargo fmt --check`、`clippy -D warnings`、
`cargo test`、release 构建做门禁;bench job 仅手动触发。

## ⚙️ 配置

`%ProgramData%\WFSearch\config.toml`(不存在则用默认值;也可 `--config <FILE>`):

```toml
drives = ["auto"]          # 或 ["C", "D"]
http_port = 15100          # 仅绑定 127.0.0.1
pipe_name = "\\\\.\\pipe\\wfs-engine-v1"
poll_ms = 100              # journal 轮询间隔
max_limit = 1000           # 单次查询 limit 上限
acl = "open"               # 谁能访问引擎:open(默认)| restricted

[content]                  # 文档内容检索(content: 词)
enabled = true             # false 时 content: 查询直接报错
max_candidates = 2000      # 单次扫描的候选窗口
max_file_bytes = 8388608   # 单文件上限(8 MiB)
timeout_ms = 10000         # 单次扫描时间预算
max_concurrency = 4        # 并发读取线程数
```

> 💡 `acl = "restricted"` 同时收紧**两个通道**,只放行 SYSTEM + Administrators;`open`
> 放行所有控制台登录用户。写错的值回退 `open` 并大声告警,不会静默降级。pipe 靠 DACL
> 鉴权(操作系统白送);HTTP 靠 bearer token —— 令牌发布在 `%ProgramData%\WFSearch\http.token`,
> 因为回环连接不携带用户身份。**能读到这个文件就等于有凭证**,而 `acl` 正是控制这一点的。

## 📚 文档

| | 中文 | English |
|---|---|---|
| 📡 线上协议(pipe 帧格式 + HTTP API) | [protocol.md](docs/protocol.md) | [protocol.en.md](docs/protocol.en.md) |
| 🛠️ 部署、配置、故障排查 | [deploy.md](docs/deploy.md) | [deploy.en.md](docs/deploy.en.md) |
| 🧩 客户端接入示例(Python / C# / Rust) | [clients.md](docs/clients.md) | [clients.en.md](docs/clients.en.md) |
| 📈 基准与验收数据 | [benchmarks.md](docs/benchmarks.md) | [benchmarks.en.md](docs/benchmarks.en.md) |

## ⚠️ 已知边界

- 🔐 需要管理员权限(LocalSystem 服务已满足;console 模式需提升后的终端)。
- 💾 仅 NTFS 固定盘 —— USN Journal 限制;ReFS/网络驱动器不支持。
- 🐢 **路径匹配**(`match_path`,或词里直接写 `\` / `/`)与 `sort=name|path`
  都要对每个候选物化完整路径,比名字匹配慢 5~10×;名字匹配才是快路径。
- 🔎 **内容检索**仅覆盖纯文本 + OOXML/ODF —— 不含 PDF、旧版二进制 Office
  (`.doc`/`.xls`/`.ppt`)、ANSI/GBK 编码与加密容器。查询不带文件名条件时候选窗口按
  索引序任意截断——先用文件名缩小范围。它以服务账户身份读文件,见上方安全说明。
- 📏 文件大小/修改时间未索引(后续 MFT 原始记录解析可补)。
