use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::Duration;
use clap::{Args, Parser, Subcommand};

use crate::state::HeartbeatPolicy;

/// joop watches a GitHub repository for issues labelled `loop` and gives each one to a
/// Pi agent in its own Zellij tab, then supervises them.
#[derive(Debug, Parser)]
#[command(name = "joop", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub run: RunArgs,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Watch the repository and supervise agents (the default).
    Run(RunArgs),
    /// Report a Pi session's status back to a running joop.
    Report(ReportArgs),
    /// Print the current sessions and exit.
    Status(StatusArgs),
    /// Forget an issue so that joop picks it up again on the next poll.
    Forget(ForgetArgs),
}

#[derive(Debug, Args, Clone)]
pub struct ForgetArgs {
    /// The issue number to forget.
    #[arg(long)]
    pub issue: u64,

    /// Repository the issue belongs to, as `owner/name`.
    #[arg(long, env = "JOOP_REPO")]
    pub repo: Option<String>,

    /// Where joop keeps its state.
    #[arg(long)]
    pub state_file: Option<PathBuf>,
}

#[derive(Debug, Args, Clone)]
pub struct RunArgs {
    /// Repository to watch, as `owner/name`. Taken from --repo-path, or asked for, if omitted.
    #[arg(long, env = "JOOP_REPO")]
    pub repo: Option<String>,

    /// Local checkout the worktrees are cut from.
    #[arg(long, default_value = ".")]
    pub repo_path: PathBuf,

    /// Label that marks an issue as joop's to pick up.
    #[arg(long, default_value = "loop")]
    pub label: String,

    /// Zellij session the agent tabs are opened in.
    #[arg(long, default_value = "joop")]
    pub session: String,

    /// Where per-issue worktrees are created. Defaults to a sibling of --repo-path.
    #[arg(long)]
    pub worktree_root: Option<PathBuf>,

    /// Where joop keeps its state.
    #[arg(long)]
    pub state_file: Option<PathBuf>,

    /// Minutes between automatic polls for new issues.
    #[arg(long, default_value_t = 10)]
    pub poll_interval: i64,

    /// Minutes an agent has to send its first update.
    #[arg(long, default_value_t = 2)]
    pub first_update: i64,

    /// Minutes allowed between an agent's later updates.
    #[arg(long, default_value_t = 20)]
    pub update_interval: i64,

    /// Minutes of silence after which an agent is declared dead and replaced.
    #[arg(long, default_value_t = 30)]
    pub dead_after: i64,

    /// Fraction of a Claude usage limit at which joop stops starting new work.
    #[arg(long, default_value_t = 0.9)]
    pub limit_threshold: f64,

    /// Do not read Claude usage limits at all.
    #[arg(long)]
    pub no_limit_checks: bool,

    /// Port the reporting API listens on.
    #[arg(long, default_value_t = 7777)]
    pub api_port: u16,

    /// Skill command the agent should drive the work with.
    #[arg(long, default_value = "/implement")]
    pub skill: String,

    /// The agent binary to run in each tab.
    #[arg(long, default_value = "pi")]
    pub agent: String,

    /// How many agents may be given one issue before joop gives up on it.
    #[arg(long, default_value_t = 3)]
    pub max_attempts: u32,

    /// Most issues to consider in one poll.
    #[arg(long, default_value_t = 50)]
    pub issue_limit: u32,

    /// Log to the terminal instead of drawing the dashboard.
    #[arg(long)]
    pub headless: bool,
}

#[derive(Debug, Args, Clone)]
pub struct ReportArgs {
    /// The issue number being worked on.
    #[arg(long)]
    pub issue: u64,

    /// One of: started, working, blocked, review, done, failed.
    #[arg(long)]
    pub status: String,

    /// What the agent is doing, shown on joop's dashboard.
    #[arg(long)]
    pub detail: Option<String>,

    /// Base URL of the joop API.
    #[arg(long, env = "JOOP_API_URL", default_value = "http://127.0.0.1:7777")]
    pub api_url: String,
}

#[derive(Debug, Args, Clone)]
pub struct StatusArgs {
    /// Where joop keeps its state.
    #[arg(long)]
    pub state_file: Option<PathBuf>,
}

impl RunArgs {
    pub fn policy(&self) -> Result<HeartbeatPolicy> {
        if self.first_update < 1 || self.update_interval < 1 || self.dead_after < 1 {
            bail!("heartbeat intervals must be at least one minute");
        }
        if self.dead_after <= self.update_interval {
            bail!(
                "--dead-after ({}m) must be longer than --update-interval ({}m), or every agent \
                 dies the moment it is late",
                self.dead_after,
                self.update_interval
            );
        }

        Ok(HeartbeatPolicy {
            first_update: Duration::minutes(self.first_update),
            subsequent_updates: Duration::minutes(self.update_interval),
            dead_after: Duration::minutes(self.dead_after),
        })
    }

    pub fn poll_interval(&self) -> Result<std::time::Duration> {
        if self.poll_interval < 1 {
            bail!("--poll-interval must be at least one minute");
        }
        Ok(std::time::Duration::from_secs(
            self.poll_interval as u64 * 60,
        ))
    }

    pub fn max_attempts(&self) -> Result<u32> {
        if self.max_attempts < 1 {
            bail!("--max-attempts must be at least 1");
        }
        Ok(self.max_attempts)
    }

    pub fn limit_threshold(&self) -> Result<f64> {
        if !(0.0..=1.0).contains(&self.limit_threshold) {
            bail!(
                "--limit-threshold must be between 0 and 1, got {}",
                self.limit_threshold
            );
        }
        Ok(self.limit_threshold)
    }

    pub fn api_address(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.api_port)
    }

    pub fn api_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.api_port)
    }

    /// Absolute path to the local checkout.
    pub fn canonical_repo_path(&self) -> Result<PathBuf> {
        self.repo_path
            .canonicalize()
            .with_context(|| format!("no such directory: {}", self.repo_path.display()))
    }

    pub fn state_path(&self) -> PathBuf {
        self.state_file.clone().unwrap_or_else(default_state_path)
    }
}

/// `$XDG_STATE_HOME/joop/state.json`, falling back to `~/.local/state/joop/state.json`.
pub fn default_state_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("state")
        });

    base.join("joop").join("state.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn run_args(extra: &[&str]) -> RunArgs {
        let mut argv = vec!["joop"];
        argv.extend_from_slice(extra);
        Cli::parse_from(argv).run
    }

    #[test]
    fn the_defaults_match_the_agreed_supervision_rules() {
        let args = run_args(&[]);
        let policy = args.policy().unwrap();

        assert_eq!(args.label, "loop");
        assert_eq!(args.session, "joop");
        assert_eq!(args.skill, "/implement");
        assert_eq!(args.agent, "pi");
        assert_eq!(policy.first_update, Duration::minutes(2));
        assert_eq!(policy.subsequent_updates, Duration::minutes(20));
        assert_eq!(policy.dead_after, Duration::minutes(30));
        assert_eq!(args.poll_interval().unwrap().as_secs(), 600);
        assert_eq!(args.max_attempts().unwrap(), 3);
    }

    #[test]
    fn a_zero_attempt_cap_is_rejected() {
        assert!(run_args(&["--max-attempts", "0"]).max_attempts().is_err());
    }

    #[test]
    fn forgetting_takes_an_issue_number() {
        match Cli::parse_from(["joop", "forget", "--issue", "42"]).command {
            Some(Command::Forget(args)) => assert_eq!(args.issue, 42),
            other => panic!("expected a forget command, got {other:?}"),
        }
    }

    #[test]
    fn a_death_deadline_inside_the_update_window_is_rejected() {
        let args = run_args(&["--update-interval", "30", "--dead-after", "20"]);

        let error = args.policy().expect_err("contradictory intervals");

        assert!(
            format!("{error:#}").contains("--dead-after"),
            "got: {error:#}"
        );
    }

    #[test]
    fn equal_update_and_death_windows_are_rejected() {
        assert!(
            run_args(&["--update-interval", "20", "--dead-after", "20"])
                .policy()
                .is_err()
        );
    }

    #[test]
    fn a_threshold_outside_zero_to_one_is_rejected() {
        assert!(
            run_args(&["--limit-threshold", "90"])
                .limit_threshold()
                .is_err()
        );
        assert!(
            run_args(&["--limit-threshold", "0.95"])
                .limit_threshold()
                .is_ok()
        );
    }

    #[test]
    fn the_api_binds_to_localhost_only() {
        let args = run_args(&["--api-port", "9000"]);

        assert_eq!(args.api_address().to_string(), "127.0.0.1:9000");
        assert_eq!(args.api_url(), "http://127.0.0.1:9000");
    }

    #[test]
    fn the_state_file_lives_under_the_xdg_state_directory() {
        let path = default_state_path();

        assert!(path.ends_with("joop/state.json"), "got: {}", path.display());
    }

    #[test]
    fn reporting_takes_an_issue_a_status_and_an_optional_detail() {
        let cli = Cli::parse_from([
            "joop",
            "report",
            "--issue",
            "42",
            "--status",
            "working",
            "--detail",
            "writing tests",
        ]);

        match cli.command {
            Some(Command::Report(args)) => {
                assert_eq!(args.issue, 42);
                assert_eq!(args.status, "working");
                assert_eq!(args.detail.as_deref(), Some("writing tests"));
                assert_eq!(args.api_url, "http://127.0.0.1:7777");
            }
            other => panic!("expected a report command, got {other:?}"),
        }
    }

    #[test]
    fn running_is_the_default_command() {
        assert!(Cli::parse_from(["joop"]).command.is_none());
    }

    #[test]
    fn a_zero_poll_interval_is_rejected() {
        assert!(run_args(&["--poll-interval", "0"]).poll_interval().is_err());
    }
}
