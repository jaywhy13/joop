use anyhow::{Context, Result};
use serde::Deserialize;

/// A GitHub issue, as much of it as `gh issue list --json` gives us.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub url: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub body: String,
    #[serde(default, deserialize_with = "label_names")]
    pub labels: Vec<String>,
}

impl Issue {
    /// Stable identifier used for the branch, worktree directory, herdr tab label
    /// and agent name.
    pub fn slug(&self) -> String {
        format!("issue-{}", self.number)
    }
}

/// Parse the JSON array written by `gh issue list --json number,title,url,body,labels`.
pub fn parse_issue_list(json: &str) -> Result<Vec<Issue>> {
    serde_json::from_str(json).context("could not parse the issue list returned by `gh`")
}

fn null_as_empty<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

fn label_names<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Label {
        name: String,
    }

    let labels = Vec::<Label>::deserialize(deserializer)?;
    Ok(labels.into_iter().map(|label| label.name).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GH_OUTPUT: &str = r#"[
      {
        "number": 42,
        "title": "Add a --dry-run flag",
        "url": "https://github.com/jaywhy13/joop/issues/42",
        "body": "It would be nice to preview dispatches.",
        "labels": [{"name": "loop"}, {"name": "enhancement"}]
      },
      {
        "number": 7,
        "title": "Fix the poll interval",
        "url": "https://github.com/jaywhy13/joop/issues/7",
        "body": "",
        "labels": [{"name": "loop"}]
      }
    ]"#;

    #[test]
    fn parses_gh_issue_list_output() {
        let issues = parse_issue_list(GH_OUTPUT).expect("valid gh output");

        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 42);
        assert_eq!(issues[0].title, "Add a --dry-run flag");
        assert_eq!(issues[0].url, "https://github.com/jaywhy13/joop/issues/42");
        assert_eq!(issues[0].body, "It would be nice to preview dispatches.");
        assert_eq!(issues[0].labels, vec!["loop", "enhancement"]);
        assert_eq!(issues[1].number, 7);
        assert_eq!(issues[1].body, "");
    }

    #[test]
    fn tolerates_a_null_body() {
        let issues = parse_issue_list(
            r#"[{"number": 1, "title": "t", "url": "u", "body": null, "labels": []}]"#,
        )
        .expect("null body is valid");

        assert_eq!(issues[0].body, "");
    }

    #[test]
    fn rejects_output_that_is_not_an_issue_list() {
        assert!(parse_issue_list("not json").is_err());
    }

    #[test]
    fn branch_and_tab_names_are_derived_from_the_number() {
        let issue = Issue {
            number: 42,
            title: "Add a --dry-run flag".into(),
            url: "https://github.com/jaywhy13/joop/issues/42".into(),
            body: String::new(),
            labels: vec![],
        };

        assert_eq!(issue.slug(), "issue-42");
    }
}
