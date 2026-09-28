# Change: implement-super-command

**Status**: archived
**Date**: 2026-09-27
**Author**: fisher

关联 Spec：`specs/command-manager-tui.md`（总设计）、`specs/tui-page-layout.md`（布局与渲染参数）、`specs/keybindings.md`（键位）。本文只做实施拆解，不改设计；设计以三档为准，冲突时以三档为准并回改本文。

> 2026-09-27 用户确认：Step 1–9 由 Agent 执行，Step 10 手工冒烟由用户执行。

## Summary

按 `command-manager-tui.md`「Implementation Plan」的 10 步拆解 greenfield 实现：Cargo 脚手架 → `config` → `recent` → `runner` → `jobs` → `tui/event` → `tui/picker` → `tui/layout`+`mod`+`main` → 全量校验 → 手工冒烟。每步给出详细开发内容与验收方法，且每步收尾前该步新增代码的**单元测试行覆盖率 ≥ 80%**。

## Motivation

三份 Spec 已定稿且逻辑闭合（5 项待定全部落定，废弃内容已清除）。需要一份可逐步执行的实施文档，把「先测后码」和覆盖率闸门固化进步骤，避免一次性铺开导致状态机与并发代码难以定位缺陷。

## Design

实施层面的三条约束（设计细节不重复，见 Spec）：

1. **TDD 顺序**：每步先写失败测试，再写实现。Spec 里「不触达真实 spawn」的两步（`runner`/`jobs`）用注入点实现：`runner` 只组装 `CommandSpec` 数据结构（`program/args/env/cwd/stdin_null`），由 `jobs`/`main` 才调用 `Command::spawn()`，因此构造逻辑可完整单测。
2. **可测性优先**：渲染代码（`tui/layout.rs`、`main.rs`）保持薄壳——所有状态判定（焦点归属、列表展开条件、popup 高度、提示行内容）先算成纯函数/纯状态再交给渲染。若某文件仍无法达到 80%，在本文「Amendment」记录原因并给出实际数值，不静默放宽闸门。
3. **测试范围分层**：Agent 只执行不依赖外部环境的单元测试（`cargo test` + `cargo llvm-cov`）。需要真终端、真 TTY、真子进程残留检查的验证（keydump 实测、手工冒烟）标注为**用户执行**，Agent 不跑、不据其结果收工。

### 覆盖率工具

`cargo-llvm-cov`（`cargo llvm-cov`）。理由：与 `cargo test` 同一条命令产出，无需额外 nightly，可按文件报告行覆盖率。

- 单步闸门命令：`cargo llvm-cov --summary-only`（读 TOTAL 与分文件行覆盖率）
- 计数口径：`#[cfg(test)]` 块内的行不计入分母（llvm-cov 默认把测试函数算作已覆盖，因此覆盖率只会偏高不会偏低，作为下限闸门可接受）
- 若环境无 `cargo-llvm-cov`，第 1 步落地时报告并等待用户安装，不改用其它工具（避免口径漂移）

## Affected Specs

- `specs/command-manager-tui.md`：无改动需求（本文是其 Implementation Plan 的展开）。若实施中发现 feature 名/依赖版本与 Spec 不符，按 Spec「开工第一步须实测校验」条款回改该档「依赖」节。
- `specs/tui-page-layout.md`、`specs/keybindings.md`：无改动需求。
- 后续候选（本次不做）：沉淀 `specs/config-format.md`。

## Affected Code

全部新建：

- `Cargo.toml`、`rustfmt.toml`、`.gitignore`
- `src/main.rs`、`src/config.rs`、`src/recent.rs`、`src/runner.rs`、`src/jobs.rs`
- `src/tui/mod.rs`、`src/tui/event.rs`、`src/tui/picker.rs`、`src/tui/layout.rs`
- 各模块内 `#[cfg(test)] mod tests`
- `tools/keydump.rs`（临时测量程序，`examples/` 下，验收后删除）

---

## Implementation Plan

### Step 1：脚手架与依赖实测校验

**开发内容**

* `cargo init --name super-command`（edition 2021 或 2024，以本机 toolchain 支持的最新为准）
* 逐条 `cargo add` 校验 Spec「依赖」节的写法，特别是四个 feature 名：`cargo add ratatui --no-default-features --features termion,all-widgets,layout-cache,macros`；再加 `termion = "4"`、`serde`（derive）、`toml = "0.8"`、`thiserror = "2"`
* 校验「0.30 起后端拆独立 crate」是否成立：`cargo tree -p ratatui` 看是否引入 `ratatui-termion`；写一个 `main.rs` 最小片段构造 `TermionBackend` 并 `cargo check`，确认后端类型路径
* 落地 `rustfmt.toml`（`max_width=100`、`tab_spaces=4`、`hard_tabs=false`、`trailing_comma="Always"`、`indent_style="Block"`、`newline_style="Auto"`、`use_small_heuristics="Max"`）
* 安装/确认 `cargo-llvm-cov` 可用
* `.gitignore`：`/target`

**验收方法**

* `cargo check` 无 error 无 warning
* `cargo tree -p ratatui -e features` 输出中 feature 集合与 `Cargo.toml` 一致，且未引入 crossterm/其它后端
* `cargo run --bin super-command`（当前为空白终端切换）能进备用屏、显示光标恢复，无 panic
* `cargo llvm-cov` 能出报告（此时覆盖率不作要求）

**覆盖率闸门**：本步无业务逻辑，闸门 N/A。但**必须**留下可运行的 `cargo llvm-cov` 输出，否则后续步骤无法验收。

**若 feature 名对不上**：以实测为准修改 `Cargo.toml` 与 `command-manager-tui.md`「依赖」节，并在本文 Amendment 记录差异。

---

### Step 2：`config.rs` — 命令库只读加载

**开发内容**

* `CommandEntry { name, program, args: Vec<String>, env: BTreeMap<String,String>, cwd: Option<String> }`，`#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]`，`args`/`env`/`cwd` 带 `#[serde(default)]`
* `Config { entries: Vec<CommandEntry> }`，公共 API 仅 `Config::load(path) -> Result<Config, ConfigError>`（只读，无 save/add/remove/update）
* 路径解析：`home_dir()` 读 `$HOME`，拼 `~/.config/super-command/commands.toml`；`$HOME` 未设置 → `ConfigError::HomeNotSet`，`main` 报错退出
* 文件不存在 → 创建父目录与空文件（`[[command]]` 空内容），返回空 `Config`
* 解析失败 → `ConfigError::Parse`，消息含**路径 + toml 错误行号**，不静默覆盖
* 加载时对 `name` 为空的条目按 Spec 处理：Spec 未定义，本步实现为跳过并保留（不报错），并在 Amendment 记录待确认

**验收方法（单元测试，全部用 `tempdir` 风格注入路径，不读真实 `$HOME`）**

* round-trip：手写 TOML 字符串 → `load` → 断言 `name/program/args/env/cwd` 逐字段相等
* 缺字段：只有 `name`+`program` 的条目，`args`/`env`/`cwd` 取默认（空 vec / 空 map / None）
* 文件不存在：`load` 成功返回空集合，且磁盘上出现空文件与父目录
* 坏 TOML（缺右括号）：返回 `Err`，错误消息包含文件路径
* `env` 多条键值：用 `BTreeMap` 保证顺序稳定，断言序列化输出确定
* 依赖注入：`load` 接收显式 `&Path`，`$HOME` 拼接逻辑单独抽 `library_path_from(home: &str)` 纯函数测

```bash
cargo test config:: && cargo llvm-cov --summary-only   # src/config.rs 行覆盖率 ≥ 80%
```

---

### Step 3：`recent.rs` — 最近执行副本

**开发内容**

* `RecentFile { #[serde(default)] recent: Vec<CommandEntry> }`（私有结构，仅落盘形态）
* `Recent::{load(path) -> Result<Recent, RecentError>, push(&mut self, entry: CommandEntry) -> Result<(), RecentError>, entries(&self) -> &[CommandEntry]}`
* 路径：`~/.local/super-command/recent.toml`；同样只暴露 `recent_path_from(home: &str)` 纯函数
* push 规则：完整副本判重（`CommandEntry: PartialEq` 全字段相等）→ 命中则先移除旧条目 → 前插（MRU）→ 截断到 30 条 → 原子落盘
* 原子落盘：同目录 `recent.toml.tmp` 写入 + `fsync` + `rename`；任何一步失败返回 `Err`，**内存态保留不丢**
* `load`：不存在 → 空列表并创建；解析失败 → 原文件改名 `recent.toml.bak`，返回空列表（不阻断）
* 写失败错误 `RecentError::Io` 由调用方渲染到提示行

**验收方法（单元测试，临时目录注入，禁止碰真实 `$HOME`）**

* 判重：push 同一 entry 两次 → `entries().len() == 1` 且位于首位
* MRU：push A、B、A → 顺序 `[A, B]`
* 截断：push 31 条互不相同的 entry → `len() == 30` 且首条为最后 push 的
* 部分相同：`args` 不同即视为不同条目（判重不误伤）
* round-trip：push 后重新 `load`，内容一致
* 原子性：落盘后目录内**不存在** `.tmp` 残留文件；`rename` 前内容完整
* 坏文件：预先写入非法 TOML → `load` 返回空列表且 `.bak` 出现
* 不存在：`load` 创建文件并返回空

```bash
cargo test recent:: && cargo llvm-cov --summary-only   # src/recent.rs 行覆盖率 ≥ 80%
```

---

### Step 4：`runner.rs` — 命令构造（不 spawn）

**开发内容**

* `CommandSpec { program, args, env, cwd: Option<PathBuf>, stdin_null: bool }` — 纯数据
* `build_spec(entry: &CommandEntry, home: &str) -> CommandSpec`：逐字段搬运，`stdin_null` 恒为 `true`
* `expand(path: &str, home: &str) -> PathBuf`：`~` / `~/xxx` 展开为 `$HOME`；绝对路径原样；相对路径原样（由 spawn 时 cwd 决定）
* 不 import `std::process::Command`，不触达 spawn（保持单测无副作用）
* 供 Step 5/8 使用：`to_command(&self) -> Command` 放在 `jobs.rs` 侧或直接 `impl CommandSpec`，若加则其单测仅断言 `get_program()`/`get_args()`/`get_envs()`/`get_current_dir()`，**不调用 spawn**

**验收方法（单元测试）**

* `env` 与 `args` 顺序/内容完整搬运（`args = ["--message", "hello world"]` 保持两个 arg，不重分割）
* `cwd = None` → `CommandSpec.cwd == None`；`cwd = Some("~/code")` → 展开为 `<home>/code`
* `expand` 边界：`"~"`、`"~/a/b"`、`"/abs"`、`"relative"`、`"~user"`（不展开，原样）
* `stdin_null` 恒 true
* 若实现 `to_command`：断言 `cmd.get_program() == program`、`cmd.get_args().collect::<Vec<_>>()` 与 args 一致、`cmd.get_current_dir()` 匹配

```bash
cargo test runner:: && cargo llvm-cov --summary-only   # src/runner.rs 行覆盖率 ≥ 80%
```

---

### Step 5：`jobs.rs` — Job 登记表、reader、滚动缓冲

**开发内容**

* `JobId(u32)` 单调自增；`JobState { Running { started: Instant }, Exited(i32) }`
* `Job { id, entry: CommandEntry, state, buf: Arc<Mutex<OutputBuf>>, child_pid: u32 }`
* `OutputBuf { lines: VecDeque<String>, pending: String, truncated_from_head: u64 }`，上限常量 `MAX_LINES = 2000`
* 写入约定实现：`append_bytes(chunk: &[u8])` → `String::from_utf8_lossy` → 按 `\n` 切分；无换行尾部的部分存入 `pending`；超过 2000 行从头部 `pop_front` 并累加丢弃计数
* `Jobs`（登记表，单线程访问，不加锁）：`add` / `get` / `selected` 移动 / `clear_exited(id)` / `kill_all()`
* `kill_all()`：对全部 `Running` 发 `kill()` 并 `wait()` 回收；已退出的跳过（**测试用 fake child 或纯状态断言，不 spawn 真进程**）
* reader 线程主体放 `spawn` 侧（Step 8 或本步带真实 spawn 的封装），本步**只实现字节到缓冲的纯逻辑**，线程壳留到 Step 8；EOF/退出码回写函数 `mark_exited(id, code)` 在本步实现并测试

**验收方法（单元测试，不 spawn）**

* 半行拼接：先 append `"he"`、再 append `"llo\nwor"`、再 append `"ld\n"` → lines 为 `["hello","world"]`，`pending` 为空
* 非 UTF-8：append `&[0xff, 0xfe, b'\n']` → 一行含 U+FFFD，不 panic
* 裁剪：append 2500 行 → `lines.len() == 2000`，保留的是**尾部** 2000 行，首行为第 501 行；`truncated_from_head == 500`
* 空块 / 只有换行的块：`\n\n` → 两个空行，`pending` 空
* 状态流转：`Running` → `mark_exited(1)` → `Exited(1)`；对 `Exited` 再 `mark_exited` 为幂等或 `debug_assert`（择一实现并测）
* 登记表：`clear_exited` 只作用于 `Exited`；对 `Running` 调用被忽略（对应键位「运行中忽略」）
* 选中移动：`selected_next/prev` 在边界与列表清空后的行为；`clear` 掉选中行后焦点下移（末行则上移）
* `kill_all`：构造两个 `Exited` + 一个 fake `Running`，断言只遍历 Running 且调用计数为 1

```bash
cargo test jobs:: && cargo llvm-cov --summary-only   # src/jobs.rs 行覆盖率 ≥ 80%
```

---

### Step 6：`tui/event.rs` — 先实测 keydump，再定解析

**6a. keydump 测量（用户执行，Agent 不跑）**

```bash
# 前置：真实终端（gnome-terminal / xterm / tmux 各测一遍），raw mode 程序
cargo run --example keydump
# 依次输入并观察输出：Alt+Enter、单独 Esc、Esc 后快速 Enter、Enter、Ctrl+C
```

判定标准：三种输入的**原始字节序列**与 termion `Keys` 事件序列被逐条打印。把实测结论（termion 是否直接给出 `Alt('\r')`）填回本文 Amendment，并据此定 ESC 合并规则；若 termion 已给 `Alt('\r')`，合并逻辑退化为直通，仍保留 50ms 超时分支。

`tools` 示例程序：读 `stdin` 原始字节，边打 hex 边打解析出的 `Keys`，不做其它逻辑，验收后删除。

**6b. 开发内容**

* `AppEvent { Key(KeyKind), Tick, Resize(u16,u16), ChildExited(JobId, i32), ByteChunk(JobId) }`
* `KeyKind` 覆盖 Spec 键位全集：`Char(char)`、`AltEnter`、`Enter`、`Esc`、`Tab`、`BackTab`、`Up`、`Down`、`PgUp`、`PgDn`、`Home`、`End`、`CtrlU`、`CtrlC`、`CharOther`
* 输入线程：`termion::async_stdin`/阻塞读 → `Keys::read_key` → 投递 mpsc；**ESC 前缀合并**在本线程完成：一批字节里 `ESC` 紧跟 `\r` 判为 `AltEnter`；`ESC` 后 50ms 无后续判为独立 `Esc`
* tick 线程：每 1s 投递 `Tick`
* 主循环取事件：`recv_timeout(Duration::from_millis(50))`（50ms 与 1s 取小，见 Spec）
* resize：termion 无事件，`Tick` 时比对 `size()`，变化则 `Resize`

**验收方法（单元测试：解析函数与线程解耦）**

* 把「字节序列 → `Vec<AppEvent>`」抽成纯函数 `parse_bytes(&[u8]) -> Vec<KeyKind>`（含 50ms 边界由参数注入的假时钟/显式 `esc_followed` 标志控制，不 sleep）
* 用例：`[0x1b, 0x0d]` → `[AltEnter]`；`[0x1b]` 单独 → `[Esc]`；`[0x1b, 0x0d]` 与 `[0x0d]` 混合流按序拆分；`Ctrl+C` = `0x03`；`BackTab` = `ESC [ Z` 不被误判为 Alt 组合；`Tab` = `0x09`；`Ctrl+U` = `0x15`
* 用例：`[0x1b, b'[', b'A']` → `[Up]`（ESC 序列键不误判为 Esc+Char）
* 超时分支：`parse_bytes` 在 `esc_timeout=true` 输入下产出 `[Esc]`

```bash
cargo test tui::event && cargo llvm-cov --summary-only   # src/tui/event.rs 行覆盖率 ≥ 80%
```

线程壳（`spawn` + mpsc）不强制单测覆盖，其正确性由 Step 8/10 冒烟验证；若因此压不到 80%，在 Amendment 记录并给出纯函数部分覆盖率。

---

### Step 7：`tui/picker.rs` — 两级列表状态机

**开发内容**

* `PickerState { Closed, Level1 { source: Library|Builtin, filter: String, highlight: usize }, Level2Recent { filter: String, highlight: usize } }`
* `InputArea { text: String, bound: Option<CommandEntry>, prompt: Option<Prompt> }`；`Prompt { level: Error|Warn, text: String }`
* 按键入口 `handle_key(&mut self, key: KeyKind)`，产出 `PickerAction`（`OpenLevel1`、`OpenLevel2`、`Bind`、`Execute`、`Error(text)`、`None`）供主循环消费；状态机本身不 spawn、不写 recent、不渲染，也不处理 `Tab` / `Ctrl+C` / 退出提示（全局键，归主循环）
* 规则实现清单（逐条对应 `keybindings.md`）：
  * 首字符 `#` → 库列表全量；`/` → 内置列表（当前仅 `recent`）
  * 首字符为其它可打印 → 忽略入框，但清空提示行
  * 已有触发符 → 尾部追加为过滤词，实时子串、大小写不敏感筛选
  * 匹配字段：`#` 按 `name`；`/` 按内置命令名；recent 二级按副本 `name`
  * 匹配 0 条 → 保持列表展开、渲染空列表框（状态机提供 `visible_count() == 0` 且 `is_open()` 仍为 true）
  * `Backspace`：删至只剩触发符 → 列表重开为全量；删至空 → 关闭回初始态
  * `Ctrl+U` → 清空整行，关闭列表回初始态
  * `Enter` → 绑定 + 补全显示文本（`#` 列表补全为 `#<name>`）+ 列表收起
  * 绑定后继续打字 → 解绑回过滤模式
  * 空框 `Alt+Enter` → 忽略 + 清空 + 红字；未绑定 `Alt+Enter` → 红字，不自动取第一项
  * 例外：`/` 一级列表 `Alt+Enter` 等同 `Enter`；recent 二级列表 `Enter` 直接执行（`Execute` action），执行后清空输入框、关闭列表
  * 二级列表忽略首字符限制，任意可打印字符作过滤词；清空输入框 → 回初始态（不是退回一级）
  * `Esc`：二级 → 一级（输入框恢复 `/`）；一级 → 初始态。初始态下的 `Esc` 由主循环处理为退出提示（与 `Ctrl+C` 共享），不进 picker
  * 焦点离开输入区 → 列表收起，**绑定与显示文本保留**；`Tab` / `Shift-Tab` 由主循环处理：列表展开时先收起再切焦点，列表收起时直接切焦点（不轮换提示文案）
  * 列表收起时 `↑/↓` 无动作
  * 提示行：任意按键事件即清空

**验收方法（单元测试，纯状态机 + 注入条目列表）**

* 每条上表规则至少一个用例；关键断言：非法首字符「不入框但清提示」、无匹配不收起、`Enter` 后 `text == "#<name>"` 且 `bound.is_some()`、绑定后输入字符使 `bound == None`、未绑定 `Alt+Enter` 只出红字不出 `Execute`、recent 二级 `Enter` 出 `Execute` 且状态回 `Closed`、二级 `Ctrl+U` 回 `Closed` 而非一级、`Esc` 二级→一级→初始态两层阶梯、焦点切走再回 `bound` 保留
* 过滤：`#` + `"SUB"` 匹配 `name = "Clippy-Substring"`（大小写不敏感子串）；`"xyz"` 无匹配 → `visible_count()==0 && is_open()`
* 高亮越界：过滤后条数减少时 highlight 收敛到合法范围

```bash
cargo test tui::picker && cargo llvm-cov --summary-only   # src/tui/picker.rs 行覆盖率 ≥ 80%
```

---

### Step 8：`tui/layout.rs` + `tui/mod.rs` + `main.rs` — 渲染与装配

**开发内容**

* 布局：`Layout::vertical([Min(1), Min(0), Length(2)])`（状态 / 输出 / 交互，见 [三区布局](../../tui-page-layout.md)）；三段切分**永不**随列表开合变化；popup 由 `Clear` + `List`/`Block` 叠加绘制在输出区下缘，高度 `min(visible_count, 10) + 2`
* 状态区：高度受终端 20% 上限（`max(1, area.height * 20 / 100)`），超出滚动；行内容 `名称 / 状态 / 时长 / pid`，非零退出码标红；不回收
* 交互区：第 1 行输入框（常驻光标，焦点非输入区时光标隐藏），第 2 行提示行（常驻占位，空时渲染空白行）
* 输出区：显示选中 Job 缓冲；滚动离开底部暂停自动跟随（`follow: bool`），`End` 恢复、`Home` 跳最早保留行、`PgUp/PgDn` 按页、`↑/↓` 按行、`c` 清空当前 Job 缓冲
* 主循环 `run()`：`recv_timeout(50ms)`；`Key` → 焦点/picker/状态区/输出区分派；`Tick` → drain 缓冲增量 + 刷新时长 + resize 比对；`Alt+Enter`/`Enter` 产生的 `Execute` action → 先 `Recent::push` 再 spawn 注册 Job 并自动选中、输出区跟随
* 每 Job 两条 reader 线程（stdout/stderr）：读块 → 投 `ByteChunk` → 主线程 `append_bytes`；退出码由主线程 `try_wait` 收割后投 `ChildExited`（每个 Job 只投一次），主线程 `record_exit`
* Ctrl+C 双击：首次显示「再按 Ctrl+C 退出」，2s 内第二次退出，期间任意其它键取消；`Esc` 初始态共享同一提示
* 退出清理：kill 全部 Running 并 wait → 恢复终端（显示光标、退备用屏、关 raw mode）→ `std::process::exit(0)`（不 join 输入线程）
* 纯函数抽取以便测试：`status_area_height(term_h: u16) -> u16`、`popup_height(visible: usize) -> u16`、`layout_split(area) -> [Rect;3]`、`scroll_next/prev/page/top/bottom(follow state)`

**验收方法**

* 纯函数单测：`popup_height(0)==2`、`popup_height(3)==5`、`popup_height(50)==12`；`status_area_height(40)==8`、`(5)==1`（不塌成 0）；`layout_split` 三段和等于总面积
* 滚动函数：`End` 置 `follow=true`；非底部时新写入不移动视口 offset
* 渲染冒烟（**用户执行**，需要真终端）：
  ```bash
  cargo run --release
  ```
  前置：`~/.config/super-command/commands.toml` 至少 2 条（含一条带 `args`/`env`/`cwd`）。判定：三区可见、提示行常驻底部、`#` 展开列表、匹配 0 条时出现空列表框且布局不抖、`Tab` 收列表切焦点。
* `spawn failed` 路径：库条目 `program = "definitely-not-a-command-xyz"`，`Alt+Enter` 提交 → 提示行红字 `spawn failed: No such file or directory`，不注册 Job（用户执行）

```bash
cargo test --bins && cargo llvm-cov --summary-only   # src/tui/layout.rs 行覆盖率 ≥ 80%（纯函数部分）；main.rs 不强制
```

`main.rs` 的终端生命周期与线程壳无法在单测内运行（依赖真 TTY），归入用户手工验证；若 `layout.rs` 仍压不到 80%，在 Amendment 记录实际数值与原因。

---

### Step 9：全量校验闸门

**开发内容**

* 修复前序遗留：`cargo fmt` 归一、clippy 告警清零、死代码清理
* 复核每个模块文件 ≤ 1500 行、函数 ≤ 150 行、控制流嵌套 ≤ 4；超限则拆子模块（`tui/` 下已拆，必要时 `jobs/`、`config/` 再分）
* 复核公共 API 面：`config` 暴露 `CommandEntry`/`Config::load`，`recent` 暴露 `Recent::{load,push}`，`runner` 暴露 `CommandSpec`/`build_spec`，`jobs` 暴露 `Jobs`/`Job`，`tui` 暴露 `run`；确认无 `pub mod` 未收敛、无 `pub use *`、无多余 `#[must_use]`

**验收方法**

```bash
cargo fmt --all -- --check   # 退出码 0
cargo test                   # 全绿，0 failed
cargo check                  # 0 error 0 warning
cargo clippy --all-targets   # 0 error 0 warning
cargo llvm-cov --summary-only
```

判定：以上五条全部通过；`src/` 下每个业务模块（`config.rs`/`recent.rs`/`runner.rs`/`jobs.rs`/`tui/event.rs`/`tui/picker.rs`/`tui/layout.rs`）行覆盖率均 ≥ 80%，`main.rs` 与线程壳除外（按 Step 8 归入用户验证，且需在 Amendment 列出实际值）。任一项不达标则回到对应 Step 补测，不带红灯进入 Step 10。

---

### Step 10：手工冒烟（用户执行，Agent 不跑不验收）

```bash
cargo run --release
```

前置：真终端；`commands.toml` 含 `sleep 3`（带 args）、一条长输出命令（如 `seq 1 5000`）、一条必失败命令；`/recent` 有历史。

判定清单（逐项符合 Spec）：

1. 后台并发：连提两条 `sleep 3`，界面不卡，状态区两行 running 时长同步增长，槽位独立
2. 长输出：`seq 1 5000` 期间输出区按 1s 增量刷新；向上滚动后新输出不抢视口，`End` 恢复跟随；结束后仅保留尾部 2000 行
3. `#` 库列表：选中 → 补全 → `Alt+Enter` 执行 → recent 状态区新增行 + 自动选中
4. `/recent`：二级列表 `Enter` 直接执行、输入框清空、列表关闭；重跑同一条后 recent 仍只 1 条且置顶
5. 手改 `commands.toml` 后**重启**才生效（运行中不感知）
6. `x` 清除已退出行；对 running 行 `x` 无反应
7. `Ctrl+C` 双击退出；`ps` 确认无残留子进程（对应 Spec「退出清理」）
8. 非零退出码在状态区标红

Agent 交付时只声明「单元测试与静态检查通过」，不代述本步结论。

---

## 完成定义

Step 1–9 全部通过 + Step 10 由用户执行并反馈；本文 Status 转 `implemented`，归档至 `specs/changes/archive/`。

## Amendment

（实施中追加：feature 名实测差异、无法达到 80% 的文件及原因、`name` 为空条目的处理结论、keydump 实测结论。）

* 2026-09-27 · `name` 为空条目：结论为加载时跳过、不报错，已写入 [命令管理器总设计](../../command-manager-tui.md)「数据模型」；`src/config.rs` 已按此实现并有单测。
* 2026-09-27 · Step 8 覆盖率（`cargo llvm-cov --summary-only` 行覆盖）：`app.rs` 99.6%、`layout.rs` 98.6%、`picker.rs` 98.9%、`event.rs` 80.4%（未覆盖部分为 stdin / tick 线程壳）、`main.rs` 0%（终端生命周期，归用户手工验证）。
* 2026-09-27 · 模块布局：保持 `src/` 平铺，App 状态与事件分派放 `src/app.rs`（对应 Spec 中 `tui/mod.rs` 的职责），渲染放 `src/layout.rs`。终端尺寸变化由 ratatui 每次 `draw` 比对，不再单独发 `Resize` 事件。
* 2026-09-27 · `recent.toml` 对齐 [命令管理器总设计](../../command-manager-tui.md)：数组表名由 `[[entry]]` 改为 `[[recent]]`，文件不存在时启动即创建。旧格式文件会被当作解析失败改名为 `.bak`。
* 2026-09-27 · Step 9 闸门：`cargo fmt --check`、`cargo test`（111 通过）、`cargo check`、`cargo clippy --all-targets` 均 0 告警；覆盖率同 Step 8 记录，`main.rs` 0% 归用户验证。最大文件 `app.rs` 637 行，最长函数约 47 行，控制流嵌套 ≤ 4。模块内部辅助项已收为私有；[命令管理器总设计](../../command-manager-tui.md) 与 [键位](../../keybindings.md) 的模块路径、`runner` API 已按实现同步。`rustfmt.toml` 中 `trailing_comma` / `indent_style` 为 nightly 选项，stable 下仅提示、不生效。
* keydump 实测（Step 6a）待用户执行；`examples/keydump.rs` 保留至实测结论写入本节后删除。
