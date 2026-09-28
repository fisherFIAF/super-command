//! 输入区 + 选择列表状态机（纯逻辑，无终端依赖）。
//!
//! 规则见 `specs/keybindings.md`「输入区」：
//! - `#` 弹命令库列表，`/` 弹内置命令列表；首字符非 `#` `/` 一律忽略；
//! - 子串匹配、大小写不敏感；过滤列表为空时仍弹空框（渲染层负责）；
//! - 列表状态只跟随输入框当前内容；绑定后继续打字即解绑，按新内容重新展开；
//! - `Enter` 选中/绑定；`Alt+Enter` 提交执行；未绑定按 `Alt+Enter` → 红色错误；
//! - 一级内置列表：`Enter` 与 `Alt+Enter` 同义，均为「进入二级列表」；
//! - 二级 recent 列表：`Enter` 直接执行；
//! - `Esc`：二级 → 一级（输入框恢复 `/`）→ 初始态；
//! - 列表收起时 `↑`/`↓` 空操作（空框历史召回在 app 层处理，见 `specs/keybindings.md`「历史召回」）。
//!
//! `Tab`、`Ctrl+C` 与初始态 `Esc`（退出提示）是全局键，由主循环处理，不进本状态机。

use crate::config::CommandEntry;
use crate::event::KeyKind;

/// 一级列表来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// 命令库（`commands.toml`）。
    Library,
    /// 内置命令（`/recent`）。
    Builtin,
}

impl Source {
    fn trigger(self) -> char {
        match self {
            Source::Library => '#',
            Source::Builtin => '/',
        }
    }
}

/// 选择列表状态。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PickerState {
    /// 未展开。
    #[default]
    Closed,
    /// 一级列表：`filter` 为触发字符之后的输入。
    Level1 { source: Source, filter: String, highlight: usize },
    /// 二级 recent 列表（特殊状态：不受首字符限制）。
    Level2Recent { filter: String, highlight: usize },
}

/// 状态机对外发出的动作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerAction {
    OpenLevel1(Source),
    OpenLevel2,
    /// 绑定当前高亮条目：调用方按当前列表取出条目后调 [`Picker::bind`]。
    Bind,
    /// 执行已绑定条目（[`InputArea::bound`]）。
    Execute,
    /// 执行 recent 二级列表的高亮副本（列表已收起）。
    ExecuteHighlighted,
    /// 红色错误提示文本。
    Error(String),
    None,
}

/// 调用方对「当前高亮项」的语义告知。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// 无高亮项（列表空）。
    None,
    /// 库条目高亮，可绑定。
    Library,
    /// 内置项高亮且拥有二级列表（如 `/recent`）。
    BuiltinWithLevel2,
    /// recent 二级列表高亮，可直接执行。
    RecentEntry,
}

/// 输入区：显示文本 + 绑定条目。
#[derive(Clone, Debug, Default)]
pub struct InputArea {
    /// 当前输入文本（含触发字符）。
    pub text: String,
    /// 已绑定条目；提交执行的是它，而不是显示文本。
    pub bound: Option<CommandEntry>,
}

/// 输入区 + 列表组合状态机。
#[derive(Debug, Default)]
pub struct Picker {
    pub input: InputArea,
    pub state: PickerState,
}

/// 子串匹配，大小写不敏感。空过滤串匹配一切。
pub fn matches(needle: &str, haystack: &str) -> bool {
    let needle = needle.to_lowercase();
    haystack.to_lowercase().contains(&needle)
}

/// 按 `name` 子串过滤条目列表（保持原序）。
pub fn filter_entries<'a>(entries: &'a [CommandEntry], filter: &str) -> Vec<&'a CommandEntry> {
    entries.iter().filter(|e| matches(filter, &e.name)).collect()
}

impl Picker {
    /// 处理一个按键。`count` 为当前列表可视条目数（0 = 空列表），`sel` 为高亮项语义。
    pub fn handle_key(&mut self, key: KeyKind, count: usize, sel: Selection) -> PickerAction {
        match key {
            KeyKind::Esc => {
                self.on_esc();
                PickerAction::None
            }
            KeyKind::Char('#') if self.accepts_trigger() => self.open_level1(Source::Library),
            KeyKind::Char('/') if self.accepts_trigger() => self.open_level1(Source::Builtin),
            KeyKind::Char(c) => {
                if self.input.text.is_empty() && self.state == PickerState::Closed {
                    return PickerAction::None;
                }
                self.input.text.push(c);
                self.sync_filter();
                PickerAction::None
            }
            KeyKind::Backspace => {
                if self.input.text.pop().is_some() {
                    self.sync_filter();
                }
                PickerAction::None
            }
            KeyKind::CtrlU => {
                self.input.text.clear();
                self.input.bound = None;
                self.collapse();
                PickerAction::None
            }
            KeyKind::Up => self.move_highlight(count, -1),
            KeyKind::Down => self.move_highlight(count, 1),
            KeyKind::Enter => self.on_enter(sel),
            KeyKind::AltEnter => self.on_alt_enter(sel),
            _ => PickerAction::None,
        }
    }

    /// 列表是否展开。
    pub fn is_open(&self) -> bool {
        self.state != PickerState::Closed
    }

    /// 当前高亮下标（列表收起时为 0）。
    pub fn highlight(&self) -> usize {
        match &self.state {
            PickerState::Level1 { highlight, .. } | PickerState::Level2Recent { highlight, .. } => {
                *highlight
            }
            PickerState::Closed => 0,
        }
    }

    /// 收起列表；显示文本与绑定保留（焦点离开输入区时用）。
    pub fn collapse(&mut self) {
        self.state = PickerState::Closed;
    }

    /// 绑定条目并把显示文本补全为 `#<name>`，列表收起。
    pub fn bind(&mut self, entry: CommandEntry) {
        self.input.text = format!("#{}", entry.name);
        self.input.bound = Some(entry);
        self.collapse();
    }

    /// 提交执行后：清空输入、解除绑定、收起列表。
    pub fn after_execute(&mut self) {
        self.input.text.clear();
        self.input.bound = None;
        self.collapse();
    }

    /// 触发符只在首字符位置生效；二级列表不受首字符规则约束。
    fn accepts_trigger(&self) -> bool {
        self.input.text.is_empty() && !matches!(self.state, PickerState::Level2Recent { .. })
    }

    fn on_esc(&mut self) {
        if matches!(self.state, PickerState::Level2Recent { .. }) {
            self.open_level1(Source::Builtin);
        } else {
            self.collapse();
        }
    }

    fn on_enter(&mut self, sel: Selection) -> PickerAction {
        match &self.state {
            PickerState::Level1 { source, .. } => match (*source, sel) {
                (Source::Builtin, Selection::BuiltinWithLevel2) => {
                    self.state = PickerState::Level2Recent { filter: String::new(), highlight: 0 };
                    self.input.text.clear();
                    PickerAction::OpenLevel2
                }
                (Source::Library, Selection::Library) => PickerAction::Bind,
                _ => PickerAction::None,
            },
            PickerState::Level2Recent { .. } => match sel {
                Selection::RecentEntry => {
                    self.collapse();
                    PickerAction::ExecuteHighlighted
                }
                _ => PickerAction::None,
            },
            PickerState::Closed => PickerAction::None,
        }
    }

    fn on_alt_enter(&mut self, sel: Selection) -> PickerAction {
        match &self.state {
            PickerState::Level1 { source: Source::Builtin, .. }
            | PickerState::Level2Recent { .. } => return self.on_enter(sel),
            _ => {}
        }
        if self.input.bound.is_some() {
            return PickerAction::Execute;
        }
        if self.input.text.is_empty() {
            return PickerAction::Error("输入框为空，无可执行命令".into());
        }
        PickerAction::Error("未选中命令：先按 Enter 选中，再按 Alt+Enter 执行".into())
    }

    fn open_level1(&mut self, source: Source) -> PickerAction {
        self.state = PickerState::Level1 { source, filter: String::new(), highlight: 0 };
        self.input.text = source.trigger().to_string();
        self.input.bound = None;
        PickerAction::OpenLevel1(source)
    }

    /// 输入内容变化后按「列表状态只跟随输入框当前内容」重建列表态，并解除绑定。
    fn sync_filter(&mut self) {
        self.input.bound = None;
        let text = self.input.text.clone();
        if text.is_empty() {
            self.collapse();
            return;
        }
        let in_level2 = matches!(self.state, PickerState::Level2Recent { .. });
        let source = match text.chars().next() {
            _ if in_level2 => None,
            Some('#') => Some(Source::Library),
            Some('/') => Some(Source::Builtin),
            _ => None,
        };
        self.state = match source {
            Some(source) => {
                PickerState::Level1 { source, filter: text[1..].to_string(), highlight: 0 }
            }
            None => PickerState::Level2Recent { filter: text, highlight: 0 },
        };
    }

    fn move_highlight(&mut self, count: usize, delta: i32) -> PickerAction {
        if count == 0 {
            return PickerAction::None;
        }
        let highlight = match &mut self.state {
            PickerState::Level1 { highlight, .. } | PickerState::Level2Recent { highlight, .. } => {
                highlight
            }
            PickerState::Closed => return PickerAction::None,
        };
        let len = count as i32;
        let next = ((*highlight as i32) + delta).rem_euclid(len);
        *highlight = next as usize;
        PickerAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> CommandEntry {
        CommandEntry {
            name: name.into(),
            program: "echo".into(),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
            comment: None,
            hosts: None,
        }
    }

    fn level1_filter(picker: &Picker) -> Option<(&Source, &str)> {
        match &picker.state {
            PickerState::Level1 { source, filter, .. } => Some((source, filter)),
            _ => None,
        }
    }

    fn open_recent(p: &mut Picker) {
        p.handle_key(KeyKind::Char('/'), 1, Selection::BuiltinWithLevel2);
        p.handle_key(KeyKind::Enter, 1, Selection::BuiltinWithLevel2);
    }

    #[test]
    fn hash_opens_library_level1() {
        let mut p = Picker::default();
        assert_eq!(
            p.handle_key(KeyKind::Char('#'), 3, Selection::None),
            PickerAction::OpenLevel1(Source::Library)
        );
        assert!(matches!(p.state, PickerState::Level1 { source: Source::Library, .. }));
        assert_eq!(p.input.text, "#");
        assert!(p.is_open());
    }

    #[test]
    fn slash_opens_builtin_level1() {
        let mut p = Picker::default();
        assert_eq!(
            p.handle_key(KeyKind::Char('/'), 1, Selection::None),
            PickerAction::OpenLevel1(Source::Builtin)
        );
        assert_eq!(p.input.text, "/");
    }

    #[test]
    fn non_trigger_first_char_ignored_in_closed() {
        let mut p = Picker::default();
        assert_eq!(p.handle_key(KeyKind::Char('x'), 0, Selection::None), PickerAction::None);
        assert_eq!(p.input.text, "");
        assert_eq!(p.state, PickerState::Closed);
    }

    #[test]
    fn trigger_char_updates_filter_realtime() {
        let mut p = Picker::default();
        p.handle_key(KeyKind::Char('#'), 3, Selection::None);
        p.handle_key(KeyKind::Char('c'), 3, Selection::None);
        p.handle_key(KeyKind::Char('l'), 3, Selection::None);
        let (_, filter) = level1_filter(&p).expect("still level1");
        assert_eq!(filter, "cl");
        assert_eq!(p.input.text, "#cl");
    }

    #[test]
    fn backspace_to_empty_closes() {
        let mut p = Picker::default();
        p.handle_key(KeyKind::Char('#'), 2, Selection::None);
        p.handle_key(KeyKind::Backspace, 2, Selection::None);
        assert_eq!(p.state, PickerState::Closed);
        assert_eq!(p.input.text, "");
        assert_eq!(p.handle_key(KeyKind::Backspace, 0, Selection::None), PickerAction::None);
    }

    #[test]
    fn backspace_to_trigger_reopens_full_list() {
        let mut p = Picker::default();
        p.handle_key(KeyKind::Char('#'), 2, Selection::None);
        p.handle_key(KeyKind::Char('a'), 2, Selection::None);
        p.handle_key(KeyKind::Backspace, 2, Selection::None);
        assert_eq!(level1_filter(&p), Some((&Source::Library, "")));
    }

    #[test]
    fn ctrl_u_clears_and_collapses() {
        let mut p = Picker::default();
        p.bind(entry("grep"));
        assert_eq!(p.handle_key(KeyKind::CtrlU, 2, Selection::None), PickerAction::None);
        assert_eq!(p.input.text, "");
        assert_eq!(p.input.bound, None);
        assert_eq!(p.state, PickerState::Closed);
    }

    #[test]
    fn substring_case_insensitive_filter() {
        assert!(matches("SUB", "Clippy-Substring"));
        assert!(matches("cl", "CLIPPY"));
        assert!(!matches("xyz", "clippy"));
        assert!(matches("", "anything"));
        let all = vec![entry("clippy"), entry("grep"), entry("CLANG")];
        assert_eq!(filter_entries(&all, "cl").len(), 2);
    }

    #[test]
    fn no_match_keeps_list_open() {
        let mut p = Picker::default();
        p.handle_key(KeyKind::Char('#'), 2, Selection::None);
        p.handle_key(KeyKind::Char('x'), 0, Selection::None);
        assert!(p.is_open());
        assert_eq!(p.handle_key(KeyKind::Enter, 0, Selection::None), PickerAction::None);
    }

    #[test]
    fn enter_on_library_requests_bind_then_bind_completes_text() {
        let mut p = Picker::default();
        p.handle_key(KeyKind::Char('#'), 3, Selection::None);
        assert_eq!(p.handle_key(KeyKind::Enter, 3, Selection::Library), PickerAction::Bind);
        p.bind(entry("clippy"));
        assert_eq!(p.state, PickerState::Closed);
        assert_eq!(p.input.text, "#clippy");
        assert_eq!(p.input.bound.as_ref().map(|e| e.name.as_str()), Some("clippy"));
    }

    #[test]
    fn typing_after_bind_unbinds_and_reopens() {
        let mut p = Picker::default();
        p.bind(entry("clippy"));
        p.handle_key(KeyKind::Char('x'), 0, Selection::None);
        assert_eq!(p.input.bound, None);
        assert_eq!(p.input.text, "#clippyx");
        assert_eq!(level1_filter(&p), Some((&Source::Library, "clippyx")));
    }

    #[test]
    fn backspace_after_bind_unbinds_and_reopens() {
        let mut p = Picker::default();
        p.bind(entry("ab"));
        p.handle_key(KeyKind::Backspace, 0, Selection::None);
        assert_eq!(p.input.bound, None);
        assert_eq!(level1_filter(&p), Some((&Source::Library, "a")));
    }

    #[test]
    fn enter_on_builtin_opens_level2_and_clears() {
        let mut p = Picker::default();
        p.handle_key(KeyKind::Char('/'), 1, Selection::BuiltinWithLevel2);
        assert_eq!(
            p.handle_key(KeyKind::Enter, 1, Selection::BuiltinWithLevel2),
            PickerAction::OpenLevel2
        );
        assert!(matches!(p.state, PickerState::Level2Recent { .. }));
        assert_eq!(p.input.text, "");
    }

    #[test]
    fn builtin_level1_alt_enter_same_as_enter() {
        let mut p = Picker::default();
        p.handle_key(KeyKind::Char('/'), 1, Selection::BuiltinWithLevel2);
        assert_eq!(
            p.handle_key(KeyKind::AltEnter, 1, Selection::BuiltinWithLevel2),
            PickerAction::OpenLevel2
        );
    }

    #[test]
    fn alt_enter_without_binding_errors_and_does_not_pick_first() {
        let mut p = Picker::default();
        p.handle_key(KeyKind::Char('#'), 2, Selection::None);
        assert!(matches!(
            p.handle_key(KeyKind::AltEnter, 2, Selection::Library),
            PickerAction::Error(_)
        ));
        assert_eq!(p.input.bound, None);
    }

    #[test]
    fn alt_enter_on_empty_input_errors() {
        let mut p = Picker::default();
        assert_eq!(
            p.handle_key(KeyKind::AltEnter, 0, Selection::None),
            PickerAction::Error("输入框为空，无可执行命令".into())
        );
    }

    #[test]
    fn enter_on_closed_is_noop() {
        let mut p = Picker::default();
        assert_eq!(p.handle_key(KeyKind::Enter, 0, Selection::None), PickerAction::None);
    }

    #[test]
    fn alt_enter_with_binding_executes_and_after_execute_clears() {
        let mut p = Picker::default();
        p.bind(entry("clippy"));
        assert_eq!(p.handle_key(KeyKind::AltEnter, 0, Selection::None), PickerAction::Execute);
        p.after_execute();
        assert_eq!(p.input.text, "");
        assert_eq!(p.input.bound, None);
        assert_eq!(p.state, PickerState::Closed);
    }

    #[test]
    fn recent_level2_enter_executes_directly() {
        let mut p = Picker::default();
        open_recent(&mut p);
        assert_eq!(
            p.handle_key(KeyKind::Enter, 5, Selection::RecentEntry),
            PickerAction::ExecuteHighlighted
        );
        assert_eq!(p.state, PickerState::Closed);
        assert_eq!(p.handle_key(KeyKind::Enter, 0, Selection::None), PickerAction::None);
    }

    #[test]
    fn recent_level2_enter_on_empty_list_noop() {
        let mut p = Picker::default();
        open_recent(&mut p);
        assert_eq!(p.handle_key(KeyKind::Enter, 0, Selection::None), PickerAction::None);
        assert!(p.is_open());
    }

    #[test]
    fn level2_filter_ignores_first_char_rule() {
        let mut p = Picker::default();
        open_recent(&mut p);
        p.handle_key(KeyKind::Char('#'), 5, Selection::RecentEntry);
        p.handle_key(KeyKind::Char('y'), 5, Selection::RecentEntry);
        match &p.state {
            PickerState::Level2Recent { filter, .. } => assert_eq!(filter, "#y"),
            other => panic!("expected level2, got {other:?}"),
        }
    }

    #[test]
    fn level2_cleared_returns_closed_not_level1() {
        let mut p = Picker::default();
        open_recent(&mut p);
        p.handle_key(KeyKind::Char('a'), 3, Selection::RecentEntry);
        p.handle_key(KeyKind::Backspace, 3, Selection::RecentEntry);
        assert_eq!(p.state, PickerState::Closed);

        open_recent(&mut p);
        p.handle_key(KeyKind::Char('a'), 3, Selection::RecentEntry);
        p.handle_key(KeyKind::CtrlU, 3, Selection::RecentEntry);
        assert_eq!(p.state, PickerState::Closed);
    }

    #[test]
    fn esc_ladder_level2_to_level1_to_closed() {
        let mut p = Picker::default();
        open_recent(&mut p);
        p.handle_key(KeyKind::Char('a'), 3, Selection::RecentEntry);
        p.handle_key(KeyKind::Esc, 3, Selection::None);
        assert_eq!(level1_filter(&p), Some((&Source::Builtin, "")));
        assert_eq!(p.input.text, "/");
        p.handle_key(KeyKind::Esc, 1, Selection::None);
        assert_eq!(p.state, PickerState::Closed);
    }

    #[test]
    fn collapse_keeps_binding_and_text() {
        let mut p = Picker::default();
        p.bind(entry("grep"));
        p.handle_key(KeyKind::Char('x'), 1, Selection::None);
        p.bind(entry("grep"));
        p.collapse();
        assert_eq!(p.input.text, "#grep");
        assert!(p.input.bound.is_some());
        assert_eq!(p.handle_key(KeyKind::AltEnter, 0, Selection::None), PickerAction::Execute);
    }

    #[test]
    fn arrows_noop_when_collapsed() {
        let mut p = Picker::default();
        assert_eq!(p.handle_key(KeyKind::Up, 3, Selection::None), PickerAction::None);
        assert_eq!(p.handle_key(KeyKind::Down, 3, Selection::None), PickerAction::None);
        assert_eq!(p.state, PickerState::Closed);
        assert_eq!(p.highlight(), 0);
    }

    #[test]
    fn arrows_wrap_highlight() {
        let mut p = Picker::default();
        p.handle_key(KeyKind::Char('#'), 3, Selection::None);
        p.handle_key(KeyKind::Down, 3, Selection::None);
        p.handle_key(KeyKind::Down, 3, Selection::None);
        assert_eq!(p.highlight(), 2);
        p.handle_key(KeyKind::Down, 3, Selection::None);
        assert_eq!(p.highlight(), 0);
        p.handle_key(KeyKind::Up, 3, Selection::None);
        assert_eq!(p.highlight(), 2);
    }

    #[test]
    fn arrows_noop_on_empty_list() {
        let mut p = Picker::default();
        p.handle_key(KeyKind::Char('#'), 0, Selection::None);
        p.handle_key(KeyKind::Down, 0, Selection::None);
        assert_eq!(p.highlight(), 0);
    }

    #[test]
    fn filter_edit_resets_highlight() {
        let mut p = Picker::default();
        p.handle_key(KeyKind::Char('#'), 3, Selection::None);
        p.handle_key(KeyKind::Down, 3, Selection::None);
        p.handle_key(KeyKind::Char('a'), 3, Selection::None);
        assert_eq!(p.highlight(), 0);
    }

    #[test]
    fn unhandled_keys_are_noop() {
        let mut p = Picker::default();
        assert_eq!(p.handle_key(KeyKind::Home, 0, Selection::None), PickerAction::None);
    }
}
