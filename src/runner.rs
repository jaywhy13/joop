use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// One external command joop wants to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl Invocation {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: None,
        }
    }

    pub fn in_dir(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Readable form for logs and error messages.
    pub fn display(&self) -> String {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
}

impl CommandOutput {
    pub fn succeeded(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            code: Some(0),
        }
    }

    pub fn failed(code: i32, stderr: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
            code: Some(code),
        }
    }

    pub fn is_success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Seam over process execution, so every adapter below can be tested on the exact
/// command line it would have run rather than by running `gh`, `git` or `herdr`.
pub trait CommandRunner: Send + Sync {
    fn run(&self, invocation: &Invocation) -> Result<CommandOutput>;

    /// Run a command and fail unless it exited zero, returning trimmed stdout.
    fn run_checked(&self, invocation: &Invocation) -> Result<String> {
        let output = self.run(invocation)?;
        if !output.is_success() {
            bail!(
                "`{}` failed{}: {}",
                invocation.display(),
                output
                    .code
                    .map(|code| format!(" with exit code {code}"))
                    .unwrap_or_else(|| " (killed by a signal)".to_string()),
                output.stderr.trim()
            );
        }
        Ok(output.stdout.trim().to_string())
    }
}

impl<T: CommandRunner + ?Sized> CommandRunner for &T {
    fn run(&self, invocation: &Invocation) -> Result<CommandOutput> {
        (**self).run(invocation)
    }
}

/// Runs commands for real.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, invocation: &Invocation) -> Result<CommandOutput> {
        let mut command = Command::new(&invocation.program);
        command.args(&invocation.args);
        if let Some(cwd) = &invocation.cwd {
            command.current_dir(cwd);
        }

        let output = command
            .output()
            .with_context(|| format!("could not run `{}`", invocation.display()))?;

        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            code: output.status.code(),
        })
    }
}

#[cfg(test)]
pub mod testing {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    /// Test double that records every invocation and replays queued outputs.
    #[derive(Debug, Default)]
    pub struct RecordingRunner {
        calls: Mutex<Vec<Invocation>>,
        responses: Mutex<VecDeque<CommandOutput>>,
    }

    impl RecordingRunner {
        pub fn new() -> Self {
            Self::default()
        }

        /// Queue the outputs to return, in order. Once exhausted, a bare success is returned.
        pub fn returning(outputs: impl IntoIterator<Item = CommandOutput>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(outputs.into_iter().collect()),
            }
        }

        pub fn calls(&self) -> Vec<Invocation> {
            self.calls.lock().unwrap().clone()
        }

        /// The command lines that were run, one string per invocation.
        pub fn command_lines(&self) -> Vec<String> {
            self.calls().iter().map(Invocation::display).collect()
        }

        pub fn last_call(&self) -> Invocation {
            self.calls().last().cloned().expect("a command was run")
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, invocation: &Invocation) -> Result<CommandOutput> {
            self.calls.lock().unwrap().push(invocation.clone());
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| CommandOutput::succeeded("")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::RecordingRunner;
    use super::*;

    #[test]
    fn run_checked_returns_trimmed_stdout() {
        let runner = RecordingRunner::returning([CommandOutput::succeeded("  hello\n")]);

        let stdout = runner
            .run_checked(&Invocation::new("echo", ["hello"]))
            .unwrap();

        assert_eq!(stdout, "hello");
    }

    #[test]
    fn run_checked_surfaces_the_command_and_stderr_on_failure() {
        let runner = RecordingRunner::returning([CommandOutput::failed(2, "no such label")]);

        let error = runner
            .run_checked(&Invocation::new("gh", ["issue", "list"]))
            .expect_err("non-zero exit is an error");

        let message = error.to_string();
        assert!(message.contains("gh issue list"), "got: {message}");
        assert!(message.contains("no such label"), "got: {message}");
        assert!(message.contains("exit code 2"), "got: {message}");
    }

    #[test]
    fn the_system_runner_captures_stdout_and_exit_codes() {
        let runner = SystemRunner;

        let output = runner
            .run(&Invocation::new(
                "sh",
                ["-c", "echo out; echo err >&2; exit 3"],
            ))
            .unwrap();

        assert_eq!(output.stdout.trim(), "out");
        assert_eq!(output.stderr.trim(), "err");
        assert_eq!(output.code, Some(3));
    }
}
