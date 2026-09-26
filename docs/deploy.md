# WFSearch 部署指南

## 构建

```powershell
cargo build --release
# 产物:
#   target\release\wfs-server.exe
#   target\release\wfs-cli.exe
```

## 开发调试(console 模式)

MFT/USN 访问需要提升权限,**必须用管理员终端**:

```powershell
wfs-server.exe console                 # 全部固定盘
wfs-server.exe --drives C,D console    # 指定盘
wfs-server.exe --http-port 15200 console
```

REPL:输入查询词直接搜索;`status` 查看索引进度;`quit` 退出并落盘快照。

## 生产部署(Windows 服务)

```powershell
# 管理员终端
wfs-server.exe install                 # 注册服务(LocalSystem,自启动)
sc start WFSearch                      # 启动
wfs-cli.exe status                     # 验证

sc stop WFSearch                       # 停止(自动保存快照)
wfs-server.exe uninstall               # 卸载注册
```

服务日志**追加**到 `%ProgramData%\WFSearch\wfs.log`(超过 16MB 时启动时轮转为 `wfs.log.old`)——
服务进程没有控制台,不落文件就什么也看不到。调试时可先 `console` 模式排查(日志到 stderr),
或设置环境变量 `RUST_LOG=debug`。

## 真实磁盘验收

管理员终端跑一次,拿到冷启动/查询延迟/变更可见延迟/暖启动/内存五项指标,并逐条对照 README 目标:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\acceptance.ps1
```

脚本只写 `%TEMP%` 下的临时数据目录并起一个 console 模式进程,**不安装服务、不碰 `%ProgramData%`**;
流程是:冷启动(全量 MFT 构建)→ 200 轮 × 3 种查询延迟 → `%TEMP%` 建/删文件的可见延迟 →
`quit` 落盘后重启测暖启动。输出为纯 ASCII,可直接贴出。

## 数据与配置

| 文件 | 路径 | 说明 |
|---|---|---|
| 配置 | `%ProgramData%\WFSearch\config.toml` | 不存在则全默认;也可 `--config <FILE>` |
| 快照 | `%ProgramData%\WFSearch\index.bin` | 索引 + journal 位置,原子写(tmp + rename) |
| 服务日志 | `%ProgramData%\WFSearch\wfs.log` | 仅服务模式;>16MB 时轮转为 `.log.old` |
| 自定义数据目录 | config 里 `data_dir = "D:\\wfsdata"` | 快照与日志都放这里 |

首次启动:逐盘全量枚举(期间 `/status` 显示 `building` 与进度);构建完成自动落一次快照;之后每次优雅退出(服务停止 / Ctrl-C)都会刷新快照。重启时若 journal 未回绕则秒级恢复,否则该盘自动重建。

## 查询验证

```powershell
wfs-cli.exe search "*.docx"
wfs-cli.exe search "report C: 2026" --limit 50 --sort name
wfs-cli.exe search "src\core" --match-path         # 整条路径匹配
wfs-cli.exe search "*.md" --http                     # 走 HTTP 通道
wfs-cli.exe status
```

配置项里 `pipe_acl = "restricted"`(仅 SYSTEM + Administrators)会同时影响所有客户端:
此模式下 wfs-cli 必须以管理员身份运行(HTTP 通道不受影响)。写错的值回退为 `open` 并在 stderr 告警。

## 常见问题

| 现象 | 原因与处理 |
|---|---|
| `status` 里 volume `failed` | console 模式未用管理员终端;服务模式下检查盘是否存在 |
| 新文件搜不到 | 查询在 1s 内属正常窗口;持续查不到则看 journal 是否回绕(会自动重建) |
| pipe 连接拒绝 | 服务未启动、`pipe_name` 配置不一致,或配置了 `pipe_acl = "restricted"` 而客户端非管理员 |
| pipe 启动即退出 | 同名的 pipe 已被占用(已有实例在跑):`--config` 换 `pipe_name`,或先停掉旧进程 |
| HTTP 端口冲突 | 改 `http_port`;仅监听 127.0.0.1,不对局域网暴露 |
| 内存增长 | 增删改产生墓碑,超过阈值(>1024 且 >5%)自动全量重建回收;可 `POST /api/v1/snapshot` 后手动重启加速回收 |
| 服务日志在哪 | `%ProgramData%\WFSearch\wfs.log`(console 模式则直接打到 stderr) |
