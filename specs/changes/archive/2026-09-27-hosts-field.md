# Change: hosts-field

**Status**: implemented
**Date**: 2026-09-27
**Author**: fisher

## Summary

`commands.toml` 条目新增可选 `hosts` 字段：设置后该命令在独立 mount namespace 中执行，`/etc/hosts` 被替换为指定文件，不影响系统 hosts；有无 `hosts`（或 `hosts` 不同）视为不同命令，recent 中分开记录。

## Motivation

部分命令需要在特定 hosts 环境运行，但不希望切换整个系统的 hosts。手工做法是 `sudo unshare --mount … /bin/bash` + `mount --bind <hosts> /etc/hosts`，在本工具中不可用：

* stdin 为 null，sudo 无法询问密码；免密只能配 `NOPASSWD unshare`，等同免密 root；
* 命令以 root 运行，sudo 重置环境变量，条目 `env` 丢失；
* 退出清理对 `sudo` 发 `SIGKILL` 无法转发，`unshare` 及其子进程成为用户杀不掉的 root 孤儿进程，违反「退出清理：不留孤儿进程」；
* 免 sudo 的 `unshare -rm` 在本机被 AppArmor 拦截（`kernel.apparmor_restrict_unprivileged_userns = 1`）。

`bwrap`（bubblewrap）是同一机制（mount namespace + bind mount）的无 root 封装，Ubuntu 桌面默认安装且有 AppArmor profile 放行。本机实测：沙箱内 `id -u` 仍为 1000、`getent hosts` 命中自定义条目、宿主侧不受影响。

## Design

### 数据模型

```toml
[[command]]
name = "api-test"
comment = "测试环境 hosts 下调健康检查"
hosts = "~/hosts/test"          # 可省略；绝对路径或 ~/ 开头
cmd = "curl -v http://api.example.com/health"
```

* `CommandEntry` 新增 `hosts: Option<String>`（`#[serde(default, skip_serializing_if = "Option::is_none")]`），存**原始写法**（未展开）；
* `cmd` 与 `program`/`args` 两种写法都可搭配 `hosts`；
* 校验（加载时，失败按现有「条目校验失败」报错退出，打印路径、条目序号与 `name`）：
  * `hosts` 为空或全空白 → `hosts 为空`（不静默当作未设置，避免误用系统 hosts 执行）；
  * `hosts` 既不是绝对路径也不以 `~/` 开头 → `hosts 必须是绝对路径或 ~/ 开头`；
* 加载时**不检查文件是否存在**：hosts 文件可能是临时生成的（如 `/tmp/...`），执行时由 bwrap 报错（见「错误处理」）。

### 执行：`build_spec` 包装

`runner::build_spec` 在 `hosts` 为 `Some` 时把原命令包进 bwrap，`env` / `cwd` 照旧直通：

```text
program = "bwrap"
args    = ["--dev-bind", "/", "/",
           "--ro-bind", <hosts 展开 ~ 后>, "/etc/hosts",
           "--unshare-pid", "--proc", "/proc",
           "--die-with-parent",
           "--", <program>, <args...>]
```

| 参数 | 作用 |
|---|---|
| `--dev-bind / /` | 沙箱内看到完整宿主文件系统（含设备），行为与直接执行一致 |
| `--ro-bind <hosts> /etc/hosts` | 仅替换 `/etc/hosts`，只读 |
| `--unshare-pid --proc /proc` | 独立 PID namespace；bwrap 退出时 namespace 内全部进程被内核回收 |
| `--die-with-parent` | 本程序退出清理对 bwrap 发 `SIGKILL` 时，沙箱内进程随之终止，不留孤儿 |

* 以当前用户身份运行，无需 sudo；子进程继承 `env`，工作目录沿用 `cwd`（`/` 整体绑定，路径不变）；
* 副作用：命令处于独立 PID namespace，`ps` 只能看到自身进程树；
* 包装只发生在 `build_spec`：`CommandEntry`、列表预览、recent 副本均保存原始 `program`/`args` + `hosts`，不出现 bwrap 参数。

### recent 判重

`hosts` 属于命令本身：`same_command` 比较字段扩为 name + program + args + env + cwd + **hosts**（`comment` 仍不参与）。同一命令有无 `hosts`、或 `hosts` 路径不同，在 recent 中是两条独立记录。

### 界面

* 列表条目第 1 行命令预览追加 `  HOSTS: <原始路径>`（位于 `ENV:` / `CWD:` 之后），与其它字段同样过长截断；
* 状态区、输出区、过滤规则不变。

### 错误处理

| 情况 | 表现 |
|---|---|
| 条目 `hosts` 为空 / 相对路径 | 启动时报错退出（条目校验失败） |
| 执行时 hosts 文件不存在 | bwrap 输出错误信息到输出区并以非零码退出，状态区标红；recent 已按「提交即写」记录 |
| 系统未安装 `bwrap` | 走现有路径：提示行红字 `spawn failed: <os error>` |

## Affected Specs

- `specs/command-manager-tui.md`：数据模型加 `hosts` 字段与校验规则、`CommandEntry` 结构、recent 判重字段、「运行方式」/「子进程 I/O 约定」补 bwrap 包装、错误处理、运行时依赖 `bwrap`
- `specs/tui-page-layout.md`：选择列表条目预览追加 `HOSTS:`

## Affected Code

本变更跨模块（用户已于 2026-09-27 许可）：

- `src/config.rs`：`CommandEntry.hosts`、`RawEntry.hosts` 与校验、`same_command` 纳入 `hosts`；单测
- `src/runner.rs`：`build_spec` 的 bwrap 包装与 `~` 展开；单测（纯构造，不真实 spawn）
- `src/layout.rs`：`entry_preview` 追加 `HOSTS:`；单测
- `src/recent.rs`：判重单测（有无 hosts 为两条）
- `src/app.rs`、`src/jobs.rs`、`src/picker.rs`、`src/layout.rs`、`src/recent.rs`、`src/runner.rs` 测试辅助构造：补 `hosts: None`（机械修改，不改逻辑）

## Implementation Plan

1. 更新 living spec：`specs/command-manager-tui.md`、`specs/tui-page-layout.md`
2. `config.rs`：字段 + 校验 + `same_command` + 单测
3. `runner.rs`：bwrap 包装 + 单测
4. `layout.rs`：预览 `HOSTS:` + 单测；`recent.rs` 判重单测；各测试辅助补 `hosts: None`
5. `cargo fmt` / `cargo test` / `cargo clippy --all-targets` 全绿，`cargo llvm-cov` 各改动模块行覆盖 > 80%
6. 手工冒烟：`hosts` 指向含自定义条目的文件，`cmd = "getent hosts <name>"` 命中；宿主 `getent` 不命中；无 hosts 与有 hosts 各执行一次，`/recent` 出现两条；退出程序后 `pgrep bwrap` 无残留
