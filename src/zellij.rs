use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::runner::{CommandRunner, Invocation};

/// What joop needs to open one agent's tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabSpec {
    pub name: String,
    pub cwd: PathBuf,
    /// The command to run in the tab, e.g. `pi` plus its opening prompt.
    pub command: Vec<String>,
}

/// The Zellij session joop drives: one tab per agent.
pub trait TabHost: Send + Sync {
    /// Make sure the session exists, creating it detached if it does not.
    fn ensure_session(&self) -> Result<()>;

    /// Open a tab and return Zellij's tab id, when it gives us one.
    fn open(&self, spec: &TabSpec) -> Result<Option<String>>;

    /// Close a tab by the id `open` returned.
    fn close(&self, tab_id: &str) -> Result<()>;

    /// Close a tab by name, for when no id was recorded.
    fn close_by_name(&self, tab_name: &str) -> Result<()>;

    /// Name of the tab the user is looking at, so joop can put it back after
    /// visiting other tabs.
    fn current_tab_name(&self) -> Result<Option<String>>;

    /// Bring a tab to the foreground, without sending it any input.
    fn focus(&self, tab_name: &str) -> Result<()>;

    /// Type a line into a tab's running program and press Enter. This is how joop
    /// tells a paused agent to carry on.
    fn send_line(&self, tab_name: &str, line: &str) -> Result<()>;

    /// Names of the tabs currently open.
    fn tab_names(&self) -> Result<Vec<String>>;
}

pub struct ZellijTabs<R: CommandRunner> {
    runner: R,
    session: String,
}

impl<R: CommandRunner> ZellijTabs<R> {
    pub fn new(runner: R, session: impl Into<String>) -> Self {
        Self {
            runner,
            session: session.into(),
        }
    }

    fn action(&self, args: impl IntoIterator<Item = impl Into<String>>) -> Invocation {
        let mut full: Vec<String> = vec!["--session".into(), self.session.clone(), "action".into()];
        full.extend(args.into_iter().map(Into::into));
        Invocation::new("zellij", full)
    }
}

impl<R: CommandRunner> TabHost for ZellijTabs<R> {
    fn ensure_session(&self) -> Result<()> {
        let existing = self
            .runner
            .run_checked(&Invocation::new("zellij", ["list-sessions", "--short"]))
            .unwrap_or_default();

        if existing.lines().any(|line| line.trim() == self.session) {
            return Ok(());
        }

        self.runner
            .run_checked(&Invocation::new(
                "zellij",
                ["attach", "--create-background", &self.session],
            ))
            .with_context(|| format!("could not create the Zellij session `{}`", self.session))?;
        Ok(())
    }

    fn open(&self, spec: &TabSpec) -> Result<Option<String>> {
        let mut args: Vec<String> = vec![
            "new-tab".into(),
            "--name".into(),
            spec.name.clone(),
            "--cwd".into(),
            spec.cwd.display().to_string(),
        ];
        args.push("--".into());
        args.extend(spec.command.iter().cloned());

        let stdout = self
            .runner
            .run_checked(&self.action(args))
            .with_context(|| format!("could not open the Zellij tab `{}`", spec.name))?;

        // `zellij action new-tab` prints the new tab's id; older versions print nothing.
        Ok(stdout
            .lines()
            .last()
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
            .map(str::to_string))
    }

    fn close(&self, tab_id: &str) -> Result<()> {
        self.runner
            .run_checked(&self.action(["close-tab-by-id", tab_id]))
            .with_context(|| format!("could not close Zellij tab {tab_id}"))?;
        Ok(())
    }

    fn close_by_name(&self, tab_name: &str) -> Result<()> {
        // There is no close-tab-by-name, so focus it and close what is focused.
        self.focus(tab_name)?;
        self.runner
            .run_checked(&self.action(["close-tab"]))
            .with_context(|| format!("could not close the Zellij tab `{tab_name}`"))?;
        Ok(())
    }

    fn current_tab_name(&self) -> Result<Option<String>> {
        let stdout = self
            .runner
            .run_checked(&self.action(["current-tab-info", "--json"]))
            .context("could not ask Zellij which tab is focused")?;

        #[derive(serde::Deserialize)]
        struct TabInfo {
            name: Option<String>,
        }

        let info: TabInfo =
            serde_json::from_str(&stdout).context("unexpected shape from current-tab-info")?;
        Ok(info.name)
    }

    fn focus(&self, tab_name: &str) -> Result<()> {
        self.runner
            .run_checked(&self.action(["go-to-tab-name", tab_name]))
            .with_context(|| format!("could not focus the Zellij tab `{tab_name}`"))?;
        Ok(())
    }

    fn send_line(&self, tab_name: &str, line: &str) -> Result<()> {
        // write-chars goes to the focused pane, so focus the tab first.
        self.focus(tab_name)?;
        self.runner
            .run_checked(&self.action(["write-chars", line]))
            .with_context(|| format!("could not write to the Zellij tab `{tab_name}`"))?;
        // Carriage return, which is what Enter sends.
        self.runner
            .run_checked(&self.action(["write", "13"]))
            .with_context(|| format!("could not submit the message to `{tab_name}`"))?;
        Ok(())
    }

    fn tab_names(&self) -> Result<Vec<String>> {
        let stdout = self.runner.run_checked(&self.action(["query-tab-names"]))?;
        Ok(stdout
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CommandOutput;
    use crate::runner::testing::RecordingRunner;

    fn spec() -> TabSpec {
        TabSpec {
            name: "issue-42".into(),
            cwd: PathBuf::from("/worktrees/issue-42"),
            command: vec!["pi".into(), "/implement issue 42".into()],
        }
    }

    #[test]
    fn opens_a_named_tab_in_the_worktree_running_the_given_command() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded("7\n")]);

        let tab_id = ZellijTabs::new(&runner, "joop").open(&spec()).unwrap();

        assert_eq!(tab_id.as_deref(), Some("7"));
        assert_eq!(
            runner.last_call().display(),
            "zellij --session joop action new-tab --name issue-42 \
             --cwd /worktrees/issue-42 -- pi /implement issue 42"
        );
    }

    #[test]
    fn the_prompt_is_passed_as_one_argument_not_split_into_words() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded("7")]);

        ZellijTabs::new(&runner, "joop").open(&spec()).unwrap();

        let args = runner.last_call().args;
        assert_eq!(
            args.last().unwrap(),
            "/implement issue 42",
            "splitting the prompt would hand Pi three separate messages"
        );
    }

    #[test]
    fn tolerates_zellij_not_reporting_a_tab_id() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded("")]);

        let tab_id = ZellijTabs::new(&runner, "joop").open(&spec()).unwrap();

        assert_eq!(tab_id, None);
    }

    #[test]
    fn an_existing_session_is_not_recreated() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded("other\njoop\n")]);

        ZellijTabs::new(&runner, "joop").ensure_session().unwrap();

        assert_eq!(runner.command_lines(), vec!["zellij list-sessions --short"]);
    }

    #[test]
    fn a_missing_session_is_created_in_the_background() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded("other\n")]);

        ZellijTabs::new(&runner, "joop").ensure_session().unwrap();

        assert_eq!(
            runner.last_call().display(),
            "zellij attach --create-background joop",
            "joop must not block by attaching to the session itself"
        );
    }

    #[test]
    fn closing_a_tab_uses_its_stable_id() {
        let runner = RecordingRunner::new();

        ZellijTabs::new(&runner, "joop").close("7").unwrap();

        assert_eq!(
            runner.last_call().display(),
            "zellij --session joop action close-tab-by-id 7"
        );
    }

    #[test]
    fn sending_a_line_focuses_the_tab_types_it_then_presses_enter() {
        let runner = RecordingRunner::new();

        ZellijTabs::new(&runner, "joop")
            .send_line("issue-42", "carry on")
            .unwrap();

        assert_eq!(
            runner.command_lines(),
            vec![
                "zellij --session joop action go-to-tab-name issue-42",
                "zellij --session joop action write-chars carry on",
                "zellij --session joop action write 13",
            ]
        );
    }

    #[test]
    fn a_tab_with_no_recorded_id_is_closed_by_focusing_it_first() {
        let runner = RecordingRunner::new();

        ZellijTabs::new(&runner, "joop")
            .close_by_name("issue-42")
            .unwrap();

        assert_eq!(
            runner.command_lines(),
            vec![
                "zellij --session joop action go-to-tab-name issue-42",
                "zellij --session joop action close-tab",
            ]
        );
    }

    #[test]
    fn reads_the_focused_tab_name() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded(
            r#"{"position":0,"name":"issue-42","active":true}"#,
        )]);

        let name = ZellijTabs::new(&runner, "joop").current_tab_name().unwrap();

        assert_eq!(name.as_deref(), Some("issue-42"));
    }

    #[test]
    fn an_unnamed_focused_tab_reads_as_none() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded(r#"{"position":0}"#)]);

        assert_eq!(
            ZellijTabs::new(&runner, "joop").current_tab_name().unwrap(),
            None
        );
    }

    #[test]
    fn focusing_a_tab_sends_no_input() {
        let runner = RecordingRunner::new();

        ZellijTabs::new(&runner, "joop").focus("issue-42").unwrap();

        assert_eq!(
            runner.command_lines(),
            vec!["zellij --session joop action go-to-tab-name issue-42"]
        );
    }

    #[test]
    fn lists_the_open_tab_names() {
        let runner =
            RecordingRunner::returning([CommandOutput::succeeded("issue-7\nissue-42\n\n")]);

        let names = ZellijTabs::new(&runner, "joop").tab_names().unwrap();

        assert_eq!(names, vec!["issue-7", "issue-42"]);
    }
}
