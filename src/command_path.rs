//! Resolve whether a program name is available on `PATH`.
//!
//! SSHub used to preflight with `which(1)`, which is Unix-only. On Windows the
//! helper is missing, and TUI paths treated spawn failure as "command not found"
//! (`unwrap_or(true)`), so a perfectly valid `ssh.exe` on PATH was rejected.
//! This module walks `PATH` (and Windows `PATHEXT`) without spawning anything.

use std::env;
use std::path::{Component, Path, PathBuf};

/// True if `name` is an absolute/relative path that exists, or resolves to a
/// file on `PATH` (with Windows executable extensions applied when needed).
pub fn command_exists(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    let path = Path::new(name);
    // Path-like: contains a separator or is absolute → check the path itself.
    if path_is_explicit(path) {
        return is_runnable(path);
    }

    let Some(path_var) = env::var_os("PATH") else {
        return false;
    };

    for dir in env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if is_runnable(&dir.join(name)) {
            return true;
        }
    }
    false
}

fn path_is_explicit(path: &Path) -> bool {
    path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        || path_has_separator(path)
}

fn path_has_separator(path: &Path) -> bool {
    let s = path.as_os_str();
    #[cfg(windows)]
    {
        s.to_string_lossy().contains('\\') || s.to_string_lossy().contains('/')
    }
    #[cfg(not(windows))]
    {
        s.to_string_lossy().contains('/')
    }
}

fn is_runnable(path: &Path) -> bool {
    if file_exists(path) {
        return true;
    }
    #[cfg(windows)]
    {
        // CreateProcess appends PATHEXT when the name has no extension.
        if path.extension().is_some() {
            return false;
        }
        for ext in pathext_list() {
            let mut candidate = PathBuf::from(path);
            let mut file_name = match candidate.file_name() {
                Some(n) => n.to_os_string(),
                None => continue,
            };
            file_name.push(ext);
            candidate.set_file_name(file_name);
            if file_exists(&candidate) {
                return true;
            }
        }
        false
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

fn file_exists(path: &Path) -> bool {
    path.is_file()
}

#[cfg(windows)]
fn pathext_list() -> Vec<std::ffi::OsString> {
    let raw =
        env::var_os("PATHEXT").unwrap_or_else(|| std::ffi::OsString::from(".COM;.EXE;.BAT;.CMD"));
    raw.to_string_lossy()
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let s = s.trim();
            if s.starts_with('.') {
                std::ffi::OsString::from(s)
            } else {
                let mut o = std::ffi::OsString::from(".");
                o.push(s);
                o
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_name_is_missing() {
        assert!(!command_exists(""));
    }

    #[test]
    fn missing_program_is_false() {
        assert!(!command_exists(
            "sshub-definitely-not-a-real-binary-xyzzy-9f3a"
        ));
    }

    #[test]
    fn explicit_missing_path_is_false() {
        assert!(!command_exists("/no/such/sshub/binary/xyzzy"));
        #[cfg(windows)]
        assert!(!command_exists(r"C:\no\such\sshub\binary\xyzzy.exe"));
    }

    #[test]
    fn finds_ssh_on_path_when_present() {
        // CI and developer machines running SSH tests have OpenSSH; skip soft.
        if !command_exists("ssh") && !command_exists("ssh.exe") {
            eprintln!("skip: ssh not on PATH");
            return;
        }
        assert!(
            command_exists("ssh") || command_exists("ssh.exe"),
            "ssh should resolve on PATH"
        );
    }

    #[cfg(windows)]
    #[test]
    fn finds_where_exe() {
        // where.exe lives in System32 on every supported Windows install.
        assert!(
            command_exists("where") || command_exists("where.exe"),
            "where.exe should be on PATH"
        );
    }

    #[cfg(unix)]
    #[test]
    fn finds_sh() {
        assert!(command_exists("sh") || Path::new("/bin/sh").is_file());
    }
}
