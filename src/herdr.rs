use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::runner::{CommandRunner, Invocation};

/// What joop needs to put one agent to work on an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSpec {
    /// Tab label and agent name, e.g. `issue-42`.
    pub name: String,
    pub cwd: PathBuf,
    /// Agent kind herdr should recognise, e.g. `claude`.
    pub kind: String,
    /// Arguments passed to the agent binary itself, after `--`.
    pub agent_args: Vec<String>,
    /// Environment the tab's process starts with, e.g. the joop API URL.
    pub env: Vec<(String, String)>,
}

/// Where herdr put an agent, as reported back by `agent start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningAgent {
    pub tab_id: String,
    pub pane_id: String,
    /// The agent name, which is also how herdr addresses it.
    pub name: String,
}

/// Display-only facts joop pushes to herdr so they show under the agent in its UI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneMetadata {
    pub title: Option<String>,
    /// Arbitrary key/value shown against the pane; at most 16, keys `[A-Za-z0-9_-]{1,32}`.
    pub tokens: Vec<(String, String)>,
}

/// The herdr session joop drives: one tab per issue, each running one agent.
pub trait AgentHost: Send + Sync {
    /// Find or create the workspace joop's tabs live in, returning its id.
    fn ensure_workspace(&self) -> Result<String>;

    /// Open a tab and start the agent in it.
    fn start(&self, spec: &AgentSpec) -> Result<RunningAgent>;

    /// Submit an opening prompt, or any later instruction, to a running agent.
    fn prompt(&self, agent_name: &str, text: &str) -> Result<()>;

    /// Close a tab by the id `start` returned.
    fn close_tab(&self, tab_id: &str) -> Result<()>;

    /// Bring a tab to the front of herdr's UI.
    fn focus_tab(&self, tab_id: &str) -> Result<()>;

    /// Attach display-only metadata to an agent's pane.
    fn report_metadata(&self, pane_id: &str, metadata: &PaneMetadata) -> Result<()>;

    /// Labels of the tabs currently open in joop's workspace.
    fn tab_labels(&self) -> Result<Vec<String>>;
}

pub struct HerdrAgents<R: CommandRunner> {
    runner: R,
    /// Label of the workspace joop keeps its tabs in.
    workspace_label: String,
    /// Resolved once: a workspace joop made does not move for the life of the process.
    workspace_id: Mutex<Option<String>>,
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    result: T,
}

#[derive(Debug, Deserialize)]
struct WorkspaceList {
    workspaces: Vec<WorkspaceSummary>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceSummary {
    workspace_id: String,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceCreated {
    workspace: WorkspaceSummary,
}

#[derive(Debug, Deserialize)]
struct TabCreated {
    tab: TabSummary,
    root_pane: PaneSummary,
}

#[derive(Debug, Deserialize)]
struct TabSummary {
    tab_id: String,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PaneSummary {
    pane_id: String,
}

#[derive(Debug, Deserialize)]
struct TabList {
    tabs: Vec<TabSummary>,
}

#[derive(Debug, Deserialize)]
struct AgentStarted {
    agent: StartedAgent,
}

#[derive(Debug, Deserialize)]
struct StartedAgent {
    name: String,
    tab_id: String,
    pane_id: String,
}

impl<R: CommandRunner> HerdrAgents<R> {
    pub fn new(runner: R, workspace_label: impl Into<String>) -> Self {
        Self {
            runner,
            workspace_label: workspace_label.into(),
            workspace_id: Mutex::new(None),
        }
    }

    fn herdr(&self, args: impl IntoIterator<Item = impl Into<String>>) -> Invocation {
        Invocation::new("herdr", args)
    }

    /// herdr answers most commands with `{"id": ..., "result": {...}}`.
    fn run_json<T: serde::de::DeserializeOwned>(&self, invocation: &Invocation) -> Result<T> {
        let stdout = self.runner.run_checked(invocation)?;
        let envelope: Envelope<T> = serde_json::from_str(&stdout)
            .with_context(|| format!("unexpected answer from `{}`", invocation.display()))?;
        Ok(envelope.result)
    }

    fn cached_workspace_id(&self) -> Option<String> {
        self.workspace_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn remember_workspace_id(&self, workspace_id: &str) {
        *self
            .workspace_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(workspace_id.to_string());
    }

    fn find_workspace_id(&self) -> Result<Option<String>> {
        let listed: WorkspaceList = self
            .run_json(&self.herdr(["workspace", "list"]))
            .context("could not list herdr workspaces")?;

        Ok(listed
            .workspaces
            .into_iter()
            .find(|workspace| workspace.label.as_deref() == Some(self.workspace_label.as_str()))
            .map(|workspace| workspace.workspace_id))
    }
}

impl<R: CommandRunner> AgentHost for HerdrAgents<R> {
    fn ensure_workspace(&self) -> Result<String> {
        if let Some(workspace_id) = self.cached_workspace_id() {
            return Ok(workspace_id);
        }
        if let Some(workspace_id) = self.find_workspace_id()? {
            self.remember_workspace_id(&workspace_id);
            return Ok(workspace_id);
        }

        let created: WorkspaceCreated = self
            .run_json(&self.herdr([
                "workspace",
                "create",
                "--label",
                &self.workspace_label,
                "--no-focus",
            ]))
            .with_context(|| {
                format!(
                    "could not create the herdr workspace `{}`",
                    self.workspace_label
                )
            })?;

        self.remember_workspace_id(&created.workspace.workspace_id);
        Ok(created.workspace.workspace_id)
    }

    fn start(&self, spec: &AgentSpec) -> Result<RunningAgent> {
        let workspace_id = self.ensure_workspace()?;

        let mut tab_args: Vec<String> = vec![
            "tab".into(),
            "create".into(),
            "--workspace".into(),
            workspace_id,
            "--cwd".into(),
            spec.cwd.display().to_string(),
            "--label".into(),
            spec.name.clone(),
            "--no-focus".into(),
        ];
        for (name, value) in &spec.env {
            tab_args.push("--env".into());
            tab_args.push(format!("{name}={value}"));
        }

        let tab: TabCreated = self
            .run_json(&self.herdr(tab_args))
            .with_context(|| format!("could not open a herdr tab for {}", spec.name))?;

        // herdr starts the agent in a pane that is already at its shell prompt, and only
        // answers once it has seen the agent come up ready for input.
        let mut agent_args: Vec<String> = vec![
            "agent".into(),
            "start".into(),
            spec.name.clone(),
            "--kind".into(),
            spec.kind.clone(),
            "--pane".into(),
            tab.root_pane.pane_id.clone(),
        ];
        if !spec.agent_args.is_empty() {
            agent_args.push("--".into());
            agent_args.extend(spec.agent_args.iter().cloned());
        }

        let started: AgentStarted = match self.run_json(&self.herdr(agent_args)) {
            Ok(started) => started,
            Err(error) => {
                // A tab with no agent in it is litter joop cannot supervise.
                let _ = self.close_tab(&tab.tab.tab_id);
                return Err(error)
                    .with_context(|| format!("could not start {} in its tab", spec.kind));
            }
        };

        Ok(RunningAgent {
            tab_id: started.agent.tab_id,
            pane_id: started.agent.pane_id,
            name: started.agent.name,
        })
    }

    fn prompt(&self, agent_name: &str, text: &str) -> Result<()> {
        self.runner
            .run_checked(&self.herdr(["agent", "prompt", agent_name, text]))
            .with_context(|| format!("could not prompt the agent `{agent_name}`"))?;
        Ok(())
    }

    fn close_tab(&self, tab_id: &str) -> Result<()> {
        self.runner
            .run_checked(&self.herdr(["tab", "close", tab_id]))
            .with_context(|| format!("could not close herdr tab {tab_id}"))?;
        Ok(())
    }

    fn focus_tab(&self, tab_id: &str) -> Result<()> {
        self.runner
            .run_checked(&self.herdr(["tab", "focus", tab_id]))
            .with_context(|| format!("could not focus herdr tab {tab_id}"))?;
        Ok(())
    }

    fn report_metadata(&self, pane_id: &str, metadata: &PaneMetadata) -> Result<()> {
        if metadata.title.is_none() && metadata.tokens.is_empty() {
            return Ok(());
        }
        if metadata.tokens.len() > MAX_TOKENS {
            bail!(
                "herdr accepts at most {MAX_TOKENS} metadata tokens, got {}",
                metadata.tokens.len()
            );
        }

        let mut args: Vec<String> = vec![
            "pane".into(),
            "report-metadata".into(),
            pane_id.to_string(),
            "--source".into(),
            METADATA_SOURCE.into(),
        ];
        if let Some(title) = &metadata.title {
            args.push("--title".into());
            args.push(title.clone());
        }
        for (name, value) in &metadata.tokens {
            args.push("--token".into());
            args.push(format!("{name}={value}"));
        }

        self.runner
            .run_checked(&self.herdr(args))
            .with_context(|| format!("could not report metadata for pane {pane_id}"))?;
        Ok(())
    }

    fn tab_labels(&self) -> Result<Vec<String>> {
        let Some(workspace_id) = self.cached_workspace_id().or(self.find_workspace_id()?) else {
            return Ok(Vec::new());
        };

        let listed: TabList =
            self.run_json(&self.herdr(["tab", "list", "--workspace", &workspace_id]))?;
        Ok(listed
            .tabs
            .into_iter()
            .filter_map(|tab| tab.label)
            .collect())
    }
}

/// Names joop's metadata so herdr can tell it apart from another reporter's.
const METADATA_SOURCE: &str = "joop";

/// herdr's limit on display tokens per pane.
const MAX_TOKENS: usize = 16;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CommandOutput;
    use crate::runner::testing::RecordingRunner;

    fn spec() -> AgentSpec {
        AgentSpec {
            name: "issue-42".into(),
            cwd: PathBuf::from("/worktrees/issue-42"),
            kind: "claude".into(),
            agent_args: vec!["--dangerously-skip-permissions".into()],
            env: vec![("JOOP_API_URL".into(), "http://127.0.0.1:7777".into())],
        }
    }

    fn workspaces(label: &str) -> CommandOutput {
        CommandOutput::succeeded(format!(
            r#"{{"result":{{"workspaces":[{{"workspace_id":"w1","label":"meal-planner"}},{{"workspace_id":"w4","label":"{label}"}}]}}}}"#
        ))
    }

    fn tab_created() -> CommandOutput {
        CommandOutput::succeeded(
            r#"{"result":{"tab":{"tab_id":"w4:t2","label":"issue-42"},"root_pane":{"pane_id":"w4:p2"}}}"#,
        )
    }

    fn agent_started() -> CommandOutput {
        CommandOutput::succeeded(
            r#"{"result":{"agent":{"name":"issue-42","tab_id":"w4:t2","pane_id":"w4:p2"},"argv":["claude","--dangerously-skip-permissions"]}}"#,
        )
    }

    #[test]
    fn an_existing_workspace_is_reused() {
        let runner = RecordingRunner::returning([workspaces("joop")]);

        let workspace_id = HerdrAgents::new(&runner, "joop")
            .ensure_workspace()
            .unwrap();

        assert_eq!(workspace_id, "w4");
        assert_eq!(runner.command_lines(), vec!["herdr workspace list"]);
    }

    #[test]
    fn a_missing_workspace_is_created_without_stealing_focus() {
        let runner = RecordingRunner::returning([
            workspaces("something-else"),
            CommandOutput::succeeded(
                r#"{"result":{"workspace":{"workspace_id":"w9","label":"joop"}}}"#,
            ),
        ]);

        let workspace_id = HerdrAgents::new(&runner, "joop")
            .ensure_workspace()
            .unwrap();

        assert_eq!(workspace_id, "w9");
        assert_eq!(
            runner.last_call().display(),
            "herdr workspace create --label joop --no-focus"
        );
    }

    #[test]
    fn the_workspace_is_only_looked_up_once() {
        let runner = RecordingRunner::returning([
            workspaces("something-else"),
            CommandOutput::succeeded(
                r#"{"result":{"workspace":{"workspace_id":"w9","label":"joop"}}}"#,
            ),
        ]);
        let agents = HerdrAgents::new(&runner, "joop");

        agents.ensure_workspace().unwrap();
        agents.ensure_workspace().unwrap();

        assert_eq!(
            runner.command_lines(),
            vec![
                "herdr workspace list",
                "herdr workspace create --label joop --no-focus"
            ],
            "a workspace joop made does not move, so asking herdr again is wasted work"
        );
    }

    #[test]
    fn starting_an_agent_opens_a_labelled_tab_in_the_worktree() {
        let runner =
            RecordingRunner::returning([workspaces("joop"), tab_created(), agent_started()]);

        HerdrAgents::new(&runner, "joop").start(&spec()).unwrap();

        assert_eq!(
            runner.command_lines()[1],
            "herdr tab create --workspace w4 --cwd /worktrees/issue-42 --label issue-42 \
             --no-focus --env JOOP_API_URL=http://127.0.0.1:7777"
        );
    }

    #[test]
    fn the_agent_is_started_in_the_new_tabs_pane_with_its_own_arguments() {
        let runner =
            RecordingRunner::returning([workspaces("joop"), tab_created(), agent_started()]);

        HerdrAgents::new(&runner, "joop").start(&spec()).unwrap();

        assert_eq!(
            runner.last_call().display(),
            "herdr agent start issue-42 --kind claude --pane w4:p2 -- --dangerously-skip-permissions",
            "agent arguments must come after `--` or herdr reads them as its own"
        );
    }

    #[test]
    fn starting_returns_the_ids_herdr_reported_rather_than_guessing_them() {
        let runner =
            RecordingRunner::returning([workspaces("joop"), tab_created(), agent_started()]);

        let agent = HerdrAgents::new(&runner, "joop").start(&spec()).unwrap();

        assert_eq!(
            agent,
            RunningAgent {
                tab_id: "w4:t2".into(),
                pane_id: "w4:p2".into(),
                name: "issue-42".into(),
            }
        );
    }

    #[test]
    fn a_tab_whose_agent_will_not_start_is_closed_again() {
        let runner = RecordingRunner::returning([
            workspaces("joop"),
            tab_created(),
            CommandOutput::failed(1, "claude did not become ready"),
        ]);

        let error = HerdrAgents::new(&runner, "joop")
            .start(&spec())
            .expect_err("a failed agent start is an error");

        assert!(format!("{error:#}").contains("claude"), "got: {error:#}");
        assert_eq!(
            runner.last_call().display(),
            "herdr tab close w4:t2",
            "an empty tab is litter joop cannot supervise"
        );
    }

    #[test]
    fn an_agent_with_no_extra_arguments_gets_no_trailing_separator() {
        let runner =
            RecordingRunner::returning([workspaces("joop"), tab_created(), agent_started()]);
        let mut spec = spec();
        spec.agent_args.clear();

        HerdrAgents::new(&runner, "joop").start(&spec).unwrap();

        assert_eq!(
            runner.last_call().display(),
            "herdr agent start issue-42 --kind claude --pane w4:p2"
        );
    }

    #[test]
    fn prompting_goes_through_the_agent_surface_rather_than_typing_into_a_terminal() {
        let runner = RecordingRunner::new();

        HerdrAgents::new(&runner, "joop")
            .prompt("issue-42", "/implement issue 42")
            .unwrap();

        let call = runner.last_call();
        assert_eq!(call.program, "herdr");
        assert_eq!(
            call.args,
            vec!["agent", "prompt", "issue-42", "/implement issue 42"]
        );
    }

    #[test]
    fn closing_a_tab_uses_the_id_herdr_gave() {
        let runner = RecordingRunner::new();

        HerdrAgents::new(&runner, "joop")
            .close_tab("w4:t2")
            .unwrap();

        assert_eq!(runner.last_call().display(), "herdr tab close w4:t2");
    }

    #[test]
    fn focusing_a_tab_uses_its_id() {
        let runner = RecordingRunner::new();

        HerdrAgents::new(&runner, "joop")
            .focus_tab("w4:t2")
            .unwrap();

        assert_eq!(runner.last_call().display(), "herdr tab focus w4:t2");
    }

    #[test]
    fn metadata_is_reported_against_joop_as_its_source() {
        let runner = RecordingRunner::new();

        HerdrAgents::new(&runner, "joop")
            .report_metadata(
                "w4:p2",
                &PaneMetadata {
                    title: Some("issue-42 · PR #7".into()),
                    tokens: vec![
                        ("pull_request".into(), "7".into()),
                        ("status".into(), "review".into()),
                    ],
                },
            )
            .unwrap();

        assert_eq!(
            runner.last_call().display(),
            "herdr pane report-metadata w4:p2 --source joop --title issue-42 · PR #7 \
             --token pull_request=7 --token status=review"
        );
    }

    #[test]
    fn reporting_nothing_touches_herdr_not_at_all() {
        let runner = RecordingRunner::new();

        HerdrAgents::new(&runner, "joop")
            .report_metadata("w4:p2", &PaneMetadata::default())
            .unwrap();

        assert!(runner.calls().is_empty());
    }

    #[test]
    fn more_tokens_than_herdr_accepts_is_refused_before_the_call() {
        let runner = RecordingRunner::new();
        let tokens = (0..17)
            .map(|index| (format!("token_{index}"), index.to_string()))
            .collect();

        let result = HerdrAgents::new(&runner, "joop").report_metadata(
            "w4:p2",
            &PaneMetadata {
                title: None,
                tokens,
            },
        );

        assert!(result.is_err());
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn tab_labels_come_from_joops_own_workspace() {
        let runner = RecordingRunner::returning([
            workspaces("joop"),
            CommandOutput::succeeded(
                r#"{"result":{"tabs":[{"tab_id":"w4:t1","label":"1"},{"tab_id":"w4:t2","label":"issue-42"}]}}"#,
            ),
        ]);

        let labels = HerdrAgents::new(&runner, "joop").tab_labels().unwrap();

        assert_eq!(labels, vec!["1", "issue-42"]);
        assert_eq!(
            runner.last_call().display(),
            "herdr tab list --workspace w4"
        );
    }

    #[test]
    fn no_workspace_yet_means_no_tabs_rather_than_an_error() {
        let runner = RecordingRunner::returning([workspaces("something-else")]);

        assert_eq!(
            HerdrAgents::new(&runner, "joop").tab_labels().unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_garbled_answer_names_the_command_that_produced_it() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded("not json")]);

        let error = HerdrAgents::new(&runner, "joop")
            .ensure_workspace()
            .expect_err("herdr must answer with JSON");

        assert!(
            format!("{error:#}").contains("herdr workspace list"),
            "got: {error:#}"
        );
    }
}
