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

/// Handle on the background thread that does the timed work.
pub struct Supervisor {
    requests: Sender<Request>,
    handle: Option<thread::JoinHandle<()>>,
}

/// How often the heartbeat audit and the limit check run, independent of the issue poll.
const AUDIT_EVERY: Duration = Duration::from_secs(30);
const LIMIT_CHECK_EVERY: Duration = Duration::from_secs(60);
const TICK: Duration = Duration::from_millis(500);

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
    let _ = engine.check_limits();
    poll(&engine);

    let mut last_poll = Instant::now();
    let mut last_audit = Instant::now();
    let mut last_limit_check = Instant::now();

    loop {
        match inbox.recv_timeout(TICK) {
            Ok(Request::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Request::PollNow) => {
                check_limits(&engine);
                poll(&engine);
                last_poll = Instant::now();
                last_limit_check = Instant::now();
            }
            Ok(Request::CheckLimitsNow) => {
                check_limits(&engine);
                last_limit_check = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        if last_audit.elapsed() >= AUDIT_EVERY {
            if let Err(error) = engine.audit() {
                engine.log(Level::Error, format!("heartbeat audit failed: {error:#}"));
            }
            last_audit = Instant::now();
        }

        if last_limit_check.elapsed() >= LIMIT_CHECK_EVERY {
            check_limits(&engine);
            last_limit_check = Instant::now();
        }

        if last_poll.elapsed() >= poll_interval {
            poll(&engine);
            last_poll = Instant::now();
        }
    }
}

fn poll(engine: &Engine) {
    if let Err(error) = engine.poll() {
        engine.log(Level::Error, format!("poll failed: {error:#}"));
    }
}

/// A limit check that cannot be taken is logged by the engine and otherwise ignored:
/// joop keeps working rather than stalling on a reading it could not get.
fn check_limits(engine: &Engine) {
    let _ = engine.check_limits();
}
