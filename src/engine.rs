use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::github::IssueSource;
use crate::herdr::{AgentHost, AgentSpec, PaneMetadata};
use crate::issue::Issue;
use crate::limits::{LimitSnapshot, LimitWatcher};
use crate::prompt::{Briefing, build_prompt};
use crate::state::{Dispatch, HeartbeatPolicy, PullRequest, Session, State, Status};
use crate::worktree::{WorktreeProvider, branch_for};

/// How many recent events the dashboard keeps.
pub const MAX_EVENTS: usize = 200;

/// What joop was told to do, fixed for the life of the process.
#[derive(Debug, Clone)]
pub struct EngineSettings {
    pub repo: String,
    pub label: String,
    pub skill_command: String,
    pub api_url: String,
    /// Agent kind herdr should start, e.g. `claude`.
    pub agent_kind: String,
    /// Arguments passed to the agent binary itself, e.g. `--dangerously-skip-permissions`.
    pub agent_args: Vec<String>,
    pub policy: HeartbeatPolicy,
    /// Fraction of a Claude limit at which joop stops starting new work.
    pub limit_threshold: f64,
    /// How many agents may be given one issue before joop gives up on it.
    pub max_attempts: u32,
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

/// A labelled issue joop has seen but not given to an agent, because it was paused or
/// the dispatch failed. Shown on the dashboard so nothing is silently dropped.
#[derive(Debug, Clone)]
pub struct PendingIssue {
    pub number: u64,
    pub title: String,
}

/// Read-only view of joop for the dashboard to render.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub repo: String,
    pub label: String,
    pub sessions: Vec<Session>,
    /// Labelled issues waiting for an agent.
    pub pending: Vec<PendingIssue>,
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
    agents: Arc<dyn AgentHost>,
    limits: Option<Arc<dyn LimitWatcher>>,
    clock: Arc<dyn Clock>,
    state: Arc<Mutex<State>>,
    observed: Mutex<Observed>,
    /// The event ring has its own lock so that `log` can be called from anywhere,
    /// including while `observed` is held.
    events: Mutex<VecDeque<Event>>,
}

impl crate::api::Reporter for Engine {
    fn record_report(
        &self,
        issue_number: u64,
        status: Status,
        detail: Option<&str>,
        pull_request: Option<PullRequest>,
    ) -> Result<()> {
        Engine::record_report(self, issue_number, status, detail, pull_request)
    }

    fn sessions(&self) -> Vec<Session> {
        Engine::sessions(self)
    }
}

/// What joop has noticed, as opposed to what it has recorded in `state.json`.
#[derive(Default)]
struct Observed {
    last_poll_at: Option<DateTime<Utc>>,
    pause: Option<Pause>,
    limits: Option<LimitSnapshot>,
    pending: Vec<PendingIssue>,
}

/// Take a lock, recovering from a panic in another thread rather than poisoning the
/// whole supervisor. joop would rather carry on with the state it has.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Engine {
    pub fn new(
        settings: EngineSettings,
        issues: Arc<dyn IssueSource>,
        worktrees: Arc<dyn WorktreeProvider>,
        agents: Arc<dyn AgentHost>,
        limits: Option<Arc<dyn LimitWatcher>>,
        clock: Arc<dyn Clock>,
        state: Arc<Mutex<State>>,
    ) -> Self {
        Self {
            settings,
            issues,
            worktrees,
            agents,
            limits,
            clock,
            state,
            observed: Mutex::new(Observed::default()),
            events: Mutex::new(VecDeque::new()),
        }
    }

    pub fn settings(&self) -> &EngineSettings {
        &self.settings
    }

    /// Look for labelled issues and give each one joop has not seen an agent. Does
    /// nothing while joop is paused on Claude limits.
    pub fn poll(&self) -> Result<()> {
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
        // Only stamped once GitHub has actually answered, so the dashboard never shows
        // a fresh poll that did not happen.
        lock(&self.observed).last_poll_at = Some(self.clock.now());

        let unclaimed: Vec<Issue> = {
            let state = lock(&self.state);
            issues
                .into_iter()
                .filter(|issue| !state.is_claimed(&self.settings.repo, issue.number))
                .collect()
        };

        if unclaimed.is_empty() {
            lock(&self.observed).pending.clear();
            self.log(
                Level::Info,
                format!("polled {}: nothing new to pick up", self.settings.repo),
            );
            return Ok(());
        }

        self.remember_pending(&unclaimed);

        for issue in &unclaimed {
            match self.dispatch(issue) {
                Ok(()) => self.forget_pending(issue.number),
                Err(error) => self.log(
                    Level::Error,
                    format!("could not dispatch #{}: {error:#}", issue.number),
                ),
            }
        }

        // A dead agent may have been waiting for capacity; take care of it now.
        self.reassign_queued()
    }

    /// Check every live session against the heartbeat policy, cleaning up and replacing
    /// agents that have gone quiet.
    ///
    /// While joop is paused on Claude limits the heartbeat clock does not run: the agents
    /// are out of capacity too, so declaring them dead would close every tab there is and
    /// leave nothing to resume.
    pub fn audit(&self) -> Result<()> {
        if self.is_paused() {
            return Ok(());
        }

        self.reconcile_tabs();

        let now = self.clock.now();
        let audit = lock(&self.state).audit(&self.settings.policy, now)?;

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

        let was_paused = {
            let mut observed = lock(&self.observed);
            observed.limits = Some(snapshot.clone());
            observed.pause.is_some()
        };

        if exhausted {
            if !was_paused {
                let until = snapshot.resets_at;
                lock(&self.observed).pause = Some(Pause {
                    snapshot: snapshot.clone(),
                    until,
                });
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

        if !was_paused {
            return Ok(());
        }

        // Lift the pause once the reset time has passed, or straight away when usage has
        // dropped back by itself.
        let still_waiting = lock(&self.observed)
            .pause
            .as_ref()
            .and_then(|pause| pause.until)
            .is_some_and(|until| now < until);

        if still_waiting {
            return Ok(());
        }

        lock(&self.observed).pause = None;
        self.log(
            Level::Info,
            format!(
                "Claude capacity is back ({:.0}% used): resuming",
                snapshot.percent()
            ),
        );

        self.nudge_running_agents(now)?;
        self.reassign_queued()
    }

    pub fn is_paused(&self) -> bool {
        lock(&self.observed).pause.is_some()
    }

    pub fn paused_until(&self) -> Option<DateTime<Utc>> {
        lock(&self.observed)
            .pause
            .as_ref()
            .and_then(|pause| pause.until)
    }

    fn pause(&self) -> Option<Pause> {
        lock(&self.observed).pause.clone()
    }

    /// Apply an agent's report, then act on what it said: a pull request is pushed onto the
    /// agent's herdr pane, and a merged one means the work has landed, so its tab closes.
    pub fn record_report(
        &self,
        issue_number: u64,
        status: Status,
        detail: Option<&str>,
        pull_request: Option<PullRequest>,
    ) -> Result<()> {
        lock(&self.state).record_report(
            &self.settings.repo,
            issue_number,
            status,
            detail,
            pull_request,
            self.clock.now(),
        )?;

        let Some(session) = lock(&self.state)
            .session(&self.settings.repo, issue_number)
            .cloned()
        else {
            return Ok(());
        };

        self.describe_agent_to_herdr(
            &session.pane_id,
            issue_number,
            session.status,
            session.pull_request.as_ref(),
        );

        if let (Status::Review, Some(pull_request)) = (status, &session.pull_request) {
            self.log(
                Level::Info,
                format!("#{issue_number} opened PR #{}", pull_request.number),
            );
        }

        if status == Status::Merged {
            let landed = session
                .pull_request
                .as_ref()
                .map(|pull_request| format!("PR #{}", pull_request.number))
                .unwrap_or_else(|| "its work".to_string());
            self.log(
                Level::Info,
                format!("#{issue_number}: {landed} merged; closing its herdr tab"),
            );
            self.abandon_tab(&session.tab_id);
        }

        Ok(())
    }

    /// Every session joop knows about, for the reporting API to serve.
    pub fn sessions(&self) -> Vec<Session> {
        lock(&self.state).sessions().values().cloned().collect()
    }

    /// Forget joop's record of an issue so that a later poll picks it up again.
    pub fn forget(&self, issue_number: u64) -> Result<bool> {
        let forgotten = lock(&self.state).forget(&self.settings.repo, issue_number)?;
        if forgotten {
            self.log(
                Level::Info,
                format!("forgot #{issue_number}; the next poll will pick it up again"),
            );
        }
        Ok(forgotten)
    }

    pub fn snapshot(&self) -> Snapshot {
        let observed = lock(&self.observed);
        let sessions = lock(&self.state).sessions().values().cloned().collect();
        let events = lock(&self.events).iter().cloned().collect();

        Snapshot {
            repo: self.settings.repo.clone(),
            label: self.settings.label.clone(),
            sessions,
            pending: observed.pending.clone(),
            events,
            last_poll_at: observed.last_poll_at,
            pause: observed.pause.clone(),
            limits: observed.limits.clone(),
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

        let mut events = lock(&self.events);
        events.push_back(Event {
            at: self.clock.now(),
            level,
            message,
        });
        while events.len() > MAX_EVENTS {
            events.pop_front();
        }
    }

    fn remember_pending(&self, issues: &[Issue]) {
        lock(&self.observed).pending = issues
            .iter()
            .map(|issue| PendingIssue {
                number: issue.number,
                title: issue.title.clone(),
            })
            .collect();
    }

    fn forget_pending(&self, issue_number: u64) {
        lock(&self.observed)
            .pending
            .retain(|pending| pending.number != issue_number);
    }

    /// Give one issue to a fresh agent.
    fn dispatch(&self, issue: &Issue) -> Result<()> {
        let slug = issue.slug();
        let branch = branch_for(&slug);
        let worktree = self.worktrees.prepare(&slug, &branch)?;
        let agent = self.start_agent(issue, &slug, &worktree, 1)?;

        let record = lock(&self.state).record_dispatch(Dispatch {
            repo: self.settings.repo.clone(),
            issue_number: issue.number,
            issue_title: issue.title.clone(),
            issue_url: issue.url.clone(),
            issue_body: issue.body.clone(),
            agent_name: agent.name.clone(),
            tab_id: agent.tab_id.clone(),
            pane_id: agent.pane_id.clone(),
            branch,
            worktree,
            at: self.clock.now(),
            attempt: 1,
        });

        // An agent joop failed to record is an agent it cannot supervise, and the next
        // poll would start a second one in the same worktree. Shut it down.
        if let Err(error) = record {
            self.abandon_tab(&agent.tab_id);
            return Err(error);
        }

        self.log(
            Level::Info,
            format!(
                "#{} \"{}\" handed to a new agent in tab {slug}",
                issue.number, issue.title
            ),
        );
        Ok(())
    }

    /// Open a tab, start the agent in it, and hand it its brief. herdr starts the agent
    /// and the prompt goes in separately, so a tab that comes up without an agent never
    /// silently swallows the instructions.
    fn start_agent(
        &self,
        issue: &Issue,
        slug: &str,
        worktree: &std::path::Path,
        attempt: u32,
    ) -> Result<crate::herdr::RunningAgent> {
        let agent = self.agents.start(&AgentSpec {
            name: slug.to_string(),
            cwd: worktree.to_path_buf(),
            kind: self.settings.agent_kind.clone(),
            agent_args: self.settings.agent_args.clone(),
            env: vec![("JOOP_API_URL".into(), self.settings.api_url.clone())],
        })?;

        let prompt = build_prompt(&Briefing {
            issue,
            repo: &self.settings.repo,
            skill_command: &self.settings.skill_command,
            api_url: &self.settings.api_url,
            policy: &self.settings.policy,
            attempt,
        });

        if let Err(error) = self.agents.prompt(&agent.name, &prompt) {
            self.abandon_tab(&agent.tab_id);
            return Err(error);
        }

        self.describe_agent_to_herdr(&agent.pane_id, issue.number, Status::Dispatched, None);
        Ok(agent)
    }

    /// Push what joop knows onto the agent's herdr pane, so the issue and its pull request
    /// show up under the agent in herdr's own UI rather than only on joop's dashboard.
    fn describe_agent_to_herdr(
        &self,
        pane_id: &str,
        issue_number: u64,
        status: Status,
        pull_request: Option<&PullRequest>,
    ) {
        let title = match pull_request {
            Some(pull_request) => format!("#{issue_number} · PR #{}", pull_request.number),
            None => format!("#{issue_number}"),
        };

        let mut tokens = vec![
            ("issue".to_string(), issue_number.to_string()),
            ("status".to_string(), status.as_str().to_string()),
        ];
        if let Some(pull_request) = pull_request {
            tokens.push(("pull_request".to_string(), pull_request.number.to_string()));
            tokens.push(("pr_url".to_string(), pull_request.url.clone()));
        }

        let metadata = PaneMetadata {
            title: Some(title),
            tokens,
        };

        if let Err(error) = self.agents.report_metadata(pane_id, &metadata) {
            self.log(
                Level::Warn,
                format!("could not show #{issue_number} on its herdr pane: {error:#}"),
            );
        }
    }

    /// Close a tab joop has decided it no longer owns. Returns whether it is gone.
    fn abandon_tab(&self, tab_id: &str) -> bool {
        match self.agents.close_tab(tab_id) {
            Ok(()) => true,
            Err(error) => {
                self.log(
                    Level::Warn,
                    format!("could not close herdr tab {tab_id}: {error:#}"),
                );
                false
            }
        }
    }

    /// Declare dead any session whose herdr tab has gone, rather than waiting out its
    /// heartbeat. A tab the user closed by hand is an agent that is not coming back.
    fn reconcile_tabs(&self) {
        let open = match self.agents.tab_labels() {
            Ok(names) => names,
            // Not knowing is not evidence of absence; the heartbeat still covers this.
            Err(_) => return,
        };

        let live: Vec<(u64, String)> = {
            let state = lock(&self.state);
            state
                .sessions()
                .values()
                .filter(|session| session.status.is_live())
                .map(|session| (session.issue_number, session.agent_name.clone()))
                .collect()
        };

        for (issue_number, agent_name) in live {
            if open.iter().any(|label| label == &agent_name) {
                continue;
            }

            let marked = lock(&self.state).mark_dead(
                &self.settings.repo,
                issue_number,
                "its herdr tab is gone",
            );

            match marked {
                Ok(true) => self.log(
                    Level::Warn,
                    format!("tab {agent_name} has gone; reassigning #{issue_number}"),
                ),
                Ok(false) => {}
                Err(error) => self.log(
                    Level::Error,
                    format!("could not mark #{issue_number} dead: {error:#}"),
                ),
            }
        }
    }

    /// Hand every dead session's issue to a replacement agent, as long as there is capacity.
    fn reassign_queued(&self) -> Result<()> {
        if self.is_paused() {
            return Ok(());
        }

        let queued = lock(&self.state).awaiting_reassignment();
        if queued.is_empty() {
            return Ok(());
        }

        for session in queued {
            if session.attempts >= self.settings.max_attempts {
                self.give_up_on(&session);
                continue;
            }

            if let Err(error) = self.reassign(&session) {
                self.log(
                    Level::Error,
                    format!("could not reassign #{}: {error:#}", session.issue_number),
                );
            }
        }

        Ok(())
    }

    /// Stop cycling agents through an issue that keeps killing them.
    fn give_up_on(&self, session: &Session) {
        let reason = format!(
            "{} agents went quiet on this issue; joop has stopped reassigning it",
            session.attempts
        );

        // Nothing more will be done with this issue, so its tab should not be left open.
        self.abandon_tab(&session.tab_id);

        let parked = lock(&self.state).abandon(&self.settings.repo, session.issue_number, &reason);

        match parked {
            Ok(()) => self.log(Level::Error, format!("#{}: {reason}", session.issue_number)),
            Err(error) => self.log(
                Level::Error,
                format!("could not park #{}: {error:#}", session.issue_number),
            ),
        }
    }

    fn reassign(&self, session: &Session) -> Result<()> {
        // The dead agent's tab must be gone before a replacement starts, or two agents
        // end up writing to one worktree.
        if !self.abandon_tab(&session.tab_id) {
            return Err(anyhow!(
                "its tab is still open, so a replacement would share the worktree;                  will try again on the next audit"
            ));
        }

        let issue = Issue {
            number: session.issue_number,
            title: session.issue_title.clone(),
            url: session.issue_url.clone(),
            body: session.issue_body.clone(),
            labels: vec![self.settings.label.clone()],
        };
        let slug = issue.slug();

        // Reuse the branch and worktree: they may hold the dead agent's partial work.
        let worktree = self.worktrees.prepare(&slug, &session.branch)?;
        let attempt = session.attempts + 1;
        let agent = self.start_agent(&issue, &slug, &worktree, attempt)?;

        let recorded = lock(&self.state).record_reassignment(
            &self.settings.repo,
            session.issue_number,
            agent.name.clone(),
            agent.tab_id.clone(),
            agent.pane_id.clone(),
            self.clock.now(),
        );

        let attempts = match recorded {
            Ok(attempts) => attempts,
            Err(error) => {
                self.abandon_tab(&agent.tab_id);
                return Err(error);
            }
        };

        self.log(
            Level::Info,
            format!(
                "#{} reassigned to a new agent (attempt {attempts})",
                session.issue_number
            ),
        );
        Ok(())
    }

    /// Tell every still-running agent to carry on, after a limit pause, and restart their
    /// heartbeat clocks from now.
    fn nudge_running_agents(&self, now: DateTime<Utc>) -> Result<()> {
        let resumed = lock(&self.state).resume_live_sessions(now)?;
        if resumed.is_empty() {
            return Ok(());
        }

        for session in &resumed {
            let issue_number = session.issue_number;
            let message = format!(
                "Claude usage limits have reset and joop is running again. Please continue \
                 working on issue #{issue_number} from where you stopped, and report your status \
                 with `joop report --issue {issue_number} --status working --detail \"<what you \
                 are doing>\"`."
            );

            match self.agents.prompt(&session.agent_name, &message) {
                Ok(()) => self.log(
                    Level::Info,
                    format!("told {} to continue", session.agent_name),
                ),
                Err(error) => self.log(
                    Level::Warn,
                    format!("could not nudge {}: {error:#}", session.agent_name),
                ),
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::herdr::{AgentHost, AgentSpec, PaneMetadata, RunningAgent};
    use crate::issue::Issue;
    use crate::limits::{LimitSnapshot, LimitWatcher};
    use crate::state::{HeartbeatPolicy, PullRequest, State, Status};
    use crate::worktree::WorktreeProvider;
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
        fails: Mutex<bool>,
    }

    impl WorktreeProvider for FakeWorktrees {
        fn prepare(&self, slug: &str, _branch: &str) -> Result<PathBuf> {
            if *self.fails.lock().unwrap() {
                anyhow::bail!("no space left on device");
            }
            self.prepared.lock().unwrap().push(slug.to_string());
            Ok(self.path_for(slug))
        }
        fn path_for(&self, slug: &str) -> PathBuf {
            PathBuf::from("/worktrees").join(slug)
        }
    }

    #[derive(Debug, Default)]
    struct FakeHerdr {
        started: Mutex<Vec<AgentSpec>>,
        prompts: Mutex<Vec<(String, String)>>,
        closed: Mutex<Vec<String>>,
        focused: Mutex<Vec<String>>,
        metadata: Mutex<Vec<(String, PaneMetadata)>>,
        workspaces_ensured: Mutex<u32>,
        next_id: Mutex<u32>,
        /// Tab labels herdr reports as open; None means "whatever was started".
        open_labels: Mutex<Option<Vec<String>>>,
        /// When set, every close fails, as if herdr were unreachable.
        closing_fails: Mutex<bool>,
    }

    impl FakeHerdr {
        fn started_names(&self) -> Vec<String> {
            self.started
                .lock()
                .unwrap()
                .iter()
                .map(|spec| spec.name.clone())
                .collect()
        }

        fn last_prompt(&self) -> String {
            self.prompts
                .lock()
                .unwrap()
                .last()
                .expect("a prompt was sent")
                .1
                .clone()
        }

        /// The metadata most recently pushed for a pane.
        fn metadata_for(&self, pane_id: &str) -> Option<PaneMetadata> {
            self.metadata
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(pane, _)| pane == pane_id)
                .map(|(_, metadata)| metadata.clone())
        }
    }

    impl AgentHost for FakeHerdr {
        fn ensure_workspace(&self) -> Result<String> {
            *self.workspaces_ensured.lock().unwrap() += 1;
            Ok("w4".into())
        }

        fn start(&self, spec: &AgentSpec) -> Result<RunningAgent> {
            // The real adapter resolves the workspace as part of starting; model that so
            // callers cannot quietly depend on doing it themselves.
            self.ensure_workspace()?;
            self.started.lock().unwrap().push(spec.clone());
            let mut next = self.next_id.lock().unwrap();
            *next += 1;
            Ok(RunningAgent {
                tab_id: format!("w4:t{next}"),
                pane_id: format!("w4:p{next}"),
                name: spec.name.clone(),
            })
        }

        fn prompt(&self, agent_name: &str, text: &str) -> Result<()> {
            self.prompts
                .lock()
                .unwrap()
                .push((agent_name.to_string(), text.to_string()));
            Ok(())
        }

        fn close_tab(&self, tab_id: &str) -> Result<()> {
            if *self.closing_fails.lock().unwrap() {
                anyhow::bail!("herdr is not answering");
            }
            self.closed.lock().unwrap().push(tab_id.to_string());
            Ok(())
        }

        fn focus_tab(&self, tab_id: &str) -> Result<()> {
            self.focused.lock().unwrap().push(tab_id.to_string());
            Ok(())
        }

        fn report_metadata(&self, pane_id: &str, metadata: &PaneMetadata) -> Result<()> {
            self.metadata
                .lock()
                .unwrap()
                .push((pane_id.to_string(), metadata.clone()));
            Ok(())
        }

        fn tab_labels(&self) -> Result<Vec<String>> {
            Ok(self
                .open_labels
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| self.started_names()))
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
        worktrees: Arc<FakeWorktrees>,
        herdr: Arc<FakeHerdr>,
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
        let herdr = Arc::new(FakeHerdr::default());
        let worktrees = Arc::new(FakeWorktrees::default());

        let engine = Engine::new(
            EngineSettings {
                repo: "jaywhy13/joop".into(),
                label: "loop".into(),
                skill_command: "/implement".into(),
                api_url: "http://127.0.0.1:7777".into(),
                agent_kind: "claude".into(),
                agent_args: vec!["--dangerously-skip-permissions".into()],
                policy: HeartbeatPolicy::default(),
                limit_threshold: 0.9,
                max_attempts: 3,
            },
            issues.clone(),
            worktrees.clone(),
            herdr.clone(),
            Some(limits.clone()),
            clock.clone(),
            state.clone(),
        );

        Harness {
            engine,
            issues,
            worktrees,
            herdr,
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

        assert_eq!(harness.herdr.started_names(), vec!["issue-42"]);
        let spec = harness.herdr.started.lock().unwrap()[0].clone();
        assert_eq!(spec.cwd, PathBuf::from("/worktrees/issue-42"));
        assert_eq!(spec.kind, "claude");
        assert_eq!(spec.agent_args, vec!["--dangerously-skip-permissions"]);
        assert_eq!(
            spec.env,
            vec![(
                "JOOP_API_URL".to_string(),
                "http://127.0.0.1:7777".to_string()
            )],
            "the agent needs to know where to report"
        );
        assert!(harness.herdr.last_prompt().starts_with("/implement "));
    }

    #[test]
    fn an_agent_joop_cannot_record_is_shut_down_again() {
        // A read-only directory, so the state file loads empty but every write fails.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let state = Arc::new(Mutex::new(State::load(&locked.join("state.json")).unwrap()));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let herdr = Arc::new(FakeHerdr::default());
        let engine = Engine::new(
            EngineSettings {
                repo: "jaywhy13/joop".into(),
                label: "loop".into(),
                skill_command: "/implement".into(),
                api_url: "http://127.0.0.1:7777".into(),
                agent_kind: "claude".into(),
                agent_args: vec!["--dangerously-skip-permissions".into()],
                policy: HeartbeatPolicy::default(),
                limit_threshold: 0.9,
                max_attempts: 3,
            },
            Arc::new(FakeIssues::with(vec![issue(42)])),
            Arc::new(FakeWorktrees::default()),
            herdr.clone(),
            None,
            Arc::new(FixedClock::new(at(0))),
            state.clone(),
        );

        engine.poll().unwrap();

        assert_eq!(
            *herdr.closed.lock().unwrap(),
            vec!["w4:t1"],
            "an agent joop cannot supervise must not be left running, or the next poll \
             starts a second one in the same worktree"
        );
        assert!(state.lock().unwrap().session("jaywhy13/joop", 42).is_none());
        assert!(
            engine
                .snapshot()
                .events
                .iter()
                .any(|event| event.message.contains("could not dispatch #42")),
            "the failure must be visible on the dashboard"
        );
    }

    #[test]
    fn the_workspace_is_left_to_herdr_rather_than_ensured_separately() {
        let harness = harness(vec![issue(42)]);

        harness.engine.poll().unwrap();

        assert_eq!(
            *harness.herdr.workspaces_ensured.lock().unwrap(),
            1,
            "starting an agent already resolves the workspace, so joop must not ask twice"
        );
    }

    #[test]
    fn records_the_dispatch_with_its_tab_branch_and_worktree() {
        let harness = harness(vec![issue(42)]);

        harness.engine.poll().unwrap();

        let state = harness.state.lock().unwrap();
        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(session.status, Status::Dispatched);
        assert_eq!(session.agent_name, "issue-42");
        assert_eq!(session.tab_id, "w4:t1");
        assert_eq!(session.pane_id, "w4:p1");
        assert_eq!(session.branch, "joop/issue-42");
        assert_eq!(session.worktree, PathBuf::from("/worktrees/issue-42"));
        assert_eq!(session.attempts, 1);
    }

    #[test]
    fn does_not_open_a_second_tab_for_an_issue_already_being_worked() {
        let harness = harness(vec![issue(42)]);

        harness.engine.poll().unwrap();
        harness.engine.poll().unwrap();

        assert_eq!(harness.herdr.started_names(), vec!["issue-42"]);
    }

    #[test]
    fn dispatches_each_labelled_issue_once() {
        let harness = harness(vec![issue(42), issue(7)]);

        harness.engine.poll().unwrap();

        let mut names = harness.herdr.started_names();
        names.sort();
        assert_eq!(names, vec!["issue-42", "issue-7"]);
    }

    #[test]
    fn a_finished_issue_is_not_dispatched_again_while_it_keeps_its_label() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness
            .state
            .lock()
            .unwrap()
            .record_report("jaywhy13/joop", 42, Status::Done, None, None, at(5))
            .unwrap();

        harness.engine.poll().unwrap();

        assert_eq!(
            harness.herdr.started_names().len(),
            1,
            "an open issue keeps its label after the work is done, so re-dispatching on \
             status alone would open a tab on every poll, forever"
        );
    }

    #[test]
    fn a_forgotten_issue_is_picked_up_again() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();

        assert!(harness.engine.forget(42).unwrap());
        harness.engine.poll().unwrap();

        assert_eq!(harness.herdr.started_names().len(), 2);
    }

    #[test]
    fn forgetting_an_issue_joop_never_saw_reports_nothing_to_forget() {
        let harness = harness(vec![]);

        assert!(!harness.engine.forget(99).unwrap());
    }

    #[test]
    fn the_issue_body_is_kept_so_a_replacement_gets_the_full_brief() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();

        harness.clock.advance(Duration::minutes(31));
        harness.engine.audit().unwrap();

        assert!(
            harness.herdr.last_prompt().contains("Do the thing."),
            "the replacement must get the issue description, got: {}",
            harness.herdr.last_prompt()
        );
    }

    #[test]
    fn joop_gives_up_after_the_attempt_cap_rather_than_cycling_agents_forever() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();

        // Three agents in total: the first plus two replacements.
        for _ in 0..4 {
            harness.clock.advance(Duration::minutes(31));
            harness.engine.audit().unwrap();
        }

        assert_eq!(
            harness.herdr.started_names().len(),
            3,
            "max_attempts is 3, so joop must stop at three agents"
        );
        let state = harness.state.lock().unwrap();
        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(session.status, Status::Failed);
        assert!(
            session
                .detail
                .as_deref()
                .unwrap()
                .contains("stopped reassigning")
        );
    }

    #[test]
    fn a_tab_that_cannot_be_closed_blocks_the_replacement_rather_than_doubling_up() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        *harness.herdr.closing_fails.lock().unwrap() = true;

        harness.clock.advance(Duration::minutes(31));
        harness.engine.audit().unwrap();

        assert_eq!(
            harness.herdr.started_names().len(),
            1,
            "two agents must never share one worktree"
        );
        assert!(
            harness
                .state
                .lock()
                .unwrap()
                .session("jaywhy13/joop", 42)
                .unwrap()
                .needs_reassignment(),
            "the issue stays queued so the next audit can try again"
        );
    }

    #[test]
    fn an_agent_whose_herdr_tab_has_vanished_is_replaced_without_waiting_out_its_heartbeat() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        // The user closed the tab by hand.
        *harness.herdr.open_labels.lock().unwrap() = Some(vec![]);

        harness.engine.audit().unwrap();

        assert_eq!(
            harness.herdr.started_names().len(),
            2,
            "an agent whose tab is gone is not coming back"
        );
    }

    #[test]
    fn a_herdr_that_cannot_be_queried_leaves_sessions_alone() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        *harness.herdr.open_labels.lock().unwrap() = Some(vec!["issue-42".into()]);

        harness.engine.audit().unwrap();

        assert_eq!(harness.herdr.started_names().len(), 1);
        assert_eq!(
            harness
                .state
                .lock()
                .unwrap()
                .session("jaywhy13/joop", 42)
                .unwrap()
                .status,
            Status::Dispatched
        );
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
                agent_kind: "claude".into(),
                agent_args: vec!["--dangerously-skip-permissions".into()],
                policy: HeartbeatPolicy::default(),
                limit_threshold: 0.9,
                max_attempts: 3,
            },
            Arc::new(FailingIssues),
            Arc::new(FakeWorktrees::default()),
            Arc::new(FakeHerdr::default()),
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
            *harness.herdr.closed.lock().unwrap(),
            vec!["w4:t1"],
            "the dead agent's tab must be cleaned up"
        );
        assert_eq!(
            harness.herdr.started_names(),
            vec!["issue-42", "issue-42"],
            "the issue must be given to another agent"
        );
        let state = harness.state.lock().unwrap();
        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(session.attempts, 2);
        assert_eq!(session.status, Status::Dispatched);
        assert_eq!(session.tab_id, "w4:t2");
    }

    #[test]
    fn the_replacement_agent_is_told_it_is_a_second_attempt() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness.clock.advance(Duration::minutes(31));

        harness.engine.audit().unwrap();

        assert!(
            harness.herdr.last_prompt().contains("attempt 2"),
            "got: {}",
            harness.herdr.last_prompt()
        );
    }

    #[test]
    fn an_agent_that_is_merely_late_is_flagged_but_left_running() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();

        harness.clock.advance(Duration::minutes(3));
        harness.engine.audit().unwrap();

        assert!(harness.herdr.closed.lock().unwrap().is_empty());
        assert_eq!(harness.herdr.started_names().len(), 1);
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
                    None,
                    harness.clock.now(),
                )
                .unwrap();
            harness.engine.audit().unwrap();
        }

        assert!(harness.herdr.closed.lock().unwrap().is_empty());
        assert_eq!(harness.herdr.started_names().len(), 1);
    }

    #[test]
    fn polling_stops_once_the_claude_limit_is_reached() {
        let harness = harness(vec![issue(42)]);
        harness.limits.set(0.95, Some(at(60)));

        harness.engine.check_limits().unwrap();
        harness.engine.poll().unwrap();

        assert!(
            harness.herdr.started_names().is_empty(),
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
        assert_eq!(harness.herdr.started_names(), vec!["issue-42"]);
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

        let prompts = harness.herdr.prompts.lock().unwrap().clone();
        let nudges: Vec<&(String, String)> = prompts
            .iter()
            .filter(|(_, text)| text.to_lowercase().contains("continue working"))
            .collect();
        let mut nudged: Vec<String> = nudges.iter().map(|(agent, _)| agent.clone()).collect();
        nudged.sort();
        assert_eq!(nudged, vec!["issue-42", "issue-7"]);
    }

    #[test]
    fn finished_agents_are_not_told_to_carry_on() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness
            .state
            .lock()
            .unwrap()
            .record_report("jaywhy13/joop", 42, Status::Done, None, None, at(1))
            .unwrap();
        harness.limits.set(0.95, Some(at(60)));
        harness.engine.check_limits().unwrap();

        harness.clock.advance(Duration::minutes(61));
        harness.limits.set(0.05, None);
        harness.engine.check_limits().unwrap();

        assert!(
            !harness
                .herdr
                .prompts
                .lock()
                .unwrap()
                .iter()
                .any(|(_, text)| text.to_lowercase().contains("continue working"))
        );
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
        assert_eq!(harness.herdr.started_names(), vec!["issue-42"]);
    }

    #[test]
    fn the_heartbeat_clock_stops_while_joop_is_paused() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness.limits.set(0.95, Some(at(600)));
        harness.engine.check_limits().unwrap();

        // Ten times the thirty-minute death deadline.
        harness.clock.advance(Duration::minutes(300));
        harness.engine.audit().unwrap();

        assert!(
            harness.herdr.closed.lock().unwrap().is_empty(),
            "the agents are out of capacity too, so declaring them dead would close every \
             tab there is and leave nothing for the resumption message to reach"
        );
        assert_eq!(
            harness
                .state
                .lock()
                .unwrap()
                .session("jaywhy13/joop", 42)
                .unwrap()
                .status,
            Status::Dispatched
        );
    }

    #[test]
    fn a_resumed_agent_is_not_held_to_account_for_silence_it_spent_paused() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness.limits.set(0.95, Some(at(60)));
        harness.engine.check_limits().unwrap();

        harness.clock.advance(Duration::minutes(300));
        harness.limits.set(0.05, None);
        harness.engine.check_limits().unwrap();
        // One minute after being told to carry on: inside the two-minute first window.
        harness.clock.advance(Duration::minutes(1));
        harness.engine.audit().unwrap();

        assert_eq!(
            harness
                .state
                .lock()
                .unwrap()
                .session("jaywhy13/joop", 42)
                .unwrap()
                .status,
            Status::Dispatched,
            "the clock restarts from the resumption, not from before the pause"
        );
    }

    #[test]
    fn a_resumed_agent_that_stays_quiet_is_still_eventually_replaced() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness.limits.set(0.95, Some(at(60)));
        harness.engine.check_limits().unwrap();
        harness.clock.advance(Duration::minutes(300));
        harness.limits.set(0.05, None);
        harness.engine.check_limits().unwrap();

        harness.clock.advance(Duration::minutes(31));
        harness.engine.audit().unwrap();

        assert_eq!(
            harness.herdr.started_names().len(),
            2,
            "the pause buys an agent time, it does not make it immortal"
        );
    }

    fn pull_request() -> PullRequest {
        PullRequest {
            number: 7,
            url: "https://github.com/jaywhy13/joop/pull/7".into(),
        }
    }

    #[test]
    fn a_new_agent_gets_its_issue_shown_on_its_herdr_pane() {
        let harness = harness(vec![issue(42)]);

        harness.engine.poll().unwrap();

        let metadata = harness
            .herdr
            .metadata_for("w4:p1")
            .expect("metadata was pushed");
        assert_eq!(metadata.title.as_deref(), Some("#42"));
        assert!(metadata.tokens.contains(&("issue".into(), "42".into())));
    }

    #[test]
    fn a_reported_pull_request_is_shown_against_the_agent_in_herdr() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();

        harness
            .engine
            .record_report(
                42,
                Status::Review,
                Some("opened for review"),
                Some(pull_request()),
            )
            .unwrap();

        let metadata = harness
            .herdr
            .metadata_for("w4:p1")
            .expect("metadata was pushed");
        assert_eq!(
            metadata.title.as_deref(),
            Some("#42 · PR #7"),
            "the pane title is where the pull request becomes visible"
        );
        assert!(
            metadata
                .tokens
                .contains(&("pull_request".into(), "7".into()))
        );
        assert!(metadata.tokens.contains(&(
            "pr_url".into(),
            "https://github.com/jaywhy13/joop/pull/7".into()
        )));
        assert!(
            metadata
                .tokens
                .contains(&("status".into(), "review".into()))
        );
    }

    #[test]
    fn a_reported_pull_request_is_kept_in_joops_own_state() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();

        harness
            .engine
            .record_report(42, Status::Review, None, Some(pull_request()))
            .unwrap();

        let session = harness.engine.snapshot().sessions[0].clone();
        assert_eq!(session.pull_request, Some(pull_request()));
        assert_eq!(session.status, Status::Review);
    }

    #[test]
    fn a_merged_pull_request_closes_the_whole_herdr_tab() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness
            .engine
            .record_report(42, Status::Review, None, Some(pull_request()))
            .unwrap();

        harness
            .engine
            .record_report(42, Status::Merged, None, None)
            .unwrap();

        assert_eq!(
            *harness.herdr.closed.lock().unwrap(),
            vec!["w4:t1"],
            "the work has landed, so the tab has nothing left to do"
        );
    }

    #[test]
    fn a_merged_issue_is_not_dispatched_again_or_audited() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness
            .engine
            .record_report(42, Status::Merged, None, None)
            .unwrap();

        harness.clock.advance(Duration::minutes(500));
        harness.engine.audit().unwrap();
        harness.engine.poll().unwrap();

        assert_eq!(
            harness.herdr.started_names().len(),
            1,
            "a merged issue is finished, not fair game for a new agent"
        );
    }

    #[test]
    fn a_working_report_does_not_close_anything() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();

        harness
            .engine
            .record_report(42, Status::Working, Some("writing tests"), None)
            .unwrap();

        assert!(harness.herdr.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_report_for_an_issue_joop_never_dispatched_is_an_error() {
        let harness = harness(vec![]);

        assert!(
            harness
                .engine
                .record_report(99, Status::Working, None, None)
                .is_err()
        );
    }

    #[test]
    fn an_agent_watching_a_pull_request_goes_stale_after_five_quiet_minutes() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness
            .engine
            .record_report(42, Status::Review, None, Some(pull_request()))
            .unwrap();

        harness.clock.advance(Duration::minutes(6));
        harness.engine.audit().unwrap();

        assert_eq!(
            harness.engine.snapshot().sessions[0].status,
            Status::Stale,
            "an agent told to look for reviews every five minutes is late at six"
        );
    }

    #[test]
    fn an_agent_still_working_is_not_held_to_the_review_rhythm() {
        let harness = harness(vec![issue(42)]);
        harness.engine.poll().unwrap();
        harness
            .engine
            .record_report(42, Status::Working, None, None)
            .unwrap();

        harness.clock.advance(Duration::minutes(6));
        harness.engine.audit().unwrap();

        assert_eq!(
            harness.engine.snapshot().sessions[0].status,
            Status::Working
        );
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
            snapshot.pending.is_empty(),
            "the issue was dispatched, not left pending"
        );
        assert!(
            !snapshot.events.is_empty(),
            "the dashboard shows recent activity"
        );
    }

    #[test]
    fn an_issue_that_could_not_be_started_is_shown_as_waiting() {
        let harness = harness(vec![issue(42)]);
        *harness.worktrees.fails.lock().unwrap() = true;

        harness.engine.poll().unwrap();

        let pending = harness.engine.snapshot().pending;
        assert_eq!(pending.len(), 1, "a dropped issue must still be visible");
        assert_eq!(pending[0].number, 42);
        assert!(harness.herdr.started_names().is_empty());
    }

    #[test]
    fn an_issue_that_started_is_no_longer_shown_as_waiting() {
        let harness = harness(vec![issue(42)]);

        harness.engine.poll().unwrap();

        assert!(harness.engine.snapshot().pending.is_empty());
    }

    #[test]
    fn labelled_issues_are_visible_while_joop_is_paused() {
        let harness = harness(vec![issue(42)]);
        harness.limits.set(0.95, Some(at(600)));
        harness.engine.check_limits().unwrap();

        harness.engine.poll().unwrap();

        // Paused, so nothing was dispatched; the dashboard must still say why #42 is waiting.
        assert!(harness.herdr.started_names().is_empty());
        assert!(harness.engine.snapshot().pause.is_some());
    }

    #[test]
    fn a_last_poll_time_is_only_recorded_when_github_actually_answered() {
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
                agent_kind: "claude".into(),
                agent_args: vec!["--dangerously-skip-permissions".into()],
                policy: HeartbeatPolicy::default(),
                limit_threshold: 0.9,
                max_attempts: 3,
            },
            Arc::new(FailingIssues),
            Arc::new(FakeWorktrees::default()),
            Arc::new(FakeHerdr::default()),
            None,
            Arc::new(FixedClock::new(at(0))),
            state,
        );

        assert!(engine.poll().is_err());

        assert!(
            engine.snapshot().last_poll_at.is_none(),
            "a failed poll must not look like a fresh one on the dashboard"
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
