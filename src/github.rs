use anyhow::{Context, Result};

use crate::issue::{Issue, parse_issue_list};
use crate::runner::{CommandRunner, Invocation};

/// Source of the issues joop should work on.
pub trait IssueSource: Send + Sync {
    /// Open issues carrying `label`, newest-numbered first.
    fn labelled(&self, label: &str) -> Result<Vec<Issue>>;
}

/// Reads issues through the GitHub CLI, so joop inherits whatever `gh auth` is set up.
pub struct GhIssues<R: CommandRunner> {
    runner: R,
    repo: String,
    limit: u32,
}

impl<R: CommandRunner> GhIssues<R> {
    pub fn new(runner: R, repo: impl Into<String>, limit: u32) -> Self {
        Self {
            runner,
            repo: repo.into(),
            limit,
        }
    }

    fn list_invocation(&self, label: &str) -> Invocation {
        Invocation::new(
            "gh",
            [
                "issue",
                "list",
                "--repo",
                &self.repo,
                "--label",
                label,
                "--state",
                "open",
                "--limit",
                &self.limit.to_string(),
                "--json",
                "number,title,url,body,labels",
            ],
        )
    }
}

impl<R: CommandRunner> IssueSource for GhIssues<R> {
    fn labelled(&self, label: &str) -> Result<Vec<Issue>> {
        let stdout = self
            .runner
            .run_checked(&self.list_invocation(label))
            .with_context(|| format!("could not list `{label}` issues in {}", self.repo))?;

        parse_issue_list(&stdout)
    }
}

/// The `owner/name` of the repository in `dir`, as GitHub sees it.
pub fn repo_in_dir(runner: &dyn CommandRunner, dir: &std::path::Path) -> Result<String> {
    runner
        .run_checked(
            &Invocation::new(
                "gh",
                [
                    "repo",
                    "view",
                    "--json",
                    "nameWithOwner",
                    "--jq",
                    ".nameWithOwner",
                ],
            )
            .in_dir(dir),
        )
        .with_context(|| {
            format!(
                "could not work out which GitHub repository {} belongs to",
                dir.display()
            )
        })
}

/// Check that `repo` exists and is readable before joop starts polling it.
pub fn verify_repo(runner: &dyn CommandRunner, repo: &str) -> Result<()> {
    runner
        .run_checked(&Invocation::new(
            "gh",
            ["repo", "view", repo, "--json", "nameWithOwner"],
        ))
        .with_context(|| format!("cannot read the GitHub repository `{repo}`"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CommandOutput;
    use crate::runner::testing::RecordingRunner;

    const ONE_ISSUE: &str =
        r#"[{"number":42,"title":"t","url":"u","body":"b","labels":[{"name":"loop"}]}]"#;

    #[test]
    fn asks_gh_for_open_issues_with_the_label_in_the_configured_repo() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded(ONE_ISSUE)]);
        let issues = GhIssues::new(&runner, "jaywhy13/joop", 50);

        issues.labelled("loop").unwrap();

        assert_eq!(
            runner.last_call().display(),
            "gh issue list --repo jaywhy13/joop --label loop --state open --limit 50 \
             --json number,title,url,body,labels"
        );
    }

    #[test]
    fn returns_the_parsed_issues() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded(ONE_ISSUE)]);

        let issues = GhIssues::new(&runner, "jaywhy13/joop", 50)
            .labelled("loop")
            .unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 42);
    }

    #[test]
    fn names_the_repository_when_gh_fails() {
        let runner = RecordingRunner::returning([CommandOutput::failed(1, "not found")]);

        let error = GhIssues::new(&runner, "jaywhy13/joop", 50)
            .labelled("loop")
            .expect_err("gh failure is an error");

        assert!(
            format!("{error:#}").contains("jaywhy13/joop"),
            "got: {error:#}"
        );
    }

    #[test]
    fn resolves_the_repository_of_a_directory() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded("jaywhy13/joop\n")]);

        let repo = repo_in_dir(&runner, std::path::Path::new("/home/jaywhy13/code/joop")).unwrap();

        assert_eq!(repo, "jaywhy13/joop");
        let call = runner.last_call();
        assert_eq!(
            call.display(),
            "gh repo view --json nameWithOwner --jq .nameWithOwner"
        );
        assert_eq!(
            call.cwd.unwrap(),
            std::path::Path::new("/home/jaywhy13/code/joop")
        );
    }

    #[test]
    fn verifying_an_unreadable_repository_is_an_error() {
        let runner = RecordingRunner::returning([CommandOutput::failed(1, "Could not resolve")]);

        assert!(verify_repo(&runner, "jaywhy13/nope").is_err());
    }
}
