# Super Command - 命令管理器 TUI

一个结构化的命令管理工具，用于持久化记录和执行常用命令（程序路径、参数、环境变量、工作目录）。

## 核心特性

- **结构化命令库**：TOML 格式存储命令及其元数据（args/env/cwd），支持 `~` 展开
- **快速触发**：输入框通过 `#`（命令库）和 `/`（内置命令）弹出选择列表
- **并发执行**：支持多条指令同时后台运行，实时输出展示
- **最近执行历史**：MRU（最近使用）顺序记录最近 30 条执行的命令副本
- **独立 hosts 环境**：通过 bubblewrap 为命令提供独立的 `/etc/hosts` 文件，不影响系统配置
- **三区布局**：状态区（Job 列表）、输出区（滚动缓冲）、交互区（输入框 + 提示行）

## 设计文档

详细的设计规范位于 [specs/](specs/) 目录：

### 主设计

- **[specs/command-manager-tui.md](specs/command-manager-tui.md)** — 总体架构、技术选型、数据模型、模块结构、线程模型、错误处理

### 界面与交互

- **[specs/tui-page-layout.md](specs/tui-page-layout.md)** — 三区页面布局、渲染行为、popup 覆盖规则、刷新参数
- **[specs/keybindings.md](specs/keybindings.md)** — 完整的键位表、焦点切换规则、两级选择列表交互逻辑

### 变更归档

- **[specs/changes/archive/](specs/changes/archive/)** — 已实施的变更记录

## 快速开始

### 数据存储

| 用途 | 路径 | 说明 |
|---|---|---|
| 命令库 | `~/.config/super-command/commands.toml` | 手工编辑，程序启动时只读加载 |
| 最近执行 | `~/.local/super-command/recent.toml` | 程序自动读写，MRU 顺序保留最多 30 条 |

### 命令条目格式

```toml
[[command]]
name = "clippy-all"              # 显示名（必填）
comment = "全量 clippy 检查"     # 说明（可选）
program = "cargo"                # 程序路径
args = ["clippy", "--all-targets"]
env = { RUST_LOG = "debug" }    # 环境变量（可选）
cwd = "~/code/rust/foo"         # 工作目录（可选，支持 ~ 展开）
hosts = "~/hosts/test"          # 独立 hosts 文件（可选）

# 或使用 cmd 整行写法（与 program/args 二选一）
[[command]]
name = "cowork-docker"
cmd = """
docker run -d --network host \
  -v /home/fisher/docker/cowork/workspaces:/home/workspaces \
  --name cowork cowork
"""
```

### 运行时依赖

- **bubblewrap**（`bwrap`）：仅当命令条目设置了 `hosts` 字段时需要（Ubuntu 桌面默认安装）

## 交互概览

```
▶ cargo clippy --all-targets   [running 00:12] pid 4812   ┐ 状态区
▶ cargo test                   [running 00:03] pid 4899   │
✓ build.sh                     [exit 0]                   ┘
                                                              ← 空行
── clippy-all ─────────────────────────────────────────────  ← 分隔线①
 warning: unused variable: x                               ┐ 输出区
  --> src/main.rs:3:9                                      │
 …                                                         │
───────────────────────────────────────────────────────────  ← 分隔线②
 > #clip│                                                  ┐ 交互区
   （提示行：空白或错误信息）                                ┘
```

### 基本操作流

1. 按 `#` 打开命令库列表，输入过滤词筛选
2. `↑/↓` 移动高亮，`Enter` 选中绑定到输入框
3. 按 `Alt+Enter` 提交执行（后台并发，不阻塞界面）
4. 状态区自动新增一行并选中，输出区跟随显示该 Job 的输出
5. 按 `Tab` 在三区之间循环切换焦点

完整键位见 [keybindings.md](keybindings.md)。

## 技术栈

- **TUI 框架**：`ratatui` + `termion`（Unix-only）
- **存储格式**：TOML（`serde` + `toml`）
- **命令拆分**：`shlex`（POSIX shell 引号/转义规则）
- **错误处理**：`thiserror`
- **沙箱**：`bubblewrap`（`--unshare-pid --ro-bind hosts`）

## 边界说明

本工具**只执行命令库中预先注册的条目**，不接受自由文本命令行，不支持交互式命令（vim、htop、python REPL 等需要用户持续输入的 TTY 程序）。
