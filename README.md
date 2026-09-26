[中文](README.md) | [English](README.en.md)

# WFSearch — Windows 文件搜索底层服务

一个 Everything 风格的**文件名搜索引擎**:直接读取 NTFS MFT 建立全量内存索引,用 USN Journal 做实时增量更新,通过 **Named Pipe + 本地 HTTP** 供任意应用程序查询。目标是**速度极快、占用极低**:

| 指标 | 目标 | 实测¹ |
|---|---|---|
| 全量索引(百万文件,SSD) | < 15 s | **2.2 s**(124.6 万文件) |
| 单次查询(百万文件) | < 30 ms(P99) | **10.0 ms**(引擎)/ 14.7 ms(HTTP 往返) |
| 空闲 CPU | ≈ 0%(100ms 一次 journal 轮询,单次 syscall) | —(脚本未覆盖) |
| 内存 | ≤ ~100 MB / 百万文件 | **87 B / 文件**(约 103 MB) |
| 变更可见延迟 | ≤ 1 s | **创建 72 ms / 删除 75 ms** |
| 带有效快照重启恢复 | ≤ 3 s | **0.5 s** |

> ¹ 实测来自 2026-09-26 的一次真实盘验收(C 盘,124.6 万文件,管理员终端运行
> [`scripts/acceptance.ps1`](scripts/acceptance.ps1)),五项门禁全部 PASS。
> 换机器请重跑该脚本,以本机输出为准;查询引擎的合成基准见
> [docs/benchmarks.md](docs/benchmarks.md)。

> v1 范围:**仅文件名/路径搜索**(子串 + `*` `?` 通配符,大小写不敏感,支持中文)。内容全文检索、拼音匹配、ReFS/网络盘不在本期范围。

## 验证状态

| 项目 | 状态 | 依据 |
|---|---|---|
| 查询引擎(匹配语义、排序、路径匹配) | **已验证** | 20 个单测 + criterion 基准(合成 100 万条) |
| 协议 JSON 契约(pipe/HTTP 载荷) | **已验证** | wfs-proto 契约测试 + pipe/HTTP 端到端测试(真实管道与 socket) |
| 快照格式(写入/校验/拒绝损坏镜像) | **已验证** | 往返测试 + 损坏/异版本镜像拒绝测试 |
| MFT 全量枚举、USN journal 增量 | **已验证** | 真实盘验收:全量构建 + 创建/删除可见延迟 < 100 ms;字节级记录解析单测(含 FRN 序列号位回归) |
| 性能/内存目标(5/6 项) | **已验证(单机)** | acceptance.ps1 五项 PASS(实测见上表);空闲 CPU 项脚本未覆盖 |
| 服务模式安装/SCM 生命周期 | **已实现待验证** | 代码完整;需在目标机 `install` + `sc start` 实测 |
| 内容全文检索、拼音、ReFS/网络盘 | **未实现** | 不在 v1 范围 |


## 工作原理

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

- **全量构建**:一次性从卷句柄枚举全部 MFT 文件记录(FRN/父FRN/文件名),百万文件秒级。文件引用号在解析时统一剥掉高位的 NTFS 序列号(只保留 48 位记录号),父子锚定才不会失配。
- **实时更新**:每 ~100ms 读一次 USN Journal,批量应用 create/delete/rename;journal 回绕或删除时自动全量重建。
- **冷启动**:索引 + journal 位置序列化到 `index.bin`;重启时校验 journal 未回绕则直接续跑,否则重建。
- **删除策略**:墓碑标记(BFS 清子树),墓碑比例超阈值自动重建回收内存。

## 快速开始

```powershell
# 1. 开发者控制台模式(需要管理员权限,MFT 访问要求提升)
cargo run --release -p wfs-server -- console

# 2. 另开终端查询
cargo run --release -p wfs-client -- search "*.rs"     # named pipe
cargo run --release -p wfs-client -- status --http     # HTTP 通道
cargo run --release -p wfs-client -- search "src\core" --match-path   # 按路径匹配
curl "http://127.0.0.1:15100/api/v1/search?q=%E9%A1%B9%E7%9B%AE&limit=10"

# 3. 安装为 Windows 服务(管理员)
wfs-server.exe install
sc start WFSearch

# 4. 真实磁盘验收(管理员终端,跑一次拿到全部指标)
powershell -ExecutionPolicy Bypass -File scripts\acceptance.ps1
```

REPL 命令:`直接输入查询词`、`status`、`quit`。查询语法:`report *.docx C:`(多词 AND、通配符、盘符过滤);
词里含 `\` 或 `/` 时自动按路径匹配(`src\core`),也可用 `--match-path` 把全部词按路径匹配。

卷起不来时先用 `doctor` 自检(管理员终端,逐步打印每个 ioctl 的结果,失败时 exit code 1):

```powershell
wfs-server.exe doctor C        # 打开卷 / 查 journal / 全量枚举 / 读 journal 四步逐条报结果
wfs-server.exe doctor          # 不带盘符:探测配置里(或自动识别)的所有盘
```

## 仓库结构

```
crates/
├── wfs-proto/    协议类型(JSON serde),两种传输共用(6 个契约测试)
├── wfs-core/     内存索引 + 查询引擎(纯逻辑,20 个单测 + criterion 基准)
├── wfs-fs/       MFT 枚举、USN Journal(手写 kernel32 FFI + 字节级记录解析,17 个单测)
├── wfs-server/   服务二进制:console/service 模式、pipe+http、快照、SCM 生命周期
│                 (11 个测试:含 pipe/HTTP 端到端、快照往返)
└── wfs-client/   Rust SDK(参考实现)+ wfs-cli 调试工具(3 个测试:URL 编码 + CLI 参数回归)
scripts/
└── acceptance.ps1  真实磁盘验收(管理员跑一次:doctor 自检 + 冷/热启动、延迟、可见性,逐条 PASS/FAIL)
docs/
├── protocol.md        线上协议完整参考(英文:protocol.en.md)
├── deploy.md          部署/配置/服务管理(英文:deploy.en.md)
├── clients.md         Python / C# 接入示例(英文:clients.en.md)
└── benchmarks.md      基准数据与 RwLock/ArcSwap 决策依据(英文:benchmarks.en.md)
```

CI(GitHub Actions,windows-latest)对 `cargo fmt --check`、`cargo clippy -D warnings`、
`cargo test`、release 构建做门禁;`.github/workflows/ci.yml` 里的 bench job 仅手动触发。

## 配置

`%ProgramData%\WFSearch\config.toml`(不存在则用默认值;也可 `--config <FILE>` 指定):

```toml
drives = ["auto"]          # 或 ["C", "D"]
http_port = 15100          # 仅绑定 127.0.0.1
pipe_name = "\\\\.\\pipe\\wfs-engine-v1"
poll_ms = 100              # journal 轮询间隔
max_limit = 1000           # 单次查询 limit 上限
pipe_acl = "open"          # 管道 DACL:open(默认,本机任意用户)| restricted(仅 SYSTEM + 管理员)
```

`pipe_acl = "restricted"` 适用于"只有特定服务/管理员能查询"的场景;写错的值会回退到
`open` 并在 stderr 大声告警(不会静默降级)。注意 pipe 的 ACL **不影响 HTTP 通道**:
HTTP 始终只绑回环,但本机任何用户都能访问 —— 要完全收紧得两个通道一起考虑。

## 文档

- [协议参考](docs/protocol.md)([EN](docs/protocol.en.md)) — Named Pipe 帧格式 + HTTP API
- [部署指南](docs/deploy.md)([EN](docs/deploy.en.md)) — 服务安装、配置、故障排查
- [接入示例](docs/clients.md)([EN](docs/clients.en.md)) — Python / C# / Rust 代码
- [基准测试](docs/benchmarks.md)([EN](docs/benchmarks.en.md)) — 实测数据、复现方式、真实磁盘验收

## 已知边界(v1)

- 需要管理员权限(LocalSystem 服务已满足;console 模式需提升后的终端)。
- 仅 NTFS 固定盘(USN Journal 限制);ReFS/网络驱动器不支持。
- **路径匹配**(`match_path` 请求标志,或查询词里直接写 `\` / `/`)与 `sort=name|path`
  都需要对每个候选物化完整路径,比名字匹配慢 5~10×(实测见
  [benchmarks.md](docs/benchmarks.md));名字匹配才是快路径。
- 文件大小/修改时间未索引(枚举接口不含,后续 MFT 原始记录解析可补)。
