# shunt

[English](README.md) | 简体中文

![shunt —— 将实验任务调度到计算资源的铁路编组场](docs/assets/readme/hero.png)

<p align="center">
  <strong>运行实验，协调算力。</strong><br>
  面向人与 AI agent 的实验运行管理层，从本地工作站到远程服务器。
</p>

<p align="center">
  <a href="Cargo.toml"><img alt="Rust 2021" src="https://img.shields.io/badge/Rust-2021-313c40?style=flat-square"></a>
  <img
    alt="平台：Linux"
    src="https://img.shields.io/badge/platform-Linux-313c40?style=flat-square"
  >
  <a href="docs/design.md#5-wire-protocol"><img alt="通信：SSH + JSON" src="https://img.shields.io/badge/wire-SSH_%2B_JSON-24776b?style=flat-square"></a>
  <a href="LICENSE"><img alt="许可证：MIT" src="https://img.shields.io/badge/license-MIT-bb6238?style=flat-square"></a>
</p>

<p align="center">
  <a href="#快速上手">快速上手</a> ·
  <a href="#工作原理">架构</a> ·
  <a href="#协调共享算力">协作</a> ·
  <a href="#面向-agent">Agent 指南</a> ·
  <a href="docs/design.md">设计规范</a> ·
  <a href="#参与贡献">参与贡献</a>
</p>

实验往往横跨多个项目、机器、环境和长时间运行的会话。shunt 将这些工作整合到一个 CLI：登记机器、将项目绑定到队列、暂存并运行任务、查看结果与资源用量，以及协调共享算力的人与 agent。

无论本地工作站、实验室服务器，还是云端 CPU 或 GPU 实例，都可以使用同一套工作流。同一个 Rust 二进制运行在两端：远程机器通过 SSH 操作，本地直接执行进程。任务、日志和队列状态都保存在磁盘上。计费元数据与空闲关机是按需启用的辅助功能。

## 为什么选择 shunt

| 当你需要……                       | shunt 提供……                                                                                                 |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| **跨机器、跨项目工作**           | TOML registry、多对多项目绑定、每条绑定独立的工作目录与环境，以及 SSH 或本地 transport。                     |
| **在同一台机器上分开不同工作流** | 同一个 client 身份下的具名队列，各自拥有独立目录、守护进程、worker 池、shell 和环境初始化配置。              |
| **机器尚未连通就提交任务**       | 开发机上持久化的 spool；机器恢复可达后 dispatcher 自动投递，推送前会拉起已停止的队列守护进程。               |
| **让实验持续推进**               | 并发 worker、FIFO 领取顺序、手动优先级、运行中调整池大小、任务超时，以及显式开启且次数有限的资源争用重调度。 |
| **了解一次运行实际用了多少资源** | 机器与 GPU 的可用状态、每个任务的 RAM 与 VRAM 峰值、CPU 时间，以及并列展示的资源声明与实测用量。             |
| **协调共享算力**                 | 项目协作房间，支持未读跟踪、协定记录、对话摘要和归档。                                                       |
| **恢复与排查**                   | 至少一次执行、孤儿任务恢复、产物 guard、投递去重、任务日志、提交台账和事件流。                               |
| **通过 agent 操作**              | 同一套 CLI，提供 JSON 状态、JSONL 任务列表、随附的操作 skill，以及持久化反馈通道。                           |

机器管理还包括受管 SSH 命令执行、bootstrap 与升级、可选的反向 SOCKS5 代理、队列排空后退出守护进程，以及显式开启的空闲关机。共享机器可以明确拒绝从 server 侧开启关机。

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

# 试用结束后，让本地守护进程等待运行中的任务完成再退出。
shunt stop --root ~/.config/shunt/queue
```

执行是异步的：稍等片刻再重复 `status` 或 `logs` 查看结果。默认本地队列是 `~/.config/shunt/queue/`；这个演示会在里面留下 `hello.done`，所以重复执行会跳过已完成的任务。`stop` 会等待守护进程中正在运行的任务完成；空闲关机是另一项需要显式开启的独立操作。

### 3. 接入一台实验机器

在开发机上运行以下命令，接入一台已经可以连接的 Linux 服务器。将示例路径替换为你在该机器上有权限使用的目录。本例复用已授权的 SSH 密钥，并将服务器标记为共享机器：

```bash
shunt client add lab --template generic \
    --key ~/.ssh/id_ed25519 --deploy-root /path/you/own \
    --qroot /path/you/own/shunt-queue \
    --shared --exit-on-idle --expire unlimited

# 填入服务器的 ssh.host、ssh.port 和 ssh.user。
${EDITOR:-vi} ~/.config/shunt/clients/lab.toml

shunt client bootstrap lab

shunt project add experiments
shunt project bind experiments --client lab \
    --workdir /path/to/experiments --env OMP_NUM_THREADS=4 --env MKL_NUM_THREADS=4
```

`--shared` 阻止从 server 侧开启空闲关机；`--exit-on-idle` 让队列守护进程在排空后退出，并在下次投递时重新启动。如果你使用独占机器并希望守护进程常驻，可以省略这两个参数。`expire = "unlimited"` 适用于自有或实验室机器；云端机器也可以使用 `"metered"` 或订阅到期时间，并按需配置计费费率。

Bootstrap 将二进制安装到队列根目录下、检查运行时兼容性、初始化队列、启动守护进程并探测机器信息。如果本地构建依赖较新的 glibc，它可以获取匹配版本的静态 release；其他架构或尚未发布的构建可能需要显式指定 `--bin-path`。参见[二进制兼容性](docs/design.md#15-binary-compatibility-at-bootstrap)。

对于独占的云端实例，`--template autodl` 等厂商模板可预填部署默认值。不指定 `--key` 时，shunt 会生成专用密钥对：你可以自行安装公钥；也可以在独占机器上配置 `ssh.password` 并安装 `sshpass`，用密码辅助部署。共享 client 跳过自动密钥部署。

绑定的 `workdir` 必须已经包含你的代码、数据和运行环境。Bootstrap 部署 shunt 本体；项目绑定记录实验在哪里运行。

### 4. 提交并查看实验

假设你的 `train.py` 成功结束后会写出 `runs/exp1/metrics.json`：

```bash
shunt submit -p experiments -c lab --slug exp1 \
    --guard runs/exp1/metrics.json --timeout 14400 -- \
    uv run python train.py --out runs/exp1

shunt dispatch --once               # 投递当前 spool
shunt status -c lab --json           # 查看队列状态与资源用量
shunt logs -c lab exp1               # 查看任务日志
```

在开发机的另一个终端里让 `shunt dispatch` 持续运行，即可持续投递，包括机器离线期间提交的任务。投递成功后，机器自己的队列守护进程独立运行任务，不再依赖 dispatcher 或提交任务的终端。

`--timeout 14400` 为任务设置四小时的实际经过时间上限。`--guard` 检查相对绑定 `workdir` 的路径，已存在则跳过执行。`uv` 和 `train.py` 是示例工作负载的依赖，替换成你项目需要的命令即可。

## 工作原理

![架构：开发机持有 registry、spool、dispatcher 和协作房间；通过 SSH 或本地执行连接实验机器，机器上设有彼此独立的具名队列](docs/assets/readme/architecture.svg)

名字来自铁路调车（shunting）。开发机是编组场，每台实验机器是一条目的地侧线。**Client** 是登记的机器；**queue** 是机器上的一条执行通道；**项目绑定**为 `(client, queue)` 对提供工作目录和环境。

| 开发机上                                                                                | 每台实验机器上                                                                |
| --------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| **Registry** —— 可手动编辑的 TOML 文件，记录 client、队列、项目、绑定和可选的计费信息。 | **队列** —— `pending/`、`running/`、`done/`、`failed/` 下的任务目录。         |
| **Spool 与 dispatcher** —— 暂存任务直至确认收讫、投递任务，并拉起已停止的守护进程。     | **守护进程** —— 每条队列一个，支持并发 worker、日志、用量采样和孤儿任务恢复。 |
| **任务台账与房间** —— 跟踪提交并保留项目协作历史。                                      | **任务环境** —— 队列的初始化配置与 shell、项目绑定配置，以及任务级覆盖。      |

### 项目与具名队列

在同一台机器上分开训练、评估或不同项目，无需重复登记 SSH 身份：

```bash
shunt queue add eval -c lab --qroot /path/you/own/shunt-eval \
    --workers 2 --exit-on-idle
shunt queue bootstrap eval -c lab
shunt project bind experiments --client lab --queue eval \
    --workdir /path/to/experiments --env OMP_NUM_THREADS=2 --env MKL_NUM_THREADS=2
shunt submit -p experiments -c lab --queue eval --slug eval1 \
    --guard runs/eval1.json -- python evaluate.py --out runs/eval1.json
shunt status -c lab --queue eval --json
```

每条队列各自拥有 qroot、序列号空间、worker 池、守护进程、二进制、shell 和环境初始化配置。队列选择顺序为 `--queue` → 配置的 `default_queue` → 唯一队列；仍无法唯一确定时会报歧义错误。已暂存的任务固定在提交时选定的队列和 qroot，之后改变默认值不会让它们改投。

在队列上配置 `env_setup` 和 `shell`，用于激活环境或设置共享环境变量，再用 `shunt queue env <name> -c lab` 推送；`shunt client env lab` 更新所有队列。项目与任务级的环境覆盖让一条队列也能服务多种工作负载。

通信链路就是 `ssh <machine> 'shunt <cmd> --json'`。无需 HTTP 服务，也不需要机器反向连接开发机。同一个二进制还为 `localhost` 和通过 `client add --local` 添加的其他 client 提供直接的本地 transport。

### 可以直接检查的队列

![任务生命周期：pending 任务被领取进入 running，然后结束于 done 或 failed；守护进程重启会将孤儿 running 条目重新排队；开启空闲关机后，倒计时只在队列排空后开始](docs/assets/readme/queue-lifecycle.svg)

每个任务是一个名为 `NNNN_<slug>` 的目录，包含 `task.sh`，并逐渐积累 `log`、实时 `pid` 记录、实测 `usage` 和最终的 `exit` 码。队列通过文件系统 rename 迁移状态。每次轮询都会重新读取 worker 数量，调整池大小无需重启；减少 worker 时，会让当前任务完成。任务运行在独立进程组中，发送信号前会核对记录的进程身份。

**执行语义是至少一次。** Server 生成的任务 ID 可以对仍在队列各状态目录中的重复投递去重，但守护进程恢复或超出去重窗口的投递仍可能触发重跑。Guard 是执行前的存在性检查，因此只有当产物代表已完成的工作时才发布它，并确保重跑安全。它不提供恰好一次执行，也不是防止重叠执行的锁。参见 [guard 契约](docs/design.md#313-task-file-format-and-the-guard-contract) 和[任务模板](task_template.sh)。

## 感知资源的实验运行

声明资源估计值、设置运行时间预算，并允许资源暂时不可用时进行有限重试：

```bash
shunt submit -p experiments -c lab --slug exp2 \
    --guard runs/exp2/metrics.json --timeout 14400 \
    --retry 2 --retry-delay 60 --ram-mb 8192 --cpu-cores 4 --gpu-mem-mb 4096 \
    --env CUDA_VISIBLE_DEVICES=0 --env OMP_NUM_THREADS=4 --env MKL_NUM_THREADS=4 -- \
    uv run python train.py --out runs/exp2
shunt status -c lab --json
```

`--retry 2` 允许在退出码为 75（`EX_TEMPFAIL`，表示资源繁忙）或日志匹配已识别的内存不足特征时，再尝试两次。每次至少等待 `--retry-delay` 秒，任务才可重新被领取。其他失败和超时仍按失败处理；重试必须显式声明次数预算。

Status 将资源声明与采样得到的每个任务 RAM 峰值、各 GPU 的 VRAM 峰值及累计 CPU 秒数并列展示，同时报告当前 GPU 显存与利用率、主机 RAM 和负载；机器探测提供考虑 cgroup 限额的容量信息。GPU 遥测在可用时使用 `nvidia-smi`。采样可能漏掉短命进程或短暂峰值。

**资源估计是标注，不会预留资源，也不执行准入检查。** 目标机器、worker 数量和 GPU 分配由你或 agent 决定。纯 CPU 任务使用同一套队列，无需 GPU 参数。参见[资源遥测](docs/design.md#316-resource-declarations-and-usage-telemetry)。

## 协调共享算力

多个项目共享机器时，各自的 agent 需要一个地方约定 worker 上限、内存预算或 GPU 分配。协作房间将对话及其当前协定保存在 registry 旁：

```bash
# 假设 experiments 和 analysis 已登记为项目。
shunt room ls --as experiments          # 房间、未读数和建议分组
shunt room create shared-compute --members experiments,analysis --clients lab
shunt room say shared-compute --as experiments \
    --body "Training needs GPU 0 until 18:00; can evaluation use GPU 1?"
shunt room read shared-compute --as analysis
shunt room decide shared-compute --as analysis \
    --note "Training uses GPU 0; evaluation uses GPU 1 with two workers."
shunt status --all --json --as experiments
```

房间记录项目成员、各身份的未读位置和当前决议。`room ls` 会建议将绑定到同一台机器的项目分组。长对话可以压缩成消息流中的摘要，同时保留已归档消息，供按范围读取；整个房间也可以归档和恢复。

房间保存在一台开发机上，可关联其登记的多个 client。共享该 registry 的 agent 可以在调整共享算力前轮询房间。参与者通过 `workers` 或 `submit --env` 等命令落实约定后，协定才会生效；房间本身不强制执行调度或资源分配。参见[协作房间](docs/design.md#17-coordination-rooms-server-side)。

## 日常操作

以下命令在开发机上运行。针对具体 client 的示例使用已登记的 `lab`；加上 `--queue eval` 即可指定具名队列。

| 意图                         | 命令                                                  |
| ---------------------------- | ----------------------------------------------------- |
| 查看所有机器、队列和房间     | `shunt status --all --json`                           |
| 查看等待投递的提交           | `shunt spool ls`                                      |
| 按 ID 查看已提交的任务       | `shunt task show <id> --json`                         |
| 读取 server 侧的任务事件流   | `shunt task events`                                   |
| 调整 worker 池大小           | `shunt workers 4 -c lab`                              |
| 将 pending 任务移到队首      | `shunt prio exp1 0 -c lab`                            |
| 取消 pending 任务            | `shunt cancel exp1 -c lab`                            |
| 停止运行中任务的进程组       | `shunt kill exp1 -c lab`                              |
| 排空并停止一条队列的守护进程 | `shunt queue stop eval -c lab`                        |
| 刷新机器信息                 | `shunt client probe lab`                              |
| 列出机器上的队列             | `shunt queue ls -c lab`                               |
| 通过受管 SSH 执行临时命令    | `shunt client exec lab -- nvidia-smi`                 |
| 更新发生变动的 SSH 端点      | `shunt client update lab --host <host> --port <port>` |
| 清理过期的主机密钥与连接     | `shunt client hostkey lab`                            |
| 打开可选的反向 SOCKS5 代理   | `shunt tunnel up -c lab`                              |
| 生成 shell 补全              | `shunt completions zsh`                               |
| 查看本地版本与构建信息       | `shunt version --json`                                |
| 检查新版本                   | `shunt upgrade --check --json`                        |
| 升级本地二进制               | `shunt upgrade`                                       |
| 升级一条远程队列的二进制     | `shunt upgrade -c lab --queue eval`                   |

像 `exp1` 这样的任务控制 pattern 必须能唯一匹配一个任务；存在歧义时，使用完整的任务条目名。任务台账将提交 ID 映射到该条目名。`task show` 按需查询机器以确定结果；超出近期状态窗口的条目可能显示为 `unknown`。

升级会在安装前校验 release 校验和与二进制版本。运行中的远程守护进程会继续使用已加载的版本，直到通过 `queue stop` / `queue start` 排空并重启；替换二进制不会中断当前工作。预构建 release 升级目前面向 Linux x86_64。

用 `shunt <command> --help` 查看参数，完整命令见 [CLI 参考](docs/design.md#10-cli-surface)。`status`、`logs` 等队列命令也可以加上 `--root <qroot>`，直接在机器上运行。

### 可选的空闲关机

如果希望独占机器在一批任务结束后关机，使用 `shunt shutdown -c <client> on` 显式开启；`off` 关闭，`status` 查看解析后的关机命令。队列排空后才开始倒计时，新任务会取消倒计时。开启时，server 会预检已登记的兄弟队列；运行时的检查仅覆盖同一账号下仍在运行的队列守护进程，无法证明其他用户或未受管工作负载已空闲。标记为 `shared` 的 client 拒绝从 server 侧开启关机。

对于需要保持开机的工作站或共享服务器，可以使用 `exit_on_idle` 仅退出守护进程，也可以让它常驻、等待下一批任务。本地演示与共享服务器快速上手均无需配置电源管理。

## 面向 agent

CLI 同时也是 agent 接口。队列状态与任务台账查询提供结构化 JSON 或 JSONL；任务及守护进程日志提供这些状态背后的细节。机器信息、实时资源、资源声明与实测用量对比，以及房间未读数，为 agent 选择目标、规划批量任务和协商共享算力提供上下文。机器有成本或到期限制时，配置好的计费费率与到期元数据也会提供参考。

随附的 [shunt skill](skills/shunt/SKILL.md) 涵盖初始化配置、带 guard 的提交、队列解读、协作、失败处理，以及可选的成本感知操作。让 agent 管理实验机器前，先将这个 skill 交给它。

把 `skills/shunt/` 链接到你的 agent 加载 skill 的目录即可安装：

```bash
ln -s "$PWD/skills/shunt" ~/.agents/skills/shunt   # 共享的 agentskills 目录
# 或： cp -r skills/shunt ~/.claude/skills/shunt   # Claude Code 用户级 skill
# 或： cp -r skills/shunt .claude/skills/          # 项目级
```

```bash
shunt task ls --json
shunt status --all --json --as experiments
shunt task show <id> --json
```

shunt 提供事实并执行明确的命令。选择机器、落实资源协定、判断一次运行是否符合预算，以及配置或开启机器，仍由人或外围的 agent 工作流负责。

当 agent 遇到 shunt 的缺陷或不顺手之处，`shunt feedback "title" --body "…"` 会将反馈提交为 GitHub issue。反馈通道会擦除 registry 中已知的凭据，以及已登记的 client、项目和队列名称；先在本地保存报告，再投递，并通过重复去重和每小时上限限制提交。用 `--dry-run` 预览、`feedback ls --json` 查看待投递报告、`feedback retry` 重新投递。报告默认公开，擦除器无法识别任意形式的秘密信息。参见[反馈通道](docs/design.md#14-feedback-channel)。

## 运行边界

- **任务独立，显式指定目标。** 没有依赖图、自动选机或多机负载均衡；每次提交都指定 client 和队列。资源声明与房间决议不会预留硬件或隔离租户。
- **恢复次数有限。** 资源繁忙或已识别的内存不足失败，只有声明了重试预算才会重新调度。不符合这些条件的失败、超时及重试耗尽的任务进入 `failed/`；查看日志后酌情重新提交。守护进程重启后的孤儿恢复是另一回事。
- **协作限于本地 registry。** 房间服务于共享同一开发机 registry 的 agent，提供需要轮询的协作记录；没有跨 registry 消息、推送通知，也不强制执行参与者之间的访问控制。
- **关机需要显式开启。** 空闲关机使用可配置的命令和宽限期，默认 60 秒。受管队列为空，无法证明其他用户或未受管工作负载已空闲。请确认关机命令适用于你的机器；shunt 不控制厂商的计费规则。与最后一次关机检查同时发生的入队，仍可能来不及阻止关机。
- **部署需要显式操作。** 项目绑定记录路径和环境；它不同步源码、不安装依赖，也不创建远程 workdir。

Rust 实现覆盖 registry、具名队列、spool、dispatcher、SSH 与本地 transport、任务遥测、协作房间、tunnel、升级、反馈和 client 队列守护进程。项目仍在积极开发；[docs/design.md](docs/design.md) 定义行为契约及其边界情况。

## 参与贡献

先读[设计规范](docs/design.md)和[仓库指南](AGENTS.md)。改动队列行为时，请同步更新契约，并覆盖相关的状态迁移或失败场景。

```bash
just setup       # 启用版本化的 Git hooks
just fmt         # 格式化 Rust、Markdown、TOML 和 JSON
just check       # 格式检查 + clippy (-D warnings) + 测试
```

这些 recipe 需要安装 `just` 和 `dprint`；Rust 格式化和 Clippy 来自配置好的工具链。Hooks 在提交前检查格式、推送前运行 Clippy。它们从不改写文件；完整测试套件通过 `just check` 或 `just test` 运行。

### 发布版本

在要发布的 commit 上打一个 SemVer tag，例如 `v0.1.0`（也支持 `0.1.0`），然后推送。去掉可选的 `v` 前缀后，版本必须与 `Cargo.toml` 中的 `package.version` 一致；修改版本时也要同步更新 `Cargo.lock`。该 commit 必须包含 [release workflow](.github/workflows/release.yml)。

```bash
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin v0.1.0
```

Workflow 会检查格式、运行 Clippy，并对两个 target 运行测试；通过后发布 GitHub Release，附带自动生成的更新说明、Linux x86_64 musl（静态）与 GNU 二进制压缩包，以及各 target 的 `SHA256SUMS-<target>` 文件。带预发布后缀的 tag（例如 `v0.2.0-rc.1`）会创建 prerelease。使用内置的 `GITHUB_TOKEN`，无需额外配置 secret。重跑不会覆盖已发布的 release。

| 从这里开始                           | 用途                                     |
| ------------------------------------ | ---------------------------------------- |
| [设计规范](docs/design.md)           | 队列语义、通信协议、遥测、房间和测试策略 |
| [Agent skill](skills/shunt/SKILL.md) | AI agent 的操作手册                      |
| [厂商模板](etc/vendors/)             | AutoDL 和 generic 的 client 默认配置     |
| [任务模板](task_template.sh)         | 编写带 guard 的任务脚本                  |
| [集成测试](tests/)                   | 队列、进程、关机和投递行为               |

## License

[MIT](LICENSE)。
