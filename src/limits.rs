use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;

/// Claude's usage endpoint, as read by Claude Code's own `/usage`.
const USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// How much of a Claude usage limit is gone, and when it comes back.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitSnapshot {
    /// The most-consumed limit, 0.0 to 1.0.
    pub utilization: f64,
    /// Which limit that was, e.g. `session` or `weekly_all`.
    pub limit: String,
    /// When that limit resets, if the endpoint said.
    pub resets_at: Option<DateTime<Utc>>,
}

impl LimitSnapshot {
    /// Unconstrained, used when joop cannot read the usage endpoint. joop keeps working
    /// rather than stalling on a reading it could not take.
    pub fn unknown() -> Self {
        Self {
            utilization: 0.0,
            limit: "unknown".into(),
            resets_at: None,
        }
    }

    /// True once usage has reached `threshold` (a fraction, e.g. 0.9).
    pub fn is_exhausted(&self, threshold: f64) -> bool {
        self.utilization >= threshold
    }

    pub fn percent(&self) -> f64 {
        self.utilization * 100.0
    }
}

/// Reads how much Claude capacity is left.
pub trait LimitWatcher: Send + Sync {
    fn snapshot(&self) -> Result<LimitSnapshot>;
}

/// Reads Claude Code's OAuth credentials and asks Claude for current usage.
///
/// The usage endpoint is internal to Claude Code and undocumented, so every failure here
/// is reported as an error for the caller to treat as "unknown" rather than "exhausted".
pub struct ClaudeLimits {
    credentials_path: PathBuf,
    endpoint: String,
}

impl ClaudeLimits {
    pub fn new(credentials_path: impl Into<PathBuf>) -> Self {
        Self {
            credentials_path: credentials_path.into(),
            endpoint: USAGE_ENDPOINT.to_string(),
        }
    }

    /// Default location of Claude Code's credentials.
    pub fn default_credentials_path() -> PathBuf {
        home_dir().join(".claude").join(".credentials.json")
    }
}

impl LimitWatcher for ClaudeLimits {
    fn snapshot(&self) -> Result<LimitSnapshot> {
        let token = access_token(&self.credentials_path)?;

        let body = ureq::get(&self.endpoint)
            .header("Authorization", format!("Bearer {token}"))
            .header("anthropic-beta", OAUTH_BETA)
            .config()
            .timeout_global(Some(StdDuration::from_secs(15)))
            .build()
            .call()
            .context("could not reach the Claude usage endpoint")?
            .body_mut()
            .read_to_string()
            .context("could not read the Claude usage response")?;

        parse_usage(&body)
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Debug, Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OauthCredentials>,
}

#[derive(Debug, Deserialize)]
struct OauthCredentials {
    #[serde(rename = "accessToken")]
    access_token: String,
}

fn access_token(path: &Path) -> Result<String> {
    let contents = std::fs::read_to_string(path).with_context(|| {
        format!(
            "could not read Claude credentials at {} — run `claude auth` or pass --no-limit-checks",
            path.display()
        )
    })?;

    let credentials: Credentials = serde_json::from_str(&contents)
        .with_context(|| format!("{} is not a Claude credentials file", path.display()))?;

    match credentials.oauth {
        Some(oauth) => Ok(oauth.access_token),
        None => bail!(
            "{} holds no Claude subscription credentials",
            path.display()
        ),
    }
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    #[serde(default)]
    limits: Vec<UsageLimit>,
}

#[derive(Debug, Deserialize)]
struct UsageLimit {
    kind: String,
    /// Percentage consumed, 0-100.
    #[serde(default)]
    percent: f64,
    #[serde(default)]
    resets_at: Option<DateTime<Utc>>,
}

/// Pull the tightest limit out of a usage response.
pub fn parse_usage(json: &str) -> Result<LimitSnapshot> {
    let response: UsageResponse =
        serde_json::from_str(json).context("unexpected shape from the Claude usage endpoint")?;

    let tightest = response
        .limits
        .into_iter()
        .max_by(|a, b| a.percent.total_cmp(&b.percent));

    match tightest {
        Some(limit) => Ok(LimitSnapshot {
            utilization: (limit.percent / 100.0).clamp(0.0, 1.0),
            limit: limit.kind,
            resets_at: limit.resets_at,
        }),
        None => Ok(LimitSnapshot::unknown()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const USAGE: &str = r#"{
      "limits": [
        {"kind": "session", "group": "session", "percent": 7, "severity": "normal",
         "resets_at": "2026-10-12T07:19:59.835533+00:00", "is_active": true},
        {"kind": "weekly_all", "group": "weekly", "percent": 93, "severity": "warning",
         "resets_at": "2026-10-16T16:59:59.835558+00:00", "is_active": false}
      ]
    }"#;

    #[test]
    fn reports_the_most_consumed_limit() {
        let snapshot = parse_usage(USAGE).unwrap();

        assert_eq!(snapshot.limit, "weekly_all");
        assert!((snapshot.utilization - 0.93).abs() < 1e-9);
        assert_eq!(
            snapshot.resets_at,
            Some(
                Utc.with_ymd_and_hms(2026, 10, 16, 16, 59, 59).unwrap()
                    + chrono::Duration::nanoseconds(835558000)
            )
        );
    }

    #[test]
    fn a_response_without_limits_reads_as_unknown() {
        let snapshot = parse_usage(r#"{"limits": []}"#).unwrap();

        assert_eq!(snapshot, LimitSnapshot::unknown());
    }

    #[test]
    fn an_unexpected_shape_is_an_error_not_a_zero_reading() {
        assert!(
            parse_usage(r#"{"limits": "nope"}"#).is_err(),
            "a shape change must not look like plenty of capacity"
        );
    }

    #[test]
    fn exhaustion_is_measured_against_the_threshold() {
        let snapshot = parse_usage(USAGE).unwrap();

        assert!(snapshot.is_exhausted(0.9));
        assert!(!snapshot.is_exhausted(0.95));
    }

    #[test]
    fn an_unknown_snapshot_is_never_exhausted() {
        assert!(!LimitSnapshot::unknown().is_exhausted(0.9));
    }

    #[test]
    fn reads_the_access_token_from_claude_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-xyz","subscriptionType":"pro"}}"#,
        )
        .unwrap();

        assert_eq!(access_token(&path).unwrap(), "sk-ant-oat01-xyz");
    }

    #[test]
    fn a_credentials_file_without_oauth_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, "{}").unwrap();

        assert!(access_token(&path).is_err());
    }

    #[test]
    fn missing_credentials_point_at_the_fix() {
        let error = access_token(Path::new("/nonexistent/.credentials.json")).unwrap_err();

        assert!(
            format!("{error:#}").contains("--no-limit-checks"),
            "got: {error:#}"
        );
    }
}
