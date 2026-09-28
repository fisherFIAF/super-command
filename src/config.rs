//! 命令库只读加载：`~/.config/super-command/commands.toml`。
//!
//! 程序内不提供增删改（见 `specs/command-manager-tui.md`），条目由用户手工编辑文件后重启生效。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 一条注册命令：列表显示名 + 完整可再执行的命令行快照。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CommandEntry {
    pub name: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// 替换 `/etc/hosts` 的文件（原始写法，未展开）；执行时经 bwrap 生效。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<String>,
}

impl CommandEntry {
    /// 命令全等：比较 name/program/args/env/cwd/hosts，不含 `comment`（recent 判重口径）。
    pub fn same_command(&self, other: &CommandEntry) -> bool {
        self.name == other.name
            && self.program == other.program
            && self.args == other.args
            && self.env == other.env
            && self.cwd == other.cwd
            && self.hosts == other.hosts
    }
}

/// 库文件中的原始条目：`cmd` 与 `program`/`args` 二选一，校验后转换为 [`CommandEntry`]。
#[derive(Deserialize)]
struct RawEntry {
    name: String,
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    program: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    comment: Option<String>,
    #[serde(default)]
    hosts: Option<String>,
}

impl RawEntry {
    fn into_entry(self) -> Result<CommandEntry, String> {
        let (program, args) = match (self.cmd, self.program, self.args) {
            (Some(cmd), None, None) => split_cmd(&cmd)?,
            (Some(_), _, _) => return Err("cmd 与 program/args 不能同时出现".into()),
            (None, Some(program), args) => (program, args.unwrap_or_default()),
            (None, None, Some(_)) => return Err("有 args 但缺少 program".into()),
            (None, None, None) => return Err("缺少 cmd 或 program".into()),
        };
        if let Some(hosts) = &self.hosts {
            check_hosts(hosts)?;
        }
        let comment = self.comment.filter(|c| !c.trim().is_empty());
        Ok(CommandEntry {
            name: self.name,
            program,
            args,
            env: self.env,
            cwd: self.cwd,
            comment,
            hosts: self.hosts,
        })
    }
}

/// `hosts` 必须非空，且为绝对路径或 `~/` 开头；文件是否存在留到执行时由 bwrap 判定。
fn check_hosts(hosts: &str) -> Result<(), String> {
    if hosts.trim().is_empty() {
        return Err("hosts 为空".into());
    }
    if !hosts.starts_with('/') && !hosts.starts_with("~/") {
        return Err("hosts 必须是绝对路径或 ~/ 开头".into());
    }
    Ok(())
}

/// 按 shell 引号/转义规则拆词：首项为 program，其余为 args。不经 shell 执行。
fn split_cmd(cmd: &str) -> Result<(String, Vec<String>), String> {
    let mut words = shlex::split(cmd).ok_or_else(|| "cmd 引号或转义未闭合".to_string())?;
    if words.is_empty() {
        return Err("cmd 为空".into());
    }
    let program = words.remove(0);
    Ok((program, words))
}

/// 加载后的命令库。
#[derive(Debug, Default)]
pub struct Config {
    entries: Vec<CommandEntry>,
}

impl Config {
    /// 从 `path` 加载命令库。
    ///
    /// 文件不存在时创建（含父目录）并返回空库；解析失败返回带路径与行号的错误，不静默覆盖。
    /// `name` 为空的条目被跳过（无法显示也无法匹配），不视为错误，且跳过先于条目校验。
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        if !path.exists() {
            create_empty(path)?;
            return Ok(Config::default());
        }
        let text = fs::read_to_string(path)
            .map_err(|source| ConfigError::Io { path: path.to_path_buf(), source })?;
        let file: ConfigFile = toml::from_str(&text)
            .map_err(|source| ConfigError::Parse { path: path.to_path_buf(), source })?;
        let mut entries = Vec::new();
        for (i, raw) in file.command.into_iter().enumerate() {
            if raw.name.trim().is_empty() {
                continue;
            }
            let name = raw.name.clone();
            let entry = raw.into_entry().map_err(|reason| ConfigError::Invalid {
                path: path.to_path_buf(),
                index: i + 1,
                name,
                reason,
            })?;
            entries.push(entry);
        }
        Ok(Config { entries })
    }

    /// 全部条目（保持文件内顺序）。
    pub fn entries(&self) -> &[CommandEntry] {
        &self.entries
    }
}

/// TOML 顶层形态：`[[command]]` 数组。
#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    command: Vec<RawEntry>,
}

/// 命令库加载错误。
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("$HOME 未设置，无法定位 ~/.config/super-command/commands.toml")]
    HomeNotSet,
    #[error("读取 {path} 失败: {source}", path = .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("解析 {path} 失败: {source}", path = .path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("解析 {path} 失败: 第 {index} 个 [[command]]（{name}）: {reason}", path = .path.display())]
    Invalid { path: PathBuf, index: usize, name: String, reason: String },
}

/// 由 home 目录拼出命令库路径（纯函数，便于单测注入）。
pub fn library_path_from(home: &str) -> PathBuf {
    PathBuf::from(home).join(".config").join("super-command").join("commands.toml")
}

/// 读取 `$HOME`；未设置返回 [`ConfigError::HomeNotSet`]。
///
/// 仅入口 `main` 调用，单测不覆盖（避免依赖环境变量）。
pub fn home_dir() -> Result<String, ConfigError> {
    std::env::var("HOME").map_err(|_| ConfigError::HomeNotSet)
}

fn create_empty(path: &Path) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|source| ConfigError::Io { path: parent.to_path_buf(), source })?;
    }
    fs::write(path, "").map_err(|source| ConfigError::Io { path: path.to_path_buf(), source })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单元测试专用临时目录（agent-testing-policy 允许临时/fixture 文件系统）。
    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "super-command-config-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_toml(dir: &Path, content: &str) -> PathBuf {
        let path = dir.join("commands.toml");
        fs::write(&path, content).expect("write toml");
        path
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

    #[test]
    fn load_round_trip_all_fields() {
        let dir = temp_dir("roundtrip");
        let path = write_toml(
            &dir,
            r#"
[[command]]
name = "clippy-all"
program = "cargo"
args = ["clippy", "--all-targets"]
cwd = "~/code/rust/foo"
[command.env]
RUST_LOG = "debug"
"#,
        );

        let config = Config::load(&path).expect("load ok");
        assert_eq!(config.entries().len(), 1);
        let got = &config.entries()[0];
        assert_eq!(got.name, "clippy-all");
        assert_eq!(got.program, "cargo");
        assert_eq!(got.args, vec!["clippy", "--all-targets"]);
        assert_eq!(got.env.get("RUST_LOG").map(String::as_str), Some("debug"));
        assert_eq!(got.cwd.as_deref(), Some("~/code/rust/foo"));
    }

    #[test]
    fn load_fills_serde_defaults() {
        let dir = temp_dir("defaults");
        let path = write_toml(
            &dir,
            r#"
[[command]]
name = "ls"
program = "ls"
"#,
        );

        let config = Config::load(&path).expect("load ok");
        assert_eq!(config.entries(), &[entry("ls", "ls")]);
    }

    #[test]
    fn load_missing_file_creates_empty_library() {
        let dir = temp_dir("missing");
        let path = dir.join("sub").join("commands.toml");
        assert!(!path.exists());

        let config = Config::load(&path).expect("load ok");
        assert!(config.entries().is_empty());
        assert!(path.exists(), "empty file should be created");
        assert!(dir.join("sub").is_dir());
    }

    #[test]
    fn load_bad_toml_reports_path_and_line() {
        let dir = temp_dir("bad");
        let path = write_toml(
            &dir,
            "[[command]\nname = \"x\"\nprogram = \"y\n", // 缺右括号 + 未闭合字符串
        );

        let err = Config::load(&path).expect_err("should fail");
        let msg = err.to_string();
        assert!(msg.contains(&path.display().to_string()), "msg: {msg}");
        assert!(
            msg.contains("line") || msg.contains("Line"),
            "msg should contain line number: {msg}"
        );
    }

    #[test]
    fn env_btree_map_serializes_deterministically() {
        let mut e = entry("n", "p");
        e.env.insert("ZZ".into(), "1".into());
        e.env.insert("AA".into(), "2".into());
        let toml = toml::to_string(&e).expect("serialize");
        let aa = toml.find("AA").expect("AA present");
        let zz = toml.find("ZZ").expect("ZZ present");
        assert!(aa < zz, "BTreeMap order must be key-sorted: {toml}");
    }

    #[test]
    fn load_skips_blank_name_entries() {
        let dir = temp_dir("blank");
        let path = write_toml(
            &dir,
            r#"
[[command]]
name = "  "
program = "x"

[[command]]
name = "keep"
program = "y"
"#,
        );

        let config = Config::load(&path).expect("load ok");
        assert_eq!(config.entries(), &[entry("keep", "y")]);
    }

    #[test]
    fn cmd_splits_into_program_and_args_with_quotes() {
        let dir = temp_dir("cmd");
        let path = write_toml(
            &dir,
            r#"
[[command]]
name = "docker"
comment = "启动 cowork"
cmd = """
docker run -d \
  -v '/a b:/c' --name "co work" \
  cowork
"""
"#,
        );

        let config = Config::load(&path).expect("load ok");
        let got = &config.entries()[0];
        assert_eq!(got.program, "docker");
        assert_eq!(got.args, vec!["run", "-d", "-v", "/a b:/c", "--name", "co work", "cowork"]);
        assert_eq!(got.comment.as_deref(), Some("启动 cowork"));
    }

    #[test]
    fn cmd_does_not_expand_shell_syntax() {
        assert_eq!(
            split_cmd("echo $HOME ~ a|b").expect("split"),
            ("echo".into(), vec!["$HOME".into(), "~".into(), "a|b".into()])
        );
    }

    #[test]
    fn invalid_entries_report_index_name_and_reason() {
        let cases = [
            ("cmd = \"ls\"\nprogram = \"ls\"", "cmd 与 program/args 不能同时出现"),
            ("cmd = \"ls\"\nargs = [\"-l\"]", "cmd 与 program/args 不能同时出现"),
            ("args = [\"-l\"]", "有 args 但缺少 program"),
            ("", "缺少 cmd 或 program"),
            ("cmd = \"  \"", "cmd 为空"),
            ("cmd = \"echo 'oops\"", "cmd 引号或转义未闭合"),
            ("cmd = \"ls\"\nhosts = \" \"", "hosts 为空"),
            ("cmd = \"ls\"\nhosts = \"hosts/test\"", "hosts 必须是绝对路径或 ~/ 开头"),
            ("cmd = \"ls\"\nhosts = \"~fisher/h\"", "hosts 必须是绝对路径或 ~/ 开头"),
        ];
        for (i, (body, reason)) in cases.into_iter().enumerate() {
            let dir = temp_dir(&format!("invalid{i}"));
            let text = format!("[[command]]\nname = \"ok\"\nprogram = \"x\"\n\n[[command]]\nname = \"bad\"\n{body}\n");
            let path = write_toml(&dir, &text);
            let msg = Config::load(&path).expect_err(body).to_string();
            assert!(msg.contains(&path.display().to_string()), "{msg}");
            assert!(msg.contains("第 2 个 [[command]]（bad）"), "{msg}");
            assert!(msg.contains(reason), "{msg}");
        }
    }

    #[test]
    fn blank_name_skipped_before_validation() {
        let dir = temp_dir("blank-invalid");
        let path = write_toml(&dir, "[[command]]\nname = \"\"\ncmd = \"'\"\n");
        assert!(Config::load(&path).expect("load ok").entries().is_empty());
    }

    #[test]
    fn blank_comment_becomes_none() {
        let dir = temp_dir("blank-comment");
        let path =
            write_toml(&dir, "[[command]]\nname = \"ls\"\nprogram = \"ls\"\ncomment = \" \"\n");
        assert_eq!(Config::load(&path).expect("load ok").entries(), &[entry("ls", "ls")]);
    }

    #[test]
    fn same_command_ignores_comment_only() {
        let a = entry("a", "p");
        let mut commented = a.clone();
        commented.comment = Some("说明".into());
        assert!(a.same_command(&commented));
        assert_ne!(a, commented);
        let mut other = a.clone();
        other.cwd = Some("/tmp".into());
        assert!(!a.same_command(&other));
    }

    #[test]
    fn same_command_distinguishes_hosts() {
        let a = entry("a", "p");
        let mut with_hosts = a.clone();
        with_hosts.hosts = Some("/tmp/h1".into());
        assert!(!a.same_command(&with_hosts), "有无 hosts 为不同命令");
        let mut other_hosts = a.clone();
        other_hosts.hosts = Some("/tmp/h2".into());
        assert!(!with_hosts.same_command(&other_hosts), "hosts 不同为不同命令");
    }

    #[test]
    fn hosts_loaded_raw_with_both_forms() {
        let dir = temp_dir("hosts");
        let path = write_toml(
            &dir,
            r#"
[[command]]
name = "a"
cmd = "getent hosts x"
hosts = "~/hosts/test"

[[command]]
name = "b"
program = "curl"
hosts = "/tmp/h"
"#,
        );
        let config = Config::load(&path).expect("load ok");
        assert_eq!(config.entries()[0].hosts.as_deref(), Some("~/hosts/test"), "存原始写法");
        assert_eq!(config.entries()[1].hosts.as_deref(), Some("/tmp/h"));
    }

    #[test]
    fn none_optional_fields_not_serialized() {
        let text = toml::to_string(&entry("a", "p")).expect("serialize");
        assert!(!text.contains("comment") && !text.contains("hosts"), "{text}");
    }

    #[test]
    fn library_path_from_joins_config_dir() {
        let path = library_path_from("/home/fisher");
        assert_eq!(path, PathBuf::from("/home/fisher/.config/super-command/commands.toml"));
    }
}
