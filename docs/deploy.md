[中文](deploy.md) | [English](deploy.en.md)

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

注册由**部署方**(或嵌入引擎的宿主程序)负责,`wfs-server.exe` 自身没有 `install`/`uninstall`
子命令 —— 服务只认 `binPath` 里的那个字符串,而这个二进制不该替别人决定装到哪、装几次。

```powershell
# 管理员终端。binPath 必须是绝对路径;结尾的 `run` 是 SCM 调用的入口子命令。
$exe = 'C:\Program Files\WFSearch\wfs-server.exe'
sc.exe create WFSearch binPath= "`"$exe`" run" obj= LocalSystem start= auto DisplayName= "WFSearch File Search Engine"
sc.exe description WFSearch "Fast NTFS filename search engine (MFT index + USN journal). Query via named pipe \\.\pipe\wfs-engine-v1 or http://127.0.0.1:15100."
sc.exe start WFSearch
wfs-cli.exe status                     # 验证

sc.exe stop WFSearch                   # 停止(自动保存快照)
sc.exe delete WFSearch                 # 卸载注册(先停止)
```

`obj= LocalSystem` 是 MFT 访问的硬要求。换版本走下面的"停 → 改名 → 落新 → 起",
**不要**删了重建服务(重建会丢掉 `binPath` 之外的一切,还要重新拷文件)。

服务日志**追加**到 `%ProgramData%\WFSearch\wfs.log`(超过 16MB 时启动时轮转为 `wfs.log.old`)——
服务进程没有控制台,不落文件就什么也看不到。调试时可先 `console` 模式排查(日志到 stderr),
或设置环境变量 `RUST_LOG=debug`。

## 作为组件集成(由宿主软件更新 exe)

发布渠道是 GitHub Releases:推一个 `v*` tag,CI 构建并把两个 exe、两个 `.sha256` 和一份
`latest.json` 挂上 Release。宿主只认这一个入口,它不需要 API token,因此不受匿名
`/repos/.../releases/latest` 每小时 60 次的限流:

```
https://github.com/mnigc/WFSearch/releases/latest/download/latest.json
```

```jsonc
{
  "version": "0.1.1", "tag": "v0.1.1", "protocol": 1,
  "released_at": "2026-09-27T07:48:08Z",
  "assets": {
    "wfs-server.exe": { "url": "…/releases/download/v0.1.1/wfs-server.exe",
                        "sha256": "9408…f8b6", "size": 3483136 }
  }
}
```

`url` 指向固定版本(想灰度就别追 latest),`protocol` 供宿主判断能力。exe 也始终可以用
`releases/latest/download/wfs-server.exe` 直接拿最新版。

### 换文件:停 → 改名 → 落新 → 起

服务的 `binPath` 是写死的,所以**文件名必须保持不变**,换版本因此是这四步:

1. **查**:`GET latest.json`,比 `version` 与本地记录;一样就结束。
2. **下**:下到临时文件,**校验 SHA-256** 后再用。从浏览器/HTTP 下来的文件带
   Zone.Identifier 数据流,不清掉会在首次运行时触发 SmartScreen:
   `Remove-Item -LiteralPath $exe -Stream Zone.Identifier -ErrorAction SilentlyContinue`。
3. **停 + 换**:`sc stop WFSearch`,轮询 `sc query` 直到 STOPPED(子进程形态则
   `TerminateProcess` + **等进程真正退出**,.NET 的 `Kill()` 是异步的,不等会撞文件锁)。
   然后把旧文件**重命名**为 `wfs-server.exe.old` —— Windows 允许重命名正在运行的 exe,
   所以即使第 3 步没停成功这一步也总能做完,同时它天然就是回滚副本;再把新 exe 放到原路径。
   `binPath` 没变,**不需要** `sc config`,也不需要重装服务。
4. **起 + 定论**:`sc start WFSearch`,等 `/api/v1/status` 里各卷 `phase` 到 `ready`。
   成功了才删掉 `.old`;失败就把 `.old` 改回原名再 `sc start` 一次。快照保证重启是暖启动,
   1~2 秒内就能判断成败,不必重建索引。

残留的 `.old` 留给**下次宿主启动时**清理(改名后若立刻拉起新进程,旧文件当时删不掉)。

### 集成的前提承诺

以下三条是对宿主(以及本项目后续版本)的承诺,破坏任何一条"替换一个文件"就不再成立:

- **单一 exe、零外部 DLL**:不引入需要随包分发的原生依赖。
- **数据目录固定在 `%ProgramData%\WFSearch\`**:与 exe 所在路径和文件名无关,换位置不丢索引。
- **HTTP 协议与快照格式向后兼容**:宿主可能停留在旧版 exe;字段只增不改语义。

走 HTTP 的宿主还要知道一点:**token 每次启动换发**,不要缓存 —— 拉起(或重启)引擎后重新读一次
`%ProgramData%\WFSearch\http.token`。嫌麻烦就改用 named pipe,DACL 由操作系统鉴权,没有凭据要管。

## 真实磁盘验收

管理员终端跑一次,拿到冷启动/查询延迟/变更可见延迟/暖启动/内存五项指标,并逐条对照 README 目标:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\acceptance.ps1
```

脚本只写 `%TEMP%` 下的临时数据目录并起一个 console 模式进程,**不安装服务、不碰 `%ProgramData%`**;
流程是:卷自检(`doctor`)→ 冷启动(全量 MFT 构建)→ 200 轮 × 3 种查询延迟 → `%TEMP%` 建/删文件的可见延迟 →
`quit` 落盘后重启测暖启动。输出为纯 ASCII,可直接贴出;第一步带上 `doctor` 结果,之后任何一步失败都会
先停引擎、再把引擎日志里 `volume/ERROR/WARN/journal/panic` 命中的行打印出来(引擎以
`RUST_LOG=info,wfs_server=debug,wfs_fs=debug` 启动,所以日志里也有 journal 的 records→events 计数)。

## 卷自检(doctor)

索引建不起来时,不要靠猜——`doctor` 把 `run_volume` 的每一步单独跑一遍并打印内核返回:

```powershell
wfs-server.exe doctor C
```

```
volume C:
  open \\.\C:            ok
  FSCTL_QUERY_USN_JOURNAL: ok - id 0x1d95f206cae361d, next_usn 49521681376, lowest_valid 0, max 64 MB, record versions (2, 4)
  ensure USN journal      : ok - id 0x1d95f206cae361d
  FSCTL_ENUM_USN_DATA     : ok - 1433097 records (24-byte input, first entry "System Volume Information")
  FSCTL_READ_USN_JOURNAL  : ok - 0 pending record(s), 0 event(s), 0 unparsable, next_usn 49521681376
  journal history         : usn 49521681376..49521681376 - 0 record(s), 0 event(s), 0 unparsable [0 create, 0 delete, 0 rename]
```

任一行失败即打印 `FAILED - <原因>` 并以 exit code 1 结束(便于脚本判断)。`open` 失败且原因是
`access denied` 时说明当前进程没提升;`32-byte input` 表示内核只接受带版本号的
`MFT_ENUM_DATA_V1`(新版本 Windows 可能如此,引擎会自动切换);`0 records` 说明枚举被内核
当场判定为空(历史上是 `HighUsn` 写成了 `u64::MAX`,即有符号 USN 域里的 -1)。

最后两行是"新文件搜不到"的定位依据,注意区分 `record(s)` 与 `event(s)`:
`journal history` 从卷上最早的记录重放一遍(NTFS 的 journal 只保留最近一段,这里通常是几十万条),
`records > 0` 但 `events == 0` 表示内核交出的记录不是 `USN_RECORD_V2`(索引器读不了,日志里会
有一条对应的 `not USN_RECORD_V2` 警告);`records == 0` 表示这卷的 journal 里确实没有可重放的记录。
这两行只用于排查,不影响退出码。

## 数据与配置

| 文件 | 路径 | 说明 |
|---|---|---|
| 配置 | `%ProgramData%\WFSearch\config.toml` | 不存在则全默认;也可 `--config <FILE>` |
| 快照 | `%ProgramData%\WFSearch\index.bin` | 索引 + journal 位置,原子写(tmp + rename) |
| 服务日志 | `%ProgramData%\WFSearch\wfs.log` | 仅服务模式;>16MB 时轮转为 `.log.old` |
| HTTP token | `%ProgramData%\WFSearch\http.token` | 启动时随机生成;HTTP 通道的凭据,DACL 跟着 `acl` 走 |
| 自定义数据目录 | config 里 `data_dir = "D:\\wfsdata"` | 快照、日志与 token 都放这里 |

首次启动:逐盘全量枚举(期间 `/status` 显示 `building` 与进度);构建完成自动落一次快照;之后每次优雅退出(服务停止 / Ctrl-C)都会刷新快照。重启时若 journal 未回绕则秒级恢复,否则该盘自动重建。

## 查询验证

```powershell
wfs-cli.exe search "*.docx"
wfs-cli.exe search "report C: 2026" --limit 50 --sort name
wfs-cli.exe search "src\core" --match-path         # 整条路径匹配
wfs-cli.exe search "*.md" --http                     # 走 HTTP 通道(自己读 http.token)
wfs-cli.exe search "*.md" --http --token <值>         # 数据目录非默认时手动给 token
wfs-cli.exe status
```

访问控制由 `acl` 一项管两个通道:`open`(默认)放行本机登录用户;`restricted` 只放行
SYSTEM + Administrators —— 此时 wfs-cli 两种通道都要在管理员终端里跑,演示 UI 也只有管理员
打得开(它读不到 token)。写错的值回退为 `open` 并在 stderr 告警。

浏览器打开演示 UI:`http://127.0.0.1:15100/?token=<Get-Content $env:ProgramData\WFSearch\http.token>`
(地址栏填不了请求头,所以首页认 `?token=`;页面自己发请求时改回头部)。不带 token 会看到一页
中文说明,而不是搜索结果。

## 常见问题

| 现象 | 原因与处理 |
|---|---|
| `status` 里 volume `failed` | 先跑 `wfs-server.exe doctor <盘>`:它逐步报出是哪个 ioctl 失败、Win32 错误码是什么。原因通常是 console 模式没提权、盘不存在、或卷上 USN journal 不可用 |
| 新文件搜不到 | 查询在 1s 内属正常窗口;若一直搜不到,先跑 `doctor`(看上一节的 `journal history`),再以 `RUST_LOG=info,wfs_server=debug,wfs_fs=debug` 起引擎:watch 循环每 15s 打一行 `journal at usn <n> - <polls> poll(s), <records> record(s) read so far`,有变化时打 `journal +N records -> M events`。位置不动或 `records > 0, events == 0` 即可判定是 journal 读取还是解析的问题 |
| pipe 连接拒绝 | 服务未启动、`pipe_name` 配置不一致,或 `acl = "restricted"` 而客户端非管理员 |
| pipe 启动即退出 | 同名的 pipe 已被占用(已有实例在跑):`--config` 换 `pipe_name`,或先停掉旧进程 |
| HTTP 返回 401 / `code:4` | 没带 `x-wfs-token`,或 token 与数据目录里的 `http.token` 不一致(引擎重启会换发、`acl` 改过、`data_dir` 换过都会) |
| HTTP 读不到 `http.token` | 该文件的 DACL 由 `acl` 决定,`restricted` 下非管理员进程读不到 —— 这正是收紧的开关,换管理员终端或改回 `open` |
| HTTP 端口冲突 | 改 `http_port`;仅监听 127.0.0.1,不对局域网暴露 |
| 内存增长 | 增删改产生墓碑,超过阈值(>1024 且 >5%)自动全量重建回收;可 `POST /api/v1/snapshot` 后手动重启加速回收 |
| 服务日志在哪 | `%ProgramData%\WFSearch\wfs.log`(console 模式则直接打到 stderr) |
