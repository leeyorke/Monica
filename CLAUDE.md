# CLAUDE.md

本文件为 Claude Code (claude.ai/code) 在处理此存储库中的代码时提供指导。

## 常用命令

### 测试（最常用）

```bash
node desktop/scripts/sidecar.mjs          # 必须先跑一次，见下方"关键陷阱"
cargo test --workspace                    # 全量测试
cargo test --workspace --exclude fury-desktop --all-targets   # CI 在 Linux 上跑的就是这条
cargo test -p fury-agent sync_tests       # 单个模块的测试
```

### 构建

```bash
cargo build --release                     # agent + server + shared-rs 等所有 Rust crate
cd desktop && npm install && npm run build    # 仅 typecheck + 打包前端（vite）
cd desktop && npm run app:build           # 完整 Tauri 桌面安装包
```

### 本地运行

```bash
cargo run -p fury-agent -- serve          # 启动本地守护进程（solo 模式的全部产品）
FURY_CORE=/path/to/Chromium cargo run -p fury-agent -- serve   # 指定 core 路径
cargo run -p fury-agent -- launch shared/personas/<persona>.json --proxy socks5://... --timezone Europe/Berlin
cargo run -p fury-agent -- mcp            # MCP server（stdin/stdout）
```

### 检测与度量

```bash
cd tools/detect-suite && python3 -m http.server 8791   # 起 probe 服务
cargo run -p fury-detect -- gate <capture>.json        # 13 项门禁，非 0 退出即失败
cargo run -p fury-detect -- diff --mode spoof <chrome.json> <fury.json>
cargo run -p fury-detect -- persona <capture.json> > persona.json   # 从采集生成 persona
cargo run -p fury-detect -- redact <capture>.json <out.json>        # 去除出口 IP 后再分享
```

### Chromium core 构建（重活，非日常）

`core/` 下的脚本操作一个 ~39 GB 的 Chromium checkout，**core 源码本身不在这个仓库里**，由 `core/build/fetch.sh` 下载。完整流程与前提条件见 `docs/03-chromium-fork.md`（俄语）。核心脚本：

```bash
core/build/fetch.sh <tag>      # depot_tools + gclient sync 到指定版本
core/build/apply.sh            # 按 core/patches/series 顺序铺补丁
core/build/build.sh <config>   # gn gen + autoninja，配置取自 core/args/
core/build/refresh.sh <patch>  # 改完补丁后回写
core/build/rebase.sh           # 迁移到新 upstream tag
```

### 服务端自托管

`docker-compose.yml` 只含 postgres + fury-server。环境变量见 `.env.example`（`FURY_DB_PASSWORD`、`FURY_VERSION` 等）。完整指南：`docs/13-self-hosting.md`。

### 发布

签名/公证脚本在 `tools/release/`（`sign-core.sh`、`sign-shell.sh`），CI 工作流在 `.github/workflows/`。

## 高等级架构

### 项目定位

Monica 是开源指纹浏览器（anti-detect browser），fork 自 Fury。核心能力是把每个浏览器 profile 的设备指纹伪装成另一台真实机器，用于多账号隔离、QA、广告验证等合法场景（见 `ACCEPTABLE_USE.md`）。**本仓库的品牌正处于 Fury → Monica 的迁移中**：面向用户的名称、网站（`site/`）、logo 已是 Monica，但所有 Rust crate 名、环境变量前缀（`FURY_*`）、IPC 常量、Chromium 内部路径仍是 `fury`。改代码时沿用现有的 `fury` 命名，不要擅自重命名。

### 四个组件与数据流

```
desktop (Tauri 2 + React)  ──IPC (Unix socket/named pipe)──▶  agent (Rust 守护进程)
                                                                    │ spawn
                                                                    ▼
                                                            core (Chromium fork 二进制)
                                                                    │
        desktop ──HTTPS──▶ server (可选，axum + Postgres，团队模式)   │ 经 relay
                                                                    ▼
                                                              客户端代理
```

- **core**（`core/patches/`，41 个补丁，BSD-3）：所有指纹伪装都在 Chromium C++ 里完成，**绝无 JS 注入**。补丁是 quilt 系列（`core/patches/series` 是设计记录），不是带 merge commit 的 fork 分支。规范：一个补丁只管一个指纹向量；每个补丁在 `core/verify/` 里要有对应脚本驱动真实浏览器断言其效果。
- **agent**（`agent/src/`，AGPL）：唯一持有解密后机密的组件。负责代理 relay、profile 进程生命周期、本地自动化 REST API（`127.0.0.1:35000`）、vault 加密、bundle 打包同步、MCP server。
- **server**（`server/src/`，AGPL）：**刻意保持"笨"** —— 不生成指纹、不解密 bundle、不校验代理。只管组织/项目/RBAC/分布式锁/加密 bundle 存储。bundle 字节落在磁盘目录（`FURY_BUNDLE_DIR`，默认 `/var/lib/fury/bundles`），没有对象存储客户端。
- **desktop**（`desktop/src/`，Tauri 2 而非 Electron）：壳，约 12 MB 安装包。

### 三条核心设计原则

1. **一致性胜过独特性**（`docs/01`）：profile 从单个 seed 确定性生成，所有值源自同一"设备人格"。合法但现实中不存在的组合，比不伪装更糟 —— 会被一次跨上下文对比抓住。`shared-rs/src/persona.rs` 开头解释了为什么不用 seed 推导硬件值。
2. **跨执行上下文一致是门禁**：主 frame、Worker、SharedWorker、ServiceWorker、AudioWorklet、三种 iframe 共 8 个上下文，`crossContext.disagreements` 必须为 0。`fury-detect gate` 的 13 项检查是发布门禁，任何伪装补丁都要过这道门。
3. **测量优先，不接受声称**（`CONTRIBUTING.md`）：本项目的方法论是"每个关于浏览器的陈述背后都有一个来自真实运行浏览器的数字"。有测量的补丁哪怕代码糙也容易接受；没有测量的补丁一律拒收。

### 指纹配置如何送达浏览器（关键机制）

配置**不走命令行**（argv 对系统其他进程可见），也不走 pipe（core 读到 EOF，会迫使 agent 全程持写端）。流程（`agent/src/launcher.rs`，补丁 `0001`）：

1. agent 创建匿名文件（0600，立刻 unlink），写入指纹 JSON。
2. 文件作为**继承的文件描述符 3** 传给 core，argv 里只有 `--fury-fp-fd=3`（一个号码，不是配置）。OS-crypt cookie 密钥同理走 fd 4（补丁 `0110`）。
3. browser process 启动时读一次，存为 `base::Value`，再拷进 **read-only 共享内存**，子进程只收 handle。
4. 每个新 isolate（worker/OOPIF）在执行页面第一行脚本前必须拿到配置，入口统一在 `ContentMainRunnerImpl::Initialize`；失败即 `LOG(FATAL)`。

配置启动后**不可变** —— 不提供运行时更新，否则等于给页面留了一条探测伪装机制的信道的可能。

### 三种部署形态

| 形态 | 用户自建 | 场景 |
|---|---|---|
| Solo（默认） | 无 | 单人，profile/代理全在本地，无账号无 DB |
| 团队自托管 | 一个二进制 + Postgres | 团队/机构 |
| 托管 | 无 | 不想自己跑服务器的人 |

Team 层是后来加的开关，**不是**首屏必需。加密 bundle 的客户端加密（`agent/src/bundle.rs`，XChaCha20-Poly1305）是托管模式的前提：服务器只存密文，自己读不了。

### 三种许可证按目录划分（不可混用）

- `agent/`、`server/`、`desktop/`、`shared-rs/`、`tools/`、`core/build|args|verify/` —— **AGPL-3.0-or-later**
- `core/patches/` —— **BSD-3-Clause**（Chromium 衍生，让只想要浏览器的人不被 copyleft 波及）
- `shared/` schema —— **Apache-2.0**（便于他人实现兼容）

## 关键陷阱与约定

### sidecar 先于首次 cargo test

`desktop/src-tauri/tauri.conf.json` 把 agent 声明为 Tauri sidecar，`tauri-build` 要在 `binaries/fury-agent-<target-triple>` 找到它，而该目录被 gitignore。**新 clone 后不先跑 `node desktop/scripts/sidecar.mjs`，`fury-desktop` crate 编译不过**，报的是一条从不存在过的路径。跳过它跑 `cargo test --workspace --exclude fury-desktop` 仍然可用（CI 在 Linux 上就这么做），但要编译桌面壳或跑全量 workspace 测试（macOS job）就必须先建 sidecar。

### core 构建不可在 hosted runner 上做

Chromium checkout ~38 GB，首次构建约 6 小时 / 16 GB RAM；GitHub hosted runner 只有 14 GB 磁盘和 6 小时上限，两头都装不下。所以 `ci.yml` 只测补丁系列、detect-suite 和所有 Rust/TS crate；真正浏览器的验证（`core/verify/`）在自托管 runner（`self-hosted.yml`）上做。

### Linux 只是"能编译"

Linux 不是发布目标（明确决策，见 `docs/03`）。Rust 在 Linux 上继续编译，纯粹是为了 CI 和贡献者能跑测试套件。没有 Linux release、没有 Linux gn 配置、也没有计划。`fury-desktop` 在 Linux 上依赖 GTK/WebKitGTK，而这个项目永不在 Linux 上发布 —— 所以 CI 把它排除掉，而不是在 runner 上装一套永远用不上的 GTK 栈。

### 测试平台分工

- `fury-desktop` 的 Rust 测试只在 macOS 上跑（CI `shell-rust` job），因为它依赖 WKWebView。
- Windows 相关代码（named pipe、安全描述符、句柄继承、unsafe FFI）集中在一个 crate，以便 `windows-surface` job 能 `cargo check --target x86_64-pc-windows-msvc` —— 这是能 hosted 编译的上限，运行时行为要 `tools/verify-windows.ps1` 在真 Windows 上验。

### 本仓库的代码风格约定

无格式检查、无 lint 门禁。要求是匹配周边代码，这比任何规则集都更具体：
- 注释解释**为什么**，不是**是什么**。
- 涉及 upstream Chromium 代码的断言要标 `file:line`。
- 记录**实际测到的**而非假设的，包括先做错了什么 —— commit log 是语气的范本。

### 文档语言

`docs/` 下文档大多为俄语（15、19 有 `*.ru.md` 与英文版）。README 顶部是 Monica，`<details>` 折叠部分是上游 Fury 的内容。
