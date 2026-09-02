//! Cheap process inspection for a pane's foreground leader: its command line,
//! basename, and live working directory. Platform lookups are isolated behind
//! pure parsers so the wire formats can be unit-tested without a live process.

use std::path::PathBuf;
use std::process::Command;

/// Live working directory of `pid`, or `None` when it can't be read.
///
/// Linux reads `/proc/<pid>/cwd`; macOS shells out to `lsof` since there is no
/// procfs. Callers cache the result (see `Pane`), so one lookup per tick is fine.
pub fn cwd_of(pid: i32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = Command::new("lsof")
            .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
            .output()
            .ok()?;
        parse_lsof_cwd(&String::from_utf8_lossy(&output.stdout))
    }
}

/// Full command line of `pid` via `ps -p PID -o args=`, or `None` if empty.
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

/// Wrap the pieces of a command so the login shell runs them verbatim. Each
/// argument is single-quoted (no external `shell-escape` dependency).
pub fn shell_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Single-quote one argument for POSIX shells, escaping embedded quotes.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Parse the `n` field of `lsof -Fn` output into a path.
#[cfg(any(not(target_os = "linux"), test))]
fn parse_lsof_cwd(output: &str) -> Option<PathBuf> {
    output
        .lines()
        .find_map(|line| line.strip_prefix('n'))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

/// Parse `ps -o args=` output: the single trimmed line, or `None` if blank.
fn parse_ps_args(output: &str) -> Option<String> {
    let trimmed = output.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(test)]
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
