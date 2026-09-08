//! Cheap process inspection for a pane's foreground leader: its command line,
//! basename, and live working directory. Platform lookups are isolated behind
//! pure parsers so the wire formats can be unit-tested without a live process.

#[cfg(not(windows))]
use std::path::PathBuf;
#[cfg(not(windows))]
use std::process::Command;

/// Live working directory of `pid`, or `None` when it can't be read.
///
/// Linux reads `/proc/<pid>/cwd`; macOS shells out to `lsof` since there is no
/// procfs. Callers cache the result (see `Pane`), so one lookup per tick is fine.
#[cfg(not(windows))]
pub fn cwd_of(pid: i32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("lsof")
            .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
            .output()
            .ok()?;
        parse_lsof_cwd(&String::from_utf8_lossy(&output.stdout))
    }
}

/// Full command line of `pid` via `ps -p PID -o args=`, or `None` if empty.
#[cfg(not(windows))]
pub fn command_of(pid: i32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output()
        .ok()?;
    parse_ps_args(&String::from_utf8_lossy(&output.stdout))
}

/// Program basename for detection: strips the path and any login-shell `-`
/// prefix (argv0 of a login shell is reported as `-zsh`).
pub fn process_basename(command: &str) -> Option<String> {
    let first = command.split_whitespace().next()?;
    let name = std::path::Path::new(first).file_name()?.to_str()?;
    Some(name.trim_start_matches('-').to_owned())
}

/// Return the live program under a ConPTY's initial shell. Windows has no
/// process-group leader API, so inspect the bounded Toolhelp snapshot and walk
/// descendants until a non-shell child (the actual agent) appears.
#[cfg(windows)]
pub fn descendant_process(root: u32) -> Option<(i32, Option<String>)> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
    };
    #[derive(Clone)]
    struct Entry {
        pid: u32,
        parent: u32,
        name: String,
    }
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    let mut entries = Vec::new();
    unsafe {
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                entries.push(Entry {
                    pid: entry.th32ProcessID,
                    parent: entry.th32ParentProcessID,
                    name: String::from_utf16_lossy(&entry.szExeFile[..end]),
                });
                entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    let shell = |name: &str| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "cmd.exe" | "conhost.exe"
        )
    };
    let mut queue = vec![root];
    while let Some(parent) = queue.pop() {
        for child in entries.iter().filter(|entry| entry.parent == parent) {
            if !shell(&child.name) {
                return Some((child.pid as i32, Some(windows_process_name(&child.name))));
            }
            queue.push(child.pid);
        }
    }
    entries.iter().find(|entry| entry.pid == root).map(|entry| {
        (
            root as i32,
            (!shell(&entry.name)).then(|| windows_process_name(&entry.name)),
        )
    })
}

/// Toolhelp exposes executable filenames (`node.exe`), while manifests and
/// hook adapters name the foreground program (`node`). Keep the identity in
/// that shared form before comparing a hook to the live pane process.
#[cfg(windows)]
fn windows_process_name(name: &str) -> String {
    name.strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))
        .unwrap_or(name)
        .to_owned()
}

/// Wrap the pieces of a command so the login shell runs them verbatim. Each
/// argument is single-quoted (no external `shell-escape` dependency).
pub fn shell_command(args: &[String]) -> String {
    #[cfg(unix)]
    {
        args.iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    }
    #[cfg(windows)]
    {
        args.join(" ")
    }
}

/// Single-quote one argument for POSIX shells, escaping embedded quotes.
#[cfg(unix)]
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Parse the `n` field of `lsof -Fn` output into a path.
#[cfg(any(target_os = "macos", all(test, unix)))]
fn parse_lsof_cwd(output: &str) -> Option<PathBuf> {
    output
        .lines()
        .find_map(|line| line.strip_prefix('n'))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

/// Parse `ps -o args=` output: the single trimmed line, or `None` if blank.
#[cfg(not(windows))]
fn parse_ps_args(output: &str) -> Option<String> {
    let trimmed = output.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_cwd_field() {
        // Captured `lsof -a -p 4321 -d cwd -Fn` output.
        let sample = "p4321\nfcwd\nn/Users/keith/src/repo\n";
        assert_eq!(
            parse_lsof_cwd(sample),
            Some(PathBuf::from("/Users/keith/src/repo"))
        );
        assert_eq!(parse_lsof_cwd("p4321\nfcwd\n"), None);
        assert_eq!(parse_lsof_cwd(""), None);
    }

    #[test]
    fn parses_ps_args_line() {
        assert_eq!(
            parse_ps_args("/bin/zsh -l\n"),
            Some("/bin/zsh -l".to_owned())
        );
        assert_eq!(parse_ps_args("   \n"), None);
    }

    #[test]
    fn basename_strips_path_and_login_dash() {
        assert_eq!(process_basename("-zsh").as_deref(), Some("zsh"));
        assert_eq!(process_basename("/bin/zsh -l").as_deref(), Some("zsh"));
        assert_eq!(
            process_basename("node /usr/local/bin/claude").as_deref(),
            Some("node")
        );
        assert_eq!(process_basename("").as_deref(), None);
    }

    #[test]
    fn shell_command_single_quotes_each_argument() {
        assert_eq!(shell_command(&["claude".into()]), "'claude'");
        assert_eq!(
            shell_command(&["echo".into(), "a b".into()]),
            "'echo' 'a b'"
        );
        // An embedded single quote closes, escapes, and reopens the quoting.
        assert_eq!(shell_command(&["it's".into()]), "'it'\\''s'");
    }
}
