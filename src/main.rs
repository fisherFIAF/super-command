mod app;
mod config;
mod event;
mod jobs;
mod layout;
mod picker;
mod recent;
mod runner;

use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use ratatui::backend::TermionBackend;
use ratatui::Terminal;
use termion::raw::IntoRawMode;
use termion::screen::IntoAlternateScreen;

use crate::app::{App, Flow};
use crate::config::Config;
use crate::event::AppEvent;
use crate::recent::Recent;

/// 主循环 `recv_timeout` 上限：取 ESC 判定（50ms）与 tick（1s）的较小值。
const LOOP_TIMEOUT: Duration = Duration::from_millis(50);

fn main() {
    let (mut app, rx) = match load() {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("super-command: {e}");
            std::process::exit(1);
        }
    };
    match run(&mut app, &rx) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("super-command: {e}");
            std::process::exit(1);
        }
    }
}

/// 启动加载：命令库解析失败或 `$HOME` 未设置时在进入 TUI 前报错退出。
fn load() -> Result<(App, Receiver<AppEvent>), Box<dyn std::error::Error>> {
    let home = config::home_dir()?;
    let library = Config::load(&config::library_path_from(&home))?;
    let recent_path = recent::recent_path_from(&home);
    let recent = Recent::load(&recent_path)?;
    let (tx, rx) = mpsc::channel();
    event::spawn_input(tx.clone());
    event::spawn_tick(tx.clone());
    let app = App::new(library.entries().to_vec(), recent, recent_path, home, tx);
    Ok((app, rx))
}

/// 终端生命周期：raw mode + 备用屏。退出时先 kill 全部运行中 Job，再显示光标、
/// 通过 drop 退出备用屏并关闭 raw mode；`std::process::exit` 不跑析构，故须在此显式 drop。
fn run(app: &mut App, rx: &Receiver<AppEvent>) -> io::Result<()> {
    let stdout = io::stdout().into_raw_mode()?.into_alternate_screen()?;
    let mut terminal = Terminal::new(TermionBackend::new(stdout))?;
    let result = event_loop(&mut terminal, app, rx);
    app.jobs.kill_all();
    let _ = terminal.show_cursor();
    drop(terminal);
    result
}

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    rx: &Receiver<AppEvent>,
) -> io::Result<()>
where
    io::Error: From<B::Error>,
{
    let mut dirty = true;
    loop {
        app.jobs.poll_exits();
        if dirty {
            let size = terminal.size()?;
            app.area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
            terminal.draw(|frame| layout::draw(frame, app))?;
            dirty = false;
        }
        match rx.recv_timeout(LOOP_TIMEOUT) {
            Ok(event) => match app.handle_event(event, Instant::now()) {
                Flow::Quit => return Ok(()),
                Flow::Redraw => dirty = true,
                Flow::Idle => {}
            },
            Err(RecvTimeoutError::Timeout) => dirty |= app.expire_exit_prompt(Instant::now()),
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}
