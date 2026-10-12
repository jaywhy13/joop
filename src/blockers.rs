use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::github::IssueState;
use crate::runner::{CommandRunner, Invocation};

/// The model that judges whether a held issue has been unblocked. The judgement is cheap
/// and runs once an hour, so the smallest model is enough.
pub const JUDGE_MODEL: &str = "claude-haiku-5-5";

/// Longest issue description sent to the judge; anything past it is cut off.
const MAX_BODY_CHARS: usize = 4_000;

/// How many waiting issues go into one question to the judge.
const BATCH_SIZE: usize = 10;

/// An issue an agent stopped on because other issues must land first, with the current
/// state of each of those blockers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingIssue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub blockers: Vec<IssueState>,
}

/// Decides which waiting issues can now start. This is a language-model call rather than a
/// lookup because a blocker counts as resolved when its work has landed, which a closed
/// issue does not always show, and the issue text is what says so.
pub trait BlockerJudge: Send + Sync {
    /// The numbers, taken from `waiting`, of the issues that are no longer blocked.
    fn unblocked(&self, waiting: &[WaitingIssue]) -> Result<Vec<u64>>;
}

/// Asks Claude through its command line, so joop uses whatever login `claude` already has.
pub struct ClaudeJudge<R: CommandRunner> {
    runner: R,
    model: String,
}

impl<R: CommandRunner> ClaudeJudge<R> {
    pub fn new(runner: R, model: impl Into<String>) -> Self {
        Self {
            runner,
            model: model.into(),
        }
    }
}

impl<R: CommandRunner> BlockerJudge for ClaudeJudge<R> {
    fn unblocked(&self, waiting: &[WaitingIssue]) -> Result<Vec<u64>> {
        if waiting.is_empty() {
            return Ok(Vec::new());
        }

        // The question travels as one command-line argument, which the operating system caps
        // in size, so a long list of waiting issues is judged in batches.
        let mut unblocked = Vec::new();
        for batch in waiting.chunks(BATCH_SIZE) {
            let question = build_question(batch);
            let invocation = Invocation::new("claude", ["--model", &self.model, "-p", &question]);

            let answer = self
                .runner
                .run_checked(&invocation)
                .context("could not ask Claude whether any waiting issue is unblocked")?;

            unblocked.extend(parse_verdict(&answer)?);
        }

        Ok(unblocked)
    }
}

/// The question put to the judge: what counts as unblocked, what to reply with, and the
/// facts it needs for each waiting issue.
pub fn build_question(waiting: &[WaitingIssue]) -> String {
    let mut question = String::from(
        "You are checking whether GitHub issues that are waiting on other issues can start now.\n\
         \n\
         An issue is unblocked when every issue it is blocked by has been resolved: its work has \
         landed on the main branch, or it was closed with the blocking work done. The issue \
         descriptions say what each one needs. An issue with any blocker still open is still \
         blocked.\n\
         \n\
         Reply with only a JSON object and nothing else, in this form: \
         {\"unblocked\": [<issue numbers>]}. Use an empty list when none are unblocked.\n",
    );

    for issue in waiting {
        question.push_str(&format!("\n## Issue #{}: {}\n", issue.number, issue.title));

        let body = issue.body.trim();
        if body.is_empty() {
            question.push_str("Description: (none)\n");
        } else {
            question.push_str("Description:\n");
            question.extend(body.chars().take(MAX_BODY_CHARS));
            question.push('\n');
        }

        question.push_str("Blocked by:\n");
        for blocker in &issue.blockers {
            let state = if blocker.open { "open" } else { "closed" };
            question.push_str(&format!(
                "- #{} \"{}\" ({state})\n",
                blocker.number, blocker.title
            ));
        }
    }

    question
}

#[derive(Deserialize)]
struct Verdict {
    unblocked: Vec<u64>,
}

/// Read the judge's answer. The model is asked for bare JSON but may still wrap it in prose,
/// so the object is taken from the first `{` to the last `}`.
pub fn parse_verdict(answer: &str) -> Result<Vec<u64>> {
    let start = answer
        .find('{')
        .ok_or_else(|| anyhow!("Claude's answer had no JSON object: {answer}"))?;
    let end = answer
        .rfind('}')
        .filter(|end| *end >= start)
        .ok_or_else(|| anyhow!("Claude's answer had no closing brace: {answer}"))?;

    let verdict: Verdict = serde_json::from_str(&answer[start..=end])
        .with_context(|| format!("could not read Claude's answer: {answer}"))?;
    Ok(verdict.unblocked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CommandOutput;
    use crate::runner::testing::RecordingRunner;

    fn waiting_on_87_and_89() -> WaitingIssue {
        WaitingIssue {
            number: 92,
            title: "Add the recipe list page".into(),
            body: "Needs the import service.".into(),
            blockers: vec![
                IssueState {
                    number: 87,
                    title: "Recipe import".into(),
                    open: false,
                },
                IssueState {
                    number: 89,
                    title: "Typed failures".into(),
                    open: true,
                },
            ],
        }
    }

    #[test]
    fn the_question_names_each_blocker_with_its_state() {
        let question = build_question(&[waiting_on_87_and_89()]);

        assert!(question.contains("#92: Add the recipe list page"));
        assert!(question.contains("- #87 \"Recipe import\" (closed)"));
        assert!(question.contains("- #89 \"Typed failures\" (open)"));
    }

    #[test]
    fn the_question_asks_for_bare_json() {
        let question = build_question(&[waiting_on_87_and_89()]);

        assert!(question.contains("{\"unblocked\": [<issue numbers>]}"));
    }

    #[test]
    fn an_overlong_description_is_cut_short() {
        let mut issue = waiting_on_87_and_89();
        issue.body = "x".repeat(MAX_BODY_CHARS + 500);

        let question = build_question(&[issue]);

        assert!(question.matches('x').count() == MAX_BODY_CHARS);
    }

    #[test]
    fn reads_the_numbers_out_of_a_bare_answer() {
        assert_eq!(
            parse_verdict(r#"{"unblocked": [92, 93]}"#).unwrap(),
            vec![92, 93]
        );
    }

    #[test]
    fn reads_the_numbers_out_of_an_answer_wrapped_in_prose() {
        let answer = "Issue 92 can start.\n```json\n{\"unblocked\": [92]}\n```";

        assert_eq!(parse_verdict(answer).unwrap(), vec![92]);
    }

    #[test]
    fn an_empty_list_means_nothing_is_unblocked() {
        assert!(parse_verdict(r#"{"unblocked": []}"#).unwrap().is_empty());
    }

    #[test]
    fn an_answer_without_json_is_an_error() {
        assert!(parse_verdict("I cannot tell.").is_err());
    }

    #[test]
    fn an_answer_with_the_wrong_shape_is_an_error() {
        assert!(parse_verdict(r#"{"ready": [92]}"#).is_err());
    }

    #[test]
    fn asks_claude_once_with_the_model_and_question() {
        let runner =
            RecordingRunner::returning([CommandOutput::succeeded(r#"{"unblocked": [92]}"#)]);
        let judge = ClaudeJudge::new(&runner, JUDGE_MODEL);

        let unblocked = judge.unblocked(&[waiting_on_87_and_89()]).unwrap();

        assert_eq!(unblocked, vec![92]);
        let call = runner.last_call();
        assert_eq!(call.program, "claude");
        assert_eq!(call.args[0..2], ["--model", "claude-haiku-5-5"]);
        assert_eq!(call.args[2], "-p");
    }

    #[test]
    fn a_long_list_of_waiting_issues_is_judged_in_batches() {
        let waiting: Vec<WaitingIssue> = (1..=(BATCH_SIZE as u64 + 1))
            .map(|number| WaitingIssue {
                number,
                title: "t".into(),
                body: String::new(),
                blockers: Vec::new(),
            })
            .collect();
        let runner = RecordingRunner::returning([
            CommandOutput::succeeded(r#"{"unblocked": [1]}"#),
            CommandOutput::succeeded(r#"{"unblocked": [11]}"#),
        ]);
        let judge = ClaudeJudge::new(&runner, JUDGE_MODEL);

        let unblocked = judge.unblocked(&waiting).unwrap();

        assert_eq!(unblocked, vec![1, 11]);
        assert_eq!(runner.calls().len(), 2, "one model call per batch");
    }

    #[test]
    fn asks_nothing_when_no_issue_is_waiting() {
        let runner = RecordingRunner::new();
        let judge = ClaudeJudge::new(&runner, JUDGE_MODEL);

        assert!(judge.unblocked(&[]).unwrap().is_empty());
        assert!(
            runner.calls().is_empty(),
            "no model call without waiting issues"
        );
    }

    #[test]
    fn a_failed_model_call_is_an_error_not_an_empty_answer() {
        let runner = RecordingRunner::returning([CommandOutput::failed(1, "not logged in")]);
        let judge = ClaudeJudge::new(&runner, JUDGE_MODEL);

        assert!(judge.unblocked(&[waiting_on_87_and_89()]).is_err());
    }
}
