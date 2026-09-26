<div align="center">

# 🔍 WFSearch

**Windows 上的极速文件名搜索服务。**

*整卷 NTFS MFT 常驻内存索引 · USN Journal 实时增量 · Named Pipe + 本地 HTTP 查询。*

[![CI](https://github.com/mnigc/WFSearch/actions/workflows/ci.yml/badge.svg)](https://github.com/mnigc/WFSearch/actions/workflows/ci.yml)
![Platform](https://img.shields.io/badge/platform-Windows%20NTFS-0078D6)
![Language](https://img.shields.io/badge/language-Rust-dea584)

[English](README.md) · 中文

</div>

---

## ✨ 亮点

- ⚡ **毫秒级查询** —— 子串 + `*`/`?` 通配符,百万文件 P99 ≈ 10 ms;大小写不敏感、Unicode 折叠,中文无压力
- 🗂️ **秒级全量索引** —— 直接读 NTFS MFT,125 万文件约 2 秒
- 🔄 **实时增量** —— 每 100 ms 读一次 USN Journal,变更 < 100 ms 可见;journal 回绕自动全量重建
- 🪶 **占用极低** —— 约 87 字节/文件(125 万条约 103 MB),空闲 CPU ≈ 0%
- 🔌 **两种通道一套协议** —— Named Pipe 延迟最低,回环 HTTP 方便脚本接入;JSON 载荷完全一致
- 🩺 **自诊断** —— `doctor` 逐步探测每个 ioctl;验收脚本在真实磁盘上逐项实测全部指标

> **v1 范围**:仅文件名/路径搜索。内容全文检索、拼音匹配、ReFS/网络盘不在本期范围。

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
cargo run --release -p wfs-client -- status --http     # HTTP 通道
cargo run --release -p wfs-client -- search "src\core" --match-path   # 按路径匹配
curl "http://127.0.0.1:15100/api/v1/search?q=report&limit=10"

# 3️⃣ 安装为 Windows 服务(管理员)
wfs-server.exe install
sc start WFSearch

# 4️⃣ 真实磁盘验收 —— 跑一次拿到全部指标(管理员)
powershell -ExecutionPolicy Bypass -File scripts\acceptance.ps1
```

**REPL** —— 直接输入查询词,或 `status` / `quit`。**查询语法** —— `report *.docx C:`:
多词 AND、通配符、盘符过滤;词里含 `\` 或 `/` 时自动按路径匹配,也可用 `--match-path` 强制全部按路径。

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

## ✅ 验证状态

| 项目 | 状态 | 依据 |
|---|---|---|
| 查询引擎(匹配、排序、路径匹配) | ✅ 已验证 | 20 个单测 + criterion 基准(合成 100 万条) |
| 协议 JSON 契约(pipe/HTTP 载荷) | ✅ 已验证 | 契约测试 + pipe/HTTP 端到端测试 |
| 快照格式(写入/校验/拒绝损坏) | ✅ 已验证 | 往返测试 + 损坏/异版本镜像拒绝测试 |
| MFT 全量枚举、USN journal 增量 | ✅ 已验证 | 真实盘验收:全量构建 + 变更可见 < 100 ms;字节级解析单测(含 FRN 序列号位回归) |
| 性能/内存目标(6 项中 5 项) | ✅ 已验证(单机) | 验收脚本五项 PASS(见上表);空闲 CPU 项未覆盖 |
| 服务安装 / SCM 生命周期 | 🚧 已实现待验证 | 需在目标机 `install` + `sc start` 实测 |
| 全文检索、拼音、ReFS/网络盘 | ❌ v1 不含 | 不在范围 |

## 📁 仓库结构

```
crates/
├── wfs-proto/    协议类型(JSON serde),两种传输共用 —— 6 个契约测试
├── wfs-core/     内存索引 + 查询引擎(纯逻辑)—— 20 个单测 + criterion 基准
├── wfs-fs/       MFT 枚举、USN Journal(手写 kernel32 FFI + 字节级解析)—— 17 个单测
├── wfs-server/   服务二进制:console/service 模式、pipe+http、快照、SCM —— 11 个测试
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
pipe_acl = "open"          # 管道 DACL:open(默认)| restricted(仅 SYSTEM + 管理员)
```

> 💡 `pipe_acl = "restricted"` 把 *pipe* 查询限制为 SYSTEM + Administrators;写错的值
> 回退 `open` 并大声告警,不会静默降级。注意 pipe 的 ACL **不影响 HTTP 通道**:HTTP
> 始终只绑回环,但本机任何用户都能访问 —— 要完全收紧得两个通道一起考虑。

## 📚 文档

| | 中文 | English |
|---|---|---|
| 📡 线上协议(pipe 帧格式 + HTTP API) | [protocol.md](docs/protocol.md) | [protocol.en.md](docs/protocol.en.md) |
| 🛠️ 部署、配置、故障排查 | [deploy.md](docs/deploy.md) | [deploy.en.md](docs/deploy.en.md) |
| 🧩 客户端接入示例(Python / C# / Rust) | [clients.md](docs/clients.md) | [clients.en.md](docs/clients.en.md) |
| 📈 基准与验收数据 | [benchmarks.md](docs/benchmarks.md) | [benchmarks.en.md](docs/benchmarks.en.md) |

## ⚠️ 已知边界(v1)

- 🔐 需要管理员权限(LocalSystem 服务已满足;console 模式需提升后的终端)。
- 💾 仅 NTFS 固定盘 —— USN Journal 限制;ReFS/网络驱动器不支持。
- 🐢 **路径匹配**(`match_path`,或词里直接写 `\` / `/`)与 `sort=name|path`
  都要对每个候选物化完整路径,比名字匹配慢 5~10×;名字匹配才是快路径。
- 📏 文件大小/修改时间未索引(后续 MFT 原始记录解析可补)。
