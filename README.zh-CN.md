# agproc

**简体中文** | [English](README.md)

> 给 AI Agent 和人同时使用的**开发时进程管理器**（目前仅支持 Linux）

把项目的常驻服务（后端 / 前端 / worker）写进 `agproc.toml`，agproc 负责
**build → run → 探针**，并把进程状态与日志落在 `.agproc/`，
于是「反复调用」对人类和 Agent 都是安全的。

```bash
agproc start              # 并行启动所有 service，等待就绪
agproc ps                 # 谁在跑、pid、运行时长、失败原因
agproc restart backend    # 改完后端代码后的标准动作
agproc logs -f            # 带服务名前缀的实时日志
agproc stop               # 全部停掉
```

## 为什么需要它

| 问题 | agproc 的做法 |
|---|---|
| Agent 改代码频繁，反复触发构建/重启，甚至起出第二个 dev server | `start` 是**幂等**的：服务已在跑时只打印 `ALREADY RUNNING` 并返回 0，不 build、不重启。配合 `run-cmd` 使用**非 watch** 服务（如 `vite preview`），改完代码显式 `restart`，每次行为都可预期 |
| Rust 项目要先编译再运行，失败信息散落 | `build-cmd` + `run-cmd` 分离，编译失败有专属 marker 与**退出码 4**，Agent 无需解析文本 |
| 只按「进程还在」判断就绪，误报很多 | 内置 `http-get` / `tcp-connect` 探针，重试到 `failure-threshold` 才判定失败（**退出码 6**） |
| 端口被残留进程占着，探针"对着别人的进程"通过 | 端口预检告警 + 就绪瞬间的**归属校验** + 子进程退出事件优先，三重机制，绝不谎报成功 |
| 日志被管道块缓冲，就绪前的关键输出看不到 | 每个 stream 一个 PTY，子进程保持**行缓冲**，输出实时可见 |

## 安装

```bash
cargo install agproc          # 从 crates.io 安装（需要 Rust 1.89+）

# 或从源码安装
cargo install --path .
# 或只构建二进制
cargo build --release && install -m755 target/release/agproc ~/.local/bin/
```

## 快速开始

```bash
cd your-project
agproc init            # 生成 agproc.toml 模板（自动探测 Cargo.toml / package.json）
$EDITOR agproc.toml    # 填 build-cmd / run-cmd / probe
agproc start           # 启动全部并等待就绪
```

## 命令

```
agproc start   [service...] [--timeout-seconds N]   # 未运行则 build + run + 等就绪；已在跑则 no-op
agproc restart [service...] [--timeout-seconds N]   # 先停，再 build + run + 等就绪
agproc stop    [service...]                         # 停止运行中的服务，或取消进行中的 build
agproc ps      [service...] [--json]                # 状态查询
agproc logs    [service...] [--tail N] [-f] [--stream both|stdout|stderr]
agproc skills  [--json]                             # 输出项目专属的 Agent 指引
agproc init    [--force]                            # 生成模板
```

- 省略 `[service...]` 表示**全部** service；支持一次写多个名字。
- `-C/--config <PATH>` 或环境变量 `AGPROC_CONFIG` 指定配置；否则像 git/cargo 一样从当前目录**向上查找** `agproc.toml`，`.agproc/` 建在配置文件同目录。
- `--timeout-seconds` 只约束**命令等待**：超时打印 `STILL STARTING` 并返回 1，**不会杀掉后台 runner**，可继续用 `ps` / `logs` 观察。
- `agproc logs` 只回放**最后一次 run-cmd** 的 stdout/stderr：没有 agproc 的 marker、没有 build 输出、也没有更早的运行记录。`--tail N` 对**每条流**各取末 N 行；`-f` 会在服务停止后**自动返回**，不会挂住 Agent。

## 配置文件 `agproc.toml`

```toml
[settings]
stop-timeout-seconds = 10         # SIGTERM -> SIGKILL 宽限期
log-max-bytes = 33554432          # 单文件超过 32 MiB 轮转为 <name>.1（0 = 不限）
port-check = true                 # 端口预检 + 归属校验

[[service]]
name = "backend"                  # 必填，唯一，[A-Za-z0-9._-]
cwd = "."                         # 可选，相对项目根
env = { RUST_LOG = "debug" }      # 可选，合并进继承的环境
build-cmd = ["cargo", "build"]    # 可选；省略则跳过 BUILD 阶段
build-timeout-seconds = 0         # 可选；0 = 不限
run-cmd = ["./target/debug/api"]  # 必填；argv 数组，直接 exec（不经 shell）
stop-timeout-seconds = 10         # 可选，覆盖 settings

probe = {                         # 可选；省略则「进程存活」即视为就绪
  http-get = { scheme = "http", host = "127.0.0.1", port = 3000, path = "/healthz" },
  initial-delay-seconds = 1,      # 首次探测前等待
  period-seconds = 1,             # 探测间隔
  timeout-seconds = 2,            # 单次探测超时
  failure-threshold = 3,          # 连续失败多少次判定失败
}
```

`probe` 也可以写成 TCP 连通性探测：

```toml
probe = { tcp-connect = { host = "127.0.0.1", port = 5173 },
          initial-delay-seconds = 1, period-seconds = 1,
          timeout-seconds = 2, failure-threshold = 3 }
```

规则：

- 字面量一律 kebab-case；**未知字段直接报错**（配置往往由 Agent 编写，写错键名必须立刻失败）。
- `build-cmd` / `run-cmd` 是 **argv 数组、直接 exec**，中间没有 shell：第一个元素是程序，其余是参数
  （所以 `"cargo build"` 不是命令，要写 `["cargo", "build"]`）。需要 shell 语法（`|`、`&&`、`>`、
  通配符、`$VAR`）时显式写 `run-cmd = ["sh", "-c", "a | b"]`。旧的字符串形式（会执行
  `<shell> -c "..."`）与 `[settings] shell` 已移除：字符串在加载阶段就报错（退出码 3），
  错误信息里直接给出改法。
- `http-get` 仅支持 `http`（本地开发端点），`https` 会在加载时报错；`2xx/3xx` 视为通过。
- 多个 service 会**并行**启动。

## 日志约定

agproc 自己的日志统一是 `===== XXX =====`：

```
===== BUILDING =====
===== BUILD SUCCEED =====            —— 失败则是 ===== BUILD FAILED (exit code 101) =====
===== RUNNING =====
===== PROBE ATTEMPT 2/3 FAILED: connection refused (http://127.0.0.1:3000/healthz) =====
===== PROBE PASSED (attempt 2) =====
===== PROBE FAILED: 3 consecutive failures, last: ... =====
===== RUNNING FAILED (exit code 1) =====
===== SERVICE EXITED (exit code 0, ready for 12s) =====
===== STOPPED ===== / ===== ALREADY RUNNING (pid 1234, uptime 2m3s, ready) =====
===== START IN PROGRESS (pid 1234, phase building) =====
===== WARNING: PORT 3000 ALREADY IN USE BY pid 614089 (node) =====
```

**这些行出现的位置**：`start` / `restart`（以及 `stop`）的控制台，**不写入服务日志文件**——
所以 `agproc logs` 的输出里不会出现它们。

**流分离**：子进程的 stdout 原样进入 agproc 的 stdout，stderr 原样进入 agproc 的 stderr。
单 service 时不加前缀（可直接管道）；多个 service 时每行都带服务名前缀，名字按**最长**的那个补齐
空格，让 `|` 列对齐：

```
backend  | ===== RUNNING =====          # `backend` 补齐到 `frontend` 的宽度
frontend | ===== PROBE PASSED =====
```

stdout 与 stderr 用的是**同一个前缀**——两条流靠去向区分，stderr 行不会再在文本里自我标注。

> 为什么能同时做到实时与分流：给 stdout 和 stderr 各分配一个 PTY，子进程认为自己在终端里，
> 因此保持**行缓冲**；若直接重定向到文件/管道，Python 等会切成全缓冲——实测 1.2 秒内文件里是 0 字节。

## 退出码

| 码 | 含义 |
|---|---|
| 0 | 成功：就绪 / already running / stop 成功 / ps / logs / skills |
| 1 | 通用错误（含命令等待超时） |
| 2 | 用法错误 |
| 3 | 配置错误（找不到或非法 `agproc.toml`、未知 service 名） |
| 4 | build 失败（非零退出或超时） |
| 5 | run 失败（就绪前退出） |
| 6 | 探针失败 |
| 7 | 该 service 已有 start/restart 在进行 |
| 8 | 本次 start 被其他 agproc 调用取代（例如被 `stop` 打断） |

## 状态（`agproc ps`）

`building`、`starting`（已运行未就绪）、`running`、`build failed`、`run failed`、
`running failed`（就绪后又退出）、`probe failed`、`stopped`、
`stale`（runner 被 `kill -9`；下次 `start` 会先清理残留进程组再重建）。

`agproc ps --json` 提供稳定结构（`phase` / `pid` / `runner_pid` / `child_pid` / `uptime_seconds` /
`build_exit_code` / `run_exit_code` / `probe{kind,target,attempts,last_error}` / `config_changed` /
`log_stdout` / `log_stderr`）。

## `.agproc/` 目录

```
.agproc/
├── logs/<service>.stdout.log      # 只有 run-cmd 的 stdout（session 开始时截断）
├── logs/<service>.stderr.log      # 只有 run-cmd 的 stderr
├── tmp/<service>.console.stdout    # 临时控制台流：marker + build 输出 + 就绪前的 run 输出
├── tmp/<service>.console.stderr    # 同上 stderr 侧，另含 runner 自身的报错
├── state/<service>.json           # 运行时状态（原子写入，ps 的唯一事实源）
├── lock/<service>.lock            # 编排锁（flock）
└── tmp/                           # 原子写临时文件
```

**run 日志**里只有服务自己打印的内容，这正是 `agproc logs` 精确的原因。agproc 自己的
`===== ... =====` 行与 build-cmd 的输出改走**控制台流**：它是 `start`/`restart` 实时转发的内容，
stdout/stderr 分别映射到 agproc 自己的两条流，并且是临时的——下一次 session 会截断它，
而一个**走到就绪**的 session 结束时会被删除。失败路径会保留它，所以构建失败仍可排查：
`cat .agproc/tmp/<service>.console.stdout`。

`agproc init` 会把 `.agproc/` 追加进 `.gitignore`。

## 给 AI Agent 使用

仓库内自带一份与 [skills.sh](https://www.skills.sh/) 兼容的 Skill：

```bash
npx skills add jmjoy/agproc      # 安装 skills/agproc/SKILL.md（发现用 stub）
agproc skills                    # 输出完整指引 + 本项目的真实 service 表
agproc skills --json             # 同上，附结构化数据
```

`agproc skills --json` 里每个 service 的 `build_cmd` / `run_cmd` 是真正的 **argv 数组**
（`["pnpm", "dev:serve"]`），与配置写法一致。

Skill 的核心纪律：**项目根存在 `agproc.toml` 时，不要直接用 `pnpm dev` / `cargo run` 起服务**，
先 `agproc skills` 取指引，再用 agproc 管理进程；改完代码用 `restart`，失败先看退出码再看日志。

## 设计要点

- **每个 service 一个 detached runner**（`agproc __runner`，`setsid`），没有中心 daemon、没有 socket。
  状态与日志全部是 `.agproc/` 里的普通文件，Agent 可以直接 `cat`/`grep`；
  CLI 被超时杀掉也不影响正在进行的 build 与探针。
- **runner 是子进程生命周期的唯一权威**：独立的 `waitpid` 与探针循环并发，子进程一退出就立即落 `run-failed`，
  优先级高于任何探针结果——探针永远不会把已死的服务报成就绪。
- **端口占用三机制**：起 `run-cmd` 前扫描 `/proc/net/tcp{,6}` 记录占用者并告警；探针通过瞬间校验端口归属
  （仍属于最初那个外来进程则判失败；属于容器运行时等则只告警）；子进程退出事件优先。
- **PID 复用安全**：状态文件记录 runner 的 `/proc/<pid>/stat` starttime，`ps` 据此判断存活。
- **编排锁 + generation**：同一 service 同时只会有一个 start/restart；`stop` 不参与加锁，随时可打断。

## 明确不做（v0.1）

`depends-on` 依赖排序（当前全部并行启动）、崩溃自动重启、watch/HMR 感知、
`https` 探针、macOS/Windows。

## License

本项目采用[木兰宽松许可证，第 2 版](LICENSE)（`MulanPSL-2.0`）。
