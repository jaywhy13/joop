use anyhow::{Context, Result};

use crate::issue::{Issue, parse_issue_list};
use crate::runner::{CommandRunner, Invocation};

/// Whether one issue is still open. Enough to tell whether it still blocks another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueState {
    pub number: u64,
    pub title: String,
    pub open: bool,
}

/// Source of the issues joop should work on.
pub trait IssueSource: Send + Sync {
    /// Open issues carrying `label`, newest-numbered first.
    fn labelled(&self, label: &str) -> Result<Vec<Issue>>;

    /// The current state of one issue, open or closed.
    fn state_of(&self, number: u64) -> Result<IssueState>;
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

impl<R: CommandRunner> GhIssues<R> {
    fn view_invocation(&self, number: u64) -> Invocation {
        Invocation::new(
            "gh",
            [
                "issue",
                "view",
                &number.to_string(),
                "--repo",
                &self.repo,
                "--json",
                "number,title,state",
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

    fn state_of(&self, number: u64) -> Result<IssueState> {
        #[derive(serde::Deserialize)]
        struct View {
            number: u64,
            title: String,
            state: String,
        }

        let stdout = self
            .runner
            .run_checked(&self.view_invocation(number))
            .with_context(|| format!("could not read #{number} in {}", self.repo))?;
        let view: View = serde_json::from_str(&stdout)
            .with_context(|| format!("could not parse #{number} as returned by `gh`"))?;

        Ok(IssueState {
            number: view.number,
            title: view.title,
            open: view.state == "OPEN",
        })
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
    fn reads_one_issue_by_number_in_the_configured_repo() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded(
            r#"{"number":87,"title":"Recipe import","state":"CLOSED"}"#,
        )]);

        let state = GhIssues::new(&runner, "jaywhy13/meal-planner", 50)
            .state_of(87)
            .unwrap();

        assert_eq!(
            runner.last_call().display(),
            "gh issue view 87 --repo jaywhy13/meal-planner --json number,title,state"
        );
        assert_eq!(
            state,
            IssueState {
                number: 87,
                title: "Recipe import".into(),
                open: false,
            }
        );
    }

    #[test]
    fn an_open_issue_is_reported_as_open() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded(
            r#"{"number":89,"title":"Typed failures","state":"OPEN"}"#,
        )]);

        let state = GhIssues::new(&runner, "jaywhy13/meal-planner", 50)
            .state_of(89)
            .unwrap();

        assert!(state.open);
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
