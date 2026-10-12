use crate::issue::Issue;

/// Everything a spawned agent needs to know: the issue, how to report back, and how
/// often joop expects to hear from it.
#[derive(Debug, Clone)]
pub struct Briefing<'a> {
    pub issue: &'a Issue,
    pub repo: &'a str,
    /// Skill the agent should drive the work with, e.g. `/implement`.
    pub skill_command: &'a str,
    /// Base URL of joop's reporting API.
    pub api_url: &'a str,
    /// Minutes allowed before the first report.
    pub first_update_minutes: i64,
    /// Minutes allowed between later reports.
    pub update_interval_minutes: i64,
    /// Minutes of silence after which joop reassigns the issue.
    pub dead_after_minutes: i64,
    /// 1 for the first agent on this issue, 2 for a replacement, and so on.
    pub attempt: u32,
}

/// Build the opening message handed to Pi.
pub fn build_prompt(briefing: &Briefing<'_>) -> String {
    let Briefing {
        issue,
        repo,
        skill_command,
        api_url,
        first_update_minutes,
        update_interval_minutes,
        dead_after_minutes,
        attempt,
    } = briefing;

    let mut prompt = format!(
        "{skill_command} Implement GitHub issue {repo}#{number}: {title}\n\nIssue URL: {url}",
        number = issue.number,
        title = issue.title,
        url = issue.url,
    );

    let body = issue.body.trim();
    if !body.is_empty() {
        prompt.push_str("\n\nIssue body:\n");
        prompt.push_str(body);
    }

    if *attempt > 1 {
        prompt.push_str(&format!(
            "\n\nNote: you are attempt {attempt} on this issue. A previous agent went quiet and \
             was replaced. The worktree you are in may already contain its partial work — \
             check `git status` and `git log` before starting, and build on anything useful."
        ));
    }

    prompt.push_str(&format!(
        "\n\n---\nYou are being supervised by joop, which is tracking this issue. \
         Reporting is mandatory:\n\
         - Report within {first_update_minutes} minutes of starting, and then at least every \
         {update_interval_minutes} minutes.\n\
         - If joop hears nothing for {dead_after_minutes} minutes it will close this tab and give \
         the issue to another agent, so report before any long-running step.\n\
         - Report by running: `joop report --issue {number} --status <status> --detail \"<what you \
         are doing>\"`\n\
         - Statuses: `started` when you begin, `working` for progress, `blocked` if you need a \
         human, `review` once a pull request is open, `done` when it is merged or finished, \
         `failed` if you cannot continue.\n\
         - joop's API is at {api_url} if you would rather POST to /report directly.\n\
         Start by reporting `started`, then work the issue.\n",
        number = issue.number,
    ));

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue() -> Issue {
        Issue {
            number: 42,
            title: "Add a --dry-run flag".into(),
            url: "https://github.com/jaywhy13/joop/issues/42".into(),
            body: "Preview dispatches without spawning tabs.".into(),
            labels: vec!["loop".into()],
        }
    }

    fn briefing<'a>(issue: &'a Issue, attempt: u32) -> Briefing<'a> {
        Briefing {
            issue,
            repo: "jaywhy13/joop",
            skill_command: "/implement",
            api_url: "http://127.0.0.1:7777",
            first_update_minutes: 2,
            update_interval_minutes: 20,
            dead_after_minutes: 30,
            attempt,
        }
    }

    #[test]
    fn leads_with_the_skill_command() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, 1));

        assert!(
            prompt.starts_with("/implement "),
            "prompt should invoke the skill first, got: {prompt}"
        );
    }

    #[test]
    fn carries_the_issue_number_title_url_and_body() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, 1));

        assert!(prompt.contains("jaywhy13/joop#42"));
        assert!(prompt.contains("Add a --dry-run flag"));
        assert!(prompt.contains("https://github.com/jaywhy13/joop/issues/42"));
        assert!(prompt.contains("Preview dispatches without spawning tabs."));
    }

    #[test]
    fn omits_the_body_section_when_the_issue_has_no_body() {
        let mut issue = issue();
        issue.body = "   ".into();

        let prompt = build_prompt(&briefing(&issue, 1));

        assert!(!prompt.contains("Issue body:"), "got: {prompt}");
    }

    #[test]
    fn spells_out_the_reporting_deadlines() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, 1));

        assert!(prompt.contains("within 2 minutes"));
        assert!(prompt.contains("every 20 minutes"));
        assert!(prompt.contains("30 minutes"));
    }

    #[test]
    fn gives_the_exact_command_for_reporting_back() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, 1));

        assert!(
            prompt.contains("joop report --issue 42 --status <status>"),
            "the agent needs a copy-pasteable command, got: {prompt}"
        );
        assert!(prompt.contains("http://127.0.0.1:7777"));
    }

    #[test]
    fn a_first_attempt_is_not_told_about_previous_work() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, 1));

        assert!(!prompt.contains("attempt"), "got: {prompt}");
    }

    #[test]
    fn a_replacement_agent_is_told_to_build_on_the_partial_work() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, 2));

        assert!(prompt.contains("attempt 2"));
        assert!(prompt.contains("git status"));
    }
}
