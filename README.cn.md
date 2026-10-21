# shunt

[English](README.md) | 简体中文

![shunt —— 一座铁路调车场，把任务车厢调度进 GPU 计算模块](docs/assets/readme/hero.png)

<p align="center">
  <strong>把实验排进队列，跑完自动关机。</strong><br>
  为租用的 GPU 机器打造的任务队列，由你或你的 AI agent 通过 SSH 操作。
</p>

<p align="center">
  <a href="Cargo.toml"><img alt="Rust 2021" src="https://img.shields.io/badge/Rust-2021-313c40?style=flat-square"></a>
  <img
    alt="Platform: Linux"
    src="https://img.shields.io/badge/platform-Linux-313c40?style=flat-square"
  >
  <a href="docs/design.md#5-wire-protocol"><img alt="Wire: SSH + JSON" src="https://img.shields.io/badge/wire-SSH_%2B_JSON-24776b?style=flat-square"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-bb6238?style=flat-square"></a>
</p>

<p align="center">
  <a href="#快速上手">快速上手</a> ·
  <a href="#工作原理">架构</a> ·
  <a href="#为-agent-而生">Agent 指南</a> ·
  <a href="docs/design.md">设计规范</a> ·
  <a href="#参与贡献">参与贡献</a>
</p>

GPU 实验可以通宵运行，而按小时计费在实验结束后仍在继续。shunt 填补的正是这段空档：从开发机暂存任务、让租用机器的 worker 池保持忙碌，并在队列排空且已启用空闲关机时让机器自行断电。

同一个 Rust 二进制跑在两侧。任务、日志和队列状态都落在磁盘上；远程接口就是 SSH 之上的 CLI。人在厂商控制台开机，剩下的交给 shunt。

## 为什么是 shunt

| 当你需要……                 | shunt 提供……                                                                                             |
| -------------------------- | -------------------------------------------------------------------------------------------------------- |
| **机器还没开机就先提交**   | 开发机侧持久化的 spool。SSH 可达时 dispatcher 负责投递，投递前会先拉起已死的队列守护进程。               |
| **同时推进多个实验**       | 并发 worker 池、FIFO 领取顺序、手动优先级档，以及在线调整池大小。缩容时让运行中的任务自然跑完。          |
| **中断之后恢复**           | 至少一次执行：守护进程重启时，孤儿 running 任务回到队列头部。artifact guard 可以让重跑跳过已完成的工作。 |
| **停掉一个任务及其子进程** | 每个任务运行在独立的进程组中，发信号前做身份校验。队列操作只针对记录过的任务进程。                       |
| **不为空队列付费**         | 可选的空闲关机，带宽限倒计时。新任务会取消倒计时；同一台机器上仍在忙碌的兄弟队列会阻止关机。             |
| **给 agent 有用的上下文**  | 结构化的队列状态、任务台账、感知 cgroup 的机器信息，以及配置好的计费元数据，供 agent 推理使用。          |

## 快速上手

### 1. 从源码安装

需要 Linux、stable Rust 工具链、系统链接器和 Bash。远程操作还需要开发机上装有 OpenSSH 工具（`ssh`、`scp` 和 `ssh-keygen`）。

```bash
git clone https://github.com/ShinoharaHaruna/shunt.git
cd shunt
cargo install --path . --locked
shunt --help
```

### 2. 在本地试用队列

内置的 `localhost` client 通过本地进程执行同一套 CLI 协议。不需要 SSH 或 GPU，就能走完 spool → dispatcher → 队列的完整链路：

```bash
# 该任务会在本地队列目录中创建一个完成标记。
shunt submit -c localhost --slug hello --guard hello.done -- touch hello.done
shunt dispatch --once
shunt status -c localhost --json
shunt logs -c localhost hello

# 试验结束后，排空本地守护进程并退出。
shunt stop --root ~/.config/shunt/queue
```

执行是异步的：稍等片刻再重复 `status` 或 `logs` 查看结果。默认本地队列是 `~/.config/shunt/queue/`；这个演示会在里面留下 `hello.done`，所以重复执行会跳过已完成的任务。`stop` 会排空守护进程正在运行的任务；空闲关机是另一项独立的、需要显式开启的操作。

### 3. 接入一台租用的 GPU 机器

在厂商控制台开机，然后在开发机上运行以下命令。本例使用 AutoDL 的持久化挂载目录；其他厂商请选择 `generic` 模板并配置相应路径。

```bash
shunt client add gpu-a --template autodl

# 从厂商控制台填入 ssh.host 和 ssh.port。
# shunt 会生成专用密钥对并自动填好 ssh.key。
${EDITOR:-vi} ~/.config/shunt/clients/gpu-a.toml

shunt client bootstrap gpu-a

shunt project add experiments
shunt project bind experiments --client gpu-a \
    --workdir /root/autodl-tmp/experiments --env TASK_THREADS=4
```

bootstrap 之前先把生成的公钥装到机器上；或者在 client 记录里配置 `ssh.password` 并安装 `sshpass`，用密码辅助部署密钥。Bootstrap 会把当前二进制拷到机器上、初始化队列、启动守护进程并探测机器信息，因此二进制必须与机器的 CPU 架构和运行时匹配。

绑定的 `workdir` 必须已经包含你的代码、数据和运行环境。Bootstrap 部署的是 shunt 本体；项目绑定只是记录实验在哪里运行。

### 4. 提交、投递、排空

假设你的 `train.py` 成功结束后会写出 `runs/exp1/metrics.json`：

```bash
shunt submit -p experiments -c gpu-a --slug exp1 \
    --guard runs/exp1/metrics.json --timeout 14400 -- \
    uv run python train.py --out runs/exp1

shunt dispatch --once               # 投递当前 spool
shunt status -c gpu-a --json         # 查看 pending 和 running 的任务
shunt logs -c gpu-a exp1             # 查看任务日志

# 提交完这一批后再开启；机器在本地自行检查是否空闲。
shunt shutdown -c gpu-a on
```

在开发机的另一个终端里让 `shunt dispatch` 持续运行，即可实现持续投递——包括机器关机期间提交的任务。投递成功后，任务进入机器自己的队列，由它的守护进程独立运行，不再依赖 dispatcher。

`--timeout 14400` 给这个任务设四小时的 wall-clock 上限。`--guard` 检查相对绑定 `workdir` 的路径，已存在则跳过执行。`uv` 和 `train.py` 只是示例工作负载，换成你项目需要的命令即可。

## 工作原理

![架构：开发机持有 registry、spool 和 dispatcher；SSH 把命令送到 GPU 机器并带回 JSON；机器上运行文件系统队列、worker 池和可选的空闲关机](docs/assets/readme/architecture.svg)

名字来自铁路调车（shunting）。开发机是编组场，每台 GPU 机器是一条目的地侧线。项目与 client 是多对多关系：每条绑定为那台机器上的那个项目提供工作目录和环境变量。

| 开发机上                                                                    | 每台 GPU 机器上                                                       |
| --------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| **Registry** —— 可手动编辑的 TOML 文件，记录 client、项目、绑定和计费信息。 | **队列** —— `pending/`、`running/`、`done/`、`failed/` 下的任务目录。 |
| **Spool** —— 提交先暂存，直到机器确认收讫。                                 | **守护进程** —— 领取任务、运行 worker、记录日志、恢复孤儿条目。       |
| **Dispatcher** —— 通过 SSH 推送任务，拉起已死但可达的守护进程。             | **空闲关机** —— 关机前检查本机队列和已登记的兄弟队列。                |

通信链路就是 `ssh <box> 'shunt <cmd> --json'`。不需要 HTTP 服务，也不需要机器反向连接开发机。同一个二进制还为 `localhost` 提供本地 transport。

### 一个可以直接检查的队列

![任务生命周期：pending 任务被领取进 running，然后终结到 done 或 failed；守护进程重启会把孤儿 running 条目重新排队；开启空闲关机后，倒计时只在队列排空后才开始](docs/assets/readme/queue-lifecycle.svg)

每个任务是一个名为 `NNNN_<slug>` 的目录，包含 `task.sh`，并逐渐积累 `log`、实时更新的 `pid` 记录和最终的 `exit` 码。队列状态迁移就是文件系统 rename。worker 数量每次轮询都会重新读取，调整池大小无需重启。

**执行语义是至少一次。** 守护进程重启或丢失投递确认都可能导致重跑。guard 是执行前的存在性检查：只有当产物代表已完成的工作时才发布它，并让重跑安全。它不提供恰好一次执行，也不是防止并发执行的锁。参见 [guard 契约](docs/design.md#313-task-file-format-and-the-guard-contract) 和 [任务模板](task_template.sh)。

## 日常操作

以下命令都在开发机上运行。针对具体 client 的示例使用已注册的 `gpu-a`。

| 意图                          | 命令                             |
| ----------------------------- | -------------------------------- |
| 查看所有已登记的机器          | `shunt status --all --json`      |
| 查看等待投递的提交            | `shunt spool ls`                 |
| 按 ID 查看某个已提交的任务    | `shunt task show <id> --json`    |
| 读取 server 侧的任务事件流    | `shunt task events`              |
| 调整 worker 池大小            | `shunt workers 4 -c gpu-a`       |
| 把一个 pending 任务提到队首   | `shunt prio exp1 0 -c gpu-a`     |
| 取消一个 pending 任务         | `shunt cancel exp1 -c gpu-a`     |
| 停掉一个 running 任务的进程组 | `shunt kill exp1 -c gpu-a`       |
| 开启空闲关机                  | `shunt shutdown -c gpu-a on`     |
| 关闭空闲关机                  | `shunt shutdown -c gpu-a off`    |
| 查看解析出的关机命令          | `shunt shutdown -c gpu-a status` |
| 刷新机器信息                  | `shunt client probe gpu-a`       |
| 打开可选的机器侧 SOCKS5 代理  | `shunt tunnel up -c gpu-a`       |

像 `exp1` 这样的任务控制 pattern 必须能唯一匹配一个任务；pattern 有歧义时请用完整的队列名。任务台账把提交 ID 映射到队列名。`task show` 会按需向机器查询以确定结果；超出近期状态窗口的条目可能显示为 `unknown`。

用 `shunt <command> --help` 查看各命令参数，完整命令面见 [CLI 参考](docs/design.md#10-cli-surface)。client 命令也可以用 `--root <qroot>` 直接在机器上运行。

## 为 agent 而生

CLI 本身就是 agent 接口。队列状态和任务台账查询输出结构化 JSON；任务和守护进程日志提供这些状态背后的细节。机器探测读取 cgroup 的 CPU 与内存限额，status 还会暴露配置好的计费费率和到期元数据。agent 可以利用这些上下文选择目标机器并规划一批任务。

随附的 [shunt skill](skills/shunt/SKILL.md) 教会 agent 初始化配置、带 guard 的提交、队列状态解读、失败处理，以及成本感知的操作循环。让 agent 管理租用机器之前，先把这个 skill 交给它。

安装方式：把 `skills/shunt/` 链接到你的 agent 加载 skill 的目录：

```bash
ln -s "$PWD/skills/shunt" ~/.agents/skills/shunt   # 共享的 agentskills 目录
# 或： cp -r skills/shunt ~/.claude/skills/shunt   # Claude Code 用户级 skill
# 或： cp -r skills/shunt .claude/skills/          # 项目级
```

```bash
shunt task ls --json
shunt status --all --json
shunt task show <id> --json
```

shunt 负责暴露事实并执行明确的命令。选哪台机器、这次运行是否超预算、以及开机，仍然由人或外围的 agent 工作流决定。

## 边界

- **任务相互独立。** 没有依赖图、没有自动选机、没有多机负载均衡；每次提交都指定一个 client。
- **失败处理靠显式操作。** 失败、被杀死、超时的任务进入 `failed/`。没有自动重试策略；查看日志、酌情重新提交。守护进程重启后的孤儿恢复是另一回事。
- **关机需要显式开启。** 空闲关机使用可配置的命令和一段宽限期，默认 60 秒。请确认该命令真的能停掉你的厂商实例；shunt 不控制厂商的计费规则。与最后一次关机检查赛跑的入队仍可能输给关机。
- **部署靠显式操作。** 项目绑定只记录路径和环境；它不同步源码、不安装依赖、不创建远端 workdir。

Rust 实现覆盖 registry、spool、dispatcher、SSH 与本地 transport、tunnel，以及 client 队列守护进程。项目活跃开发中；[docs/design.md](docs/design.md) 定义了行为契约及其边界情况。

## 参与贡献

先读[设计规范](docs/design.md)和[仓库指南](AGENTS.md)。改动队列行为时，请同步更新契约，并覆盖相关的状态迁移或失败场景。

```bash
just setup       # 启用版本化的 Git hooks
just fmt         # 格式化 Rust、Markdown、TOML 和 JSON
just check       # 格式检查 + clippy (-D warnings) + 测试
```

这些 recipe 需要安装 `just` 和 `dprint`；Rust 格式化和 Clippy 来自配置好的工具链。Hooks 在提交前检查格式、推送前跑 Clippy。它们从不改写文件；完整测试套件通过 `just check` 或 `just test` 运行。

| 从这里开始                           | 用途                                 |
| ------------------------------------ | ------------------------------------ |
| [设计规范](docs/design.md)           | 队列语义、通信协议、计费和测试策略   |
| [Agent skill](skills/shunt/SKILL.md) | AI agent 的操作手册                  |
| [厂商模板](etc/vendors/)             | AutoDL 和 generic 的 client 默认配置 |
| [任务模板](task_template.sh)         | 编写带 guard 的任务脚本              |
| [集成测试](tests/)                   | 队列、进程、关机和投递行为           |

## License

[MIT](LICENSE)。
