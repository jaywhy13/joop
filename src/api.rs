use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::clock::Clock;
use crate::state::{State, Status};

/// A status report from a Pi session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub issue: u64,
    pub status: String,
    #[serde(default)]
    pub detail: Option<String>,
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
    pub state: Arc<Mutex<State>>,
    pub repo: String,
    pub clock: Arc<dyn Clock>,
}

/// Route and handle one request. Kept free of any HTTP machinery so it can be tested directly.
pub fn handle(context: &ApiContext, method: &str, path: &str, body: &str) -> ApiResponse {
    match (method, path) {
        ("GET", "/health") => ApiResponse::json(200, serde_json::json!({"ok": true})),
        ("GET", "/sessions") => match context.state.lock() {
            Ok(state) => ApiResponse::json(200, state.sessions()),
            Err(_) => ApiResponse::error(500, "joop state is poisoned"),
        },
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

    let mut state = match context.state.lock() {
        Ok(state) => state,
        Err(_) => return ApiResponse::error(500, "joop state is poisoned"),
    };

    match state.record_report(
        &context.repo,
        report.issue,
        status,
        report.detail.as_deref(),
        context.clock.now(),
    ) {
        Ok(()) => ApiResponse::json(
            200,
            serde_json::json!({"ok": true, "issue": report.issue, "status": status.as_str()}),
        ),
        Err(error) => ApiResponse::error(404, format!("{error:#}")),
    }
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
    use crate::clock::FixedClock;
    use crate::state::Dispatch;
    use chrono::{TimeZone, Utc};

    fn context(dir: &tempfile::TempDir) -> ApiContext {
        let mut state = State::load(&dir.path().join("state.json")).unwrap();
        state
            .record_dispatch(Dispatch {
                repo: "jaywhy13/joop".into(),
                issue_number: 42,
                issue_title: "t".into(),
                issue_url: "u".into(),
                tab_name: "issue-42".into(),
                tab_id: Some("7".into()),
                branch: "joop/issue-42".into(),
                worktree: "/worktrees/issue-42".into(),
                at: Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap(),
                attempt: 1,
            })
            .unwrap();

        ApiContext {
            state: Arc::new(Mutex::new(state)),
            repo: "jaywhy13/joop".into(),
            clock: Arc::new(FixedClock::new(
                Utc.with_ymd_and_hms(2026, 10, 11, 12, 5, 0).unwrap(),
            )),
        }
    }

    #[test]
    fn health_is_served() {
        let dir = tempfile::tempdir().unwrap();

        let response = handle(&context(&dir), "GET", "/health", "");

        assert_eq!(response.code, 200);
        assert!(response.body.contains("\"ok\":true"));
    }

    #[test]
    fn a_report_records_the_status_and_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        let context = context(&dir);

        let response = handle(
            &context,
            "POST",
            "/report",
            r#"{"issue":42,"status":"working","detail":"writing tests"}"#,
        );

        assert_eq!(response.code, 200, "body: {}", response.body);
        let state = context.state.lock().unwrap();
        let session = state.session("jaywhy13/joop", 42).unwrap();
        assert_eq!(session.status, Status::Working);
        assert_eq!(session.detail.as_deref(), Some("writing tests"));
        assert_eq!(
            session.last_update_at,
            Some(Utc.with_ymd_and_hms(2026, 10, 11, 12, 5, 0).unwrap())
        );
    }

    #[test]
    fn a_report_without_a_detail_is_still_a_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        let context = context(&dir);

        let response = handle(
            &context,
            "POST",
            "/report",
            r#"{"issue":42,"status":"started"}"#,
        );

        assert_eq!(response.code, 200, "body: {}", response.body);
        assert!(
            context
                .state
                .lock()
                .unwrap()
                .session("jaywhy13/joop", 42)
                .unwrap()
                .last_update_at
                .is_some()
        );
    }

    #[test]
    fn an_unknown_issue_is_rejected_with_404() {
        let dir = tempfile::tempdir().unwrap();

        let response = handle(
            &context(&dir),
            "POST",
            "/report",
            r#"{"issue":99,"status":"working"}"#,
        );

        assert_eq!(response.code, 404);
        assert!(response.body.contains("99"), "body: {}", response.body);
    }

    #[test]
    fn an_unknown_status_is_rejected_with_400() {
        let dir = tempfile::tempdir().unwrap();

        let response = handle(
            &context(&dir),
            "POST",
            "/report",
            r#"{"issue":42,"status":"vibing"}"#,
        );

        assert_eq!(response.code, 400);
        assert!(response.body.contains("vibing"), "body: {}", response.body);
    }

    #[test]
    fn agents_may_not_report_statuses_that_joop_owns() {
        let dir = tempfile::tempdir().unwrap();

        for status in ["dispatched", "stale", "dead"] {
            let response = handle(
                &context(&dir),
                "POST",
                "/report",
                &format!(r#"{{"issue":42,"status":"{status}"}}"#),
            );

            assert_eq!(response.code, 400, "`{status}` should be rejected");
        }
    }

    #[test]
    fn malformed_json_is_rejected_with_400() {
        let dir = tempfile::tempdir().unwrap();

        let response = handle(&context(&dir), "POST", "/report", "{ not json");

        assert_eq!(response.code, 400);
    }

    #[test]
    fn sessions_are_listed_for_inspection() {
        let dir = tempfile::tempdir().unwrap();

        let response = handle(&context(&dir), "GET", "/sessions", "");

        assert_eq!(response.code, 200);
        assert!(response.body.contains("jaywhy13/joop#42"));
        assert!(response.body.contains("issue-42"));
    }

    #[test]
    fn unknown_routes_are_404() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(handle(&context(&dir), "GET", "/nope", "").code, 404);
    }

    #[test]
    fn query_strings_do_not_break_routing() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(handle(&context(&dir), "GET", "/health", "").code, 200);
    }
}
