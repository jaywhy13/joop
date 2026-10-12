use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::github::IssueSource;
use crate::issue::Issue;
use crate::limits::{LimitSnapshot, LimitWatcher};
use crate::prompt::{Briefing, build_prompt};
use crate::state::{Dispatch, HeartbeatPolicy, Session, State};
use crate::worktree::{WorktreeProvider, branch_for};
use crate::zellij::{TabHost, TabSpec};

/// How many recent events the dashboard keeps.
pub const MAX_EVENTS: usize = 200;

/// What joop was told to do, fixed for the life of the process.
#[derive(Debug, Clone)]
pub struct EngineSettings {
    pub repo: String,
    pub label: String,
    pub skill_command: String,
    pub api_url: String,
    /// The agent to run in each tab, e.g. `["pi"]`.
    pub agent_command: Vec<String>,
    pub policy: HeartbeatPolicy,
    /// Fraction of a Claude limit at which joop stops starting new work.
    pub limit_threshold: f64,
}

/// One line of history for the dashboard.
#[derive(Debug, Clone)]
pub struct Event {
    pub at: DateTime<Utc>,
    pub level: Level,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// Why joop has stopped starting new work.
#[derive(Debug, Clone)]
pub struct Pause {
    pub snapshot: LimitSnapshot,
    pub until: Option<DateTime<Utc>>,
}

/// Read-only view of joop for the dashboard to render.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub repo: String,
    pub label: String,
    pub sessions: Vec<Session>,
    pub events: Vec<Event>,
    pub last_poll_at: Option<DateTime<Utc>>,
    pub pause: Option<Pause>,
    pub limits: Option<LimitSnapshot>,
    pub policy: HeartbeatPolicy,
    pub now: DateTime<Utc>,
}

/// Drives the whole cycle: find labelled issues, give each one an agent in its own tab,
/// watch for agents that go quiet, and stand down when Claude capacity runs out.
pub struct Engine {
    settings: EngineSettings,
    issues: Arc<dyn IssueSource>,
    worktrees: Arc<dyn WorktreeProvider>,
    tabs: Arc<dyn TabHost>,
    limits: Option<Arc<dyn LimitWatcher>>,
    clock: Arc<dyn Clock>,
    state: Arc<Mutex<State>>,
    runtime: Mutex<Runtime>,
}

#[derive(Default)]
struct Runtime {
    events: VecDeque<Event>,
    last_poll_at: Option<DateTime<Utc>>,
    pause: Option<Pause>,
    limits: Option<LimitSnapshot>,
    session_ensured: bool,
}

impl Engine {
    pub fn new(
        settings: EngineSettings,
        issues: Arc<dyn IssueSource>,
        worktrees: Arc<dyn WorktreeProvider>,
        tabs: Arc<dyn TabHost>,
        limits: Option<Arc<dyn LimitWatcher>>,
        clock: Arc<dyn Clock>,
        state: Arc<Mutex<State>>,
    ) -> Self {
        Self {
            settings,
            issues,
            worktrees,
            tabs,
            limits,
            clock,
            state,
            runtime: Mutex::new(Runtime::default()),
        }
    }

    pub fn settings(&self) -> &EngineSettings {
        &self.settings
    }

    /// Look for labelled issues and give each unclaimed one an agent. Does nothing while
    /// joop is paused on Claude limits.
    pub fn poll(&self) -> Result<()> {
        let now = self.clock.now();
        self.runtime.lock().unwrap().last_poll_at = Some(now);

        if let Some(pause) = self.pause() {
            self.log(
                Level::Info,
                format!(
                    "paused on the Claude {} limit ({:.0}% used){}",
                    pause.snapshot.limit,
                    pause.snapshot.percent(),
                    pause
                        .until
                        .map(|until| format!(", resets at {}", until.format("%H:%M")))
                        .unwrap_or_default()
                ),
            );
            return Ok(());
        }

        let issues = self.issues.labelled(&self.settings.label)?;
        let unclaimed: Vec<Issue> = {
            let state = self.state.lock().unwrap();
            issues
                .into_iter()
                .filter(|issue| !state.is_claimed(&self.settings.repo, issue.number))
                .collect()
        };

        if unclaimed.is_empty() {
            self.log(
                Level::Info,
                format!("polled {}: nothing new to pick up", self.settings.repo),
            );
            return Ok(());
        }

        self.ensure_session()?;

        for issue in unclaimed {
            if let Err(error) = self.dispatch(&issue) {
                self.log(
                    Level::Error,
                    format!("could not dispatch #{}: {error:#}", issue.number),
                );
            }
        }

        // A dead agent may have been waiting for capacity; take care of it now.
        self.reassign_queued()?;
        Ok(())
    }

    /// Check every live session against the heartbeat policy, cleaning up and replacing
    /// agents that have gone quiet.
    pub fn audit(&self) -> Result<()> {
        let now = self.clock.now();
        let audit = {
            let mut state = self.state.lock().unwrap();
            state.audit(&self.settings.policy, now)?
        };

        for key in &audit.stale {
            self.log(
                Level::Warn,
                format!("{key} has missed a check-in; still waiting"),
            );
        }

        for key in &audit.dead {
            self.log(
                Level::Warn,
                format!("{key} went quiet; closing its tab and reassigning"),
            );
        }

        if !audit.dead.is_empty() {
            self.close_dead_tabs(&audit.dead);
        }

        self.reassign_queued()
    }

    /// Read Claude usage and pause or resume accordingly. Resuming nudges every running
    /// agent to carry on, because a paused agent will not restart by itself.
    pub fn check_limits(&self) -> Result<()> {
        let Some(watcher) = &self.limits else {
            return Ok(());
        };

        let snapshot = match watcher.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.log(
                    Level::Warn,
                    format!("could not read Claude usage, carrying on: {error:#}"),
                );
                return Err(error);
            }
        };

        let now = self.clock.now();
        let exhausted = snapshot.is_exhausted(self.settings.limit_threshold);
        let mut runtime = self.runtime.lock().unwrap();
        runtime.limits = Some(snapshot.clone());
        let was_paused = runtime.pause.is_some();

        if exhausted {
            if !was_paused {
                let until = snapshot.resets_at;
                runtime.pause = Some(Pause {
                    snapshot: snapshot.clone(),
                    until,
                });
                drop(runtime);
                self.log(
                    Level::Warn,
                    format!(
                        "Claude {} limit at {:.0}%: pausing new work{}",
                        snapshot.limit,
                        snapshot.percent(),
                        until
                            .map(|until| format!(" until {}", until.format("%Y-%m-%d %H:%M UTC")))
                            .unwrap_or_default()
                    ),
                );
            }
            return Ok(());
        }

        // Not exhausted. Lift the pause once the reset time has passed, or straight away
        // when usage has dropped back by itself.
        if was_paused {
            let still_waiting = runtime
                .pause
                .as_ref()
                .and_then(|pause| pause.until)
                .is_some_and(|until| now < until);

            if !still_waiting {
                runtime.pause = None;
                drop(runtime);
                self.log(
                    Level::Info,
                    format!(
                        "Claude capacity is back ({:.0}% used): resuming",
                        snapshot.percent()
                    ),
                );
                self.nudge_running_agents();
                self.reassign_queued()?;
            }
        }

        Ok(())
    }

    pub fn is_paused(&self) -> bool {
        self.runtime.lock().unwrap().pause.is_some()
    }

    pub fn paused_until(&self) -> Option<DateTime<Utc>> {
        self.runtime
            .lock()
            .unwrap()
            .pause
            .as_ref()
            .and_then(|pause| pause.until)
    }

    fn pause(&self) -> Option<Pause> {
        self.runtime.lock().unwrap().pause.clone()
    }

    pub fn snapshot(&self) -> Snapshot {
        let runtime = self.runtime.lock().unwrap();
        let state = self.state.lock().unwrap();

        Snapshot {
            repo: self.settings.repo.clone(),
            label: self.settings.label.clone(),
            sessions: state.sessions().values().cloned().collect(),
            events: runtime.events.iter().cloned().collect(),
            last_poll_at: runtime.last_poll_at,
            pause: runtime.pause.clone(),
            limits: runtime.limits.clone(),
            policy: self.settings.policy,
            now: self.clock.now(),
        }
    }

    pub fn log(&self, level: Level, message: impl Into<String>) {
        let message = message.into();
        match level {
            Level::Info => tracing::info!("{message}"),
            Level::Warn => tracing::warn!("{message}"),
            Level::Error => tracing::error!("{message}"),
        }

        let mut runtime = self.runtime.lock().unwrap();
        runtime.events.push_back(Event {
            at: self.clock.now(),
            level,
            message,
        });
        while runtime.events.len() > MAX_EVENTS {
            runtime.events.pop_front();
        }
    }

    fn ensure_session(&self) -> Result<()> {
        let already = self.runtime.lock().unwrap().session_ensured;
        if already {
            return Ok(());
        }
        self.tabs.ensure_session()?;
        self.runtime.lock().unwrap().session_ensured = true;
        Ok(())
    }

    /// Give one issue to a fresh agent.
    fn dispatch(&self, issue: &Issue) -> Result<()> {
        let slug = issue.slug();
        let branch = branch_for(&slug);
        let worktree = self.worktrees.prepare(&slug, &branch)?;
        let tab_id = self.open_tab(issue, &slug, &worktree, 1)?;

        self.state.lock().unwrap().record_dispatch(Dispatch {
            repo: self.settings.repo.clone(),
            issue_number: issue.number,
            issue_title: issue.title.clone(),
            issue_url: issue.url.clone(),
            tab_name: slug.clone(),
            tab_id,
            branch,
            worktree,
            at: self.clock.now(),
            attempt: 1,
        })?;

        self.log(
            Level::Info,
            format!(
                "#{} \"{}\" handed to a new agent in tab {slug}",
                issue.number, issue.title
            ),
        );
        Ok(())
    }

    fn open_tab(
        &self,
        issue: &Issue,
        slug: &str,
        worktree: &std::path::Path,
        attempt: u32,
    ) -> Result<Option<String>> {
        let prompt = build_prompt(&Briefing {
            issue,
            repo: &self.settings.repo,
            skill_command: &self.settings.skill_command,
            api_url: &self.settings.api_url,
            first_update_minutes: self.settings.policy.first_update.num_minutes(),
            update_interval_minutes: self.settings.policy.subsequent_updates.num_minutes(),
            dead_after_minutes: self.settings.policy.dead_after.num_minutes(),
            attempt,
        });

        let mut command = self.settings.agent_command.clone();
        command.push(prompt);

        self.tabs.open(&TabSpec {
            name: slug.to_string(),
            cwd: worktree.to_path_buf(),
            command,
        })
    }

    fn close_dead_tabs(&self, dead: &[String]) {
        let tab_ids: Vec<(String, Option<String>)> = {
            let state = self.state.lock().unwrap();
            dead.iter()
                .filter_map(|key| state.sessions().get(key))
                .map(|session| (session.tab_name.clone(), session.tab_id.clone()))
                .collect()
        };

        for (tab_name, tab_id) in tab_ids {
            let Some(tab_id) = tab_id else {
                self.log(
                    Level::Warn,
                    format!("no Zellij tab id recorded for {tab_name}; leaving it alone"),
                );
                continue;
            };
            if let Err(error) = self.tabs.close(&tab_id) {
                self.log(
                    Level::Warn,
                    format!("could not close tab {tab_name}: {error:#}"),
                );
            }
        }
    }

    /// Hand every dead session's issue to a replacement agent, as long as there is capacity.
    fn reassign_queued(&self) -> Result<()> {
        if self.is_paused() {
            return Ok(());
        }

        let queued = self.state.lock().unwrap().awaiting_reassignment();
        if queued.is_empty() {
            return Ok(());
        }

        self.ensure_session()?;

        for session in queued {
            if let Err(error) = self.reassign(&session) {
                self.log(
                    Level::Error,
                    format!("could not reassign #{}: {error:#}", session.issue_number),
                );
            }
        }

        Ok(())
    }

    fn reassign(&self, session: &Session) -> Result<()> {
        let issue = Issue {
            number: session.issue_number,
            title: session.issue_title.clone(),
            url: session.issue_url.clone(),
            body: String::new(),
            labels: vec![self.settings.label.clone()],
        };
        let slug = issue.slug();

        // Reuse the branch and worktree: they may hold the dead agent's partial work.
        let worktree = self.worktrees.prepare(&slug, &session.branch)?;
        let attempt = session.attempts + 1;
        let tab_id = self.open_tab(&issue, &slug, &worktree, attempt)?;

        let attempts = self.state.lock().unwrap().record_reassignment(
            &self.settings.repo,
            session.issue_number,
            slug.clone(),
            tab_id,
            self.clock.now(),
        )?;

        self.log(
            Level::Info,
            format!(
                "#{} reassigned to a new agent (attempt {attempts})",
                session.issue_number
            ),
        );
        Ok(())
    }

    /// Tell every still-running agent to carry on, after a limit pause.
    fn nudge_running_agents(&self) {
        let running: Vec<(String, u64)> = {
            let state = self.state.lock().unwrap();
            state
                .sessions()
                .values()
                .filter(|session| session.status.is_live())
                .map(|session| (session.tab_name.clone(), session.issue_number))
                .collect()
        };

        for (tab_name, issue_number) in running {
            let message = format!(
                "Claude usage limits have reset and joop is running again. Please continue working \
                 on issue #{issue_number} from where you stopped, and report your status with \
                 `joop report --issue {issue_number} --status working --detail \"<what you are doing>\"`."
            );

            match self.tabs.send_line(&tab_name, &message) {
                Ok(()) => self.log(Level::Info, format!("nudged {tab_name} to continue")),
                Err(error) => self.log(
                    Level::Warn,
                    format!("could not nudge {tab_name}: {error:#}"),
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::issue::Issue;
    use crate::limits::{LimitSnapshot, LimitWatcher};
    use crate::state::{HeartbeatPolicy, State, Status};
    use crate::worktree::WorktreeProvider;
    use crate::zellij::{TabHost, TabSpec};
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn at(minutes: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap() + Duration::minutes(minutes)
    }

    fn issue(number: u64) -> Issue {
        Issue {
            number,
            title: format!("Issue {number}"),
            url: format!("https://github.com/jaywhy13/joop/issues/{number}"),
            body: "Do the thing.".into(),
            labels: vec!["loop".into()],
        }
    }

    #[derive(Default)]
    struct FakeIssues(Mutex<Vec<Issue>>);

    impl FakeIssues {
        fn with(issues: Vec<Issue>) -> Self {
            Self(Mutex::new(issues))
        }
        fn set(&self, issues: Vec<Issue>) {
            *self.0.lock().unwrap() = issues;
        }
    }

    impl IssueSource for FakeIssues {
        fn labelled(&self, _label: &str) -> Result<Vec<Issue>> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    struct FailingIssues;

    impl IssueSource for FailingIssues {
        fn labelled(&self, _label: &str) -> Result<Vec<Issue>> {
            anyhow::bail!("gh exploded")
        }
    }

    #[derive(Default)]
    struct FakeWorktrees {
        prepared: Mutex<Vec<String>>,
    }

    impl WorktreeProvider for FakeWorktrees {
        fn prepare(&self, slug: &str, _branch: &str) -> Result<PathBuf> {
            self.prepared.lock().unwrap().push(slug.to_string());
            Ok(self.path_for(slug))
        }
        fn path_for(&self, slug: &str) -> PathBuf {
            PathBuf::from("/worktrees").join(slug)
        }
    }

    #[derive(Debug, Default)]
    struct FakeTabs {
        opened: Mutex<Vec<TabSpec>>,
        closed: Mutex<Vec<String>>,
        lines: Mutex<Vec<(String, String)>>,
        sessions_ensured: Mutex<u32>,
        next_id: Mutex<u32>,
    }

    impl FakeTabs {
        fn opened_names(&self) -> Vec<String> {
            self.opened
                .lock()
                .unwrap()
                .iter()
                .map(|spec| spec.name.clone())
                .collect()
        }
        fn last_prompt(&self) -> String {
            self.opened
                .lock()
                .unwrap()
                .last()
                .expect("a tab was opened")
                .command
                .last()
                .expect("a prompt was passed")
                .clone()
        }
    }

    impl TabHost for FakeTabs {
        fn ensure_session(&self) -> Result<()> {
            *self.sessions_ensured.lock().unwrap() += 1;
            Ok(())
        }
        fn open(&self, spec: &TabSpec) -> Result<Option<String>> {
            self.opened.lock().unwrap().push(spec.clone());
            let mut next = self.next_id.lock().unwrap();
            *next += 1;
            Ok(Some(next.to_string()))
        }
        fn close(&self, tab_id: &str) -> Result<()> {
            self.closed.lock().unwrap().push(tab_id.to_string());
            Ok(())
        }
        fn focus(&self, _tab_name: &str) -> Result<()> {
            Ok(())
        }
        fn send_line(&self, tab_name: &str, line: &str) -> Result<()> {
            self.lines
                .lock()
                .unwrap()
                .push((tab_name.to_string(), line.to_string()));
            Ok(())
        }
        fn tab_names(&self) -> Result<Vec<String>> {
            Ok(self.opened_names())
        }
    }

    struct FakeLimits(Mutex<Result<LimitSnapshot, String>>);

    impl FakeLimits {
        fn at(utilization: f64, resets_at: Option<DateTime<Utc>>) -> Self {
            Self(Mutex::new(Ok(LimitSnapshot {
                utilization,
                limit: "session".into(),
                resets_at,
            })))
        }
        fn broken() -> Self {
            Self(Mutex::new(Err("endpoint moved".into())))
        }
        fn set(&self, utilization: f64, resets_at: Option<DateTime<Utc>>) {
            *self.0.lock().unwrap() = Ok(LimitSnapshot {
                utilization,
                limit: "session".into(),
                resets_at,
            });
        }
    }

    impl LimitWatcher for FakeLimits {
        fn snapshot(&self) -> Result<LimitSnapshot> {
            self.0
                .lock()
                .unwrap()
                .clone()
                .map_err(|error| anyhow::anyhow!(error))
        }
    }

    struct Harness {
        engine: Engine,
        issues: Arc<FakeIssues>,
        tabs: Arc<FakeTabs>,
        limits: Arc<FakeLimits>,
        clock: Arc<FixedClock>,
        state: Arc<Mutex<State>>,
        _dir: tempfile::TempDir,
    }

    fn harness(issues: Vec<Issue>) -> Harness {
        build_harness(
            Arc::new(FakeIssues::with(issues)),
            Arc::new(FakeLimits::at(0.1, None)),
        )
    }

    fn build_harness(issues: Arc<FakeIssues>, limits: Arc<FakeLimits>) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(
            State::load(&dir.path().join("state.json")).unwrap(),
        ));
        let clock = Arc::new(FixedClock::new(at(0)));
        let tabs = Arc::new(FakeTabs::default());

        let engine = Engine::new(
            EngineSettings {
                repo: "jaywhy13/joop".into(),
                label: "loop".into(),
                skill_command: "/implement".into(),
                api_url: "http://127.0.0.1:7777".into(),
                agent_command: vec!["pi".into()],
                policy: HeartbeatPolicy::default(),
                limit_threshold: 0.9,
            },
            issues.clone(),
            Arc::new(FakeWorktrees::default()),
            tabs.clone(),
            Some(limits.clone()),
            clock.clone(),
            state.clone(),
        );

        Harness {
            engine,
            issues,
            tabs,
            limits,
            clock,
            state,
            _dir: dir,
        }
    }

    #[test]
    fn dispatches_a_labelled_issue_into_its_own_tab() {
        let harness = harness(vec![issue(42)]);

        harness.engine.poll().unwrap();

        assert_eq!(harness.tabs.opened_names(), vec!["issue-42"]);
        let spec = harness.tabs.opened.lock().unwrap()[0].clone();
        assert_eq!(spec.cwd, PathBuf::from("/worktrees/issue-42"));
        assert_eq!(spec.command[0], "pi");
        assert!(harness.tabs.last_prompt().starts_with("/implement "));
    }

    #[test]
    fn makes_sure_the_zellij_session_exists_before_opening_tabs() {
        let harness = harness(vec![issue(42)]);

        harness.engine.poll().unwrap();

        assert_eq!(*harness.tabs.sessions_ensured.lock().unwrap(), 1);
    }

    #[test]
    fn records_the_dispatch_with_its_tab_branch_and_worktree() {
        let harness = harness(vec![issue(42)]);

        harness.engine.poll().unwrap();

        let state = harness.state.lock().unwrap();
        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(session.status, Status::Dispatched);
        assert_eq!(session.tab_name, "issue-42");
        assert_eq!(session.tab_id.as_deref(), Some("1"));
        assert_eq!(session.branch, "joop/issue-42");
        assert_eq!(session.worktree, PathBuf::from("/worktrees/issue-42"));
        assert_eq!(session.attempts, 1);
    }

    #[test]
    fn does_not_open_a_second_tab_for_an_issue_already_being_worked() {
        let harness = harness(vec![issue(42)]);

        harness.engine.poll().unwrap();
        harness.engine.poll().unwrap();

        assert_eq!(harness.tabs.opened_names(), vec!["issue-42"]);
    }

    #[test]
    fn dispatches_each_labelled_issue_once() {
        let harness = harness(vec![issue(42), issue(7)]);

        harness.engine.poll().unwrap();

        let mut names = harness.tabs.opened_names();
        names.sort();
        assert_eq!(names, vec!["issue-42", "issue-7"]);
    }

    #[test]
    fn a_finished_issue_that_is_relabelled_is_picked_up_again() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness
            .state
            .lock()
            .unwrap()
            .record_report("jaywhy13/joop", 42, Status::Done, None, at(5))
            .unwrap();

        harness.engine.poll().unwrap();

        assert_eq!(harness.tabs.opened_names().len(), 2);
    }

    #[test]
    fn a_failing_issue_source_is_reported_without_crashing_the_loop() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(
            State::load(&dir.path().join("state.json")).unwrap(),
        ));
        let engine = Engine::new(
            EngineSettings {
                repo: "jaywhy13/joop".into(),
                label: "loop".into(),
                skill_command: "/implement".into(),
                api_url: "http://127.0.0.1:7777".into(),
                agent_command: vec!["pi".into()],
                policy: HeartbeatPolicy::default(),
                limit_threshold: 0.9,
            },
            Arc::new(FailingIssues),
            Arc::new(FakeWorktrees::default()),
            Arc::new(FakeTabs::default()),
            None,
            Arc::new(FixedClock::new(at(0))),
            state,
        );

        assert!(engine.poll().is_err());
    }

    #[test]
    fn a_silent_agent_has_its_tab_closed_and_the_issue_handed_to_a_replacement() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness.issues.set(vec![]);

        harness.clock.advance(Duration::minutes(31));
        harness.engine.audit().unwrap();

        assert_eq!(
            *harness.tabs.closed.lock().unwrap(),
            vec!["1"],
            "the dead agent's tab must be cleaned up"
        );
        assert_eq!(
            harness.tabs.opened_names(),
            vec!["issue-42", "issue-42"],
            "the issue must be given to another agent"
        );
        let state = harness.state.lock().unwrap();
        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(session.attempts, 2);
        assert_eq!(session.status, Status::Dispatched);
        assert_eq!(session.tab_id.as_deref(), Some("2"));
    }

    #[test]
    fn the_replacement_agent_is_told_it_is_a_second_attempt() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness.clock.advance(Duration::minutes(31));

        harness.engine.audit().unwrap();

        assert!(
            harness.tabs.last_prompt().contains("attempt 2"),
            "got: {}",
            harness.tabs.last_prompt()
        );
    }

    #[test]
    fn an_agent_that_is_merely_late_is_flagged_but_left_running() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();

        harness.clock.advance(Duration::minutes(3));
        harness.engine.audit().unwrap();

        assert!(harness.tabs.closed.lock().unwrap().is_empty());
        assert_eq!(harness.tabs.opened_names().len(), 1);
        assert_eq!(
            harness
                .state
                .lock()
                .unwrap()
                .session("jaywhy13/joop", 42)
                .unwrap()
                .status,
            Status::Stale
        );
    }

    #[test]
    fn an_agent_that_keeps_reporting_is_never_disturbed() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();

        for minute in [10, 25, 40] {
            harness
                .clock
                .advance(Duration::minutes(if minute == 10 { 10 } else { 15 }));
            harness
                .state
                .lock()
                .unwrap()
                .record_report(
                    "jaywhy13/joop",
                    42,
                    Status::Working,
                    Some("still going"),
                    harness.clock.now(),
                )
                .unwrap();
            harness.engine.audit().unwrap();
        }

        assert!(harness.tabs.closed.lock().unwrap().is_empty());
        assert_eq!(harness.tabs.opened_names().len(), 1);
    }

    #[test]
    fn polling_stops_once_the_claude_limit_is_reached() {
        let harness = harness(vec![issue(42)]);
        harness.limits.set(0.95, Some(at(60)));

        harness.engine.check_limits().unwrap();
        harness.engine.poll().unwrap();

        assert!(
            harness.tabs.opened_names().is_empty(),
            "joop must not start new work while out of capacity"
        );
        assert!(harness.engine.is_paused());
    }

    #[test]
    fn the_pause_reports_when_capacity_returns() {
        let harness = harness(vec![issue(42)]);
        harness.limits.set(0.95, Some(at(60)));

        harness.engine.check_limits().unwrap();

        assert_eq!(harness.engine.paused_until(), Some(at(60)));
    }

    #[test]
    fn polling_resumes_once_the_limit_resets() {
        let harness = harness(vec![issue(42)]);
        harness.limits.set(0.95, Some(at(60)));
        harness.engine.check_limits().unwrap();

        harness.clock.advance(Duration::minutes(61));
        harness.limits.set(0.05, None);
        harness.engine.check_limits().unwrap();
        harness.engine.poll().unwrap();

        assert!(!harness.engine.is_paused());
        assert_eq!(harness.tabs.opened_names(), vec!["issue-42"]);
    }

    #[test]
    fn resuming_tells_every_running_agent_to_carry_on() {
        let harness = harness(vec![issue(42), issue(7)]);
        harness.engine.poll().unwrap();
        harness.limits.set(0.95, Some(at(60)));
        harness.engine.check_limits().unwrap();

        harness.clock.advance(Duration::minutes(61));
        harness.limits.set(0.05, None);
        harness.engine.check_limits().unwrap();

        let lines = harness.tabs.lines.lock().unwrap().clone();
        let mut tabs: Vec<String> = lines.iter().map(|(tab, _)| tab.clone()).collect();
        tabs.sort();
        assert_eq!(tabs, vec!["issue-42", "issue-7"]);
        assert!(
            lines[0].1.to_lowercase().contains("continue"),
            "got: {}",
            lines[0].1
        );
    }

    #[test]
    fn finished_agents_are_not_told_to_carry_on() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness
            .state
            .lock()
            .unwrap()
            .record_report("jaywhy13/joop", 42, Status::Done, None, at(1))
            .unwrap();
        harness.limits.set(0.95, Some(at(60)));
        harness.engine.check_limits().unwrap();

        harness.clock.advance(Duration::minutes(61));
        harness.limits.set(0.05, None);
        harness.engine.check_limits().unwrap();

        assert!(harness.tabs.lines.lock().unwrap().is_empty());
    }

    #[test]
    fn a_limit_with_no_reset_time_pauses_until_the_next_check_clears_it() {
        let harness = harness(vec![issue(42)]);
        harness.limits.set(0.95, None);
        harness.engine.check_limits().unwrap();
        assert!(harness.engine.is_paused());

        harness.limits.set(0.10, None);
        harness.engine.check_limits().unwrap();

        assert!(!harness.engine.is_paused());
    }

    #[test]
    fn an_unreadable_usage_endpoint_does_not_stop_the_work() {
        let harness = build_harness(
            Arc::new(FakeIssues::with(vec![issue(42)])),
            Arc::new(FakeLimits::broken()),
        );

        let result = harness.engine.check_limits();
        harness.engine.poll().unwrap();

        assert!(result.is_err(), "the failure is surfaced");
        assert!(!harness.engine.is_paused(), "but joop keeps working");
        assert_eq!(harness.tabs.opened_names(), vec!["issue-42"]);
    }

    #[test]
    fn audits_still_run_while_paused_so_dead_agents_are_cleaned_up() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness.limits.set(0.95, Some(at(600)));
        harness.engine.check_limits().unwrap();

        harness.clock.advance(Duration::minutes(31));
        harness.engine.audit().unwrap();

        assert_eq!(*harness.tabs.closed.lock().unwrap(), vec!["1"]);
        assert_eq!(
            harness.tabs.opened_names().len(),
            1,
            "a replacement must wait until there is capacity again"
        );
        assert!(
            harness
                .state
                .lock()
                .unwrap()
                .session("jaywhy13/joop", 42)
                .unwrap()
                .needs_reassignment(),
            "the issue stays queued for reassignment"
        );
    }

    #[test]
    fn a_queued_reassignment_happens_as_soon_as_capacity_returns() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness.limits.set(0.95, Some(at(60)));
        harness.engine.check_limits().unwrap();
        harness.clock.advance(Duration::minutes(31));
        harness.engine.audit().unwrap();

        harness.clock.advance(Duration::minutes(31));
        harness.limits.set(0.05, None);
        harness.engine.check_limits().unwrap();
        harness.engine.audit().unwrap();

        assert_eq!(harness.tabs.opened_names().len(), 2);
        assert_eq!(
            harness
                .state
                .lock()
                .unwrap()
                .session("jaywhy13/joop", 42)
                .unwrap()
                .attempts,
            2
        );
    }

    #[test]
    fn a_dead_agent_without_a_known_tab_id_is_still_replaced() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        {
            let mut state = harness.state.lock().unwrap();
            let path = state.path().to_path_buf();
            drop(state);
            let mut raw: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            raw["sessions"]["jaywhy13/joop#42"]["tab_id"] = serde_json::Value::Null;
            std::fs::write(&path, serde_json::to_string(&raw).unwrap()).unwrap();
            state = harness.state.lock().unwrap();
            *state = State::load(&path).unwrap();
        }

        harness.clock.advance(Duration::minutes(31));
        harness.engine.audit().unwrap();

        assert!(harness.tabs.closed.lock().unwrap().is_empty());
        assert_eq!(harness.tabs.opened_names().len(), 2);
    }

    #[test]
    fn the_engine_reports_what_the_dashboard_needs_to_show() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();

        let snapshot = harness.engine.snapshot();

        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].issue_number, 42);
        assert_eq!(snapshot.repo, "jaywhy13/joop");
        assert!(snapshot.last_poll_at.is_some());
        assert!(
            !snapshot.events.is_empty(),
            "the dashboard shows recent activity"
        );
    }

    #[test]
    fn the_event_log_is_bounded() {
        let harness = harness(vec![]);

        for _ in 0..(MAX_EVENTS + 20) {
            harness.engine.poll().unwrap();
        }

        assert!(harness.engine.snapshot().events.len() <= MAX_EVENTS);
    }
}
