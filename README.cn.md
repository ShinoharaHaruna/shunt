# shunt

[English](README.md) | 简体中文

为租用的 GPU 机器打造的任务队列，让 AI agent 替你看管机器。提交实验任务，让队列跑完，实例会在下一个计费小时到来之前自动关机。

它解决的问题：AutoDL 之类的厂商按 wall-clock 开机时长计费，而实验要跑数小时甚至数天，没人在凌晨两点盯着进度条。现在的流程是手动的——开机、跑任务、记得关机。shunt 把这个循环变成：人在厂商控制台付费并开机 → 队列守护进程领取任务 → 队列排空 → 机器自行关机。你开发机上的 agent 可以驱动整个循环。

## 功能

- **基于文件系统的状态机**，运行在 GPU 机器上：`pending/ → running/ → done/` 或 `failed/`，配套 `.log`、`.pid`、`.exit` 等 sidecar 文件。任务在轨道之间移动就是一次原子 `mv`。
- **至少一次执行（at-least-once）。** 守护进程在任务中途死掉时，下次启动会把孤儿任务重新放回队列头部。任务以其产出的 artifact 作为 guard（`--guard runs/x.json`），重跑只是廉价的 no-op。
- **按任务的进程组隔离。** 每个任务运行在独立的 pgid 下；`shunt kill` 向整个进程组发信号。队列自身 pidfile 之外的任何进程都不会被触碰。
- **可在线调整的 worker 池。** `shunt workers 4` 在下一次轮询时生效，无需重启。缩容时自然排空，绝不强杀。
- **感知计费的空闲关机。** `shunt shutdown on` 启动一个倒计时，队列清空后即关机。倒计时每秒复查，新任务到达则取消。
- **关机状态下也能提交。** 任务落在 server 侧的 spool 里，等机器上线时由 dispatcher 投递——如果守护进程死了但机器可达，会先把它拉起来。
- **SSH 是唯一的通信链路。** server 执行 `ssh box 'shunt <cmd> --json'`。机器上不开放任何端口，没有 API server，也不需要机器反向连接开发机。

## 30 秒速览

```bash
# on the dev machine: register a box (vendor template fills the rest)
shunt client add rw-a --template autodl
#   edit ~/.config/shunt/clients/rw-a.toml: ssh.host, ssh.port, ssh.key
shunt client bootstrap rw-a        # probe facts, ship binary, start daemon

shunt project add riboworld
shunt project bind riboworld --client rw-a \
    --workdir /root/autodl-tmp/RiboWorld --env TASK_THREADS=4

# submit work; lands in the spool, survives the box being off
shunt submit -p riboworld -c rw-a --guard runs/exp1.json -- \
    uv run python train.py --config exp1.yaml

shunt status --all                 # aggregate every registered box
```

面向 agent 的接口完全相同——每条命令都有 `--json`，`skills/shunt/SKILL.md` 教会 agent（Claude Code、pi 等）完整的操作语义：注册、探测、提交、解读队列状态，以及何时启动空闲关机。

还没有机器？`localhost` 是内置 client——同一条链路可以在本地运行，不需要 SSH：

```bash
shunt submit -c localhost -- echo hello from the queue
shunt dispatch --once          # sweep: starts the daemon, delivers
shunt status -c localhost --json
```

## 整体结构

```text
dev machine (always on)                 GPU box (on-demand)
┌──────────────────────────────┐        ┌───────────────────────────┐
│ shunt                        │        │ shunt (same binary)       │
│  registry: clients+projects  │  SSH   │  pending/running/done/    │
│  spool: staged submissions   │ ─────► │  failed/, workers, STOP   │
│  dispatcher: spool → client  │ ◄───── │  daemon: pool + idle off  │
└──────────────────────────────┘  JSON  └───────────────────────────┘
```

同一个 Rust 二进制跑在两侧。开发机持有 registry（可以手动编辑、可以用 git 跟踪的 TOML 文件）、spool 和 dispatcher 循环。每台机器持有自己的队列——server 是编组场，每台机器是一条目的地侧线。

项目与机器是多对多关系：一个项目可以绑定到一个或多个 client，每条绑定携带该组合的 `workdir` 和环境变量覆盖。一条 client 记录保存由 `probe` 采集的机器信息（`gpu`、`cuda`、`cores`、`mem`、`disk`）——包括 cgroup 感知的核数/内存计数，因为这些容器里的 `nproc` 报告的是宿主机而不是你的配额——外加计费费率元数据，方便 agent 推理成本。

## 命令一览

| command                                                | side   | what it does             |
| ------------------------------------------------------ | ------ | ------------------------ |
| `shunt client add\|bootstrap\|probe\|ls\|rm`           | server | 注册并初始化机器         |
| `shunt project add\|bind\|ls`                          | server | 注册项目，绑定到 client  |
| `shunt submit -p proj -c client [--guard p] -- cmd...` | server | 把任务暂存进 spool       |
| `shunt status [--all] [--json]`                        | server | 聚合各队列状态           |
| `shunt spool ls`                                       | server | 查看暂存的提交           |
| `shunt dispatch`                                       | server | 运行 spool → client 循环 |
| `shunt start\|stop\|daemon`                            | client | 队列守护进程生命周期     |
| `shunt enqueue\|new\|cancel\|prio\|kill`               | client | 任务操作                 |
| `shunt workers [N]`                                    | client | 读取/设置池大小          |
| `shunt shutdown on\|off\|status`                       | client | 空闲关机                 |
| `shunt status\|logs\|probe`                            | client | 队列状态、日志、机器信息 |

## shunt 不是什么

没有 DAG 依赖（去用 Airflow）。没有重试退避——失败的任务落在 `failed/`，由 agent 决定怎么办。没有面向用户的 HTTP API——SSH 之上的 client CLI 就是 API。没有多机负载均衡——提交时指定 client；把需求匹配到机器信息是 agent 的工作。没有远程开机——在按小时计费的厂商那里，开机_就是_付费，那一下点击永远留给人类。

## 现状

活跃开发中：Rust 实现正对照 `docs/design.md` 构建，该文件是行为规范。它移植自一个经过验证的 bash 原型（孤儿任务重排、pgid 清理、flock 保护的计数器、信号转发等等），原型作为 oracle 归档在本地的 `reference/` 下——已加入 gitignore，因为其中包含部署相关的具体细节。

## License

MIT
