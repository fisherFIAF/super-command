//! 三区布局与渲染：布局纯函数（三区 + 两条分隔线切分、popup 高度、状态区高度上限、滚动位移）+ `draw`。
//!
//! 参数来源见 `specs/tui-page-layout.md`「渲染参数」表。所有尺寸判定走纯函数，
//! 渲染函数只把 [`App`] 状态画到对应矩形上。三区不画边框，只有 popup 保留完整边框。

use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::{output_len, App, Focus, ListItems, PromptLevel};
use crate::config::CommandEntry;
use crate::jobs::{JobId, JobState};

/// 状态区高度上限占比（20%）。
const STATUS_MAX_PERCENT: u32 = 20;
/// 选择列表最多显示条数。
const POPUP_MAX_ITEMS: usize = 10;
/// 显示名与命令预览之间的固定分隔（不对齐列宽，二者均完整展示）。
const POPUP_NAME_PREVIEW_SEP: &str = "  ";
/// 选择列表高亮行：整行背景色；前景统一改白，覆盖命令预览的暗灰以保证对比度。
const POPUP_HIGHLIGHT: Style = Style::new().bg(Color::Blue).fg(Color::White);

const EMPTY_STATUS_TEXT: &str = "no jobs — 按 # 选择命令";
const INPUT_PREFIX: &str = "> ";

/// 状态区高度上限：`max(1, h * 20 / 100)`，终端再矮也不塌成 0。
fn status_area_height(term_h: u16) -> u16 {
    ((u32::from(term_h) * STATUS_MAX_PERCENT / 100) as u16).max(1)
}

/// popup 高度：前 `min(n, 10)` 条的总行数 + 2（2 为上下边框）；匹配 0 条仍有空框（高度 2）。
///
/// 只取决于过滤结果，不随高亮移动变化。
fn popup_height(item_heights: &[u16]) -> u16 {
    item_heights.iter().take(POPUP_MAX_ITEMS).sum::<u16>() + 2
}

/// 自上而下：状态区 / 状态区底部空行 / 分隔线① / 输出区 / 分隔线② / 交互区。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Areas {
    pub status: Rect,
    pub status_pad: Rect,
    pub sep_top: Rect,
    pub output: Rect,
    pub sep_bottom: Rect,
    pub input: Rect,
}

/// 切分：状态（行数 = Job 数，至少 1 行，上限 20%）+ 底部空行 1 行（不计入上限）/
/// 分隔线（各 1 行）/ 输出（剩余）/ 交互（常驻 2 行）。
///
/// 高度不够时按 交互 → 状态 → 分隔线① → 分隔线② → 状态区空行 的顺序分配，输出区最先被挤压到 0 行。
pub fn layout_split(area: Rect, status_rows: usize) -> Areas {
    let rows = u16::try_from(status_rows).unwrap_or(u16::MAX).max(1);
    let input_h = 2u16.min(area.height);
    let mut rest = area.height - input_h;
    let status_h = rows.min(status_area_height(area.height)).min(rest);
    rest -= status_h;
    let sep_top_h = 1u16.min(rest);
    rest -= sep_top_h;
    let sep_bottom_h = 1u16.min(rest);
    rest -= sep_bottom_h;
    let pad_h = 1u16.min(rest);
    rest -= pad_h;

    let band = |y: u16, height: u16| Rect { x: area.x, y, width: area.width, height };
    let status = band(area.y, status_h);
    let status_pad = band(status.bottom(), pad_h);
    let sep_top = band(status_pad.bottom(), sep_top_h);
    let output = band(sep_top.bottom(), rest);
    let sep_bottom = band(output.bottom(), sep_bottom_h);
    let input = band(sep_bottom.bottom(), input_h);
    Areas { status, status_pad, sep_top, output, sep_bottom, input }
}

/// popup 矩形：底边贴输出区下沿（即分隔线②上方），向上延伸；宽度取输出区全宽。
fn popup_rect(output: Rect, height: u16) -> Rect {
    let h = height.min(output.height);
    Rect { x: output.x, y: output.y + output.height - h, width: output.width, height: h }
}

/// 滚动状态：offset = 视口首行在 lines 中的下标；follow = 自动贴底。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scroll {
    pub offset: usize,
    pub follow: bool,
}

impl Scroll {
    /// 把 offset 收敛到 `[0, max(0, total - viewport)]`；follow 时直接贴底。
    pub fn clamp(mut self, total: usize, viewport: usize) -> Self {
        if self.follow {
            self.offset = total.saturating_sub(viewport);
            return self;
        }
        self.offset = self.offset.min(total.saturating_sub(viewport));
        self
    }

    pub fn up(mut self, n: usize) -> Self {
        self.follow = false;
        self.offset = self.offset.saturating_sub(n);
        self
    }

    pub fn down(mut self, n: usize, total: usize, viewport: usize) -> Self {
        self.offset = (self.offset + n).min(total.saturating_sub(viewport));
        self.follow = self.offset >= total.saturating_sub(viewport);
        self
    }

    /// End：跳到底部并恢复自动跟随。
    pub fn bottom(mut self, total: usize, viewport: usize) -> Self {
        self.follow = true;
        self.offset = total.saturating_sub(viewport);
        self
    }

    /// Home：跳到缓冲最早保留行。
    pub fn top(mut self) -> Self {
        self.follow = false;
        self.offset = 0;
        self
    }
}

/// 状态区视口首行：让选中行始终可见（选中行超出视口时贴底）。
fn status_offset(selected: Option<usize>, viewport: usize) -> usize {
    match selected {
        Some(i) if viewport > 0 && i >= viewport => i + 1 - viewport,
        _ => 0,
    }
}

/// 运行时长 `mm:ss`，超过 1 小时为 `h:mm:ss`。
fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// 库 / recent 条目的命令预览：`program args  ENV: k=v  CWD: dir  HOSTS: file`。
fn entry_preview(entry: &CommandEntry) -> String {
    let mut out = entry.program.clone();
    for arg in &entry.args {
        out.push(' ');
        out.push_str(arg);
    }
    if !entry.env.is_empty() {
        let env: Vec<String> = entry.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        out.push_str("  ENV: ");
        out.push_str(&env.join(" "));
    }
    if let Some(cwd) = &entry.cwd {
        out.push_str("  CWD: ");
        out.push_str(cwd);
    }
    if let Some(hosts) = &entry.hosts {
        out.push_str("  HOSTS: ");
        out.push_str(hosts);
    }
    out
}

/// 绘制整帧：状态区 / 分隔线① / 输出区 / 分隔线② / 交互区，输入区焦点且列表展开时叠加 popup。
pub fn draw(frame: &mut Frame, app: &App) {
    let ids = app.jobs.ids();
    let areas = layout_split(frame.area(), ids.len());
    draw_status(frame, app, &ids, areas.status);
    let title = app.selected.and_then(|id| app.jobs.entry(id)).map(|e| e.name.as_str());
    let top_color = if app.focus == Focus::Output { Color::Cyan } else { Color::DarkGray };
    draw_separator(frame, areas.sep_top, title, top_color);
    draw_output(frame, app, areas.output);
    draw_separator(frame, areas.sep_bottom, None, Color::DarkGray);
    draw_input(frame, app, areas.input);
    if app.focus == Focus::Input {
        if let Some(items) = app.list_items() {
            draw_popup(frame, app, &items, areas.output);
        }
    }
}

/// 分隔线文本：整宽 `─`；带标题时为 `── <title> ───…`。
fn separator_text(title: Option<&str>, width: u16) -> String {
    let head = title.map(|t| format!("── {t} ")).unwrap_or_default();
    let fill = usize::from(width).saturating_sub(Line::raw(head.as_str()).width());
    head + &"─".repeat(fill)
}

fn draw_separator(frame: &mut Frame, area: Rect, title: Option<&str>, color: Color) {
    if area.height == 0 {
        return;
    }
    let line = Line::styled(separator_text(title, area.width), Style::new().fg(color));
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_status(frame: &mut Frame, app: &App, ids: &[JobId], area: Rect) {
    if ids.is_empty() {
        let hint = Paragraph::new(EMPTY_STATUS_TEXT).style(Style::new().fg(Color::DarkGray));
        frame.render_widget(hint, area);
        return;
    }
    let selected = app.selected.and_then(|id| ids.iter().position(|x| *x == id));
    let viewport = area.height as usize;
    let now = Instant::now();
    let lines: Vec<Line> = ids
        .iter()
        .enumerate()
        .skip(status_offset(selected, viewport))
        .take(viewport)
        .map(|(i, id)| {
            let mut style = status_style(app.jobs.state(*id));
            if Some(i) == selected {
                style = style.add_modifier(if app.focus == Focus::Status {
                    Modifier::REVERSED
                } else {
                    Modifier::BOLD
                });
            }
            Line::styled(status_row(app, *id, now), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn status_style(state: Option<&JobState>) -> Style {
    match state {
        Some(JobState::Exited(code)) if *code != 0 => Style::new().fg(Color::Red),
        _ => Style::new(),
    }
}

fn status_row(app: &App, id: JobId, now: Instant) -> String {
    let name = app.jobs.entry(id).map(|e| e.name.as_str()).unwrap_or("");
    let (marker, state) = match app.jobs.state(id) {
        Some(JobState::Running { started }) => {
            ('▶', format!("[running {}]", format_elapsed(now.duration_since(*started))))
        }
        Some(JobState::Exited(0)) => ('✓', "[exit 0]".to_string()),
        Some(JobState::Exited(code)) => ('✗', format!("[exit {code}]")),
        None => (' ', String::new()),
    };
    let pid = app.jobs.pid(id).map(|p| format!("pid {p}")).unwrap_or_default();
    format!("{marker} {name:<24} {state:<18} {pid}")
}

fn draw_output(frame: &mut Frame, app: &App, area: Rect) {
    let Some(buf) = app.selected.and_then(|id| app.jobs.output(id)) else {
        return;
    };
    let viewport = area.height as usize;
    let offset = app.scroll.clamp(output_len(buf), viewport).offset;
    let pending = (!buf.pending.is_empty()).then_some(&buf.pending);
    let lines: Vec<Line> = buf
        .lines
        .iter()
        .chain(pending)
        .skip(offset)
        .take(viewport)
        .map(|l| Line::raw(l.as_str()))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let text = &app.picker.input.text;
    let mut spans = vec![Span::raw(INPUT_PREFIX), Span::raw(text.as_str())];
    if app.focus != Focus::Input {
        spans.iter_mut().for_each(|s| s.style = Style::new().fg(Color::DarkGray));
    }
    let prompt = match &app.prompt {
        Some(p) => {
            let color = match p.level {
                PromptLevel::Error => Color::Red,
                PromptLevel::Warn => Color::Yellow,
            };
            Line::styled(p.text.as_str(), Style::new().fg(color))
        }
        None => Line::raw(""),
    };
    frame.render_widget(Paragraph::new(vec![Line::from(spans), prompt]), area);
    if app.focus == Focus::Input && area.height > 0 {
        let width = Line::raw(format!("{INPUT_PREFIX}{text}")).width() as u16;
        frame.set_cursor_position((area.x + width.min(area.width.saturating_sub(1)), area.y));
    }
}

fn draw_popup(frame: &mut Frame, app: &App, items: &ListItems, output: Rect) {
    let (title, rows): (&str, Vec<ListItem>) = match items {
        ListItems::Library(v) => (" 命令库 ", v.iter().map(|e| entry_item(e)).collect()),
        ListItems::Recent(v) => (" recent ", v.iter().map(|e| entry_item(e)).collect()),
        ListItems::Builtin(v) => {
            (" 内置命令 ", v.iter().map(|name| ListItem::new(format!("/{name}"))).collect())
        }
    };
    let heights: Vec<u16> = rows.iter().map(|r| r.height() as u16).collect();
    let rect = popup_rect(output, popup_height(&heights));
    let list = List::new(rows)
        .block(Block::bordered().title(title))
        .highlight_symbol("▸ ")
        .highlight_style(POPUP_HIGHLIGHT);
    let mut state = ListState::default();
    if items.len() > 0 {
        state.select(Some(app.picker.highlight()));
    }
    frame.render_widget(Clear, rect);
    frame.render_stateful_widget(list, rect, &mut state);
}

/// 库 / recent 条目：第 1 行 显示名 + 命令预览；有 comment 时第 2 行单行展示 comment。
fn entry_item(entry: &CommandEntry) -> ListItem<'static> {
    let mut lines = vec![Line::from(vec![
        Span::styled(entry.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(POPUP_NAME_PREVIEW_SEP),
        Span::styled(entry_preview(entry), Style::new().fg(Color::DarkGray)),
    ])];
    if let Some(comment) = &entry.comment {
        let text = comment.split_whitespace().collect::<Vec<_>>().join(" ");
        let style = Style::new().fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
        lines.push(Line::styled(text, style));
    }
    ListItem::new(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AppEvent, ByteChunk, KeyKind};
    use crate::recent::Recent;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;
    use std::sync::mpsc;

    fn entry(name: &str, program: &str) -> CommandEntry {
        CommandEntry {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
            comment: None,
            hosts: None,
        }
    }

    fn test_app() -> App {
        let (tx, _rx) = mpsc::channel();
        let mut rich = entry("clippy-all", "cargo");
        rich.args = vec!["clippy".into(), "--all-targets".into()];
        rich.env.insert("RUST_LOG".into(), "debug".into());
        rich.cwd = Some("~/code".into());
        let path = std::env::temp_dir()
            .join(format!("super-command-layout-{}/recent.toml", std::process::id()));
        App::new(vec![rich, entry("ls", "ls")], Recent::default(), path, "/tmp".into(), tx)
    }

    fn render(app: &App, w: u16, h: u16) -> (Buffer, Option<(u16, u16)>) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal.draw(|f| draw(f, app)).expect("draw");
        let backend = terminal.backend();
        let pos = backend.cursor_position();
        let cursor = backend.cursor_visible().then_some((pos.x, pos.y));
        (backend.buffer().clone(), cursor)
    }

    /// 一行可见文本；宽字符后的占位格跳过。
    fn row(buf: &Buffer, y: u16) -> String {
        let mut out = String::new();
        let mut x = 0;
        while x < buf.area.width {
            let sym = buf[(x, y)].symbol();
            out.push_str(sym);
            x += (Span::raw(sym).width() as u16).max(1);
        }
        out
    }

    fn screen(buf: &Buffer) -> String {
        (0..buf.area.height).map(|y| row(buf, y) + "\n").collect()
    }

    fn key(app: &mut App, k: KeyKind) {
        app.handle_event(AppEvent::Key(k), Instant::now());
    }

    #[test]
    fn empty_state_renders_hint_input_and_cursor() {
        let app = test_app();
        let (buf, cursor) = render(&app, 60, 20);
        assert!(row(&buf, 0).starts_with(EMPTY_STATUS_TEXT));
        assert_eq!(row(&buf, 1).trim(), "", "状态区底部空行");
        assert_eq!(row(&buf, 2), "─".repeat(60), "无 Job 时分隔线①为纯横线");
        assert_eq!(row(&buf, 17), "─".repeat(60));
        assert!(!screen(&buf).contains('┌'), "三区不画边框");
        assert!(row(&buf, 18).starts_with("> "));
        assert_eq!(row(&buf, 19).trim(), "");
        assert_eq!(cursor, Some((2, 18)));
    }

    #[test]
    fn popup_lists_library_with_preview_and_empty_box_on_no_match() {
        let mut app = test_app();
        key(&mut app, KeyKind::Char('#'));
        let (buf, cursor) = render(&app, 100, 20);
        let text = screen(&buf);
        assert!(text.contains("命令库"), "{text}");
        assert!(text.contains("▸ clippy-all"), "{text}");
        assert!(text.contains("cargo clippy --all-targets  ENV: RUST_LOG=debug  CWD: ~/code"));
        assert!(row(&buf, 18).starts_with("> #"));
        assert_eq!(cursor, Some((3, 18)));
        // popup 底边紧贴分隔线②上方，不遮挡分隔线②
        assert!(row(&buf, 16).starts_with('└'));
        assert_eq!(row(&buf, 17), "─".repeat(100));

        key(&mut app, KeyKind::Char('z'));
        key(&mut app, KeyKind::Char('z'));
        let (buf, _) = render(&app, 80, 20);
        assert!(row(&buf, 15).starts_with('┌'), "空列表框只有边框");
        assert!(row(&buf, 16).starts_with('└'));
    }

    #[test]
    fn popup_highlight_row_has_background_and_follows_arrows() {
        let mut app = test_app();
        key(&mut app, KeyKind::Char('#'));
        let lit = |buf: &Buffer, y: u16| (1..79).all(|x| buf[(x, y)].bg == Color::Blue);
        let (buf, _) = render(&app, 80, 20);
        // 2 条目：边框 13，条目 14 / 15，边框 16
        assert!(row(&buf, 14).contains("▸ clippy-all"));
        assert!(lit(&buf, 14) && !lit(&buf, 15), "高亮背景铺满整行");
        assert_eq!(buf[(30, 14)].fg, Color::White, "预览文字在高亮行改白");

        key(&mut app, KeyKind::Down);
        let (buf, _) = render(&app, 80, 20);
        assert!(row(&buf, 15).contains("▸ ls"));
        assert!(!lit(&buf, 14) && lit(&buf, 15));
    }

    #[test]
    fn comment_renders_on_second_line_inside_highlight() {
        let mut app = test_app();
        app.library[0].comment = Some("全量\nclippy   检查".into());
        key(&mut app, KeyKind::Char('#'));
        let (buf, _) = render(&app, 80, 20);
        // 3 行内容：边框 12，clippy-all 13，comment 14，ls 15，边框 16
        assert!(row(&buf, 12).starts_with('┌'), "popup 高度按行数计");
        assert!(row(&buf, 13).contains("▸ clippy-all"));
        assert!(row(&buf, 14).starts_with("│  全量 clippy 检查"), "缩进对齐显示名，换行折成空格");
        assert!(buf[(3, 14)].modifier.contains(Modifier::ITALIC));
        assert_eq!(buf[(3, 14)].bg, Color::Blue, "高亮覆盖 comment 行");
        assert_eq!(buf[(3, 15)].bg, Color::Reset);
        assert!(row(&buf, 15).contains("ls"));

        key(&mut app, KeyKind::Down);
        let (buf, _) = render(&app, 80, 20);
        assert_eq!(buf[(3, 14)].bg, Color::Reset);
        assert_eq!(buf[(3, 15)].bg, Color::Blue);
    }

    #[test]
    fn builtin_and_recent_popups() {
        let mut app = test_app();
        key(&mut app, KeyKind::Char('/'));
        let text = screen(&render(&app, 60, 20).0);
        assert!(text.contains("内置命令") && text.contains("/recent"), "{text}");
        key(&mut app, KeyKind::Enter);
        let text = screen(&render(&app, 60, 20).0);
        assert!(text.contains(" recent "), "{text}");
    }

    #[test]
    fn jobs_render_status_rows_and_output_follow() {
        let mut app = test_app();
        app.library = vec![entry("sleepy", "sleep")];
        app.library[0].args = vec!["30".into()];
        key(&mut app, KeyKind::Char('#'));
        key(&mut app, KeyKind::Enter);
        key(&mut app, KeyKind::AltEnter);
        let id = app.selected.expect("job");
        let text: String = (0..50).map(|i| format!("line{i}\n")).collect();
        app.handle_event(
            AppEvent::ByteChunk(ByteChunk { id, bytes: format!("{text}tail").into_bytes() }),
            Instant::now(),
        );
        key(&mut app, KeyKind::Tab);
        let (buf, cursor) = render(&app, 60, 20);
        let status = row(&buf, 0);
        assert!(status.contains("▶ sleepy") && status.contains("[running 00:0"), "{status}");
        assert!(status.contains("pid "));
        assert_eq!(row(&buf, 1).trim(), "", "状态区底部空行");
        assert!(row(&buf, 2).starts_with("── sleepy ───"), "分隔线①嵌 Job 名");
        assert_eq!(buf[(0, 2)].fg, Color::DarkGray);
        assert_eq!(row(&buf, 3), "line37".to_string() + &" ".repeat(54), "输出区无边框");
        assert!(row(&buf, 16).starts_with("tail"), "跟随尾部，含未换行半行");
        assert_eq!(cursor, None, "非输入区焦点隐藏光标");

        key(&mut app, KeyKind::Tab);
        let (buf, _) = render(&app, 60, 20);
        assert_eq!(buf[(0, 2)].fg, Color::Cyan, "输出区焦点时分隔线①青色");
        assert_eq!(buf[(0, 17)].fg, Color::DarkGray, "分隔线②不随焦点变色");
        key(&mut app, KeyKind::Tab);

        app.jobs.kill_all();
        app.handle_event(AppEvent::ChildExited { id, code: 137 }, Instant::now());
        key(&mut app, KeyKind::Tab);
        let (buf, _) = render(&app, 60, 20);
        assert!(row(&buf, 0).contains("✗ sleepy") && row(&buf, 0).contains("[exit 137]"));
        assert_eq!(buf[(0, 0)].fg, Color::Red);
    }

    #[test]
    fn prompt_line_colors() {
        let mut app = test_app();
        key(&mut app, KeyKind::AltEnter);
        let (buf, _) = render(&app, 60, 10);
        assert!(row(&buf, 9).starts_with("输入框为空"));
        assert_eq!(buf[(0, 9)].fg, Color::Red);
        key(&mut app, KeyKind::CtrlC);
        let (buf, _) = render(&app, 60, 10);
        assert_eq!(buf[(0, 9)].fg, Color::Yellow);
    }

    #[test]
    fn status_offset_keeps_selection_visible() {
        assert_eq!(status_offset(None, 3), 0);
        assert_eq!(status_offset(Some(2), 3), 0);
        assert_eq!(status_offset(Some(5), 3), 3);
        assert_eq!(status_offset(Some(5), 0), 0);
    }

    #[test]
    fn format_elapsed_minutes_and_hours() {
        assert_eq!(format_elapsed(Duration::from_secs(12)), "00:12");
        assert_eq!(format_elapsed(Duration::from_secs(75)), "01:15");
        assert_eq!(format_elapsed(Duration::from_secs(3723)), "1:02:03");
    }

    #[test]
    fn popup_height_caps_at_ten_items() {
        assert_eq!(popup_height(&[]), 2);
        assert_eq!(popup_height(&[1; 3]), 5);
        assert_eq!(popup_height(&[1; 10]), 12);
        assert_eq!(popup_height(&[1; 50]), 12);
    }

    #[test]
    fn popup_height_counts_comment_lines() {
        assert_eq!(popup_height(&[2, 1, 2]), 7);
        assert_eq!(popup_height(&[2; 12]), 22, "10 条上限按条目计，不按行计");
    }

    #[test]
    fn status_height_20_percent_with_floor_one() {
        assert_eq!(status_area_height(40), 8);
        assert_eq!(status_area_height(5), 1);
        assert_eq!(status_area_height(0), 1);
        assert_eq!(status_area_height(100), 20);
    }

    #[test]
    fn layout_split_bands_tile_area() {
        let area = Rect::new(0, 0, 80, 40);
        let a = layout_split(area, 50);
        assert_eq!(a.status.height, 8, "Job 多时封顶 20%");
        assert_eq!(a.status_pad.height, 1, "底部空行不计入 20% 上限");
        assert_eq!(a.sep_top.height, 1);
        assert_eq!(a.sep_bottom.height, 1);
        assert_eq!(a.input.height, 2);
        assert_eq!(a.output.height, 40 - 8 - 1 - 1 - 1 - 2);
        let bands = [a.status, a.status_pad, a.sep_top, a.output, a.sep_bottom, a.input];
        for pair in bands.windows(2) {
            assert_eq!(pair[0].bottom(), pair[1].y);
        }
        assert_eq!(a.input.bottom(), area.bottom());
    }

    #[test]
    fn layout_split_status_grows_with_jobs_min_one() {
        let area = Rect::new(0, 0, 80, 40);
        assert_eq!(layout_split(area, 0).status.height, 1);
        assert_eq!(layout_split(area, 3).status.height, 3);
        assert_eq!(layout_split(area, 3).output.height, 40 - 3 - 1 - 1 - 1 - 2);
    }

    #[test]
    fn layout_split_survives_tiny_term() {
        let a = layout_split(Rect::new(0, 0, 20, 1), 0);
        assert_eq!(a.input.height, 1);
        assert_eq!(a.status.height + a.sep_top.height + a.output.height, 0);
        assert_eq!(a.sep_bottom.height + a.status_pad.height, 0);

        let heights = |a: Areas| {
            [a.status, a.status_pad, a.sep_top, a.output, a.sep_bottom, a.input].map(|r| r.height)
        };
        let a = layout_split(Rect::new(0, 0, 20, 5), 3);
        assert_eq!(heights(a), [1, 0, 1, 0, 1, 2], "输出区最先被挤压，空行其次");
        let a = layout_split(Rect::new(0, 0, 20, 6), 3);
        assert_eq!(heights(a), [1, 1, 1, 0, 1, 2]);

        let a = layout_split(Rect::new(0, 0, 20, 4), 0);
        assert_eq!((a.sep_top.height, a.sep_bottom.height), (1, 0));
    }

    #[test]
    fn separator_text_fills_width_with_optional_title() {
        assert_eq!(separator_text(None, 5), "─────");
        assert_eq!(separator_text(Some("ls"), 10), "── ls ────");
        assert_eq!(separator_text(Some("构建"), 10), "── 构建 ──", "宽字符按显示宽度计");
        assert_eq!(separator_text(Some("long-name"), 4), "── long-name ", "超宽交给渲染截断");
    }

    #[test]
    fn popup_rect_sits_on_output_bottom_edge() {
        let output = Rect::new(0, 8, 80, 30);
        let rect = popup_rect(output, 5);
        assert_eq!(rect.height, 5);
        assert_eq!(rect.y + rect.height, output.y + output.height);
        assert_eq!(rect.width, 80);
    }

    #[test]
    fn popup_rect_clipped_by_short_output() {
        let rect = popup_rect(Rect::new(0, 1, 80, 1), 7);
        assert_eq!(rect.height, 1);
        assert_eq!(rect.y, 1);
    }

    #[test]
    fn long_entry_name_shows_full_name_and_preview_with_separator() {
        let mut app = test_app();
        let long = "very-long-command-name-here";
        app.library = vec![entry(long, "echo")];
        key(&mut app, KeyKind::Char('#'));
        let text = screen(&render(&app, 120, 20).0);
        assert!(text.contains(long), "{text}");
        assert!(text.contains(&format!("{long}  echo")), "完整名称与预览之间固定分隔");
    }

    #[test]
    fn entry_preview_minimal() {
        assert_eq!(entry_preview(&entry("ls", "ls")), "ls");
    }

    #[test]
    fn entry_preview_shows_raw_hosts_after_cwd() {
        let mut e = entry("api", "curl");
        e.cwd = Some("~/code".into());
        e.hosts = Some("~/hosts/test".into());
        assert_eq!(entry_preview(&e), "curl  CWD: ~/code  HOSTS: ~/hosts/test");
    }

    #[test]
    fn scroll_follow_pins_to_bottom() {
        let s = Scroll { offset: 0, follow: true };
        let s = s.clamp(100, 10);
        assert_eq!(s.offset, 90);
    }

    #[test]
    fn scroll_up_leaves_follow() {
        let s = Scroll { offset: 90, follow: true };
        let s = s.up(5);
        assert!(!s.follow);
        assert_eq!(s.offset, 85);
    }

    #[test]
    fn scroll_down_to_bottom_reacquires_follow() {
        let s = Scroll { offset: 80, follow: false };
        let s = s.down(5, 100, 10);
        assert_eq!(s.offset, 85);
        assert!(!s.follow);
        let s = s.down(100, 100, 10);
        assert_eq!(s.offset, 90);
        assert!(s.follow);
    }

    #[test]
    fn scroll_clamp_shrinks_when_viewport_grows() {
        let s = Scroll { offset: 90, follow: false };
        let s = s.clamp(50, 10);
        assert_eq!(s.offset, 40);
    }

    #[test]
    fn scroll_top_and_bottom() {
        let s = Scroll { offset: 50, follow: false }.top();
        assert_eq!(s.offset, 0);
        assert!(!s.follow);
        let s = s.bottom(100, 10);
        assert_eq!(s.offset, 90);
        assert!(s.follow);
    }

    #[test]
    fn scroll_empty_buffer_offset_zero() {
        let s = Scroll { offset: 7, follow: false };
        let s = s.clamp(0, 10);
        assert_eq!(s.offset, 0);
    }
}
