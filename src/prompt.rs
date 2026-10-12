use crate::issue::Issue;
use crate::state::HeartbeatPolicy;

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
    /// The reporting deadlines the agent must meet.
    pub policy: &'a HeartbeatPolicy,
    /// 1 for the first agent on this issue, 2 for a replacement, and so on.
    pub attempt: u32,
}

/// Build the opening message handed to the agent.
pub fn build_prompt(briefing: &Briefing<'_>) -> String {
    let Briefing {
        issue,
        repo,
        skill_command,
        api_url,
        policy,
        attempt,
    } = briefing;
    let first_update_minutes = policy.first_update.num_minutes();
    let update_interval_minutes = policy.subsequent_updates.num_minutes();
    let review_interval_minutes = policy.review_updates.num_minutes();
    let dead_after_minutes = policy.dead_after.num_minutes();
    let number = issue.number;

    let mut prompt = format!(
        "{skill_command} Implement GitHub issue {repo}#{number}: {title}\n\nIssue URL: {url}",
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
        "\n\n---\nYou are being supervised by joop, which is tracking this issue. Reporting is \
         mandatory.\n\
         \n\
         While you are working:\n\
         - Report within {first_update_minutes} minutes of starting, and then at least every \
         {update_interval_minutes} minutes.\n\
         - Report by running: `joop report --issue {number} --status <status> --detail \"<what \
         you are doing>\"`\n\
         - Statuses: `started` when you begin, `working` for progress, `blocked` if you need a \
         human, `review` once your pull request is open, `merged` once it has landed, `done` if \
         you finished without opening one, `failed` if you cannot continue.\n\
         - If joop hears nothing for {dead_after_minutes} minutes it closes this tab and gives \
         the issue to another agent, so report before any long-running step.\n\
         \n\
         Once you open a pull request:\n\
         - Report it immediately, with its URL, so joop can show it against this agent:\n\
         `joop report --issue {number} --status review --pull-request <url> --detail \"opened \
         for review\"`\n\
         - Then check it every {review_interval_minutes} minutes for reviews and failing \
         checks: `gh pr view <url> --json reviewDecision,reviews,statusCheckRollup`. Address \
         review comments and fix failing checks as you find them.\n\
         - Report each check with `--status review`; that is also your check-in, so while a \
         pull request is open joop expects to hear from you every {review_interval_minutes} \
         minutes rather than every {update_interval_minutes}. You need not repeat \
         `--pull-request` after the first report.\n\
         - As soon as it is merged, report `--status merged`. joop closes this tab the moment \
         you do, which kills that command before it prints anything — that is expected, not a \
         failure. Make it your last action.\n\
         \n\
         joop's API is at {api_url} if you would rather POST to /report directly.\n\
         Start by reporting `started`, then work the issue.\n"
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

    fn briefing<'a>(issue: &'a Issue, policy: &'a HeartbeatPolicy, attempt: u32) -> Briefing<'a> {
        Briefing {
            issue,
            repo: "jaywhy13/joop",
            skill_command: "/implement",
            api_url: "http://127.0.0.1:7777",
            policy,
            attempt,
        }
    }

    #[test]
    fn leads_with_the_skill_command() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, &HeartbeatPolicy::default(), 1));

        assert!(
            prompt.starts_with("/implement "),
            "prompt should invoke the skill first, got: {prompt}"
        );
    }

    #[test]
    fn carries_the_issue_number_title_url_and_body() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, &HeartbeatPolicy::default(), 1));

        assert!(prompt.contains("jaywhy13/joop#42"));
        assert!(prompt.contains("Add a --dry-run flag"));
        assert!(prompt.contains("https://github.com/jaywhy13/joop/issues/42"));
        assert!(prompt.contains("Preview dispatches without spawning tabs."));
    }

    #[test]
    fn omits_the_body_section_when_the_issue_has_no_body() {
        let mut issue = issue();
        issue.body = "   ".into();

        let prompt = build_prompt(&briefing(&issue, &HeartbeatPolicy::default(), 1));

        assert!(!prompt.contains("Issue body:"), "got: {prompt}");
    }

    #[test]
    fn tells_the_agent_to_report_its_pull_request_with_a_url() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, &HeartbeatPolicy::default(), 1));

        assert!(
            prompt.contains("--status review --pull-request <url>"),
            "joop can only show a pull request it was told about, got: {prompt}"
        );
    }

    #[test]
    fn tells_the_agent_to_poll_for_reviews_every_five_minutes() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, &HeartbeatPolicy::default(), 1));

        assert!(prompt.contains("every 5 minutes"), "got: {prompt}");
        assert!(
            prompt.contains("gh pr view"),
            "the agent needs a way to look, got: {prompt}"
        );
    }

    #[test]
    fn still_tells_the_agent_how_to_finish_without_a_pull_request() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, &HeartbeatPolicy::default(), 1));

        assert!(prompt.contains("`done`"), "got: {prompt}");
    }

    #[test]
    fn tells_the_agent_the_merged_report_will_be_killed_mid_flight() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, &HeartbeatPolicy::default(), 1));

        assert!(
            prompt.contains("that is expected"),
            "an agent that does not know this will read the kill as a failure, got: {prompt}"
        );
    }

    #[test]
    fn tells_the_agent_to_report_a_merge_last_because_its_tab_closes() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, &HeartbeatPolicy::default(), 1));

        assert!(prompt.contains("--status merged"));
        assert!(
            prompt.contains("closes this tab"),
            "an agent that does not know this will lose its terminal mid-sentence, got: {prompt}"
        );
    }

    #[test]
    fn spells_out_the_reporting_deadlines() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, &HeartbeatPolicy::default(), 1));

        assert!(prompt.contains("within 2 minutes"));
        assert!(prompt.contains("every 20 minutes"));
        assert!(prompt.contains("30 minutes"));
    }

    #[test]
    fn gives_the_exact_command_for_reporting_back() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, &HeartbeatPolicy::default(), 1));

        assert!(
            prompt.contains("joop report --issue 42 --status <status>"),
            "the agent needs a copy-pasteable command, got: {prompt}"
        );
        assert!(prompt.contains("http://127.0.0.1:7777"));
    }

    #[test]
    fn a_first_attempt_is_not_told_about_previous_work() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, &HeartbeatPolicy::default(), 1));

        assert!(!prompt.contains("attempt"), "got: {prompt}");
    }

    #[test]
    fn a_replacement_agent_is_told_to_build_on_the_partial_work() {
        let issue = issue();
        let prompt = build_prompt(&briefing(&issue, &HeartbeatPolicy::default(), 2));

        assert!(prompt.contains("attempt 2"));
        assert!(prompt.contains("git status"));
    }
}
