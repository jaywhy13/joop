use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::runner::{CommandRunner, Invocation};

/// Provides each agent its own checkout, so two issues can be worked in parallel
/// without fighting over one working tree.
pub trait WorktreeProvider: Send + Sync {
    /// Directory the session should run in, creating the worktree and branch if needed.
    /// An existing worktree is reused as-is: it may hold work from a previous attempt.
    fn prepare(&self, slug: &str, branch: &str) -> Result<PathBuf>;

    /// Where `prepare` would put this worktree, without creating anything.
    fn path_for(&self, slug: &str) -> PathBuf;
}

/// Backed by `git worktree`.
pub struct GitWorktrees<R: CommandRunner> {
    runner: R,
    repo_path: PathBuf,
    root: PathBuf,
}

impl<R: CommandRunner> GitWorktrees<R> {
    pub fn new(runner: R, repo_path: impl Into<PathBuf>, root: impl Into<PathBuf>) -> Self {
        Self {
            runner,
            repo_path: repo_path.into(),
            root: root.into(),
        }
    }

    fn git(&self, args: impl IntoIterator<Item = impl Into<String>>) -> Invocation {
        let mut full: Vec<String> = vec!["-C".into(), self.repo_path.display().to_string()];
        full.extend(args.into_iter().map(Into::into));
        Invocation::new("git", full)
    }

    fn branch_exists(&self, branch: &str) -> Result<bool> {
        let output = self.runner.run(&self.git([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ]))?;
        Ok(output.is_success())
    }
}

impl<R: CommandRunner> WorktreeProvider for GitWorktrees<R> {
    fn prepare(&self, slug: &str, branch: &str) -> Result<PathBuf> {
        let path = self.path_for(slug);

        if path.exists() {
            return Ok(path);
        }

        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("could not create {}", self.root.display()))?;

        let path_arg = path.display().to_string();
        let invocation = if self.branch_exists(branch)? {
            // A previous attempt left its branch behind; pick up where it stopped.
            self.git(["worktree", "add", &path_arg, branch])
        } else {
            self.git(["worktree", "add", "-b", branch, &path_arg])
        };

        self.runner
            .run_checked(&invocation)
            .with_context(|| format!("could not create a worktree for {slug} at {path_arg}"))?;

        Ok(path)
    }

    fn path_for(&self, slug: &str) -> PathBuf {
        self.root.join(slug)
    }
}

/// `joop/issue-42` — the branch an agent works the issue on.
pub fn branch_for(slug: &str) -> String {
    format!("joop/{slug}")
}

/// Default worktree root: a sibling `<repo>-joop-worktrees` directory, so worktrees
/// never land inside the repository they come from.
pub fn default_root(repo_path: &Path) -> PathBuf {
    let name = repo_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());

    repo_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("{name}-joop-worktrees"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CommandOutput;
    use crate::runner::testing::RecordingRunner;

    #[test]
    fn creates_a_new_branch_and_worktree_for_a_fresh_issue() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("worktrees");
        // rev-parse fails: the branch does not exist yet.
        let runner = RecordingRunner::returning([
            CommandOutput::failed(1, ""),
            CommandOutput::succeeded(""),
        ]);
        let worktrees = GitWorktrees::new(&runner, "/repo", &root);

        let path = worktrees.prepare("issue-42", "joop/issue-42").unwrap();

        assert_eq!(path, root.join("issue-42"));
        assert_eq!(
            runner.last_call().display(),
            format!(
                "git -C /repo worktree add -b joop/issue-42 {}",
                root.join("issue-42").display()
            )
        );
    }

    #[test]
    fn reuses_the_branch_left_behind_by_a_previous_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("worktrees");
        let runner = RecordingRunner::returning([
            CommandOutput::succeeded("abc123"),
            CommandOutput::succeeded(""),
        ]);
        let worktrees = GitWorktrees::new(&runner, "/repo", &root);

        worktrees.prepare("issue-42", "joop/issue-42").unwrap();

        assert_eq!(
            runner.last_call().display(),
            format!(
                "git -C /repo worktree add {} joop/issue-42",
                root.join("issue-42").display()
            ),
            "an existing branch must not be recreated with -b"
        );
    }

    #[test]
    fn an_existing_worktree_is_reused_without_touching_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("worktrees");
        std::fs::create_dir_all(root.join("issue-42")).unwrap();
        let runner = RecordingRunner::new();
        let worktrees = GitWorktrees::new(&runner, "/repo", &root);

        let path = worktrees.prepare("issue-42", "joop/issue-42").unwrap();

        assert_eq!(path, root.join("issue-42"));
        assert!(
            runner.calls().is_empty(),
            "reusing a worktree preserves whatever work is already in it"
        );
    }

    #[test]
    fn branches_are_namespaced_under_joop() {
        assert_eq!(branch_for("issue-42"), "joop/issue-42");
    }

    #[test]
    fn worktrees_default_to_a_sibling_of_the_repository() {
        assert_eq!(
            default_root(Path::new("/home/jaywhy13/code/joop")),
            Path::new("/home/jaywhy13/code/joop-joop-worktrees")
        );
    }
}
