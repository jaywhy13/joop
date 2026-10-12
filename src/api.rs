use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::{PullRequest, Session, Status};

/// A status report from an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub issue: u64,
    pub status: String,
    #[serde(default)]
    pub detail: Option<String>,
    /// The pull request the agent has opened, as a number or a GitHub URL.
    #[serde(default)]
    pub pull_request: Option<String>,
}

/// What the API hands an agent's report to. Implemented by the engine, which also acts on
/// it — showing the pull request on the agent's herdr pane, closing the tab once merged.
pub trait Reporter: Send + Sync {
    fn record_report(
        &self,
        issue_number: u64,
        status: Status,
        detail: Option<&str>,
        pull_request: Option<PullRequest>,
    ) -> anyhow::Result<()>;

    fn sessions(&self) -> Vec<Session>;
}

/// What the API sends back.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiResponse {
    pub code: u16,
    pub body: String,
}

impl ApiResponse {
    fn json(code: u16, body: impl Serialize) -> Self {
        Self {
            code,
            body: serde_json::to_string(&body).unwrap_or_else(|_| "{}".into()),
        }
    }

    fn error(code: u16, message: impl std::fmt::Display) -> Self {
        Self::json(
            code,
            serde_json::json!({"ok": false, "error": message.to_string()}),
        )
    }
}

/// Everything the API needs to serve a request.
pub struct ApiContext {
    pub reporter: Arc<dyn Reporter>,
}

/// Route and handle one request. Kept free of any HTTP machinery so it can be tested directly.
pub fn handle(context: &ApiContext, method: &str, path: &str, body: &str) -> ApiResponse {
    match (method, path) {
        ("GET", "/health") => ApiResponse::json(200, serde_json::json!({"ok": true})),
        ("GET", "/sessions") => ApiResponse::json(200, context.reporter.sessions()),
        ("POST", "/report") => handle_report(context, body),
        ("POST", _) | ("GET", _) => {
            ApiResponse::error(404, format!("no route for {method} {path}"))
        }
        _ => ApiResponse::error(405, format!("{method} is not allowed")),
    }
}

fn handle_report(context: &ApiContext, body: &str) -> ApiResponse {
    let report: Report = match serde_json::from_str(body) {
        Ok(report) => report,
        Err(error) => {
            return ApiResponse::error(400, format!("could not read the report: {error}"));
        }
    };

    let status: Status = match report.status.parse() {
        Ok(status) => status,
        Err(error) => return ApiResponse::error(400, error),
    };

    if !Status::reportable().contains(&status) {
        return ApiResponse::error(
            400,
            format!(
                "`{status}` is set by joop, not reported by agents; use one of: {}",
                Status::reportable()
                    .iter()
                    .map(Status::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }

    let pull_request = match report.pull_request.as_deref().map(parse_pull_request) {
        Some(Ok(pull_request)) => Some(pull_request),
        Some(Err(error)) => return ApiResponse::error(400, error),
        None => None,
    };

    match context.reporter.record_report(
        report.issue,
        status,
        report.detail.as_deref(),
        pull_request,
    ) {
        Ok(()) => ApiResponse::json(
            200,
            serde_json::json!({"ok": true, "issue": report.issue, "status": status.as_str()}),
        ),
        Err(error) => ApiResponse::error(404, format!("{error:#}")),
    }
}

/// Accept either a bare number or a GitHub pull request URL, since an agent has the URL to
/// hand and should not have to pick it apart.
pub fn parse_pull_request(reported: &str) -> Result<PullRequest, String> {
    let reported = reported.trim();

    if let Ok(number) = reported.trim_start_matches('#').parse::<u64>() {
        if number == 0 {
            return Err("pull request numbers start at 1".into());
        }
        return Ok(PullRequest { number, url: None });
    }

    let number = reported
        .rsplit('/')
        .next()
        .and_then(|tail| tail.parse::<u64>().ok())
        .ok_or_else(|| format!("could not read a pull request number from `{reported}`"))?;

    if !reported.contains("/pull/") {
        return Err(format!("`{reported}` is not a pull request URL"));
    }

    Ok(PullRequest {
        number,
        url: Some(reported.to_string()),
    })
}

/// Run the reporting API until the process exits.
pub fn serve(address: SocketAddr, context: ApiContext) -> Result<()> {
    let server = tiny_http::Server::http(address)
        .map_err(|error| anyhow::anyhow!("could not bind the joop API to {address}: {error}"))?;

    let context = Arc::new(context);
    thread::Builder::new()
        .name("joop-api".into())
        .spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                if let Err(error) = request.as_reader().read_to_string(&mut body) {
                    tracing::warn!(%error, "could not read an API request body");
                    continue;
                }

                let method = request.method().as_str().to_string();
                let path = request.url().split('?').next().unwrap_or("/").to_string();
                let response = handle(&context, &method, &path, &body);

                let http = tiny_http::Response::from_string(response.body)
                    .with_status_code(response.code)
                    .with_header(
                        "Content-Type: application/json"
                            .parse::<tiny_http::Header>()
                            .expect("static header"),
                    );
                if let Err(error) = request.respond(http) {
                    tracing::warn!(%error, "could not answer an API request");
                }
            }
        })
        .context("could not start the joop API thread")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// One report as the API passed it on.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedReport {
        issue_number: u64,
        status: Status,
        detail: Option<String>,
        pull_request: Option<PullRequest>,
    }

    #[derive(Default)]
    struct RecordingReporter {
        reports: Mutex<Vec<RecordedReport>>,
        unknown_issue: bool,
    }

    impl RecordingReporter {
        fn rejecting_unknown_issues() -> Self {
            Self {
                reports: Mutex::new(Vec::new()),
                unknown_issue: true,
            }
        }
    }

    impl Reporter for RecordingReporter {
        fn record_report(
            &self,
            issue_number: u64,
            status: Status,
            detail: Option<&str>,
            pull_request: Option<PullRequest>,
        ) -> anyhow::Result<()> {
            if self.unknown_issue {
                anyhow::bail!("joop has no session for jaywhy13/joop#{issue_number}");
            }
            self.reports.lock().unwrap().push(RecordedReport {
                issue_number,
                status,
                detail: detail.map(str::to_string),
                pull_request,
            });
            Ok(())
        }

        fn sessions(&self) -> Vec<Session> {
            Vec::new()
        }
    }

    fn context(reporter: Arc<RecordingReporter>) -> ApiContext {
        ApiContext { reporter }
    }

    fn accepting() -> Arc<RecordingReporter> {
        Arc::new(RecordingReporter::default())
    }

    #[test]
    fn health_is_served() {
        let response = handle(&context(accepting()), "GET", "/health", "");

        assert_eq!(response.code, 200);
        assert!(response.body.contains("\"ok\":true"));
    }

    #[test]
    fn a_report_is_passed_on_with_its_detail() {
        let reporter = accepting();

        let response = handle(
            &context(reporter.clone()),
            "POST",
            "/report",
            r#"{"issue":42,"status":"working","detail":"writing tests"}"#,
        );

        assert_eq!(response.code, 200, "body: {}", response.body);
        let reports = reporter.reports.lock().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].issue_number, 42);
        assert_eq!(reports[0].status, Status::Working);
        assert_eq!(reports[0].detail.as_deref(), Some("writing tests"));
    }

    #[test]
    fn a_report_without_a_detail_is_still_accepted() {
        let reporter = accepting();

        let response = handle(
            &context(reporter.clone()),
            "POST",
            "/report",
            r#"{"issue":42,"status":"started"}"#,
        );

        assert_eq!(response.code, 200, "body: {}", response.body);
        assert_eq!(reporter.reports.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_pull_request_url_is_carried_through() {
        let reporter = accepting();

        handle(
            &context(reporter.clone()),
            "POST",
            "/report",
            r#"{"issue":42,"status":"review","pull_request":"https://github.com/jaywhy13/joop/pull/7"}"#,
        );

        let reports = reporter.reports.lock().unwrap();
        assert_eq!(
            reports[0].pull_request,
            Some(PullRequest {
                number: 7,
                url: Some("https://github.com/jaywhy13/joop/pull/7".into())
            })
        );
    }

    #[test]
    fn a_review_report_without_a_pull_request_is_left_for_the_engine_to_judge() {
        let reporter = accepting();

        let response = handle(
            &context(reporter.clone()),
            "POST",
            "/report",
            r#"{"issue":42,"status":"review"}"#,
        );

        assert_eq!(
            response.code, 200,
            "an agent checking in every five minutes on a pull request joop already knows \
             about must not be refused: {}",
            response.body
        );
        assert_eq!(reporter.reports.lock().unwrap()[0].pull_request, None);
    }

    #[test]
    fn a_merged_report_needs_no_pull_request_repeated() {
        let reporter = accepting();

        let response = handle(
            &context(reporter.clone()),
            "POST",
            "/report",
            r#"{"issue":42,"status":"merged"}"#,
        );

        assert_eq!(response.code, 200, "body: {}", response.body);
        assert_eq!(reporter.reports.lock().unwrap()[0].status, Status::Merged);
    }

    #[test]
    fn an_unknown_issue_is_rejected_with_404() {
        let response = handle(
            &context(Arc::new(RecordingReporter::rejecting_unknown_issues())),
            "POST",
            "/report",
            r#"{"issue":99,"status":"working"}"#,
        );

        assert_eq!(response.code, 404);
        assert!(response.body.contains("99"), "body: {}", response.body);
    }

    #[test]
    fn an_unknown_status_is_rejected_with_400() {
        let response = handle(
            &context(accepting()),
            "POST",
            "/report",
            r#"{"issue":42,"status":"vibing"}"#,
        );

        assert_eq!(response.code, 400);
        assert!(response.body.contains("vibing"), "body: {}", response.body);
    }

    #[test]
    fn agents_may_not_report_statuses_that_joop_owns() {
        for status in ["dispatched", "stale", "dead"] {
            let response = handle(
                &context(accepting()),
                "POST",
                "/report",
                &format!(r#"{{"issue":42,"status":"{status}"}}"#),
            );

            assert_eq!(response.code, 400, "`{status}` should be rejected");
        }
    }

    #[test]
    fn malformed_json_is_rejected_with_400() {
        assert_eq!(
            handle(&context(accepting()), "POST", "/report", "{ not json").code,
            400
        );
    }

    #[test]
    fn sessions_are_listed_for_inspection() {
        let response = handle(&context(accepting()), "GET", "/sessions", "");

        assert_eq!(response.code, 200);
    }

    #[test]
    fn unknown_routes_are_404() {
        assert_eq!(handle(&context(accepting()), "GET", "/nope", "").code, 404);
    }

    #[test]
    fn a_bare_pull_request_number_is_accepted() {
        assert_eq!(
            parse_pull_request("7"),
            Ok(PullRequest {
                number: 7,
                url: None
            })
        );
        assert_eq!(parse_pull_request("#7").unwrap().number, 7);
    }

    #[test]
    fn a_pull_request_url_keeps_both_number_and_link() {
        let parsed = parse_pull_request("https://github.com/jaywhy13/joop/pull/110").unwrap();

        assert_eq!(parsed.number, 110);
        assert_eq!(
            parsed.url.as_deref(),
            Some("https://github.com/jaywhy13/joop/pull/110")
        );
    }

    #[test]
    fn an_issue_url_is_not_mistaken_for_a_pull_request() {
        assert!(parse_pull_request("https://github.com/jaywhy13/joop/issues/42").is_err());
    }

    #[test]
    fn nonsense_in_place_of_a_pull_request_is_refused() {
        assert!(parse_pull_request("soon").is_err());
        assert!(parse_pull_request("0").is_err());
    }
}
