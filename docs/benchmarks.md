# 基准测试

两级验证,数据来源不同,别混着看:

| | 测什么 | 怎么跑 | 数据在哪 |
|---|---|---|---|
| **合成 100 万条**(本页) | 查询引擎本身:解析 → 并行扫描 → 合并 → 切片 | `cargo bench -p wfs-core --bench scan` | 本页表格 |
| **真实 C: 盘**(README 目标表) | 端到端:全量构建耗时、内存/文件、变更可见延迟、暖启动 | 管理员终端 `powershell -ExecutionPolicy Bypass -File scripts\acceptance.ps1` | 脚本输出(见文末) |

合成索引的条目名按 4 种模式轮转(`report_0000000.docx` / `项目文件夹_0000000.txt` /
`IMG_202600000.jpg` / `notes-0000000.md`),其中 1/16 标记为目录。索引构建本身
(100 万条 `insert`)约 **0.13 s**。

## 查询延迟(criterion,100 万条,release)

每组 30 个样本,数值为均值:

| 用例 | 查询 | 均值 | 说明 |
|---|---|---|---|
| `match-all` | 空 | **0.44 ms** | 全量扫描的下限(无匹配判定开销) |
| `substr-ascii-common` | `report` | **1.30 ms** | 命中 25 万条,`memchr` ASCII 快路径 |
| `substr-ascii-rare` | `report_0099` | **1.63 ms** | 命中稀疏时的上限 |
| `substr-cjk` | `文件夹` | **2.14 ms** | Unicode 折叠路径,比 ASCII 慢约 1.6× |
| `multi-term` | `report 2026` | **1.54 ms** | 双词 AND(先判便宜的词) |
| `wildcard` | `*.docx` | **4.25 ms** | 通配符回溯,最贵的一类名字匹配 |
| `wildcard-cjk` | `项目文件夹_000*` | **2.97 ms** | 零分配 `match_chars`(第三档改动) |
| `sort-name` | `report` + `sort=name` | **8.07 ms** | 需收集全部 25 万命中再排序 |
| `path-term` | `文件夹_0000\0` | **11.77 ms** | 含 `\` 的 term 自动按路径匹配:逐候选物化路径 |
| `match-path-flag` | `report` + `match_path=true` | **10.76 ms** | 同上,但 25 万命中全部要物化路径(最坏情况) |
| `sort-path` | `report` + `sort=path` | **26.01 ms** | 路径排序:收集 + 逐条物化 + 排序(`sort_by_cached_key`,每条只物化一次) |

结论:

- **默认路径(不带排序/不带路径匹配)在 1~5 ms 量级**,远低于 README 的 30 ms(P99)目标。
- **路径相关功能(路径匹配、路径排序)贵 5~10×**,因为它要求对每个候选构造一次完整路径字符串。
  这是设计中明确的取舍(见 README 已知边界),不是回归。
- 中文只贵约 1.6×,通配符最贵,但都在毫秒级。

## 索引发布成本(RwLock vs ArcSwap 的决策依据)

方案评审时提出用 `ArcSwap` 换掉 `parking_lot::RwLock`,让查询完全无锁。前提是
**发版一次要便宜**:每批增量更新都得克隆整个 `VolumeIndex`(`Vec<Node>` + `frn_index`
+ `children` 两个哈希表),否则 `ArcSwap` 就不会更快。

| 用例 | 均值 |
|---|---|
| `clone-1m-volume-index`(发版一次的成本) | **17.97 ms** |
| `rwlock-read-1m`(要替换掉的读锁成本) | **≈ 0(亚微秒)** |

**结论:保留 `RwLock`。** 发版成本比读锁成本高 4~5 个数量级:每批事件克隆 18 ms 会
直接吃掉 journal 轮询间隔(poll_ms=100),而读锁本身的开销在测量精度之下。
除非将来把索引改成可共享的不可变结构(如 arena + 只拷贝元数据),这个决定才需要重评。

## 复现

```powershell
# 全部基准(release 编译,criterion 需要几分钟)
cargo bench -p wfs-core --bench scan

# 只跑一轮热身/短测量(改代码前后快速对比)
cargo bench -p wfs-core --bench scan -- --warm-up-time 1 --measurement-time 3 --sample-size 20
```

注意必须显式指定 `--bench scan`:不带目标时 cargo 会把参数同时传给 lib 测试 harness,
后者会因为不认识 criterion 的参数而报 `Unrecognized option`。

`criterion` 的历史报告在 `target/criterion/<组>/<用例>/report/index.html`(base/new 对比)。

## 真实磁盘验收

合成索引测不到的东西——MFT 枚举速度、USN journal 落地延迟、快照恢复时间——用
[`scripts/acceptance.ps1`](../scripts/acceptance.ps1) 在**管理员**终端跑一次,它会:

1. 冷启动(全量 MFT 构建)→ 记录到 `phase=ready` 的耗时、索引文件数、内存/文件;
2. 200 轮 × 3 种查询 → 报 `query_ms` 的 P50/P99(引擎侧)与 HTTP 往返 P50/P99;
3. 在 `%TEMP%` 建/删一个唯一命名的文件 → 记录索引可见延迟;
4. `quit` 落盘后重启 → 记录暖启动耗时;
5. 把结果与 README 的 5 项目标逐条 PASS/FAIL。

脚本只写 `%TEMP%` 下的临时数据目录,不碰 `%ProgramData%`,也不安装服务。输出为纯 ASCII,
可直接贴进 issue。

> 本页的表格是实测值;**README 的目标表在没有跑过该脚本的机器上属于未验证目标**
> (见 README「验证状态」)。
