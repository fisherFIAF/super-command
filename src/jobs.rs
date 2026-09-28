//! 运行中 Job 管理：子进程 spawn、双读者线程、输出环形缓冲、退出强杀。
//!
//! 输出语义（见 `specs/tui-page-layout.md`）：每 Job 尾部保留 2000 行；行按 `\n` 切分，
//! 无换行的尾部半行挂在 `pending` 里等补齐；字节流按 UTF-8 解码，非法字节用 U+FFFD。

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::Instant;

use crate::event::AppEvent;
use crate::runner::CommandSpec;

/// 输出保留上限（行）。
const MAX_LINES: usize = 2000;

/// Job 句柄 id。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub u32);

/// Job 状态。
#[derive(Debug)]
pub enum JobState {
    Running { started: Instant },
    Exited(i32),
}

/// 单 Job 输出环形缓冲（尾部 MAX_LINES 行）。
#[derive(Debug, Default)]
pub struct OutputBuf {
    pub lines: VecDeque<String>,
    pub pending: String,
    pub truncated_from_head: u64,
}

impl OutputBuf {
    /// 追加一段原始字节：先补齐挂起半行，再按 `\n` 切分，末段无换行重新挂起。
    pub fn append_bytes(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        let mut buf = std::mem::take(&mut self.pending);
        for ch in text.chars() {
            if ch == '\n' {
                if buf.ends_with('\r') {
                    buf.pop();
                }
                self.push_line(std::mem::take(&mut buf));
            } else {
                buf.push(ch);
            }
        }
        self.pending = buf;
    }

    fn push_line(&mut self, line: String) {
        if self.lines.len() >= MAX_LINES {
            self.lines.pop_front();
            self.truncated_from_head += 1;
        }
        self.lines.push_back(line);
    }
}

/// 一个已 spawn 的子进程 + 其输出缓冲。
struct Job {
    child: Child,
    pid: u32,
    entry: crate::config::CommandEntry,
    state: JobState,
    output: OutputBuf,
    tx: Sender<AppEvent>,
}

/// 全部运行中 Job 的登记表。
pub struct Jobs {
    next_id: u32,
    map: HashMap<JobId, Job>,
}

impl Jobs {
    pub fn new() -> Self {
        Jobs { next_id: 0, map: HashMap::new() }
    }

    /// spawn 子进程并起 stdout/stderr 两个读者线程，块经 `tx` 发回主循环。
    ///
    /// 读者线程持 `Sender` 克隆（不持 Job 可变引用），主循环是唯一写者。
    /// `entry` 随 Job 登记，供状态区展示名称/重跑。
    pub fn spawn(
        &mut self,
        entry: &crate::config::CommandEntry,
        spec: &CommandSpec,
        tx: Sender<AppEvent>,
    ) -> std::io::Result<JobId> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .envs(&spec.env)
            .stdin(if spec.stdin_null { Stdio::null() } else { Stdio::inherit() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd.spawn()?;
        let pid = child.id();
        let out_pipe = child.stdout.take();
        let err_pipe = child.stderr.take();
        let id = JobId(self.next_id);
        self.next_id += 1;
        self.map.insert(
            id,
            Job {
                child,
                pid,
                entry: entry.clone(),
                state: JobState::Running { started: Instant::now() },
                output: OutputBuf::default(),
                tx: tx.clone(),
            },
        );

        if let Some(out) = out_pipe {
            spawn_reader(id, out, tx.clone());
        }
        if let Some(err) = err_pipe {
            spawn_reader(id, err, tx);
        }
        Ok(id)
    }

    /// 主循环每轮调用：对 Running 的 Job `try_wait`，收割到的经 `tx` 发
    /// [`AppEvent::ChildExited`]（不改状态——状态更新留给主循环处理事件时做，保证单写者）。
    pub fn poll_exits(&mut self) {
        for (id, job) in self.map.iter_mut() {
            if !matches!(job.state, JobState::Running { .. }) {
                continue;
            }
            if let Ok(Some(status)) = job.child.try_wait() {
                let code = status.code().unwrap_or(-1);
                let _ = job.tx.send(AppEvent::ChildExited { id: *id, code });
            }
        }
    }

    /// 主循环收到 [`AppEvent::ByteChunk`] 时写入缓冲。
    pub fn record_bytes(&mut self, id: JobId, bytes: &[u8]) {
        if let Some(job) = self.map.get_mut(&id) {
            job.output.append_bytes(bytes);
        }
    }

    /// 主循环收到 [`AppEvent::ChildExited`] 时更新状态。
    pub fn record_exit(&mut self, id: JobId, code: i32) {
        if let Some(job) = self.map.get_mut(&id) {
            job.state = JobState::Exited(code);
        }
    }

    pub fn state(&self, id: JobId) -> Option<&JobState> {
        self.map.get(&id).map(|job| &job.state)
    }

    /// Job 对应的命令条目（状态区显示名 / Enter 重跑用）。
    pub fn entry(&self, id: JobId) -> Option<&crate::config::CommandEntry> {
        self.map.get(&id).map(|job| &job.entry)
    }

    pub fn output(&self, id: JobId) -> Option<&OutputBuf> {
        self.map.get(&id).map(|job| &job.output)
    }

    /// 全部 id，升序（spawn 顺序）。
    pub fn ids(&self) -> Vec<JobId> {
        let mut ids: Vec<JobId> = self.map.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// 移除条目：仅当 Job 已退出（Exited）。返回是否真的移除。
    pub fn remove(&mut self, id: JobId) -> bool {
        match self.map.get(&id) {
            Some(job) if matches!(job.state, JobState::Exited(_)) => self.map.remove(&id).is_some(),
            _ => false,
        }
    }

    /// 子进程 pid。已收割的 Exited Job 仍返回登记值（`Child::id` 在 wait 后不可靠，
    /// 故 spawn 时立即缓存）。
    pub fn pid(&self, id: JobId) -> Option<u32> {
        self.map.get(&id).map(|job| job.pid)
    }

    /// 清空指定 Job 的输出缓冲（状态区/输出区 `c` 键用）。
    pub fn clear_output(&mut self, id: JobId) {
        if let Some(job) = self.map.get_mut(&id) {
            job.output = OutputBuf::default();
        }
    }

    /// 退出前强杀全部仍 Running 的子进程（Exited 的跳过）。
    pub fn kill_all(&mut self) {
        for job in self.map.values_mut() {
            if matches!(job.state, JobState::Running { .. }) {
                let _ = job.child.kill();
                let _ = job.child.wait();
            }
        }
    }
}

impl Default for Jobs {
    fn default() -> Self {
        Jobs::new()
    }
}

fn spawn_reader<R: Read + Send + 'static>(id: JobId, reader: R, tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            match reader.fill_buf() {
                Ok(&[]) => break,
                Ok(buf) => {
                    let bytes = buf.to_vec();
                    let len = buf.len();
                    reader.consume(len);
                    let chunk = crate::event::ByteChunk { id, bytes };
                    let _ = tx.send(AppEvent::ByteChunk(chunk));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        // EOF 只代表管道关闭；退出状态由 poll_exits 收割
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn lines_of(buf: &OutputBuf) -> Vec<&str> {
        buf.lines.iter().map(String::as_str).collect()
    }

    #[test]
    fn append_bytes_carries_partial_lines() {
        let mut buf = OutputBuf::default();
        buf.append_bytes(b"he");
        assert!(buf.lines.is_empty());
        assert_eq!(buf.pending, "he");
        buf.append_bytes(b"llo\nwor");
        assert_eq!(lines_of(&buf), vec!["hello"]);
        assert_eq!(buf.pending, "wor");
        buf.append_bytes(b"ld\n");
        assert_eq!(lines_of(&buf), vec!["hello", "world"]);
        assert_eq!(buf.pending, "");
    }

    #[test]
    fn append_bytes_strips_trailing_cr() {
        let mut buf = OutputBuf::default();
        buf.append_bytes(b"line\r\nnext\r\n");
        assert_eq!(lines_of(&buf), vec!["line", "next"]);
    }

    #[test]
    fn append_bytes_decodes_lossy() {
        let mut buf = OutputBuf::default();
        buf.append_bytes(&[0xff, 0xfe, b'a', b'\n']);
        assert_eq!(lines_of(&buf), vec!["\u{FFFD}\u{FFFD}a"]);
    }

    #[test]
    fn append_bytes_tail_keeps_max_lines() {
        let mut buf = OutputBuf::default();
        let mut input = String::new();
        for i in 0..2500 {
            input.push_str(&format!("l{i}\n"));
        }
        buf.append_bytes(input.as_bytes());
        assert_eq!(buf.lines.len(), MAX_LINES);
        assert_eq!(buf.truncated_from_head, 500);
        assert_eq!(buf.lines.front().map(String::as_str), Some("l500"));
        assert_eq!(buf.lines.back().map(String::as_str), Some("l2499"));
    }

    #[test]
    fn append_bytes_empty_input_no_line() {
        let mut buf = OutputBuf::default();
        buf.append_bytes(b"");
        assert!(buf.lines.is_empty());
        assert_eq!(buf.pending, "");
    }

    fn sleep_spec() -> CommandSpec {
        CommandSpec {
            program: "sleep".into(),
            args: vec!["60".into()],
            env: Default::default(),
            cwd: None,
            stdin_null: true,
        }
    }

    fn test_entry(name: &str) -> crate::config::CommandEntry {
        crate::config::CommandEntry {
            name: name.into(),
            program: "sleep".into(),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
            comment: None,
            hosts: None,
        }
    }

    #[test]
    fn kill_all_only_touches_running() {
        let (tx, rx) = mpsc::channel::<AppEvent>();
        let mut jobs = Jobs::new();
        let id = jobs.spawn(&test_entry("sleep60"), &sleep_spec(), tx).expect("spawn sleep");
        assert!(matches!(jobs.state(id), Some(JobState::Running { .. })));
        assert_eq!(jobs.ids(), vec![id]);
        assert_eq!(jobs.entry(id).map(|e| e.name.as_str()), Some("sleep60"));

        // 记录输出字节流经 record_bytes
        jobs.record_bytes(id, b"hello\n");
        assert_eq!(jobs.output(id).map(|o| o.lines.len()), Some(1));

        jobs.kill_all();
        // kill 不改状态（状态只由事件路径更新）
        assert!(matches!(jobs.state(id), Some(JobState::Running { .. })));

        // kill 后子进程已被 reap，poll_exits 仍能为未收割的 Running 发退出事件
        jobs.poll_exits();
        match rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(AppEvent::ChildExited { id: got, .. }) => assert_eq!(got, id),
            other => panic!("expected ChildExited, got {other:?}"),
        }

        // Exited 后二次 kill_all / poll_exits 应跳过，不 panic
        jobs.record_exit(id, -1);
        jobs.kill_all();
        jobs.poll_exits();
        assert_eq!(jobs.output(id).map(|o| o.lines.len()), Some(1));
    }

    #[test]
    fn remove_only_when_exited() {
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut jobs = Jobs::new();
        let id = jobs.spawn(&test_entry("sleep60"), &sleep_spec(), tx).expect("spawn sleep");

        // Running 时 remove 不动
        assert!(!jobs.remove(id));
        assert_eq!(jobs.ids(), vec![id]);

        jobs.record_exit(id, 0);
        assert!(jobs.remove(id));
        assert!(jobs.ids().is_empty());
        // 不存在的 id remove 返回 false，不 panic
        assert!(!jobs.remove(id));
        jobs.kill_all();
    }

    #[test]
    fn pid_available_for_running_and_exited() {
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut jobs = Jobs::new();
        let spec = CommandSpec {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 3".into()],
            env: Default::default(),
            cwd: None,
            stdin_null: true,
        };
        let id = jobs.spawn(&test_entry("exit3"), &spec, tx).expect("spawn sh");
        let running_pid = jobs.pid(id).expect("pid while running");
        assert!(running_pid > 0);

        std::thread::sleep(std::time::Duration::from_millis(200));
        jobs.poll_exits();
        jobs.record_exit(id, 3);
        // 收割后 pid 仍可读（缓存值）
        assert_eq!(jobs.pid(id), Some(running_pid));
        jobs.kill_all();
    }

    #[test]
    fn clear_output_resets_lines_pending_and_truncation() {
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut jobs = Jobs::new();
        let id = jobs.spawn(&test_entry("sleep60"), &sleep_spec(), tx).expect("spawn sleep");
        jobs.record_bytes(id, b"a\nb\nc");
        jobs.clear_output(id);
        let out = jobs.output(id).expect("output");
        assert!(out.lines.is_empty());
        assert_eq!(out.pending, "");
        assert_eq!(out.truncated_from_head, 0);
        // 不存在的 id 不 panic
        jobs.clear_output(JobId(999));
        jobs.kill_all();
    }

    #[test]
    fn spawn_reader_sends_byte_chunks_and_poll_exits_reaps() {
        let (tx, rx) = mpsc::channel::<AppEvent>();
        let mut jobs = Jobs::new();
        let spec = CommandSpec {
            program: "sh".into(),
            args: vec!["-c".into(), "printf 'a\\nb\\n'".into()],
            env: Default::default(),
            cwd: None,
            stdin_null: true,
        };
        let id = jobs.spawn(&test_entry("printf"), &spec, tx).expect("spawn sh");

        // 读到 EOF 后 drain 通道：chunk 数量不定（1~2 块都可能）
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut count = 0;
        while let Ok(AppEvent::ByteChunk(chunk)) = rx.try_recv() {
            jobs.record_bytes(chunk.id, &chunk.bytes);
            count += 1;
        }
        assert!(count >= 1, "expected at least one ByteChunk, got {count}");

        jobs.poll_exits();
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(AppEvent::ChildExited { id: got, code }) => {
                assert_eq!(got, id);
                assert_eq!(code, 0);
            }
            other => panic!("expected ChildExited, got {other:?}"),
        }
        let lines: Vec<String> = jobs.output(id).expect("output").lines.iter().cloned().collect();
        assert_eq!(lines, vec!["a".to_string(), "b".to_string()]);
    }
}
