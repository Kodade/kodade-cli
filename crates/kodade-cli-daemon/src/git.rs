//! Cheap git inspection and worktree management for workspaces (#22).
//!
//! Branch reads are pure filesystem lookups (`.git/HEAD`) so they can run on the
//! 2 s process tick without a subprocess. Mutations (`worktree add|remove`) shell
//! out to `git`, which is the only safe way to keep its bookkeeping consistent.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Walk up from `path` to the first directory containing a `.git` entry (a
/// directory for a normal repo, a file for a linked worktree). Returns the
/// working-tree root, or `None` when none is found.
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Current branch for the repo rooted at `root`, read straight from `.git/HEAD`
/// (no subprocess). A detached HEAD, or a `root` that is not a repo, returns
/// `None`.
pub fn current_branch(root: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir(root)?.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(|name| name.to_owned())
}

/// Branch label for `path`: find the enclosing repo, then read its HEAD. Used to
/// tag any workspace whose root sits inside a git repo.
pub fn branch_of(path: &Path) -> Option<String> {
    current_branch(&repo_root(path)?)
}

/// Resolve the git directory for the repo rooted at `root`. A normal repo keeps
/// it at `<root>/.git`; a linked worktree stores a `.git` file that reads
/// `gitdir: <path>` and points at `<main>/.git/worktrees/<name>`.
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let path = text.trim().strip_prefix("gitdir:")?.trim();
    Some(PathBuf::from(path))
}

/// If `root` is a linked worktree, the working-tree root of its main repo;
/// `None` for a normal repo or a non-repo. Used to nest a worktree workspace
/// under its parent in the sidebar.
pub fn main_worktree_root(root: &Path) -> Option<PathBuf> {
    if root.join(".git").is_dir() {
        return None; // a normal repo is its own main worktree
    }
    let git_dir = git_dir(root)?; // <main>/.git/worktrees/<name>
                                  // `commondir` points at the shared git dir, usually the relative "../..".
    let common = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let common = common.trim();
    let common_git = if Path::new(common).is_absolute() {
        PathBuf::from(common)
    } else {
        git_dir.join(common)
    };
    // The main worktree root is the parent of the shared `.git` directory.
    let common_git = common_git.canonicalize().ok()?;
    common_git.parent().map(Path::to_path_buf)
}

/// `git -C <repo> worktree add` for `branch` into `dest`. When `branch` already
/// exists it is checked out into the new worktree; otherwise it is created with
/// `-b` from `from` (defaulting to the current HEAD).
pub fn worktree_add(repo: &Path, branch: &str, from: Option<&str>, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create worktree parent directory {}", parent.display()))?;
    }
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(["worktree", "add"]);
    if branch_exists(repo, branch) {
        command.arg(dest).arg(branch);
    } else {
        command.args(["-b", branch]).arg(dest);
        if let Some(from) = from {
            command.arg(from);
        }
    }
    run(command, "git worktree add")
}

/// `git -C <repo> worktree remove` for `dest`. `force` drops uncommitted changes;
/// git itself refuses to remove a path that is not a registered worktree, which
/// keeps the "never delete an unregistered directory" invariant.
pub fn worktree_remove(repo: &Path, dest: &Path, force: bool) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(["worktree", "remove"]);
    if force {
        command.arg("--force");
    }
    command.arg(dest);
    run(command, "git worktree remove")
}

/// Whether `refs/heads/<branch>` resolves in `repo`.
fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Run a prepared `git` command, surfacing its stderr on failure.
fn run(mut command: Command, what: &str) -> Result<()> {
    let output = command.output().with_context(|| format!("run {what}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{what} failed: {}", stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// `git init` a throwaway repo with one commit, returning its root.
    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("git runs");
            assert!(status.status.success(), "git {args:?}: {:?}", status);
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("README.md"), "hi\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "kodade-git-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn reads_branch_and_repo_root() {
        let repo = temp_dir("branch");
        init_repo(&repo);
        let root = repo.canonicalize().unwrap();
        assert_eq!(
            repo_root(&root).map(|p| p.canonicalize().unwrap()),
            Some(root.clone())
        );
        // A nested path still walks up to the repo root.
        let nested = root.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            repo_root(&nested).map(|p| p.canonicalize().unwrap()),
            Some(root.clone())
        );
        assert_eq!(current_branch(&root).as_deref(), Some("main"));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn adds_and_removes_a_worktree() {
        let repo = temp_dir("wt");
        init_repo(&repo);
        let dest = temp_dir("wt-dest").join("feat-a");
        worktree_add(&repo, "feat-a", None, &dest).expect("worktree add");
        assert!(dest.join("README.md").exists());
        // The worktree's HEAD points at the new branch, read via its `.git` file.
        assert_eq!(current_branch(&dest).as_deref(), Some("feat-a"));
        // It nests under the main repo.
        let main = main_worktree_root(&dest).expect("main worktree root");
        assert_eq!(main.canonicalize().unwrap(), repo.canonicalize().unwrap());
        // A normal repo has no parent worktree.
        assert_eq!(main_worktree_root(&repo), None);

        worktree_remove(&repo, &dest, true).expect("worktree remove");
        assert!(!dest.exists());
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn checks_out_an_existing_branch_into_a_worktree() {
        let repo = temp_dir("wt-existing");
        init_repo(&repo);
        // Create a branch first, then add a worktree that checks it out.
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["branch", "existing"])
            .output()
            .unwrap();
        let dest = temp_dir("wt-existing-dest").join("existing");
        worktree_add(&repo, "existing", None, &dest).expect("worktree add existing");
        assert_eq!(current_branch(&dest).as_deref(), Some("existing"));
        worktree_remove(&repo, &dest, true).ok();
        std::fs::remove_dir_all(&repo).ok();
    }
}
