use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Where a Pi session has got to. `Dispatched` and `Stale` are set by joop itself;
/// everything between is reported by the Pi session working the issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// joop opened the tab; Pi has not checked in yet.
    Dispatched,
    Started,
    Working,
    Blocked,
    Review,
    Done,
    Failed,
    /// Pi missed its heartbeat deadline but is still inside the grace period.
    Stale,
    /// Pi went quiet for so long that joop gave up on it, closed its tab and
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
            Status::Failed => "failed",
            Status::Stale => "stale",
            Status::Dead => "dead",
        }
    }

    /// Statuses joop expects no further heartbeats from.
    pub fn is_finished(&self) -> bool {
        matches!(self, Status::Done | Status::Failed)
    }

    /// True for statuses that mean an agent is still expected to be working.
    pub fn is_live(&self) -> bool {
        !self.is_finished() && *self != Status::Dead
    }

    /// Statuses a Pi session is allowed to report.
    pub fn reportable() -> &'static [Status] {
        &[
            Status::Started,
            Status::Working,
            Status::Blocked,
            Status::Review,
            Status::Done,
            Status::Failed,
        ]
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
            "failed" => Ok(Status::Failed),
            "stale" => Ok(Status::Stale),
            "dead" => Ok(Status::Dead),
            other => Err(anyhow!("unknown status `{other}`")),
        }
    }
}

/// How often joop expects to hear from a Pi session.
#[derive(Debug, Clone, Copy)]
pub struct HeartbeatPolicy {
    /// Grace period between opening the tab and Pi's first report.
    pub first_update: Duration,
    /// Maximum gap between subsequent reports.
    pub subsequent_updates: Duration,
    /// Silence after which joop declares the agent dead, cleans up its tab and
    /// hands the issue to another agent.
    pub dead_after: Duration,
}

impl Default for HeartbeatPolicy {
    fn default() -> Self {
        Self {
            first_update: Duration::minutes(2),
            subsequent_updates: Duration::minutes(20),
            dead_after: Duration::minutes(30),
        }
    }
}

impl HeartbeatPolicy {
    /// The instant by which this session must have reported again.
    pub fn deadline_for(&self, session: &Session) -> DateTime<Utc> {
        match session.last_update_at {
            Some(last) => last + self.subsequent_updates,
            None => session.dispatched_at + self.first_update,
        }
    }

    /// The instant after which joop stops waiting and reassigns the issue.
    pub fn death_at(&self, session: &Session) -> DateTime<Utc> {
        session.last_activity_at() + self.dead_after
    }
}

/// Everything joop knows about one issue it handed to Pi.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub repo: String,
    pub issue_number: u64,
    pub issue_title: String,
    pub issue_url: String,
    /// Name of the Zellij tab running this session.
    pub tab_name: String,
    /// Zellij's own tab id, as returned by `zellij action new-tab`.
    pub tab_id: Option<String>,
    pub branch: String,
    pub worktree: PathBuf,
    pub status: Status,
    pub detail: Option<String>,
    pub dispatched_at: DateTime<Utc>,
    pub last_update_at: Option<DateTime<Utc>>,
    /// How many agents have been given this issue, including the current one.
    #[serde(default = "one")]
    pub attempts: u32,
}

fn one() -> u32 {
    1
}

impl Session {
    /// True while joop still owns this issue, so it must not open a second tab for it.
    /// A dead session still counts: joop reassigns it rather than rediscovering it.
    pub fn is_claimed(&self) -> bool {
        !self.status.is_finished()
    }

    /// The last time joop heard anything about this session.
    pub fn last_activity_at(&self) -> DateTime<Utc> {
        self.last_update_at.unwrap_or(self.dispatched_at)
    }

    /// True once joop has given up on the agent and is waiting to reassign the issue.
    pub fn needs_reassignment(&self) -> bool {
        self.status == Status::Dead
    }
}

/// The facts joop records when it opens a tab for an issue.
#[derive(Debug, Clone)]
pub struct Dispatch {
    pub repo: String,
    pub issue_number: u64,
    pub issue_title: String,
    pub issue_url: String,
    pub tab_name: String,
    pub tab_id: Option<String>,
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

    /// True when an unfinished session already owns this issue, so joop must not
    /// open a second tab for it.
    pub fn is_claimed(&self, repo: &str, issue_number: u64) -> bool {
        self.session(repo, issue_number)
            .is_some_and(Session::is_claimed)
    }

    pub fn record_dispatch(&mut self, dispatch: Dispatch) -> Result<()> {
        let session = Session {
            repo: dispatch.repo,
            issue_number: dispatch.issue_number,
            issue_title: dispatch.issue_title,
            issue_url: dispatch.issue_url,
            tab_name: dispatch.tab_name,
            tab_id: dispatch.tab_id,
            branch: dispatch.branch,
            worktree: dispatch.worktree,
            status: Status::Dispatched,
            detail: None,
            dispatched_at: dispatch.at,
            last_update_at: None,
            attempts: dispatch.attempt,
        };

        self.sessions
            .insert(key(&session.repo, session.issue_number), session);
        self.persist()
    }

    /// Apply a report from a Pi session, which also counts as its heartbeat.
    pub fn record_report(
        &mut self,
        repo: &str,
        issue_number: u64,
        status: Status,
        detail: Option<&str>,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let Some(session) = self.sessions.get_mut(&key(repo, issue_number)) else {
            bail!("joop has no session for {repo}#{issue_number}");
        };

        session.status = status;
        session.detail = detail.map(str::to_string);
        session.last_update_at = Some(at);
        self.persist()
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

    /// Record that a replacement agent has taken over a dead session\'s issue.
    pub fn record_reassignment(
        &mut self,
        repo: &str,
        issue_number: u64,
        tab_name: String,
        tab_id: Option<String>,
        at: DateTime<Utc>,
    ) -> Result<u32> {
        let Some(session) = self.sessions.get_mut(&key(repo, issue_number)) else {
            bail!("joop has no session for {repo}#{issue_number}");
        };

        session.attempts += 1;
        session.status = Status::Dispatched;
        session.detail = None;
        session.tab_name = tab_name;
        session.tab_id = tab_id;
        session.dispatched_at = at;
        session.last_update_at = None;
        let attempts = session.attempts;

        self.persist()?;
        Ok(attempts)
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
                tab_name: format!("issue-{number}"),
                tab_id: Some("3".into()),
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
    fn a_corrupt_file_is_reported_rather_than_silently_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{ not json").unwrap();

        assert!(State::load(&path).is_err());
    }

    #[test]
    fn a_dispatched_issue_is_remembered_with_its_zellij_tab() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let mut state = State::load(&path).unwrap();

        dispatched(&mut state, 42);

        let reloaded = State::load(&path).unwrap();
        let session = reloaded.session("jaywhy13/joop", 42).expect("session");
        assert_eq!(session.tab_name, "issue-42");
        assert_eq!(session.tab_id.as_deref(), Some("3"));
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

        let result = state.record_report("jaywhy13/joop", 99, Status::Working, None, at(1));

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
            .record_report("jaywhy13/joop", 42, Status::Working, None, at(1))
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
            .record_report("jaywhy13/joop", 42, Status::Working, None, at(10))
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
            .record_report("jaywhy13/joop", 42, Status::Done, None, at(1))
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
            .record_report("jaywhy13/joop", 42, Status::Working, None, at(4))
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
                Some("9".into()),
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
        assert_eq!(session.tab_id.as_deref(), Some("9"));
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
    fn an_issue_is_only_claimed_while_its_session_is_unfinished() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(&state_path(&dir)).unwrap();
        dispatched(&mut state, 42);
        assert!(state.is_claimed("jaywhy13/joop", 42));

        state
            .record_report("jaywhy13/joop", 42, Status::Done, None, at(1))
            .unwrap();

        assert!(
            !state.is_claimed("jaywhy13/joop", 42),
            "a finished issue may be dispatched again if it is re-labelled"
        );
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

    #[test]
    fn statuses_round_trip_through_their_wire_names() {
        for (status, wire) in [
            (Status::Dispatched, "dispatched"),
            (Status::Started, "started"),
            (Status::Working, "working"),
            (Status::Blocked, "blocked"),
            (Status::Review, "review"),
            (Status::Done, "done"),
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
