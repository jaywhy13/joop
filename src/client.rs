use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::api::Report;

/// Sends one status report to a running joop. This is what `joop report` does, and what
/// the prompt tells each Pi session to run.
pub fn send_report(api_url: &str, report: &Report) -> Result<String> {
    let endpoint = format!("{}/report", api_url.trim_end_matches('/'));

    let mut response = ureq::post(&endpoint)
        .config()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .send_json(report)
        .with_context(|| format!("could not reach joop at {endpoint} — is it running?"))?;

    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .context("could not read joop's answer")?;

    if status >= 400 {
        bail!("joop rejected the report ({status}): {body}");
    }

    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreachable_joop_explains_itself() {
        let report = Report {
            issue: 42,
            status: "working".into(),
            detail: None,
        };

        // Port 1 is reserved and never listening.
        let error = send_report("http://127.0.0.1:1", &report).unwrap_err();

        assert!(
            format!("{error:#}").contains("is it running?"),
            "got: {error:#}"
        );
    }
}
