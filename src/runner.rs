//! 命令条目 → 可执行 [`CommandSpec`]，含 `~` 路径展开（纯逻辑，不 spawn）。
//!
//! 展开规则（见 `specs/command-manager-tui.md`）：
//! - `~` 单独 → home；`~/x` → home/x；`/abs` 原样；相对路径原样（以 cwd 为基准由 OS 处理）；
//! - `~user` 形式**不**展开（不做 passwd 查询）；
//! - `program` 不做展开（PATH 查找交给 OS），仅 `cwd` 与 `hosts` 展开。
//!
//! 条目设置 `hosts` 时把原命令包进 bwrap，在独立 mount/PID namespace 中以指定文件替换 `/etc/hosts`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::CommandEntry;

/// 一次执行的完整参数快照（stdin 固定 null，见「非交互命令行」约束）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub stdin_null: bool,
}

/// 由命令条目构建执行参数：`cwd` 做 `~` 展开，有 `hosts` 时包进 bwrap，其余字段直通。
pub fn build_spec(entry: &CommandEntry, home: &str) -> CommandSpec {
    let (program, args) = match &entry.hosts {
        Some(hosts) => wrap_with_hosts(&expand_path(hosts, home), entry),
        None => (entry.program.clone(), entry.args.clone()),
    };
    CommandSpec {
        program,
        args,
        env: entry.env.clone(),
        cwd: entry.cwd.as_deref().map(|cwd| expand_path(cwd, home)),
        stdin_null: true,
    }
}

/// `bwrap … -- program args…`：`--die-with-parent` + `--unshare-pid` 保证退出清理的 SIGKILL 不留孤儿。
fn wrap_with_hosts(hosts: &Path, entry: &CommandEntry) -> (String, Vec<String>) {
    let hosts = hosts.to_string_lossy();
    let mut args: Vec<String> = [
        "--dev-bind",
        "/",
        "/",
        "--ro-bind",
        hosts.as_ref(),
        "/etc/hosts",
        "--unshare-pid",
        "--proc",
        "/proc",
        "--die-with-parent",
        "--",
        entry.program.as_str(),
    ]
    .map(String::from)
    .to_vec();
    args.extend(entry.args.iter().cloned());
    ("bwrap".into(), args)
}

/// 展开 `~` 前缀路径。非 `~` 开头原样返回（保留相对/绝对语义）。
fn expand_path(raw: &str, home: &str) -> PathBuf {
    if raw == "~" {
        return PathBuf::from(home);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        let mut path = PathBuf::from(home);
        path.push(rest);
        return path;
    }
    PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn expand_tilde_only() {
        assert_eq!(expand_path("~", "/home/fisher"), PathBuf::from("/home/fisher"));
    }

    #[test]
    fn expand_tilde_slash_prefix() {
        assert_eq!(
            expand_path("~/code/rust", "/home/fisher"),
            PathBuf::from("/home/fisher/code/rust")
        );
    }

    #[test]
    fn expand_absolute_untouched() {
        assert_eq!(expand_path("/var/log", "/home/fisher"), PathBuf::from("/var/log"));
    }

    #[test]
    fn expand_relative_untouched() {
        assert_eq!(expand_path("build/out", "/home/fisher"), PathBuf::from("build/out"));
    }

    #[test]
    fn expand_tilde_user_not_expanded() {
        assert_eq!(expand_path("~root/.bashrc", "/home/fisher"), PathBuf::from("~root/.bashrc"));
    }

    #[test]
    fn build_spec_maps_fields_and_expands_cwd() {
        let mut e = entry("clippy", "cargo");
        e.args = vec!["clippy".into(), "--all-targets".into()];
        e.env.insert("RUST_LOG".into(), "debug".into());
        e.cwd = Some("~/code/rust/foo".into());

        let spec = build_spec(&e, "/home/fisher");
        assert_eq!(spec.program, "cargo");
        assert_eq!(spec.args, vec!["clippy", "--all-targets"]);
        assert_eq!(spec.env.get("RUST_LOG").map(String::as_str), Some("debug"));
        assert_eq!(spec.cwd, Some(PathBuf::from("/home/fisher/code/rust/foo")));
        assert!(spec.stdin_null, "子进程 stdin 必须固定 null");
    }

    #[test]
    fn build_spec_without_cwd_keeps_none() {
        let spec = build_spec(&entry("ls", "ls"), "/home/fisher");
        assert_eq!(spec.cwd, None);
        assert!(spec.args.is_empty());
        assert!(spec.env.is_empty());
    }

    #[test]
    fn build_spec_wraps_with_bwrap_when_hosts_set() {
        let mut e = entry("api", "curl");
        e.args = vec!["-v".into(), "http://api.example.com".into()];
        e.env.insert("K".into(), "v".into());
        e.cwd = Some("~/code".into());
        e.hosts = Some("~/hosts/test".into());

        let spec = build_spec(&e, "/home/fisher");
        assert_eq!(spec.program, "bwrap");
        assert_eq!(
            spec.args,
            vec![
                "--dev-bind",
                "/",
                "/",
                "--ro-bind",
                "/home/fisher/hosts/test",
                "/etc/hosts",
                "--unshare-pid",
                "--proc",
                "/proc",
                "--die-with-parent",
                "--",
                "curl",
                "-v",
                "http://api.example.com",
            ]
        );
        assert_eq!(spec.env, e.env, "env 直通");
        assert_eq!(spec.cwd, Some(PathBuf::from("/home/fisher/code")), "cwd 直通");
        assert!(spec.stdin_null);
    }

    #[test]
    fn build_spec_hosts_absolute_path_untouched() {
        let mut e = entry("ls", "ls");
        e.hosts = Some("/tmp/my-custom-hosts".into());
        let spec = build_spec(&e, "/home/fisher");
        assert_eq!(spec.args[4], "/tmp/my-custom-hosts");
        assert_eq!(spec.args.last().map(String::as_str), Some("ls"));
    }
}
