use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use crate::engine::{Engine, Level};

/// Things the dashboard can ask the supervisor to do straight away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Poll GitHub now, without waiting for the timer.
    PollNow,
    /// Re-read Claude usage now.
    CheckLimitsNow,
    Stop,
}

/// The periodic jobs joop runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    Poll,
    Audit,
    CheckLimits,
    CheckBlockers,
}

/// How often the heartbeat audit and the limit check run, independent of the issue poll.
const AUDIT_EVERY: Duration = Duration::from_secs(30);
const LIMIT_CHECK_EVERY: Duration = Duration::from_secs(60);
/// How often waiting issues are judged, to see whether their blockers have landed.
const BLOCKER_CHECK_EVERY: Duration = Duration::from_secs(60 * 60);
const TICK: Duration = Duration::from_millis(500);

/// Decides which periodic jobs are due. Separated from the thread loop so the cadence
/// can be tested without waiting for real time to pass.
#[derive(Debug)]
pub struct Schedule {
    poll_every: Duration,
    last_poll: Instant,
    last_audit: Instant,
    last_limit_check: Instant,
    last_blocker_check: Instant,
}

impl Schedule {
    /// Start the clocks, treating everything as having just run.
    pub fn starting_at(now: Instant, poll_every: Duration) -> Self {
        Self {
            poll_every,
            last_poll: now,
            last_audit: now,
            last_limit_check: now,
            last_blocker_check: now,
        }
    }

    /// Which jobs are due at `now`, marking each one as having run.
    pub fn due(&mut self, now: Instant) -> Vec<Task> {
        let mut due = Vec::new();

        if now.duration_since(self.last_audit) >= AUDIT_EVERY {
            self.last_audit = now;
            due.push(Task::Audit);
        }
        if now.duration_since(self.last_limit_check) >= LIMIT_CHECK_EVERY {
            self.last_limit_check = now;
            due.push(Task::CheckLimits);
        }
        if now.duration_since(self.last_poll) >= self.poll_every {
            self.last_poll = now;
            due.push(Task::Poll);
        }
        if now.duration_since(self.last_blocker_check) >= BLOCKER_CHECK_EVERY {
            self.last_blocker_check = now;
            due.push(Task::CheckBlockers);
        }

        due
    }

    /// Note that a poll has just happened outside the timer, so the next one is a full
    /// interval away.
    pub fn polled_at(&mut self, now: Instant) {
        self.last_poll = now;
        self.last_limit_check = now;
    }

    pub fn checked_limits_at(&mut self, now: Instant) {
        self.last_limit_check = now;
    }
}

/// Handle on the background thread that does the timed work.
pub struct Supervisor {
    requests: Sender<Request>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Supervisor {
    /// Start polling, auditing and limit-checking in the background.
    pub fn start(engine: Arc<Engine>, poll_interval: Duration) -> Self {
        let (requests, inbox) = channel();
        let handle = thread::Builder::new()
            .name("joop-supervisor".into())
            .spawn(move || run(engine, poll_interval, inbox))
            .expect("could not start the joop supervisor thread");

        Self {
            requests,
            handle: Some(handle),
        }
    }

    pub fn poll_now(&self) {
        let _ = self.requests.send(Request::PollNow);
    }

    pub fn check_limits_now(&self) {
        let _ = self.requests.send(Request::CheckLimitsNow);
    }

    /// False once the background thread has stopped, so joop never sits there looking
    /// busy while nothing is actually running.
    pub fn is_running(&self) -> bool {
        self.handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    /// Ask the background thread to finish and wait for it.
    pub fn stop(mut self) {
        let _ = self.requests.send(Request::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(engine: Arc<Engine>, poll_interval: Duration, inbox: Receiver<Request>) {
    // Check capacity before the first poll, so joop never opens tabs it has no budget for.
    check_limits(&engine);
    poll(&engine);

    let mut schedule = Schedule::starting_at(Instant::now(), poll_interval);

    loop {
        match inbox.recv_timeout(TICK) {
            Ok(Request::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Request::PollNow) => {
                check_limits(&engine);
                poll(&engine);
                schedule.polled_at(Instant::now());
            }
            Ok(Request::CheckLimitsNow) => {
                check_limits(&engine);
                schedule.checked_limits_at(Instant::now());
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        for task in schedule.due(Instant::now()) {
            match task {
                Task::Poll => poll(&engine),
                Task::Audit => audit(&engine),
                Task::CheckLimits => check_limits(&engine),
                Task::CheckBlockers => check_blockers(&engine),
            }
        }
    }
}

/// Each job logs its own failure and returns, so one bad poll never ends the loop.
fn poll(engine: &Engine) {
    if let Err(error) = engine.poll() {
        engine.log(Level::Error, format!("poll failed: {error:#}"));
    }
}

fn audit(engine: &Engine) {
    if let Err(error) = engine.audit() {
        engine.log(Level::Error, format!("heartbeat audit failed: {error:#}"));
    }
}

/// A failed judgement is logged by the engine and tried again next hour.
fn check_blockers(engine: &Engine) {
    if let Err(error) = engine.check_blockers() {
        engine.log(
            Level::Error,
            format!("checking waiting issues failed: {error:#}"),
        );
    }
}

/// A limit check that cannot be taken is logged by the engine and otherwise ignored:
/// joop keeps working rather than stalling on a reading it could not get.
fn check_limits(engine: &Engine) {
    let _ = engine.check_limits();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(poll_every: Duration) -> (Instant, Schedule) {
        let start = Instant::now();
        (start, Schedule::starting_at(start, poll_every))
    }

    #[test]
    fn nothing_is_due_straight_away() {
        let (start, mut schedule) = schedule(Duration::from_secs(600));

        assert!(schedule.due(start).is_empty());
    }

    #[test]
    fn the_audit_runs_every_thirty_seconds() {
        let (start, mut schedule) = schedule(Duration::from_secs(600));

        assert_eq!(
            schedule.due(start + Duration::from_secs(30)),
            vec![Task::Audit]
        );
        assert!(
            schedule.due(start + Duration::from_secs(31)).is_empty(),
            "an audit that has just run is not due again"
        );
    }

    #[test]
    fn the_limit_check_runs_every_minute() {
        let (start, mut schedule) = schedule(Duration::from_secs(600));

        assert_eq!(
            schedule.due(start + Duration::from_secs(60)),
            vec![Task::Audit, Task::CheckLimits]
        );
    }

    #[test]
    fn the_poll_runs_on_its_own_interval() {
        let (start, mut schedule) = schedule(Duration::from_secs(600));

        let due = schedule.due(start + Duration::from_secs(600));

        assert!(due.contains(&Task::Poll));
    }

    #[test]
    fn the_audit_comes_before_the_poll_so_dead_agents_are_replaced_first() {
        let (start, mut schedule) = schedule(Duration::from_secs(600));

        let due = schedule.due(start + Duration::from_secs(600));

        let audit = due.iter().position(|task| *task == Task::Audit).unwrap();
        let poll = due.iter().position(|task| *task == Task::Poll).unwrap();
        assert!(audit < poll);
    }

    #[test]
    fn an_on_demand_poll_pushes_the_next_one_a_full_interval_out() {
        let (start, mut schedule) = schedule(Duration::from_secs(600));

        schedule.polled_at(start + Duration::from_secs(500));

        assert!(
            !schedule
                .due(start + Duration::from_secs(700))
                .contains(&Task::Poll),
            "pressing p must not leave a poll due 100 seconds later"
        );
        assert!(
            schedule
                .due(start + Duration::from_secs(1101))
                .contains(&Task::Poll)
        );
    }

    #[test]
    fn waiting_issues_are_checked_once_an_hour() {
        let (start, mut schedule) = schedule(Duration::from_secs(600));

        assert!(
            !schedule
                .due(start + Duration::from_secs(3599))
                .contains(&Task::CheckBlockers),
            "not before the first hour is up"
        );
        assert!(
            schedule
                .due(start + Duration::from_secs(3600))
                .contains(&Task::CheckBlockers)
        );
        assert!(
            !schedule
                .due(start + Duration::from_secs(3601))
                .contains(&Task::CheckBlockers),
            "and not again until the next hour"
        );
    }

    #[test]
    fn an_on_demand_limit_check_resets_only_the_limit_clock() {
        let (start, mut schedule) = schedule(Duration::from_secs(600));

        schedule.checked_limits_at(start + Duration::from_secs(50));

        let due = schedule.due(start + Duration::from_secs(80));
        assert!(due.contains(&Task::Audit), "the audit clock is untouched");
        assert!(!due.contains(&Task::CheckLimits));
    }
}
