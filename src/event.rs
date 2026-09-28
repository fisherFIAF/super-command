//! 终端输入字节流 → 逻辑按键解析（纯函数），以及主循环事件类型。
//!
//! 规则（见 `specs/keybindings.md`）：
//! - ESC 前缀合并：`ESC` 紧跟 `X`（`ESC [`、`ESC O`）视为键序列；
//! - `ESC` 单独、或 `ESC` 后非 CSI/SS3 前缀：产出 `Esc`，后续字节重新解析；
//!   这保证 Alt+字符 = `ESC` + 字符 的两段序列里，带参 CSI 不会被吞成 `Alt(Esc)+[+…`；
//! - `ESC` + 普通字符 `c`：合并为 `Alt(c)`；`ESC + Enter(0x0d/0x0a)`：合并为 `AltEnter`。
//!
//! 线程（见 `specs/command-manager-tui.md`「线程职责与并发边界」）：stdin 读线程只搬字节，
//! 组装线程用 [`KeyAssembler`] 做 50ms ESC 等待后投 `AppEvent::Key`；tick 线程每 1s 投 `Tick`。
//! 终端尺寸变化由 ratatui 在每次 `draw` 时比对，不单独发事件。

use std::io::Read;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

/// 单独 `Esc` 与 Alt 前缀的区分窗口。
const ESC_TIMEOUT: Duration = Duration::from_millis(50);

/// tick 间隔（输出刷新与时长刷新）。
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// 事件通道里的一段输出字节。
#[derive(Debug)]
pub struct ByteChunk {
    pub id: crate::jobs::JobId,
    pub bytes: Vec<u8>,
}

/// 主循环统一事件。
#[derive(Debug)]
pub enum AppEvent {
    Key(KeyKind),
    Tick,
    ChildExited { id: crate::jobs::JobId, code: i32 },
    ByteChunk(ByteChunk),
}

/// 逻辑按键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    Enter,
    AltEnter,
    Tab,
    BackTab,
    Backspace,
    Esc,
    CtrlC,
    CtrlU,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    /// 可打印字符（UTF-8 解码后的单个字符）。
    Char(char),
    /// Alt + 可打印字符（ESC 前缀合并产生）。
    AltChar(char),
    /// 无法识别的字节序列，忽略。
    Unknown,
}

/// CSI 尾字节 → 按键。
fn csi_final(b: u8) -> Option<KeyKind> {
    Some(match b {
        b'A' => KeyKind::Up,
        b'B' => KeyKind::Down,
        b'C' => KeyKind::Right,
        b'D' => KeyKind::Left,
        b'Z' => KeyKind::BackTab,
        b'H' => KeyKind::Home,
        b'F' => KeyKind::End,
        b'~' => KeyKind::Unknown, // 无参 `~` 序列不细分
        _ => return None,
    })
}

/// 解析一段输入字节流，返回按序按键。
///
/// 不做跨调用挂起：每次调用对应一次「读超时窗口内」的完整 burst，
/// 半截转义序列视为 `Unknown` 收尾（由调用方 50ms 超时保证不会长期处于半截）。
pub fn parse_bytes(bytes: &[u8]) -> Vec<KeyKind> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x1b => {
                i = parse_esc(bytes, i, &mut keys);
            }
            0x0d | 0x0a => {
                keys.push(KeyKind::Enter);
                i += 1;
            }
            0x09 => {
                keys.push(KeyKind::Tab);
                i += 1;
            }
            0x7f => {
                keys.push(KeyKind::Backspace);
                i += 1;
            }
            0x03 => {
                keys.push(KeyKind::CtrlC);
                i += 1;
            }
            0x15 => {
                keys.push(KeyKind::CtrlU);
                i += 1;
            }
            _ => {
                let s = String::from_utf8_lossy(&bytes[i..]);
                match s.chars().next() {
                    Some(c) if !c.is_control() => {
                        let consumed = c.len_utf8();
                        keys.push(KeyKind::Char(c));
                        i += consumed;
                    }
                    _ => {
                        keys.push(KeyKind::Unknown);
                        i += 1;
                    }
                }
            }
        }
    }
    keys
}

/// 从 `bytes[start] == 0x1b` 开始解析一个转义序列，返回下一个未消费下标。
fn parse_esc(bytes: &[u8], start: usize, keys: &mut Vec<KeyKind>) -> usize {
    let mut i = start + 1;
    if i >= bytes.len() {
        keys.push(KeyKind::Esc);
        return i;
    }
    match bytes[i] {
        b'[' | b'O' => {
            let prefix = bytes[i];
            i += 1;
            let body_start = i;
            let body = &bytes[body_start..];
            match parse_csi_body(prefix, body) {
                Some((count, key)) => {
                    keys.push(key);
                    i + count
                }
                None => {
                    // 半截或不可识别 CSI：整段丢弃，按 Esc 处理
                    keys.push(KeyKind::Esc);
                    bytes.len()
                }
            }
        }
        0x0d | 0x0a => {
            keys.push(KeyKind::AltEnter);
            i + 1
        }
        _ => {
            let rest = String::from_utf8_lossy(&bytes[i..]);
            match rest.chars().next() {
                Some(c) if !c.is_control() => {
                    let consumed = c.len_utf8();
                    keys.push(KeyKind::AltChar(c));
                    i + consumed
                }
                _ => {
                    keys.push(KeyKind::Esc);
                    i
                }
            }
        }
    }
}

/// 解析 CSI/SS3 体（`[` 与 `O` 之后的部分）。返回 (消费字节数, 按键)。
///
/// 简单序列：SS3 直接 `O` + 尾字节；CSI 支持 `ESC [ Z`、`ESC [ 1~`、`ESC [ 3~` 等功能键数字参数。
fn parse_csi_body(prefix: u8, body: &[u8]) -> Option<(usize, KeyKind)> {
    if prefix == b'O' {
        let key = csi_final(*body.first()?)?;
        return Some((1, key));
    }
    let mut end = 0;
    while end < body.len() && body[end].is_ascii_digit() {
        end += 1;
    }
    let param: u32 =
        if end > 0 { std::str::from_utf8(&body[..end]).ok()?.parse().ok()? } else { 0 };
    let final_byte = *body.get(end)?;
    if end == 0 {
        let key = csi_final(final_byte)?;
        return Some((1, key));
    }
    let key = match (param, final_byte) {
        (1, b'H') | (1, b'~') => KeyKind::Home,
        (1, b'F') => KeyKind::End,
        (4, b'H') | (4, b'~') => KeyKind::End,
        (5, b'~') => KeyKind::PageUp,
        (6, b'~') => KeyKind::PageDown,
        (3, b'~') => KeyKind::Delete,
        (1, b'D' | b'C' | b'A' | b'B') => {
            return Some((end + 1, csi_final(final_byte)?));
        }
        _ => return None,
    };
    Some((end + 1, key))
}

/// 缓冲是否以未完成的转义序列结尾（`ESC`、`ESC O`、`ESC [` + 数字参数），需再等后续字节。
fn needs_more(bytes: &[u8]) -> bool {
    let Some(pos) = bytes.iter().rposition(|&b| b == 0x1b) else {
        return false;
    };
    match &bytes[pos + 1..] {
        [] | [b'O'] => true,
        [b'[', rest @ ..] => rest.iter().all(|b| b.is_ascii_digit() || *b == b';'),
        _ => false,
    }
}

/// 跨读批次拼接转义序列：未完成时挂起，完整或超时后整体交给 [`parse_bytes`]。
#[derive(Debug, Default)]
struct KeyAssembler {
    pending: Vec<u8>,
}

impl KeyAssembler {
    /// 喂入一批字节；序列未完成时返回空并挂起。
    fn feed(&mut self, bytes: &[u8]) -> Vec<KeyKind> {
        self.pending.extend_from_slice(bytes);
        if needs_more(&self.pending) {
            return Vec::new();
        }
        self.flush()
    }

    /// 是否有挂起字节在等 [`ESC_TIMEOUT`]。
    fn waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// 等待超时：挂起字节按现状解析（单独 `ESC` → `Esc`）。
    fn flush(&mut self) -> Vec<KeyKind> {
        let keys = parse_bytes(&self.pending);
        self.pending.clear();
        keys
    }
}

/// 启动 stdin 读线程 + 按键组装线程。stdin 阻塞读不可中断，线程随进程退出。
pub fn spawn_input(tx: Sender<AppEvent>) {
    let (raw_tx, raw_rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if raw_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    thread::spawn(move || {
        let mut asm = KeyAssembler::default();
        loop {
            let keys = if asm.waiting() {
                match raw_rx.recv_timeout(ESC_TIMEOUT) {
                    Ok(bytes) => asm.feed(&bytes),
                    Err(RecvTimeoutError::Timeout) => asm.flush(),
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match raw_rx.recv() {
                    Ok(bytes) => asm.feed(&bytes),
                    Err(_) => break,
                }
            };
            for key in keys {
                if tx.send(AppEvent::Key(key)).is_err() {
                    return;
                }
            }
        }
    });
}

/// 启动 1s tick 线程；主循环退出（接收端关闭）后线程自行结束。
pub fn spawn_tick(tx: Sender<AppEvent>) {
    thread::spawn(move || loop {
        thread::sleep(TICK_INTERVAL);
        if tx.send(AppEvent::Tick).is_err() {
            break;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_more_detects_incomplete_sequences() {
        assert!(needs_more(&[0x1b]));
        assert!(needs_more(b"\x1bO"));
        assert!(needs_more(b"\x1b["));
        assert!(needs_more(b"\x1b[1"));
        assert!(!needs_more(b"\x1b[A"));
        assert!(!needs_more(&[0x1b, 0x0d]));
        assert!(!needs_more(b"abc"));
        assert!(!needs_more(b""));
    }

    #[test]
    fn assembler_merges_split_alt_enter() {
        let mut asm = KeyAssembler::default();
        assert!(asm.feed(&[0x1b]).is_empty());
        assert!(asm.waiting());
        assert_eq!(asm.feed(&[0x0d]), vec![KeyKind::AltEnter]);
        assert!(!asm.waiting());
    }

    #[test]
    fn assembler_timeout_yields_lone_esc() {
        let mut asm = KeyAssembler::default();
        assert!(asm.feed(&[0x1b]).is_empty());
        assert_eq!(asm.flush(), vec![KeyKind::Esc]);
        assert!(!asm.waiting());
    }

    #[test]
    fn assembler_joins_split_csi() {
        let mut asm = KeyAssembler::default();
        assert!(asm.feed(b"\x1b[").is_empty());
        assert_eq!(asm.feed(b"B"), vec![KeyKind::Down]);
    }

    #[test]
    fn assembler_passes_complete_batch_through() {
        let mut asm = KeyAssembler::default();
        assert_eq!(asm.feed(b"#a\r"), vec![KeyKind::Char('#'), KeyKind::Char('a'), KeyKind::Enter]);
    }

    #[test]
    fn parse_alt_enter_from_esc_cr() {
        assert_eq!(parse_bytes(&[0x1b, 0x0d]), vec![KeyKind::AltEnter]);
        assert_eq!(parse_bytes(&[0x1b, 0x0a]), vec![KeyKind::AltEnter]);
    }

    #[test]
    fn parse_lone_esc_is_esc() {
        assert_eq!(parse_bytes(&[0x1b]), vec![KeyKind::Esc]);
    }

    #[test]
    fn parse_csi_arrows() {
        assert_eq!(parse_bytes(b"\x1b[A"), vec![KeyKind::Up]);
        assert_eq!(parse_bytes(b"\x1b[B"), vec![KeyKind::Down]);
        assert_eq!(parse_bytes(b"\x1b[C"), vec![KeyKind::Right]);
        assert_eq!(parse_bytes(b"\x1b[D"), vec![KeyKind::Left]);
        assert_eq!(parse_bytes(b"\x1b[Z"), vec![KeyKind::BackTab]);
    }

    #[test]
    fn parse_ss3_home_end() {
        assert_eq!(parse_bytes(b"\x1bOH"), vec![KeyKind::Home]);
        assert_eq!(parse_bytes(b"\x1bOF"), vec![KeyKind::End]);
    }

    #[test]
    fn parse_csi_function_keys() {
        assert_eq!(parse_bytes(b"\x1b[3~"), vec![KeyKind::Delete]);
        assert_eq!(parse_bytes(b"\x1b[5~"), vec![KeyKind::PageUp]);
        assert_eq!(parse_bytes(b"\x1b[6~"), vec![KeyKind::PageDown]);
        assert_eq!(parse_bytes(b"\x1b[1~"), vec![KeyKind::Home]);
        assert_eq!(parse_bytes(b"\x1b[4~"), vec![KeyKind::End]);
    }

    #[test]
    fn parse_ctrl_and_plain() {
        assert_eq!(parse_bytes(&[0x03]), vec![KeyKind::CtrlC]);
        assert_eq!(parse_bytes(&[0x15]), vec![KeyKind::CtrlU]);
        assert_eq!(parse_bytes(&[0x0d]), vec![KeyKind::Enter]);
        assert_eq!(parse_bytes(&[0x09]), vec![KeyKind::Tab]);
        assert_eq!(parse_bytes(&[0x7f]), vec![KeyKind::Backspace]);
        assert_eq!(parse_bytes(b"x"), vec![KeyKind::Char('x')]);
        assert_eq!(parse_bytes("#".as_bytes()), vec![KeyKind::Char('#')]);
        assert_eq!(parse_bytes("中".as_bytes()), vec![KeyKind::Char('中')]);
    }

    #[test]
    fn parse_alt_char_via_esc_prefix() {
        assert_eq!(parse_bytes(&[0x1b, b'r']), vec![KeyKind::AltChar('r')]);
    }

    #[test]
    fn parse_burst_mix() {
        assert_eq!(parse_bytes(b"\x1b[A\x1b[A\n"), vec![KeyKind::Up, KeyKind::Up, KeyKind::Enter]);
    }

    #[test]
    fn parse_unknown_bytes_ignored() {
        assert_eq!(parse_bytes(&[0x00]), vec![KeyKind::Unknown]);
    }
}
