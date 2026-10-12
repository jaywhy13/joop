use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use clap::Parser;
use joop::api::{self, ApiContext, Report};
use joop::clock::{Clock, SystemClock};
use joop::config::{Cli, Command, ForgetArgs, ReportArgs, RunArgs, StatusArgs};
use joop::engine::{Engine, EngineSettings};
use joop::github::{GhIssues, IssueSource, repo_in_dir, verify_repo};
use joop::herdr::{AgentHost, HerdrAgents};
use joop::limits::{ClaudeLimits, LimitWatcher};
use joop::runner::SystemRunner;
use joop::state::State;
use joop::supervisor::Supervisor;
use joop::ui::{Dashboard, relative, truncate};
use joop::worktree::{GitWorktrees, WorktreeProvider, default_root};

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match cli.command {
        Some(Command::Report(args)) => report(args),
        Some(Command::Status(args)) => status(args),
        Some(Command::Forget(args)) => forget(args),
        Some(Command::Run(args)) => run(args),
        None => run(cli.run),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("joop: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// `joop report` — what each Pi session runs to check in.
fn report(args: ReportArgs) -> Result<()> {
    let body = joop::client::send_report(
        &args.api_url,
        &Report {
            issue: args.issue,
            status: args.status,
            detail: args.detail,
            pull_request: args.pull_request,
        },
    )?;

    println!("{body}");
    Ok(())
}

/// `joop status` — print the sessions without starting the dashboard.
fn status(args: StatusArgs) -> Result<()> {
    let path = args
        .state_file
        .unwrap_or_else(joop::config::default_state_path);
    let state = State::load(&path)?;
    let now = SystemClock.now();

    if state.sessions().is_empty() {
        println!("No sessions in {}", path.display());
        return Ok(());
    }

    println!(
        "{:<8} {:<34} {:<11} {:<8} {:<14} {:<13} LATEST",
        "ISSUE", "TITLE", "STATUS", "PR", "TAB", "LAST UPDATE"
    );
    for session in state.sessions().values() {
        println!(
            "{:<8} {:<34} {:<11} {:<8} {:<14} {:<13} {}",
            format!("#{}", session.issue_number),
            truncate(&session.issue_title, 34),
            session.status.as_str(),
            session
                .pull_request
                .as_ref()
                .map(|pull_request| format!("#{}", pull_request.number))
                .unwrap_or_else(|| "-".into()),
            truncate(&session.agent_name, 14),
            session
                .last_update_at
                .map(|at| relative(at, now))
                .unwrap_or_else(|| "never".into()),
            truncate(session.detail.as_deref().unwrap_or("-"), 50)
        );
    }

    Ok(())
}

/// `joop forget` — drop joop's record of an issue so it can be dispatched again.
fn forget(args: ForgetArgs) -> Result<()> {
    let path = args
        .state_file
        .unwrap_or_else(joop::config::default_state_path);
    let repo = match args.repo {
        Some(repo) => repo,
        None => repo_in_dir(&SystemRunner, &std::env::current_dir()?)
            .context("no repository given: pass --repo owner/name")?,
    };

    let mut state = State::load(&path)?;
    if state.forget(&repo, args.issue)? {
        println!("Forgot {repo}#{}; joop will pick it up again.", args.issue);
    } else {
        println!("joop has no record of {repo}#{}.", args.issue);
    }
    Ok(())
}

/// `joop` / `joop run` — watch the repository and supervise the agents.
fn run(args: RunArgs) -> Result<()> {
    let policy = args.policy()?;
    let poll_interval = args.poll_interval()?;
    let limit_threshold = args.limit_threshold()?;
    let max_attempts = args.max_attempts()?;
    let repo_path = args.canonical_repo_path()?;
    let state_path = args.state_path();

    init_logging(&state_path, args.headless)?;

    let runner = SystemRunner;
    let repo = resolve_repo(&args, &repo_path)?;
    verify_repo(&runner, &repo)?;

    let state = Arc::new(Mutex::new(State::load(&state_path)?));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let issues: Arc<dyn IssueSource> =
        Arc::new(GhIssues::new(runner, repo.clone(), args.issue_limit));
    let worktree_root = args
        .worktree_root
        .clone()
        .unwrap_or_else(|| default_root(&repo_path));
    let worktrees: Arc<dyn WorktreeProvider> =
        Arc::new(GitWorktrees::new(runner, &repo_path, &worktree_root));
    let agents: Arc<dyn AgentHost> = Arc::new(HerdrAgents::new(runner, &args.workspace));

    let limits: Option<Arc<dyn LimitWatcher>> = if args.no_limit_checks {
        None
    } else {
        Some(Arc::new(ClaudeLimits::new(
            ClaudeLimits::default_credentials_path(),
        )))
    };

    let engine = Arc::new(Engine::new(
        EngineSettings {
            repo: repo.clone(),
            label: args.label.clone(),
            skill_command: args.skill.clone(),
            api_url: args.api_url(),
            agent_kind: args.agent.clone(),
            agent_args: args.agent_args.clone(),
            policy,
            limit_threshold,
            max_attempts,
        },
        issues,
        worktrees,
        agents.clone(),
        limits,
        clock.clone(),
        state.clone(),
    ));

    api::serve(
        args.api_address(),
        ApiContext {
            reporter: engine.clone(),
        },
    )?;

    let supervisor = Supervisor::start(engine.clone(), poll_interval);

    if args.headless {
        println!(
            "joop is watching {repo} for `{}` issues; agents report to {}",
            args.label,
            args.api_url()
        );
        // The supervisor thread does the work. Watch that it is still alive rather than
        // parking forever, so a crashed supervisor does not look like a healthy joop.
        while supervisor.is_running() {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        bail!("the joop supervisor stopped unexpectedly; see the log for the cause");
    }

    Dashboard::new(engine, agents).run(&supervisor)?;
    supervisor.stop();
    Ok(())
}

/// Take the repository from the command line, from the checkout, or ask for it.
fn resolve_repo(args: &RunArgs, repo_path: &std::path::Path) -> Result<String> {
    if let Some(repo) = &args.repo {
        return Ok(repo.trim().to_string());
    }

    let detected = repo_in_dir(&SystemRunner, repo_path).ok();

    if !io::stdin().is_terminal() {
        return detected.context(
            "no repository given: pass --repo owner/name (joop cannot ask when stdin is not a terminal)",
        );
    }

    match &detected {
        Some(repo) => {
            let answer = ask(&format!("Watch {repo} for labelled issues? [Y/n/other] "))?;
            match answer.trim() {
                "" | "y" | "Y" | "yes" => Ok(repo.clone()),
                "n" | "N" | "no" => ask_for_repo(),
                other => Ok(other.to_string()),
            }
        }
        None => ask_for_repo(),
    }
}

fn ask_for_repo() -> Result<String> {
    let answer = ask("Which repository should joop watch? (owner/name) ")?;
    let repo = answer.trim().to_string();

    if repo.is_empty() {
        bail!("no repository given");
    }
    if repo.matches('/').count() != 1 || repo.starts_with('/') || repo.ends_with('/') {
        bail!("`{repo}` is not an owner/name repository");
    }

    Ok(repo)
}

fn ask(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush().ok();

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("could not read your answer")?;
    Ok(answer)
}

/// In dashboard mode the logs go to a file, because stderr would scribble over the UI.
fn init_logging(state_path: &std::path::Path, headless: bool) -> Result<()> {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_env("JOOP_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    if headless {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(io::stderr)
            .init();
        return Ok(());
    }

    let directory = state_path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    let log_path = directory.join("joop.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("could not open {}", log_path.display()))?;

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(file)
        .init();
    Ok(())
}
