use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Where an agent has got to. `Dispatched`, `Stale` and `Dead` are set by joop itself;
/// everything between is reported by the agent working the issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// joop opened the tab; the agent has not checked in yet.
    Dispatched,
    Started,
    Working,
    Blocked,
    Review,
    Done,
    /// The agent's pull request has landed. joop closes its tab.
    Merged,
    Failed,
    /// The agent missed its heartbeat deadline but is still inside the grace period.
    Stale,
    /// The agent went quiet for so long that joop gave up on it, closed its tab and
    /// queued the issue for another agent.
    Dead,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Dispatched => "dispatched",
            Status::Started => "started",
            Status::Working => "working",
            Status::Blocked => "blocked",
            Status::Review => "review",
            Status::Done => "done",
            Status::Merged => "merged",
            Status::Failed => "failed",
            Status::Stale => "stale",
            Status::Dead => "dead",
        }
    }

    /// Statuses joop expects no further heartbeats from.
    pub fn is_finished(&self) -> bool {
        matches!(self, Status::Done | Status::Merged | Status::Failed)
    }

    /// True for statuses that mean an agent is still expected to be working.
    pub fn is_live(&self) -> bool {
        !self.is_finished() && *self != Status::Dead
    }

    /// Statuses an agent is allowed to report.
    pub fn reportable() -> &'static [Status] {
        &[
            Status::Started,
            Status::Working,
            Status::Blocked,
            Status::Review,
            Status::Done,
            Status::Merged,
            Status::Failed,
        ]
    }
}

// Serialised through `as_str`/`from_str` so the wire name has exactly one definition.
impl Serialize for Status {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Status {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        name.parse().map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Status {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "dispatched" => Ok(Status::Dispatched),
            "started" => Ok(Status::Started),
            "working" => Ok(Status::Working),
            "blocked" => Ok(Status::Blocked),
            "review" => Ok(Status::Review),
            "done" => Ok(Status::Done),
            "merged" => Ok(Status::Merged),
            "failed" => Ok(Status::Failed),
            "stale" => Ok(Status::Stale),
            "dead" => Ok(Status::Dead),
            other => Err(anyhow!("unknown status `{other}`")),
        }
    }
}

/// How often joop expects to hear from an agent.
#[derive(Debug, Clone, Copy)]
pub struct HeartbeatPolicy {
    /// Grace period between opening the tab and the agent's first report.
    pub first_update: Duration,
    /// Maximum gap between subsequent reports.
    pub subsequent_updates: Duration,
    /// Maximum gap once a pull request is open, where the agent polls for reviews.
    pub review_updates: Duration,
    /// Silence after which joop declares the agent dead, cleans up its tab and
    /// hands the issue to another agent.
    pub dead_after: Duration,
}

impl Default for HeartbeatPolicy {
    fn default() -> Self {
        Self {
            first_update: Duration::minutes(2),
            subsequent_updates: Duration::minutes(20),
            review_updates: Duration::minutes(5),
            dead_after: Duration::minutes(30),
        }
    }
}

impl HeartbeatPolicy {
    /// The instant by which this session must have reported again.
    pub fn deadline_for(&self, session: &Session) -> DateTime<Utc> {
        match session.last_heartbeat() {
            Some(last) => last + self.gap_allowed_for(session),
            None => session.baseline_at() + self.first_update,
        }
    }

    /// An agent watching a pull request is told to look for reviews every few minutes, so
    /// joop expects to hear from it on that rhythm rather than the slower working one.
    fn gap_allowed_for(&self, session: &Session) -> Duration {
        if session.is_watching_a_pull_request() {
            self.review_updates
        } else {
            self.subsequent_updates
        }
    }

    /// The instant after which joop stops waiting and reassigns the issue.
    pub fn death_at(&self, session: &Session) -> DateTime<Utc> {
        session.last_activity_at() + self.dead_after
    }
}

/// Everything joop knows about one issue it handed to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub repo: String,
    pub issue_number: u64,
    pub issue_title: String,
    pub issue_url: String,
    /// The issue description, kept so a replacement agent gets the full brief.
    #[serde(default)]
    pub issue_body: String,
    /// herdr's name for the agent, which is also its tab label, e.g. `issue-42`.
    pub agent_name: String,
    /// herdr tab id, e.g. `w4:t2`.
    pub tab_id: String,
    /// herdr pane id the agent occupies, e.g. `w4:p2`.
    pub pane_id: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub status: Status,
    pub detail: Option<String>,
    /// The pull request the agent opened, once it has reported one.
    #[serde(default)]
    pub pull_request: Option<PullRequest>,
    pub dispatched_at: DateTime<Utc>,
    pub last_update_at: Option<DateTime<Utc>>,
    /// How many agents have been given this issue, including the current one.
    #[serde(default = "one")]
    pub attempts: u32,
    /// When joop last told this agent to carry on after a usage-limit pause. The
    /// heartbeat clock restarts from here, because an agent cannot report while paused.
    #[serde(default)]
    pub resumed_at: Option<DateTime<Utc>>,
}

fn one() -> u32 {
    1
}

impl Session {
    /// When the current heartbeat window started: the dispatch, or the resumption that
    /// came after it.
    pub fn baseline_at(&self) -> DateTime<Utc> {
        match self.resumed_at {
            Some(resumed) if resumed > self.dispatched_at => resumed,
            _ => self.dispatched_at,
        }
    }

    /// The agent's last report, ignoring any that predates the current window.
    pub fn last_heartbeat(&self) -> Option<DateTime<Utc>> {
        self.last_update_at
            .filter(|reported| *reported >= self.baseline_at())
    }

    /// The last time joop heard anything about this session.
    pub fn last_activity_at(&self) -> DateTime<Utc> {
        self.last_heartbeat().unwrap_or_else(|| self.baseline_at())
    }

    /// True while this agent has a pull request open and is polling it for reviews. Keyed on
    /// the pull request rather than the status, because `audit` overwrites `Review` with
    /// `Stale` on the first miss, which would silently drop the faster rhythm.
    pub fn is_watching_a_pull_request(&self) -> bool {
        self.pull_request.is_some() && !self.status.is_finished()
    }

    /// True once joop has given up on the agent and is waiting to reassign the issue.
    pub fn needs_reassignment(&self) -> bool {
        self.status == Status::Dead
    }
}

/// A pull request an agent has opened for its issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    /// Known only when the agent reported a URL rather than a bare number.
    #[serde(default)]
    pub url: Option<String>,
}

/// The facts joop records when it opens a tab for an issue.
#[derive(Debug, Clone)]
pub struct Dispatch {
    pub repo: String,
    pub issue_number: u64,
    pub issue_title: String,
    pub issue_url: String,
    pub issue_body: String,
    pub agent_name: String,
    pub tab_id: String,
    pub pane_id: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub at: DateTime<Utc>,
    /// 1 for a first dispatch, 2 for the agent that takes over from a dead one, and so on.
    pub attempt: u32,
}

/// joop's `state.json`: the sessions it has dispatched and what they last told it.
#[derive(Debug)]
pub struct State {
    path: PathBuf,
    sessions: BTreeMap<String, Session>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredState {
    #[serde(default)]
    sessions: BTreeMap<String, Session>,
}

impl State {
    /// Load `state.json`, treating a missing file as empty state. A file that exists but
    /// cannot be parsed is an error: discarding it would re-dispatch work already underway.
    pub fn load(path: &Path) -> Result<Self> {
        let sessions = match fs::read_to_string(path) {
            Ok(contents) => {
                serde_json::from_str::<StoredState>(&contents)
                    .with_context(|| format!("{} is not a valid joop state file", path.display()))?
                    .sessions
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()));
            }
        };

        Ok(Self {
            path: path.to_path_buf(),
            sessions,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn sessions(&self) -> &BTreeMap<String, Session> {
        &self.sessions
    }

    pub fn session(&self, repo: &str, issue_number: u64) -> Option<&Session> {
        self.sessions.get(&key(repo, issue_number))
    }

    /// True when joop has already taken this issue on. joop handles an issue once:
    /// an open issue keeps its label after the work is finished, so re-dispatching on
    /// status alone would open a fresh tab on every poll, forever. `forget` is the way
    /// to hand the same issue out again.
    pub fn is_claimed(&self, repo: &str, issue_number: u64) -> bool {
        self.session(repo, issue_number).is_some()
    }

    /// Drop joop's record of an issue so that it can be dispatched again.
    pub fn forget(&mut self, repo: &str, issue_number: u64) -> Result<bool> {
        let id = key(repo, issue_number);
        let Some(previous) = self.sessions.remove(&id) else {
            return Ok(false);
        };
        self.persist_or_restore(&id, Some(previous))?;
        Ok(true)
    }

    pub fn record_dispatch(&mut self, dispatch: Dispatch) -> Result<()> {
        let session = Session {
            repo: dispatch.repo,
            issue_number: dispatch.issue_number,
            issue_title: dispatch.issue_title,
            issue_url: dispatch.issue_url,
            issue_body: dispatch.issue_body,
            agent_name: dispatch.agent_name,
            tab_id: dispatch.tab_id,
            pane_id: dispatch.pane_id,
            branch: dispatch.branch,
            worktree: dispatch.worktree,
            status: Status::Dispatched,
            detail: None,
            pull_request: None,
            dispatched_at: dispatch.at,
            last_update_at: None,
            attempts: dispatch.attempt,
            resumed_at: None,
        };

        let id = key(&session.repo, session.issue_number);
        let previous = self.sessions.insert(id.clone(), session);
        self.persist_or_restore(&id, previous)
    }

    /// Apply a report from an agent, which also counts as its heartbeat.
    pub fn record_report(
        &mut self,
        repo: &str,
        issue_number: u64,
        status: Status,
        detail: Option<&str>,
        pull_request: Option<PullRequest>,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let id = key(repo, issue_number);
        let Some(session) = self.sessions.get_mut(&id) else {
            bail!("joop has no session for {repo}#{issue_number}");
        };
        let previous = session.clone();

        session.status = status;
        session.detail = detail.map(str::to_string);
        // An agent that reports a later status without repeating its pull request has not
        // withdrawn it, so the one joop already knows about stands.
        if pull_request.is_some() {
            session.pull_request = pull_request;
        }
        session.last_update_at = Some(at);
        self.persist_or_restore(&id, Some(previous))
    }

    /// Compare every live session against the heartbeat policy: sessions that have missed a
    /// report go stale, and sessions that have been silent past the death deadline are marked
    /// dead so that their tab can be cleaned up and the issue handed to another agent.
    pub fn audit(&mut self, policy: &HeartbeatPolicy, now: DateTime<Utc>) -> Result<Audit> {
        let mut audit = Audit::default();

        for (key, session) in &mut self.sessions {
            if !session.status.is_live() {
                continue;
            }

            if now > policy.death_at(session) {
                session.status = Status::Dead;
                audit.dead.push(key.clone());
            } else if session.status != Status::Stale && now > policy.deadline_for(session) {
                session.status = Status::Stale;
                audit.stale.push(key.clone());
            }
        }

        if !audit.is_empty() {
            self.persist()?;
        }

        Ok(audit)
    }

    /// Declare a session dead now, for instance because its herdr tab has gone.
    /// Returns false when the session was not live to begin with.
    pub fn mark_dead(&mut self, repo: &str, issue_number: u64, reason: &str) -> Result<bool> {
        let id = key(repo, issue_number);
        let Some(session) = self.sessions.get_mut(&id) else {
            return Ok(false);
        };
        if !session.status.is_live() {
            return Ok(false);
        }
        let previous = session.clone();

        session.status = Status::Dead;
        session.detail = Some(reason.to_string());
        self.persist_or_restore(&id, Some(previous))?;
        Ok(true)
    }

    /// Restart the heartbeat clock for every live session, because joop has just told
    /// them to carry on after a usage-limit pause.
    pub fn resume_live_sessions(&mut self, at: DateTime<Utc>) -> Result<Vec<Session>> {
        let resumed: Vec<Session> = self
            .sessions
            .values_mut()
            .filter(|session| session.status.is_live())
            .map(|session| {
                session.resumed_at = Some(at);
                session.clone()
            })
            .collect();

        if !resumed.is_empty() {
            self.persist()?;
        }

        Ok(resumed)
    }

    /// Sessions joop has given up on, oldest dispatch first, awaiting reassignment.
    pub fn awaiting_reassignment(&self) -> Vec<Session> {
        let mut sessions: Vec<Session> = self
            .sessions
            .values()
            .filter(|session| session.needs_reassignment())
            .cloned()
            .collect();
        sessions.sort_by_key(|session| session.dispatched_at);
        sessions
    }

    /// Give up on an issue after too many agents have died on it.
    pub fn abandon(&mut self, repo: &str, issue_number: u64, reason: &str) -> Result<()> {
        let id = key(repo, issue_number);
        let Some(session) = self.sessions.get_mut(&id) else {
            bail!("joop has no session for {repo}#{issue_number}");
        };
        let previous = session.clone();

        session.status = Status::Failed;
        session.detail = Some(reason.to_string());
        self.persist_or_restore(&id, Some(previous))
    }

    /// Record that a replacement agent has taken over a dead session\'s issue.
    pub fn record_reassignment(
        &mut self,
        repo: &str,
        issue_number: u64,
        agent_name: String,
        tab_id: String,
        pane_id: String,
        at: DateTime<Utc>,
    ) -> Result<u32> {
        let id = key(repo, issue_number);
        let Some(session) = self.sessions.get_mut(&id) else {
            bail!("joop has no session for {repo}#{issue_number}");
        };
        let previous = session.clone();

        session.attempts += 1;
        session.status = Status::Dispatched;
        session.detail = None;
        session.agent_name = agent_name;
        session.tab_id = tab_id;
        session.pane_id = pane_id;
        session.dispatched_at = at;
        session.last_update_at = None;
        session.resumed_at = None;
        let attempts = session.attempts;

        self.persist_or_restore(&id, Some(previous))?;
        Ok(attempts)
    }

    /// Write the change, and undo it in memory if the write fails. Otherwise joop would
    /// believe an issue was claimed that `state.json` has no record of, and re-dispatch
    /// it after a restart.
    fn persist_or_restore(&mut self, key: &str, previous: Option<Session>) -> Result<()> {
        let Err(error) = self.persist() else {
            return Ok(());
        };

        match previous {
            Some(session) => self.sessions.insert(key.to_string(), session),
            None => self.sessions.remove(key),
        };
        Err(error)
    }

    fn persist(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }

        let stored = StoredState {
            sessions: self.sessions.clone(),
        };
        let json = serde_json::to_string_pretty(&stored)?;
        fs::write(&self.path, json)
            .with_context(|| format!("could not write {}", self.path.display()))
    }
}

/// What one pass of the heartbeat audit found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Audit {
    /// Sessions that have missed a report but are still inside the grace period.
    pub stale: Vec<String>,
    /// Sessions joop has given up on; their tabs need closing and their issues reassigning.
    pub dead: Vec<String>,
}

impl Audit {
    pub fn is_empty(&self) -> bool {
        self.stale.is_empty() && self.dead.is_empty()
    }
}

pub fn key(repo: &str, issue_number: u64) -> String {
    format!("{repo}#{issue_number}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};

    /// `minutes` after a fixed reference instant.
    fn at(minutes: i64) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap() + Duration::minutes(minutes)
    }

    fn state_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("nested").join("state.json")
    }

    fn dispatched(state: &mut State, number: u64) {
        state
            .record_dispatch(Dispatch {
                repo: "jaywhy13/joop".into(),
                issue_number: number,
                issue_title: format!("Issue {number}"),
                issue_url: format!("https://github.com/jaywhy13/joop/issues/{number}"),
                issue_body: format!("Body of issue {number}"),
                agent_name: format!("issue-{number}"),
                tab_id: format!("w4:t{number}"),
                pane_id: format!("w4:p{number}"),
                branch: format!("joop/issue-{number}"),
                worktree: format!("/tmp/worktrees/issue-{number}").into(),
                at: at(0),
                attempt: 1,
            })
            .unwrap();
    }

    #[test]
    fn a_missing_file_loads_as_empty_state() {
        let dir = tempfile::tempdir().unwrap();

        let state = State::load(&state_path(&dir)).expect("missing file is not an error");

        assert!(state.sessions().is_empty());
    }

    #[test]
    fn a_dispatch_that_cannot_be_written_is_not_left_in_memory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let mut state = State::load(&locked.join("state.json")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();

        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatched(&mut state, 42)));

        assert!(result.is_err(), "the write failed, so the dispatch failed");
        assert!(
            !state.is_claimed("jaywhy13/joop", 42),
            "an unwritten claim would be lost on restart and the issue dispatched twice"
        );
    }

    #[test]
    fn a_report_that_cannot_be_written_leaves_the_session_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = State::load(&path).unwrap();
        dispatched(&mut state, 42);
        // Read-only file: the write fails, the existing record must stand.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();

        let result = state.record_report("jaywhy13/joop", 42, Status::Done, Some("x"), None, at(1));

        assert!(result.is_err());
        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(session.status, Status::Dispatched);
        assert_eq!(session.detail, None);
    }

    #[test]
    fn a_corrupt_file_is_reported_rather_than_silently_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{ not json").unwrap();

        assert!(State::load(&path).is_err());
    }

    #[test]
    fn a_dispatched_issue_is_remembered_with_its_herdr_tab_and_pane() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let mut state = State::load(&path).unwrap();

        dispatched(&mut state, 42);

        let reloaded = State::load(&path).unwrap();
        let session = reloaded.session("jaywhy13/joop", 42).expect("session");
        assert_eq!(session.agent_name, "issue-42");
        assert_eq!(session.tab_id, "w4:t42");
        assert_eq!(session.pane_id, "w4:p42");
        assert_eq!(session.branch, "joop/issue-42");
        assert_eq!(session.status, Status::Dispatched);
    }

    #[test]
    fn issue_numbers_are_scoped_to_their_repository() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();

        dispatched(&mut state, 42);

        assert!(state.session("jaywhy13/other", 42).is_none());
    }

    #[test]
    fn a_report_from_a_pi_session_updates_its_status_and_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let mut state = State::load(&path).unwrap();
        dispatched(&mut state, 42);

        state
            .record_report(
                "jaywhy13/joop",
                42,
                Status::Working,
                Some("writing tests"),
                None,
                at(1),
            )
            .unwrap();

        let session = State::load(&path)
            .unwrap()
            .session("jaywhy13/joop", 42)
            .unwrap()
            .clone();
        assert_eq!(session.status, Status::Working);
        assert_eq!(session.detail.as_deref(), Some("writing tests"));
        assert_eq!(session.last_update_at, Some(at(1)));
    }

    #[test]
    fn reporting_on_an_unknown_issue_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();

        let result = state.record_report("jaywhy13/joop", 99, Status::Working, None, None, at(1));

        assert!(result.is_err());
    }

    #[test]
    fn the_first_update_is_due_two_minutes_after_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);

        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(HeartbeatPolicy::default().deadline_for(session), at(2));
    }

    #[test]
    fn later_updates_are_due_twenty_minutes_after_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state
            .record_report("jaywhy13/joop", 42, Status::Working, None, None, at(1))
            .unwrap();

        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(HeartbeatPolicy::default().deadline_for(session), at(21));
    }

    #[test]
    fn an_agent_is_declared_dead_thirty_minutes_after_its_last_update() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state
            .record_report("jaywhy13/joop", 42, Status::Working, None, None, at(10))
            .unwrap();

        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(HeartbeatPolicy::default().death_at(session), at(40));
    }

    #[test]
    fn a_session_that_misses_its_first_update_goes_stale() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);

        let audit = state.audit(&HeartbeatPolicy::default(), at(3)).unwrap();

        assert_eq!(audit.stale, vec!["jaywhy13/joop#42".to_string()]);
        assert!(audit.dead.is_empty());
        assert_eq!(
            state.session("jaywhy13/joop", 42).unwrap().status,
            Status::Stale
        );
    }

    #[test]
    fn a_session_inside_its_window_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);

        let audit = state.audit(&HeartbeatPolicy::default(), at(1)).unwrap();

        assert!(audit.is_empty());
        assert_eq!(
            state.session("jaywhy13/joop", 42).unwrap().status,
            Status::Dispatched
        );
    }

    #[test]
    fn a_session_silent_past_the_death_deadline_is_declared_dead() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);

        let audit = state.audit(&HeartbeatPolicy::default(), at(31)).unwrap();

        assert_eq!(audit.dead, vec!["jaywhy13/joop#42".to_string()]);
        assert!(
            audit.stale.is_empty(),
            "a dead session is not also reported as stale"
        );
        assert_eq!(
            state.session("jaywhy13/joop", 42).unwrap().status,
            Status::Dead
        );
    }

    #[test]
    fn a_dead_session_is_only_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state.audit(&HeartbeatPolicy::default(), at(31)).unwrap();

        let audit = state.audit(&HeartbeatPolicy::default(), at(40)).unwrap();

        assert!(
            audit.is_empty(),
            "joop must not reassign the same issue twice"
        );
    }

    #[test]
    fn a_finished_session_never_goes_stale_or_dead() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state
            .record_report("jaywhy13/joop", 42, Status::Done, None, None, at(1))
            .unwrap();

        let audit = state.audit(&HeartbeatPolicy::default(), at(500)).unwrap();

        assert!(audit.is_empty());
        assert_eq!(
            state.session("jaywhy13/joop", 42).unwrap().status,
            Status::Done
        );
    }

    #[test]
    fn a_stale_session_recovers_when_pi_reports_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state.audit(&HeartbeatPolicy::default(), at(3)).unwrap();

        state
            .record_report("jaywhy13/joop", 42, Status::Working, None, None, at(4))
            .unwrap();

        assert_eq!(
            state.session("jaywhy13/joop", 42).unwrap().status,
            Status::Working
        );
    }

    #[test]
    fn dead_sessions_are_queued_for_reassignment_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 7);
        dispatched(&mut state, 42);
        state.audit(&HeartbeatPolicy::default(), at(31)).unwrap();

        let queued: Vec<u64> = state
            .awaiting_reassignment()
            .iter()
            .map(|session| session.issue_number)
            .collect();

        assert_eq!(queued.len(), 2);
        assert!(queued.contains(&7) && queued.contains(&42));
    }

    #[test]
    fn reassignment_records_the_new_tab_and_counts_the_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let mut state = State::load(&path).unwrap();
        dispatched(&mut state, 42);
        state.audit(&HeartbeatPolicy::default(), at(31)).unwrap();

        let attempts = state
            .record_reassignment(
                "jaywhy13/joop",
                42,
                "issue-42".into(),
                "w4:t9".into(),
                "w4:p9".into(),
                at(32),
            )
            .unwrap();

        assert_eq!(attempts, 2);
        let session = State::load(&path)
            .unwrap()
            .session("jaywhy13/joop", 42)
            .unwrap()
            .clone();
        assert_eq!(session.status, Status::Dispatched);
        assert_eq!(session.tab_id, "w4:t9");
        assert_eq!(session.pane_id, "w4:p9");
        assert_eq!(
            session.last_update_at, None,
            "the replacement starts with a clean slate"
        );
        assert_eq!(
            HeartbeatPolicy::default().deadline_for(&session),
            at(34),
            "the replacement gets its own two-minute grace period"
        );
    }

    #[test]
    fn an_issue_stays_claimed_once_joop_has_taken_it_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        assert!(state.is_claimed("jaywhy13/joop", 42));

        state
            .record_report("jaywhy13/joop", 42, Status::Done, None, None, at(1))
            .unwrap();

        assert!(
            state.is_claimed("jaywhy13/joop", 42),
            "an open issue keeps its label after the work is done, so claiming on status \
             alone would re-dispatch it on every poll"
        );
    }

    #[test]
    fn forgetting_an_issue_lets_it_be_dispatched_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let mut state = State::load(&path).unwrap();
        dispatched(&mut state, 42);

        assert!(state.forget("jaywhy13/joop", 42).unwrap());

        assert!(!state.is_claimed("jaywhy13/joop", 42));
        assert!(!State::load(&path).unwrap().is_claimed("jaywhy13/joop", 42));
    }

    #[test]
    fn forgetting_an_unknown_issue_reports_nothing_to_forget() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();

        assert!(!state.forget("jaywhy13/joop", 99).unwrap());
    }

    #[test]
    fn a_resumed_session_gets_a_fresh_first_update_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state
            .record_report("jaywhy13/joop", 42, Status::Working, None, None, at(5))
            .unwrap();

        let resumed = state.resume_live_sessions(at(300)).unwrap();

        assert_eq!(resumed.len(), 1);
        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(
            HeartbeatPolicy::default().deadline_for(session),
            at(302),
            "the clock restarts from the resumption, not the report it made before pausing"
        );
        assert_eq!(
            HeartbeatPolicy::default().death_at(session),
            at(330),
            "and so does the death deadline"
        );
    }

    #[test]
    fn finished_sessions_are_not_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state
            .record_report("jaywhy13/joop", 42, Status::Done, None, None, at(1))
            .unwrap();

        assert!(state.resume_live_sessions(at(300)).unwrap().is_empty());
    }

    #[test]
    fn a_session_whose_tab_has_gone_can_be_declared_dead_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);

        assert!(
            state
                .mark_dead("jaywhy13/joop", 42, "its tab is gone")
                .unwrap()
        );

        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(session.status, Status::Dead);
        assert_eq!(session.detail.as_deref(), Some("its tab is gone"));
    }

    #[test]
    fn a_finished_session_is_not_declared_dead() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state
            .record_report("jaywhy13/joop", 42, Status::Done, None, None, at(1))
            .unwrap();

        assert!(
            !state
                .mark_dead("jaywhy13/joop", 42, "its tab is gone")
                .unwrap()
        );
    }

    #[test]
    fn abandoning_an_issue_parks_it_as_failed_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);

        state
            .abandon("jaywhy13/joop", 42, "three agents went quiet")
            .unwrap();

        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(session.status, Status::Failed);
        assert_eq!(session.detail.as_deref(), Some("three agents went quiet"));
    }

    #[test]
    fn statuses_survive_a_round_trip_through_json() {
        for status in [
            Status::Dispatched,
            Status::Started,
            Status::Working,
            Status::Blocked,
            Status::Review,
            Status::Done,
            Status::Merged,
            Status::Failed,
            Status::Stale,
            Status::Dead,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{}\"", status.as_str()));
            assert_eq!(serde_json::from_str::<Status>(&json).unwrap(), status);
        }
    }

    #[test]
    fn stale_and_dead_sessions_still_count_as_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);

        state.audit(&HeartbeatPolicy::default(), at(3)).unwrap();
        assert!(
            state.is_claimed("jaywhy13/joop", 42),
            "stale sessions own their tab"
        );

        state.audit(&HeartbeatPolicy::default(), at(31)).unwrap();
        assert!(
            state.is_claimed("jaywhy13/joop", 42),
            "dead sessions are reassigned, not rediscovered as new issues"
        );
    }

    fn pull_request() -> PullRequest {
        PullRequest {
            number: 7,
            url: Some("https://github.com/jaywhy13/joop/pull/7".into()),
        }
    }

    #[test]
    fn a_reported_pull_request_is_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let mut state = State::load(&path).unwrap();
        dispatched(&mut state, 42);

        state
            .record_report(
                "jaywhy13/joop",
                42,
                Status::Review,
                Some("opened a pull request"),
                Some(pull_request()),
                at(5),
            )
            .unwrap();

        let session = State::load(&path)
            .unwrap()
            .session("jaywhy13/joop", 42)
            .unwrap()
            .clone();
        assert_eq!(session.pull_request, Some(pull_request()));
    }

    #[test]
    fn a_later_report_without_a_pull_request_keeps_the_one_already_known() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state
            .record_report(
                "jaywhy13/joop",
                42,
                Status::Review,
                None,
                Some(pull_request()),
                at(5),
            )
            .unwrap();

        state
            .record_report(
                "jaywhy13/joop",
                42,
                Status::Review,
                Some("no reviews yet"),
                None,
                at(10),
            )
            .unwrap();

        assert_eq!(
            state.session("jaywhy13/joop", 42).unwrap().pull_request,
            Some(pull_request()),
            "reporting progress is not withdrawing the pull request"
        );
    }

    #[test]
    fn an_agent_watching_a_pull_request_is_expected_every_five_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state
            .record_report(
                "jaywhy13/joop",
                42,
                Status::Review,
                None,
                Some(pull_request()),
                at(10),
            )
            .unwrap();

        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(
            HeartbeatPolicy::default().deadline_for(session),
            at(15),
            "an agent polling for reviews every five minutes can report on that rhythm"
        );
    }

    #[test]
    fn the_review_rhythm_survives_joop_overwriting_the_status() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state
            .record_report(
                "jaywhy13/joop",
                42,
                Status::Review,
                None,
                Some(pull_request()),
                at(10),
            )
            .unwrap();

        // joop itself rewrites Review to Stale on the first missed check-in.
        state.audit(&HeartbeatPolicy::default(), at(16)).unwrap();
        assert_eq!(
            state.session("jaywhy13/joop", 42).unwrap().status,
            Status::Stale
        );

        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(
            HeartbeatPolicy::default().deadline_for(session),
            at(15),
            "the agent is still polling its pull request every five minutes, so the deadline \
             must not quietly revert to twenty just because joop relabelled the session"
        );
    }

    #[test]
    fn an_agent_blocked_on_a_review_keeps_the_review_rhythm() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state
            .record_report(
                "jaywhy13/joop",
                42,
                Status::Review,
                None,
                Some(pull_request()),
                at(10),
            )
            .unwrap();

        state
            .record_report(
                "jaywhy13/joop",
                42,
                Status::Blocked,
                Some("waiting on a human"),
                None,
                at(12),
            )
            .unwrap();

        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(HeartbeatPolicy::default().deadline_for(session), at(17));
    }

    #[test]
    fn a_finished_session_is_not_watching_anything() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state
            .record_report(
                "jaywhy13/joop",
                42,
                Status::Merged,
                None,
                Some(pull_request()),
                at(10),
            )
            .unwrap();

        assert!(
            !state
                .session("jaywhy13/joop", 42)
                .unwrap()
                .is_watching_a_pull_request()
        );
    }

    #[test]
    fn an_agent_still_writing_code_keeps_the_slower_twenty_minute_rhythm() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state
            .record_report("jaywhy13/joop", 42, Status::Working, None, None, at(10))
            .unwrap();

        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(HeartbeatPolicy::default().deadline_for(session), at(30));
    }

    #[test]
    fn a_merged_pull_request_finishes_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);

        state
            .record_report(
                "jaywhy13/joop",
                42,
                Status::Merged,
                None,
                Some(pull_request()),
                at(60),
            )
            .unwrap();

        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert!(session.status.is_finished());
        assert_eq!(session.status, Status::Merged);
        assert!(
            state
                .audit(&HeartbeatPolicy::default(), at(500))
                .unwrap()
                .is_empty(),
            "nothing more is expected from an agent whose work has landed"
        );
    }

    #[test]
    fn a_reassigned_issue_inherits_the_dead_agents_pull_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        state
            .record_report(
                "jaywhy13/joop",
                42,
                Status::Review,
                None,
                Some(pull_request()),
                at(5),
            )
            .unwrap();
        state.audit(&HeartbeatPolicy::default(), at(40)).unwrap();

        state
            .record_reassignment(
                "jaywhy13/joop",
                42,
                "issue-42".into(),
                "w4:t9".into(),
                "w4:p9".into(),
                at(41),
            )
            .unwrap();

        assert_eq!(
            state.session("jaywhy13/joop", 42).unwrap().pull_request,
            Some(pull_request()),
            "the replacement works the same branch, so it inherits the open pull request \
             rather than orphaning it"
        );
    }

    #[test]
    fn statuses_round_trip_through_their_wire_names() {
        for (status, wire) in [
            (Status::Dispatched, "dispatched"),
            (Status::Started, "started"),
            (Status::Working, "working"),
            (Status::Blocked, "blocked"),
            (Status::Review, "review"),
            (Status::Done, "done"),
            (Status::Merged, "merged"),
            (Status::Failed, "failed"),
            (Status::Stale, "stale"),
            (Status::Dead, "dead"),
        ] {
            assert_eq!(status.as_str(), wire);
            assert_eq!(wire.parse::<Status>().unwrap(), status);
        }

        assert!("nonsense".parse::<Status>().is_err());
    }
}
