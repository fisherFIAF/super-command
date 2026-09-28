//! 最近执行列表持久化：`~/.local/super-command/recent.toml`。
//!
//! 语义（见 `specs/command-manager-tui.md`）：
//! - 最多 30 条，队首为最近执行；
//! - 判重按 [`CommandEntry::same_command`]（name/program/args/env/cwd/hosts 任一不同即新条目，
//!   `comment` 不参与；命中时以新副本替换）；
//! - 写入原子：`.tmp` + `rename`，进程退出不得留下 `.tmp` 残留；
//! - 加载遇解析失败：损坏文件改名为 `.bak`，以空列表启动，不阻断运行。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::CommandEntry;

/// 列表容量上限。
const MAX_ENTRIES: usize = 30;

/// 最近执行列表（内存态，队首最新）。
#[derive(Debug, Default)]
pub struct Recent {
    entries: Vec<CommandEntry>,
}

/// TOML 顶层形态：`[[recent]]` 数组（数组顺序即 MRU 顺序）。
#[derive(Serialize, Deserialize, Default)]
struct RecentFile {
    #[serde(default)]
    recent: Vec<CommandEntry>,
}

/// recent 读写错误。
#[derive(Debug, Error)]
pub enum RecentError {
    #[error("{context}: {source}", context = .context)]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl Recent {
    /// 从 `path` 加载。
    ///
    /// 文件不存在时创建空文件（含父目录）并返回空列表；解析失败把原文件改名为 `.bak`
    /// 后返回空列表。
    pub fn load(path: &Path) -> Result<Recent, RecentError> {
        if !path.exists() {
            let recent = Recent::default();
            recent.save(path)?;
            return Ok(recent);
        }
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) => return Err(io_error("读取 recent 文件失败", path, source)),
        };
        match toml::from_str::<RecentFile>(&text) {
            Ok(file) => Ok(Recent { entries: truncate_to_max(file.recent) }),
            Err(_) => {
                let backup = backup_path(path);
                match fs::rename(path, &backup) {
                    Ok(()) => Ok(Recent::default()),
                    Err(source) => Err(io_error("改名损坏 recent 文件失败", &backup, source)),
                }
            }
        }
    }

    /// 全部条目（队首为最近执行）。
    pub fn entries(&self) -> &[CommandEntry] {
        &self.entries
    }

    /// 推入一条执行记录并立即落盘 `path`：判重移到队首，超出容量截断尾部。
    pub fn push(&mut self, path: &Path, entry: CommandEntry) -> Result<(), RecentError> {
        self.entries.retain(|e| !e.same_command(&entry));
        self.entries.insert(0, entry);
        self.entries.truncate(MAX_ENTRIES);
        self.save(path)
    }

    /// 原子写：临时文件 + `fsync` + `rename`。
    fn save(&self, path: &Path) -> Result<(), RecentError> {
        if let Some(parent) = path.parent() {
            match fs::create_dir_all(parent) {
                Ok(()) => {}
                Err(source) => return Err(io_error("创建 recent 目录失败", parent, source)),
            }
        }
        let tmp = temp_path(path);
        let text = toml::to_string_pretty(&RecentFile { recent: self.entries.clone() }).map_err(
            |source| RecentError::Io {
                context: format!("序列化 recent 内容失败: {source}"),
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
            },
        )?;

        let write_result = (|| -> Result<(), std::io::Error> {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, path)?;
            Ok(())
        })();
        match write_result {
            Ok(()) => Ok(()),
            Err(source) => {
                let _ = fs::remove_file(&tmp);
                Err(io_error("写入 recent 文件失败", path, source))
            }
        }
    }
}

/// 由 home 目录拼出 recent 文件路径（纯函数，便于单测注入）。
pub fn recent_path_from(home: &str) -> PathBuf {
    PathBuf::from(home).join(".local").join("super-command").join("recent.toml")
}

fn temp_path(path: &Path) -> PathBuf {
    path.with_extension("toml.tmp")
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("toml.bak")
}

fn truncate_to_max(mut entries: Vec<CommandEntry>) -> Vec<CommandEntry> {
    entries.truncate(MAX_ENTRIES);
    entries
}

fn io_error(context: &str, path: &Path, source: std::io::Error) -> RecentError {
    RecentError::Io { context: format!("{context}: {}", path.display()), source }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "super-command-recent-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn entry(name: &str, program: &str) -> CommandEntry {
        CommandEntry {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            comment: None,
            hosts: None,
        }
    }

    fn path_in(dir: &Path) -> PathBuf {
        dir.join("recent.toml")
    }

    #[test]
    fn load_missing_returns_empty() {
        let dir = temp_dir("missing");
        let path = dir.join("sub").join("recent.toml");
        let recent = Recent::load(&path).expect("load ok");
        assert!(recent.entries().is_empty());
        assert!(path.exists(), "missing recent file should be created");
    }

    #[test]
    fn file_format_uses_recent_array_tables() {
        let dir = temp_dir("format");
        let path = path_in(&dir);
        let mut recent = Recent::default();
        recent.push(&path, entry("a", "pa")).expect("push ok");
        let text = fs::read_to_string(&path).expect("read");
        assert!(text.contains("[[recent]]"), "{text}");
        fs::write(&path, "[[recent]]\nname = \"b\"\nprogram = \"pb\"\n").expect("write");
        assert_eq!(Recent::load(&path).expect("load").entries(), &[entry("b", "pb")]);
    }

    #[test]
    fn push_persists_and_reloads_round_trip() {
        let dir = temp_dir("roundtrip");
        let path = path_in(&dir);
        let mut recent = Recent::default();
        let mut e = entry("clippy", "cargo");
        e.args = vec!["clippy".into()];
        e.env.insert("RUST_LOG".into(), "debug".into());
        e.cwd = Some("~/code".into());

        recent.push(&path, e.clone()).expect("push ok");
        let reloaded = Recent::load(&path).expect("load ok");
        assert_eq!(reloaded.entries(), &[e]);
    }

    #[test]
    fn push_mru_front_and_dedup_full_equality() {
        let dir = temp_dir("mru");
        let path = path_in(&dir);
        let mut recent = Recent::default();
        let a = entry("a", "pa");
        let b = entry("b", "pb");
        let b_diff_env = {
            let mut e = entry("b", "pb");
            e.env.insert("X".into(), "1".into());
            e
        };

        recent.push(&path, a.clone()).expect("push a");
        recent.push(&path, b.clone()).expect("push b");
        assert_eq!(recent.entries(), &[b.clone(), a.clone()]);

        // 完整相等命中：移到队首，总数不变
        recent.push(&path, a.clone()).expect("push a again");
        assert_eq!(recent.entries(), &[a.clone(), b.clone()]);

        // 仅 env 不同 = 新条目，不判重
        recent.push(&path, b_diff_env.clone()).expect("push b'");
        assert_eq!(recent.entries().len(), 3);
    }

    #[test]
    fn push_with_and_without_hosts_are_separate_records() {
        let dir = temp_dir("hosts");
        let path = path_in(&dir);
        let mut recent = Recent::default();
        let plain = entry("api", "curl");
        let mut test_hosts = plain.clone();
        test_hosts.hosts = Some("~/hosts/test".into());
        let mut prod_hosts = plain.clone();
        prod_hosts.hosts = Some("~/hosts/prod".into());

        recent.push(&path, plain.clone()).expect("push plain");
        recent.push(&path, test_hosts.clone()).expect("push test");
        recent.push(&path, prod_hosts.clone()).expect("push prod");
        recent.push(&path, test_hosts.clone()).expect("push test again");
        assert_eq!(recent.entries(), &[test_hosts.clone(), prod_hosts, plain]);

        let text = fs::read_to_string(&path).expect("read");
        assert!(text.contains("hosts = \"~/hosts/test\""), "{text}");
        assert_eq!(Recent::load(&path).expect("load").entries()[0], test_hosts);
    }

    #[test]
    fn push_dedup_ignores_comment_and_keeps_new_copy() {
        let dir = temp_dir("comment");
        let path = path_in(&dir);
        let mut recent = Recent::default();
        let a = entry("a", "pa");
        let mut a_commented = a.clone();
        a_commented.comment = Some("新说明".into());

        recent.push(&path, a.clone()).expect("push a");
        recent.push(&path, entry("b", "pb")).expect("push b");
        recent.push(&path, a_commented.clone()).expect("push a'");
        assert_eq!(recent.entries().len(), 2, "只改 comment 视为同一命令");
        assert_eq!(recent.entries()[0], a_commented, "队首为新副本");

        let text = fs::read_to_string(&path).expect("read");
        assert!(text.contains("comment = \"新说明\""), "{text}");
        assert_eq!(Recent::load(&path).expect("load").entries()[0], a_commented);
    }

    #[test]
    fn push_caps_at_max_entries() {
        let dir = temp_dir("cap");
        let path = path_in(&dir);
        let mut recent = Recent::default();
        for i in 0..(MAX_ENTRIES + 5) {
            recent.push(&path, entry(&format!("n{i}"), "p")).expect("push ok");
        }
        assert_eq!(recent.entries().len(), MAX_ENTRIES);
        assert_eq!(recent.entries()[0].name, format!("n{}", MAX_ENTRIES + 4));
        assert_eq!(recent.entries()[MAX_ENTRIES - 1].name, "n5");

        let reloaded = Recent::load(&path).expect("load ok");
        assert_eq!(reloaded.entries().len(), MAX_ENTRIES);
    }

    #[test]
    fn load_corrupt_file_moves_to_bak_and_returns_empty() {
        let dir = temp_dir("corrupt");
        let path = path_in(&dir);
        fs::write(&path, "this is = not valid toml [[[").expect("write corrupt");

        let recent = Recent::load(&path).expect("load ok");
        assert!(recent.entries().is_empty());
        assert!(!path.exists(), "corrupt original must be moved away");
        assert!(backup_path(&path).exists(), ".bak must hold corrupt copy");
    }

    #[test]
    fn save_leaves_no_tmp_residue() {
        let dir = temp_dir("tmp");
        let path = path_in(&dir);
        let mut recent = Recent::default();
        recent.push(&path, entry("x", "y")).expect("push ok");
        recent.push(&path, entry("z", "w")).expect("push ok");

        assert!(!temp_path(&path).exists(), ".toml.tmp residue found");
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["recent.toml".to_string()]);
    }

    #[test]
    fn load_truncates_overlong_existing_file() {
        let dir = temp_dir("overlong");
        let path = path_in(&dir);
        let many: Vec<CommandEntry> =
            (0..(MAX_ENTRIES + 10)).map(|i| entry(&format!("n{i}"), "p")).collect();
        let text = toml::to_string_pretty(&RecentFile { recent: many }).expect("serialize");
        fs::write(&path, text).expect("write");

        let recent = Recent::load(&path).expect("load ok");
        assert_eq!(recent.entries().len(), MAX_ENTRIES);
    }

    #[test]
    fn recent_path_from_joins_local_dir() {
        let path = crate::recent::recent_path_from("/home/fisher");
        assert_eq!(path, PathBuf::from("/home/fisher/.local/super-command/recent.toml"));
    }
}
