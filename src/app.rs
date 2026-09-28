//! 主循环状态与事件分派：焦点、选择列表、Job 选中与滚动、提示行、退出确认、recent 历史召回。
//!
//! 不持有 `Terminal`，渲染见 [`crate::layout::draw`]；键位语义以 `specs/keybindings.md` 为准。

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use crate::config::CommandEntry;
use crate::event::{AppEvent, KeyKind};
use crate::jobs::{JobId, JobState, Jobs};
use crate::layout::{layout_split, Scroll};
use crate::picker::{self, Picker, PickerAction, PickerState, Selection, Source};
use crate::recent::Recent;
use crate::runner::build_spec;

/// `Ctrl+C` 双击确认窗口。
const EXIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);

/// 内置命令名（`/` 列表内容）。
const BUILTINS: [&str; 1] = ["recent"];

const EXIT_CONFIRM_TEXT: &str = "再按 Ctrl+C 退出";

/// 持有焦点的区域；`Tab` 顺序：交互 → 状态 → 输出 → 交互。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Input,
    Status,
    Output,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::Input => Focus::Status,
            Focus::Status => Focus::Output,
            Focus::Output => Focus::Input,
        }
    }

    fn prev(self) -> Self {
        match self {
            Focus::Input => Focus::Output,
            Focus::Status => Focus::Input,
            Focus::Output => Focus::Status,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptLevel {
    Error,
    Warn,
}

/// 提示行内容（交互区第 2 行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    pub level: PromptLevel,
    pub text: String,
}

/// 事件处理结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Idle,
    Redraw,
    Quit,
}

/// 选择列表当前可见条目（按键与渲染共用）。
pub enum ListItems<'a> {
    Library(Vec<&'a CommandEntry>),
    Builtin(Vec<&'static str>),
    Recent(Vec<&'a CommandEntry>),
}

impl ListItems<'_> {
    pub fn len(&self) -> usize {
        match self {
            ListItems::Library(v) | ListItems::Recent(v) => v.len(),
            ListItems::Builtin(v) => v.len(),
        }
    }
}

pub struct App {
    pub library: Vec<CommandEntry>,
    pub recent: Recent,
    recent_path: PathBuf,
    home: String,
    pub jobs: Jobs,
    pub picker: Picker,
    pub focus: Focus,
    pub selected: Option<JobId>,
    pub scroll: Scroll,
    pub prompt: Option<Prompt>,
    exit_armed: Option<Instant>,
    /// recent 历史召回游标（`↑/↓` 浏览队首最新；`None` = 未在召回态）。
    recall: Option<usize>,
    /// 最近一次绘制的终端区域（按页滚动需要视口高度）。
    pub area: Rect,
    tx: Sender<AppEvent>,
}

impl App {
    pub fn new(
        library: Vec<CommandEntry>,
        recent: Recent,
        recent_path: PathBuf,
        home: String,
        tx: Sender<AppEvent>,
    ) -> Self {
        App {
            library,
            recent,
            recent_path,
            home,
            jobs: Jobs::new(),
            picker: Picker::default(),
            focus: Focus::Input,
            selected: None,
            scroll: following(),
            prompt: None,
            exit_armed: None,
            recall: None,
            area: Rect::default(),
            tx,
        }
    }

    pub fn handle_event(&mut self, event: AppEvent, now: Instant) -> Flow {
        match event {
            AppEvent::Key(key) => self.on_key(key, now),
            AppEvent::Tick => Flow::Redraw,
            AppEvent::ByteChunk(chunk) => {
                self.jobs.record_bytes(chunk.id, &chunk.bytes);
                Flow::Idle
            }
            AppEvent::ChildExited { id, code } => {
                if matches!(self.jobs.state(id), Some(JobState::Running { .. })) {
                    self.jobs.record_exit(id, code);
                }
                Flow::Idle
            }
        }
    }

    /// 退出提示超过 2s 未确认则撤销；返回是否需要重绘。
    pub fn expire_exit_prompt(&mut self, now: Instant) -> bool {
        match self.exit_armed {
            Some(at) if now.duration_since(at) >= EXIT_CONFIRM_WINDOW => {
                self.exit_armed = None;
                self.prompt = None;
                true
            }
            _ => false,
        }
    }

    /// 选择列表当前可见条目；列表收起时为 `None`。
    pub fn list_items(&self) -> Option<ListItems<'_>> {
        match &self.picker.state {
            PickerState::Closed => None,
            PickerState::Level1 { source: Source::Library, filter, .. } => {
                Some(ListItems::Library(picker::filter_entries(&self.library, filter)))
            }
            PickerState::Level1 { source: Source::Builtin, filter, .. } => {
                Some(ListItems::Builtin(
                    BUILTINS.into_iter().filter(|name| picker::matches(filter, name)).collect(),
                ))
            }
            PickerState::Level2Recent { filter, .. } => {
                Some(ListItems::Recent(picker::filter_entries(self.recent.entries(), filter)))
            }
        }
    }

    /// 输出区视口高度（输出区无边框，全部行可用）。
    pub fn output_viewport(&self) -> usize {
        layout_split(self.area, self.jobs.ids().len()).output.height as usize
    }

    fn on_key(&mut self, key: KeyKind, now: Instant) -> Flow {
        if key == KeyKind::Unknown {
            return Flow::Idle;
        }
        self.prompt = None;
        if !matches!(key, KeyKind::Up | KeyKind::Down) {
            self.recall = None;
        }
        let armed =
            self.exit_armed.take().filter(|at| now.duration_since(*at) < EXIT_CONFIRM_WINDOW);
        if key == KeyKind::CtrlC {
            if armed.is_some() {
                return Flow::Quit;
            }
            self.arm_exit(now);
            return Flow::Redraw;
        }
        if armed.is_some() {
            return Flow::Redraw;
        }
        match key {
            KeyKind::Tab => {
                self.picker.collapse();
                self.focus = self.focus.next();
            }
            KeyKind::BackTab => {
                self.picker.collapse();
                self.focus = self.focus.prev();
            }
            KeyKind::Esc if self.focus == Focus::Input && self.picker.is_open() => {
                self.picker.handle_key(KeyKind::Esc, 0, Selection::None);
            }
            KeyKind::Esc => self.arm_exit(now),
            _ => match self.focus {
                Focus::Input => self.on_input_key(key),
                Focus::Status => self.on_status_key(key),
                Focus::Output => self.on_output_key(key),
            },
        }
        Flow::Redraw
    }

    fn arm_exit(&mut self, now: Instant) {
        self.exit_armed = Some(now);
        self.prompt = Some(Prompt { level: PromptLevel::Warn, text: EXIT_CONFIRM_TEXT.into() });
    }

    fn set_error(&mut self, text: String) {
        self.prompt = Some(Prompt { level: PromptLevel::Error, text });
    }

    fn on_input_key(&mut self, key: KeyKind) {
        if matches!(key, KeyKind::Up | KeyKind::Down)
            && !self.picker.is_open()
            && self.try_recall(key)
        {
            return;
        }
        let highlight = self.picker.highlight();
        let (count, sel, highlighted) = match self.list_items() {
            None => (0, Selection::None, None),
            Some(items) => {
                let count = items.len();
                let pick = |sel| if count > 0 { sel } else { Selection::None };
                match items {
                    ListItems::Library(v) => {
                        (count, pick(Selection::Library), v.get(highlight).map(|e| (*e).clone()))
                    }
                    ListItems::Builtin(_) => (count, pick(Selection::BuiltinWithLevel2), None),
                    ListItems::Recent(v) => (
                        count,
                        pick(Selection::RecentEntry),
                        v.get(highlight).map(|e| (*e).clone()),
                    ),
                }
            }
        };
        match self.picker.handle_key(key, count, sel) {
            PickerAction::Bind => {
                if let Some(entry) = highlighted {
                    self.picker.bind(entry);
                }
            }
            PickerAction::Execute => {
                let bound = self.picker.input.bound.clone();
                self.picker.after_execute();
                if let Some(entry) = bound {
                    self.execute(entry);
                }
            }
            PickerAction::ExecuteHighlighted => {
                self.picker.after_execute();
                if let Some(entry) = highlighted {
                    self.execute(entry);
                }
            }
            PickerAction::Error(text) => self.set_error(text),
            PickerAction::OpenLevel1(_) | PickerAction::OpenLevel2 | PickerAction::None => {}
        }
    }

    /// 空框或召回态下 `↑/↓` 浏览 recent（队首最新）。返回是否消费了按键。
    ///
    /// 语义：`↑` 取更旧一条（最旧处停住）；`↓` 取更新一条，最新再按回到空框；
    /// 空框按 `↓` 不消费（无动作）。召回通过 [`Picker::bind`] 复用「补全 + 绑定」，
    /// 继续打字即解绑并退出召回（召回游标由 `on_key` 顶部统一清零）。
    fn try_recall(&mut self, key: KeyKind) -> bool {
        let len = self.recent.entries().len();
        if len == 0 {
            return false;
        }
        match self.recall {
            None => {
                if key != KeyKind::Up
                    || !self.picker.input.text.is_empty()
                    || self.picker.input.bound.is_some()
                {
                    return false;
                }
                self.bind_recall(0);
                true
            }
            Some(idx) => match key {
                KeyKind::Up => {
                    self.bind_recall((idx + 1).min(len - 1));
                    true
                }
                KeyKind::Down if idx == 0 => {
                    self.recall = None;
                    self.picker.input.text.clear();
                    self.picker.input.bound = None;
                    true
                }
                KeyKind::Down => {
                    self.bind_recall(idx - 1);
                    true
                }
                _ => false,
            },
        }
    }

    fn bind_recall(&mut self, idx: usize) {
        let Some(entry) = self.recent.entries().get(idx).cloned() else {
            return;
        };
        self.recall = Some(idx);
        self.picker.bind(entry);
    }

    fn on_status_key(&mut self, key: KeyKind) {
        let ids = self.jobs.ids();
        let idx = self.selected.and_then(|id| ids.iter().position(|x| *x == id));
        match (key, idx) {
            (KeyKind::Up, Some(i)) if i > 0 => self.select(ids[i - 1]),
            (KeyKind::Down, Some(i)) if i + 1 < ids.len() => self.select(ids[i + 1]),
            (KeyKind::Up | KeyKind::Down, None) => {
                if let Some(first) = ids.first() {
                    self.select(*first);
                }
            }
            (KeyKind::Enter, Some(i)) => {
                if let Some(entry) = self.jobs.entry(ids[i]).cloned() {
                    self.execute(entry);
                }
            }
            (KeyKind::Char('x'), Some(i)) if self.jobs.remove(ids[i]) => {
                let rest = self.jobs.ids();
                let next = rest.get(i).or_else(|| i.checked_sub(1).and_then(|p| rest.get(p)));
                self.selected = next.copied();
                self.scroll = following();
            }
            _ => {}
        }
    }

    fn on_output_key(&mut self, key: KeyKind) {
        let Some(id) = self.selected else {
            return;
        };
        let total = self.jobs.output(id).map(output_len).unwrap_or(0);
        let viewport = self.output_viewport();
        let page = viewport.max(1);
        let scroll = self.scroll.clamp(total, viewport);
        self.scroll = match key {
            KeyKind::Up => scroll.up(1),
            KeyKind::Down => scroll.down(1, total, viewport),
            KeyKind::PageUp => scroll.up(page),
            KeyKind::PageDown => scroll.down(page, total, viewport),
            KeyKind::Home => scroll.top(),
            KeyKind::End => scroll.bottom(total, viewport),
            KeyKind::Char('c') => {
                self.jobs.clear_output(id);
                following()
            }
            _ => return,
        };
    }

    fn select(&mut self, id: JobId) {
        if self.selected != Some(id) {
            self.selected = Some(id);
            self.scroll = following();
        }
    }

    /// 提交执行：先写 recent（提交即写），再 spawn；新 Job 自动成为选中行。
    fn execute(&mut self, entry: CommandEntry) {
        if let Err(e) = self.recent.push(&self.recent_path, entry.clone()) {
            self.set_error(format!("recent 写入失败: {e}"));
        }
        let spec = build_spec(&entry, &self.home);
        match self.jobs.spawn(&entry, &spec, self.tx.clone()) {
            Ok(id) => {
                self.selected = Some(id);
                self.scroll = following();
            }
            Err(e) => self.set_error(format!("spawn failed: {e}")),
        }
    }
}

/// 输出缓冲可显示行数（含未换行的尾部半行）。
pub fn output_len(buf: &crate::jobs::OutputBuf) -> usize {
    buf.lines.len() + usize::from(!buf.pending.is_empty())
}

fn following() -> Scroll {
    Scroll { offset: 0, follow: true }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver};

    fn temp_recent_path(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "super-command-app-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("recent.toml")
    }

    fn entry(name: &str, program: &str, args: &[&str]) -> CommandEntry {
        CommandEntry {
            name: name.into(),
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: Default::default(),
            cwd: None,
            comment: None,
            hosts: None,
        }
    }

    fn new_app(tag: &str) -> (App, Receiver<AppEvent>) {
        let (tx, rx) = mpsc::channel();
        let library = vec![
            entry("clippy-all", "true", &[]),
            entry("broken", "definitely-not-a-command-xyz", &[]),
        ];
        let mut app =
            App::new(library, Recent::default(), temp_recent_path(tag), "/tmp".into(), tx);
        app.area = Rect::new(0, 0, 80, 40);
        (app, rx)
    }

    fn press(app: &mut App, keys: &[KeyKind]) -> Flow {
        let now = Instant::now();
        let mut flow = Flow::Idle;
        for key in keys {
            flow = app.handle_event(AppEvent::Key(*key), now);
        }
        flow
    }

    fn wait_exit(app: &mut App, rx: &Receiver<AppEvent>, id: JobId) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            app.jobs.poll_exits();
            while let Ok(ev) = rx.try_recv() {
                app.handle_event(ev, Instant::now());
            }
            if matches!(app.jobs.state(id), Some(JobState::Exited(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("job did not exit");
    }

    #[test]
    fn tab_cycles_focus_and_collapses_list() {
        let (mut app, _rx) = new_app("tab");
        press(&mut app, &[KeyKind::Char('#')]);
        assert!(app.picker.is_open());
        press(&mut app, &[KeyKind::Tab]);
        assert!(!app.picker.is_open());
        assert_eq!(app.picker.input.text, "#");
        assert_eq!(app.focus, Focus::Status);
        press(&mut app, &[KeyKind::Tab, KeyKind::Tab]);
        assert_eq!(app.focus, Focus::Input);
        press(&mut app, &[KeyKind::BackTab]);
        assert_eq!(app.focus, Focus::Output);
        press(&mut app, &[KeyKind::BackTab, KeyKind::BackTab]);
        assert_eq!(app.focus, Focus::Input);
    }

    #[test]
    fn ctrl_c_twice_quits_other_key_cancels() {
        let (mut app, _rx) = new_app("ctrlc");
        assert_eq!(press(&mut app, &[KeyKind::CtrlC]), Flow::Redraw);
        assert_eq!(app.prompt.as_ref().map(|p| p.level), Some(PromptLevel::Warn));
        assert_eq!(press(&mut app, &[KeyKind::CtrlC]), Flow::Quit);

        let (mut app, _rx) = new_app("ctrlc-cancel");
        press(&mut app, &[KeyKind::CtrlC, KeyKind::Char('#')]);
        assert_eq!(app.prompt, None);
        assert!(!app.picker.is_open(), "取消提示的按键不生效");
        assert_eq!(press(&mut app, &[KeyKind::CtrlC]), Flow::Redraw);
    }

    #[test]
    fn exit_prompt_expires_after_window() {
        let (mut app, _rx) = new_app("expire");
        let start = Instant::now();
        app.handle_event(AppEvent::Key(KeyKind::CtrlC), start);
        assert!(!app.expire_exit_prompt(start + Duration::from_millis(500)));
        assert!(app.expire_exit_prompt(start + EXIT_CONFIRM_WINDOW));
        assert_eq!(app.prompt, None);
        let later = start + EXIT_CONFIRM_WINDOW + Duration::from_millis(10);
        assert_eq!(app.handle_event(AppEvent::Key(KeyKind::CtrlC), later), Flow::Redraw);
    }

    #[test]
    fn esc_collapses_list_then_arms_exit_shared_with_ctrl_c() {
        let (mut app, _rx) = new_app("esc");
        press(&mut app, &[KeyKind::Char('#'), KeyKind::Esc]);
        assert!(!app.picker.is_open());
        assert_eq!(app.prompt, None);
        press(&mut app, &[KeyKind::Esc]);
        assert_eq!(app.prompt.as_ref().map(|p| p.text.as_str()), Some(EXIT_CONFIRM_TEXT));
        assert_eq!(press(&mut app, &[KeyKind::Esc]), Flow::Redraw, "Esc 只取消提示");
        assert_eq!(app.prompt, None);
        press(&mut app, &[KeyKind::Esc]);
        assert_eq!(press(&mut app, &[KeyKind::CtrlC]), Flow::Quit);
    }

    #[test]
    fn unknown_key_is_ignored_without_clearing_prompt() {
        let (mut app, _rx) = new_app("unknown");
        press(&mut app, &[KeyKind::AltEnter]);
        assert!(app.prompt.is_some());
        assert_eq!(press(&mut app, &[KeyKind::Unknown]), Flow::Idle);
        assert!(app.prompt.is_some());
        press(&mut app, &[KeyKind::Char('x')]);
        assert_eq!(app.prompt, None, "任意按键清空提示");
    }

    #[test]
    fn bind_then_alt_enter_spawns_and_selects_job() {
        let (mut app, rx) = new_app("exec");
        press(&mut app, &[KeyKind::Char('#'), KeyKind::Char('c'), KeyKind::Enter]);
        assert_eq!(app.picker.input.text, "#clippy-all");
        assert!(app.picker.input.bound.is_some());
        press(&mut app, &[KeyKind::AltEnter]);
        assert_eq!(app.picker.input.text, "");
        assert_eq!(app.picker.input.bound, None);
        let id = app.selected.expect("new job selected");
        assert_eq!(app.jobs.ids(), vec![id]);
        assert_eq!(app.recent.entries()[0].name, "clippy-all");
        wait_exit(&mut app, &rx, id);
        assert!(matches!(app.jobs.state(id), Some(JobState::Exited(0))));
    }

    #[test]
    fn spawn_failure_shows_error_and_keeps_recent() {
        let (mut app, _rx) = new_app("spawnfail");
        press(&mut app, &[KeyKind::Char('#'), KeyKind::Char('b'), KeyKind::Enter]);
        press(&mut app, &[KeyKind::AltEnter]);
        let prompt = app.prompt.clone().expect("error prompt");
        assert_eq!(prompt.level, PromptLevel::Error);
        assert!(prompt.text.starts_with("spawn failed: "), "{}", prompt.text);
        assert!(app.jobs.ids().is_empty());
        assert_eq!(app.recent.entries()[0].name, "broken");
    }

    #[test]
    fn recent_level2_enter_executes_highlighted_copy() {
        let (mut app, rx) = new_app("recent");
        app.recent = Recent::default();
        let path = temp_recent_path("recent-seed");
        app.recent.push(&path, entry("old", "true", &[])).expect("seed recent");
        press(&mut app, &[KeyKind::Char('/')]);
        assert!(matches!(app.list_items(), Some(ListItems::Builtin(ref v)) if v == &["recent"]));
        press(&mut app, &[KeyKind::Enter]);
        assert!(matches!(app.list_items(), Some(ListItems::Recent(ref v)) if v.len() == 1));
        press(&mut app, &[KeyKind::Enter]);
        assert!(!app.picker.is_open());
        assert_eq!(app.picker.input.text, "");
        let id = app.selected.expect("job");
        assert_eq!(app.jobs.entry(id).map(|e| e.name.as_str()), Some("old"));
        assert_eq!(app.recent.entries().len(), 1, "重跑同一副本不新增条目");
        wait_exit(&mut app, &rx, id);
    }

    #[test]
    fn status_keys_select_rerun_and_clear() {
        let (mut app, rx) = new_app("status");
        press(&mut app, &[KeyKind::Char('#'), KeyKind::Char('c'), KeyKind::Enter]);
        press(&mut app, &[KeyKind::AltEnter]);
        let first = app.selected.expect("first");
        press(&mut app, &[KeyKind::Tab, KeyKind::Enter]);
        let second = app.selected.expect("second");
        assert_ne!(first, second);
        assert_eq!(app.jobs.ids(), vec![first, second]);

        press(&mut app, &[KeyKind::Up]);
        assert_eq!(app.selected, Some(first));
        press(&mut app, &[KeyKind::Up]);
        assert_eq!(app.selected, Some(first));
        press(&mut app, &[KeyKind::Down]);
        assert_eq!(app.selected, Some(second));

        wait_exit(&mut app, &rx, first);
        wait_exit(&mut app, &rx, second);
        press(&mut app, &[KeyKind::Char('x')]);
        assert_eq!(app.selected, Some(first), "末行清除后上移");
        press(&mut app, &[KeyKind::Char('x')]);
        assert_eq!(app.selected, None);
        assert!(app.jobs.ids().is_empty());
        press(&mut app, &[KeyKind::Down, KeyKind::Char('x')]);
    }

    #[test]
    fn status_x_ignored_while_running() {
        let (mut app, _rx) = new_app("status-running");
        app.library = vec![entry("sleepy", "sleep", &["30"])];
        press(&mut app, &[KeyKind::Char('#'), KeyKind::Enter, KeyKind::AltEnter]);
        let id = app.selected.expect("job");
        app.selected = None;
        press(&mut app, &[KeyKind::Tab, KeyKind::Down]);
        assert_eq!(app.selected, Some(id));
        press(&mut app, &[KeyKind::Char('x')]);
        assert_eq!(app.jobs.ids(), vec![id]);
        app.jobs.kill_all();
    }

    #[test]
    fn output_keys_scroll_and_clear() {
        let (mut app, rx) = new_app("output");
        press(&mut app, &[KeyKind::Char('#'), KeyKind::Char('c'), KeyKind::Enter]);
        press(&mut app, &[KeyKind::AltEnter]);
        let id = app.selected.expect("job");
        wait_exit(&mut app, &rx, id);
        let text: String = (0..100).map(|i| format!("l{i}\n")).collect();
        app.handle_event(
            AppEvent::ByteChunk(crate::event::ByteChunk { id, bytes: text.into_bytes() }),
            Instant::now(),
        );
        let viewport = app.output_viewport();
        assert!(viewport > 0 && viewport < 100);

        press(&mut app, &[KeyKind::Tab, KeyKind::Tab]);
        assert_eq!(app.focus, Focus::Output);
        press(&mut app, &[KeyKind::Up]);
        assert!(!app.scroll.follow);
        assert_eq!(app.scroll.offset, 100 - viewport - 1);
        press(&mut app, &[KeyKind::PageUp]);
        assert_eq!(app.scroll.offset, 100 - 2 * viewport - 1);
        press(&mut app, &[KeyKind::PageDown, KeyKind::Down]);
        assert!(app.scroll.follow);
        press(&mut app, &[KeyKind::Home]);
        assert_eq!(app.scroll.offset, 0);
        press(&mut app, &[KeyKind::End]);
        assert!(app.scroll.follow);
        press(&mut app, &[KeyKind::Left]);
        press(&mut app, &[KeyKind::Char('c')]);
        assert_eq!(app.jobs.output(id).map(output_len), Some(0));
    }

    #[test]
    fn output_keys_without_job_are_noop() {
        let (mut app, _rx) = new_app("output-empty");
        press(&mut app, &[KeyKind::BackTab, KeyKind::Up]);
        assert_eq!(app.scroll, following());
    }

    #[test]
    fn duplicate_child_exited_is_idempotent() {
        let (mut app, rx) = new_app("dup-exit");
        press(&mut app, &[KeyKind::Char('#'), KeyKind::Char('c'), KeyKind::Enter]);
        press(&mut app, &[KeyKind::AltEnter]);
        let id = app.selected.expect("job");
        wait_exit(&mut app, &rx, id);
        app.handle_event(AppEvent::ChildExited { id, code: 9 }, Instant::now());
        assert!(matches!(app.jobs.state(id), Some(JobState::Exited(0))));
        assert_eq!(app.handle_event(AppEvent::Tick, Instant::now()), Flow::Redraw);
    }

    fn seed_recent(app: &mut App, names: &[&str]) {
        let path = app.recent_path.clone();
        for name in names {
            app.recent.push(&path, entry(name, "true", &[])).expect("seed recent");
        }
    }

    fn bound_name(app: &App) -> Option<&str> {
        app.picker.input.bound.as_ref().map(|e| e.name.as_str())
    }

    #[test]
    fn recall_up_down_walks_recent_mru() {
        let (mut app, _rx) = new_app("recall");
        seed_recent(&mut app, &["a", "b"]); // MRU: b, a
        press(&mut app, &[KeyKind::Up]);
        assert_eq!(app.picker.input.text, "#b");
        assert_eq!(bound_name(&app), Some("b"));
        press(&mut app, &[KeyKind::Up]);
        assert_eq!(bound_name(&app), Some("a"));
        press(&mut app, &[KeyKind::Up]);
        assert_eq!(bound_name(&app), Some("a"), "最旧一条停住");
        press(&mut app, &[KeyKind::Down]);
        assert_eq!(bound_name(&app), Some("b"));
        press(&mut app, &[KeyKind::Down]);
        assert_eq!(app.picker.input.text, "", "最新再按 ↓ 回空框");
        assert_eq!(app.picker.input.bound, None);
        assert_eq!(app.picker.state, PickerState::Closed);
    }

    #[test]
    fn recall_noop_without_recent_or_down_on_empty() {
        let (mut app, _rx) = new_app("recall-none");
        press(&mut app, &[KeyKind::Up, KeyKind::Down]);
        assert_eq!(app.picker.input.text, "");
        assert_eq!(app.picker.input.bound, None);

        seed_recent(&mut app, &["a"]);
        press(&mut app, &[KeyKind::Down]);
        assert_eq!(app.picker.input.text, "", "空框按 ↓ 无动作");
        assert_eq!(app.picker.input.bound, None);
    }

    #[test]
    fn recall_then_alt_enter_executes_copy_and_recall_restarts_from_front() {
        let (mut app, rx) = new_app("recall-exec");
        seed_recent(&mut app, &["first", "second"]);
        press(&mut app, &[KeyKind::Up]);
        press(&mut app, &[KeyKind::AltEnter]);
        let id = app.selected.expect("job");
        assert_eq!(app.jobs.entry(id).map(|e| e.name.as_str()), Some("second"));
        assert_eq!(app.picker.input.text, "");
        assert_eq!(app.picker.input.bound, None);
        press(&mut app, &[KeyKind::Up]);
        assert_eq!(bound_name(&app), Some("second"), "执行后队首即刚执行的副本");
        wait_exit(&mut app, &rx, id);
    }

    #[test]
    fn typing_after_recall_cancels_cursor() {
        let (mut app, _rx) = new_app("recall-type");
        seed_recent(&mut app, &["a", "b"]);
        press(&mut app, &[KeyKind::Up]);
        press(&mut app, &[KeyKind::Char('x')]);
        assert_eq!(app.picker.input.bound, None, "打字即解绑");
        press(&mut app, &[KeyKind::CtrlU]);
        press(&mut app, &[KeyKind::Up]);
        assert_eq!(bound_name(&app), Some("b"), "游标已取消：重新从队首召回而不是继续取更旧");
    }

    #[test]
    fn recall_does_not_shadow_open_list_arrows() {
        let (mut app, _rx) = new_app("recall-open");
        seed_recent(&mut app, &["a"]);
        press(&mut app, &[KeyKind::Char('#'), KeyKind::Down]);
        assert_eq!(app.picker.highlight(), 1, "列表展开时 ↑/↓ 仍移动高亮");
        assert_eq!(app.picker.input.bound, None);
    }

    #[test]
    fn focus_change_cancels_recall_cursor() {
        let (mut app, _rx) = new_app("recall-focus");
        seed_recent(&mut app, &["a", "b"]);
        press(&mut app, &[KeyKind::Up, KeyKind::Up]);
        press(&mut app, &[KeyKind::Tab, KeyKind::Tab, KeyKind::Tab]);
        assert_eq!(app.focus, Focus::Input);
        assert_eq!(bound_name(&app), Some("a"), "切焦点保留绑定");
        press(&mut app, &[KeyKind::Up]);
        assert_eq!(bound_name(&app), Some("a"), "非空框 ↑ 不召回");
        press(&mut app, &[KeyKind::CtrlU, KeyKind::Up]);
        assert_eq!(bound_name(&app), Some("b"), "清空后从队首重新开始");
    }

    #[test]
    fn output_len_counts_pending_line() {
        let mut buf = crate::jobs::OutputBuf::default();
        buf.append_bytes(b"a\nb");
        assert_eq!(output_len(&buf), 2);
    }
}
