use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::store::Session;

#[derive(Clone)]
pub struct GitHub {
    client: Client,
    token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Webhook {
    pub action: String,
    pub repository: Repository,
    pub workflow_run: WorkflowRun,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Repository {
    pub full_name: String,
}
#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub run_attempt: u64,
    pub head_sha: String,
    pub conclusion: Option<String>,
}
impl Webhook {
    pub fn session_id(&self) -> String {
        format!(
            "{}:{}",
            self.repository.full_name.replace('/', ":"),
            self.workflow_run.id
        )
    }
}

impl GitHub {
    pub fn new(token: Option<String>) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            token,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> Result<reqwest::RequestBuilder> {
        let token = self.token.as_ref().context("GitHub token missing")?;
        Ok(self
            .client
            .request(method, format!("https://api.github.com{path}"))
            .bearer_auth(token)
            .header("User-Agent", "ngrc-gdk-ci-demo")
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2026-03-10"))
    }

    pub async fn failed_jobs(&self, session: &Session) -> Result<Value> {
        if self.token.is_none() {
            return Ok(
                json!({"failed_jobs": [{"name": "integration-tests", "conclusion": "failure",
                "failed_steps": ["Download test dependencies (upstream registry timed out)"]}],
                "note": "Synthetic demo data; not a real GitHub diagnosis."}),
            );
        }
        // Do not follow webhook-provided URLs or send tokens to arbitrary hosts.
        let mut jobs = Vec::new();
        for page in 1..=10 {
            let response = self
                .request(
                    reqwest::Method::GET,
                    &format!(
                        "/repos/{}/actions/runs/{}/attempts/{}/jobs?per_page=100&page={page}",
                        session.repository, session.run_id, session.original_attempt
                    ),
                )?
                .send()
                .await?
                .error_for_status()?;
            if response.status() != reqwest::StatusCode::OK {
                bail!("unexpected GitHub jobs status: {}", response.status());
            }
            let body: Value = response.json().await?;
            let page_jobs = body["jobs"]
                .as_array()
                .context("GitHub jobs response missing jobs")?;
            for job in page_jobs {
                if job["conclusion"] == "failure" {
                    let steps: Vec<_> = job["steps"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|step| step["conclusion"] == "failure")
                        .map(|step| step["name"].clone())
                        .collect();
                    jobs.push(json!({"name": job["name"], "conclusion": job["conclusion"], "failed_steps": steps}));
                }
            }
            if page_jobs.len() < 100 {
                return Ok(
                    json!({"failed_jobs": jobs, "note": "Job/step metadata only; raw logs are not fetched."}),
                );
            }
        }
        bail!("job pagination limit exceeded")
    }

    pub async fn rerun_failed_jobs(&self, session: &Session) -> Result<()> {
        if self.token.is_none() {
            return Ok(());
        }
        let response = self
            .request(
                reqwest::Method::POST,
                &format!(
                    "/repos/{}/actions/runs/{}/rerun-failed-jobs",
                    session.repository, session.run_id
                ),
            )?
            .send()
            .await?
            .error_for_status()?;
        if response.status() != reqwest::StatusCode::CREATED {
            bail!("unexpected GitHub rerun status: {}", response.status());
        }
        Ok(())
    }
}

pub fn valid_repository(repository: &str) -> bool {
    let parts: Vec<_> = repository.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && *part != "."
                && *part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}
