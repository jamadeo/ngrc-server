use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use goose_provider_types::{
    base::Provider,
    conversation::{Conversation, message::Message},
    model::ModelConfig,
};
use hmac::{Hmac, Mac};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::Notify;

use crate::{
    agent,
    github::{GitHub, Webhook, valid_repository},
    store::{Session, Status, Store},
};

pub struct App {
    pub store: Store,
    pub github: GitHub,
    pub provider: Arc<dyn Provider>,
    pub model: ModelConfig,
    pub repository: String,
    pub webhook_secret: Option<String>,
    pub admin_token: Option<String>,
    pub demo: bool,
    pub wake: Notify,
}

impl App {
    /// A single worker is deliberate: one owner drives all state machines.
    /// HTTP only writes durable input; the worker polls it, including on restart.
    pub async fn work_once(&self) -> Result<()> {
        for session in self.store.all()? {
            let result = match session.status {
                Status::Runnable => {
                    agent::run(
                        &self.store,
                        &session.id,
                        self.github.clone(),
                        self.provider.clone(),
                        self.model.clone(),
                    )
                    .await
                }
                Status::RerunQueued => self.dispatch(&session.id).await,
                _ => continue,
            };
            if let Err(error) = result {
                tracing::error!(session = session.id, %error, "investigation failed");
                self.store.mutate(&session.id, |s| {
                    // If a completion arrived during dispatch, do not discard it.
                    if s.status != Status::Runnable || session.status == Status::Runnable {
                        s.status = Status::Error;
                        s.error = Some(error.to_string());
                        s.trace.push(format!("error: {error}"));
                    }
                    Ok(())
                })?;
            }
        }
        Ok(())
    }

    async fn dispatch(&self, id: &str) -> Result<()> {
        let session = self.store.mutate(id, |s| {
            if s.status == Status::RerunQueued {
                s.status = Status::Dispatching;
                s.trace
                    .push("dispatch: persisted intent before GitHub POST".into());
            }
            Ok(())
        })?;
        if session.status != Status::Dispatching {
            return Ok(());
        }
        // GitHub's rerun POST has no idempotency key. We never auto-retry an
        // ambiguous POST: see recover_dispatches() and the README caveat.
        self.github.rerun_failed_jobs(&session).await?;
        self.store.mutate(id, |s| {
            if s.status == Status::Dispatching {
                s.status = Status::WaitingCi;
                s.trace
                    .push("waiting_ci: agent dropped; no task is waiting for CI".into());
            }
            Ok(())
        })?;
        Ok(())
    }

    pub fn recover_dispatches(&self) -> Result<()> {
        for session in self.store.all()? {
            if session.status == Status::Dispatching {
                self.store.mutate(&session.id, |s| {
                    s.status = Status::Error;
                    s.error = Some("Restart during GitHub POST: dispatch outcome unknown. Check GitHub; automatic redispatch is intentionally disabled. A matching completion webhook can still resume this session.".into());
                    Ok(())
                })?;
            }
        }
        Ok(())
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .route("/webhooks/github", post(webhook))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(app)
}

type ApiResult<T> = std::result::Result<T, ApiError>;
struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        tracing::error!(%error, "request failed");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    }
}
fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

pub fn verify_signature(secret: &str, signature: &str, body: &[u8]) -> bool {
    let Some(signature) = signature
        .strip_prefix("sha256=")
        .and_then(|s| hex::decode(s).ok())
    else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

async fn webhook(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if let Some(secret) = &app.webhook_secret {
        if !verify_signature(secret, header(&headers, "x-hub-signature-256"), &body) {
            return Err(ApiError(
                StatusCode::UNAUTHORIZED,
                "invalid webhook signature".into(),
            ));
        }
    } else if !app.demo {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "webhook secret required".into(),
        ));
    }
    if header(&headers, "x-github-event") != "workflow_run" {
        return Ok((StatusCode::OK, Json(json!({"ignored": "not workflow_run"}))));
    }
    let delivery = header(&headers, "x-github-delivery");
    if delivery.is_empty() || delivery.len() > 200 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "x-github-delivery required (max 200 bytes)".into(),
        ));
    }
    let event: Webhook = serde_json::from_slice(&body)
        .map_err(|error| ApiError(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?;
    if !valid_repository(&event.repository.full_name)
        || event.repository.full_name != app.repository
    {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "repository is not allowlisted".into(),
        ));
    }
    if event.action != "completed" {
        return Ok((StatusCode::OK, Json(json!({"ignored": "not completed"}))));
    }
    if event.workflow_run.run_attempt == 0
        || event.workflow_run.run_attempt == u64::MAX
        || event.workflow_run.id == 0
        || event.workflow_run.head_sha.is_empty()
        || event.workflow_run.conclusion.is_none()
    {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "completed run requires id, attempt, head_sha, and conclusion".into(),
        ));
    }
    let id = event.session_id();
    let accepted = app
        .store
        .ingest(delivery, &id, |previous| ingest_event(previous, &event))?;
    app.wake.notify_one();
    // ACK after a small DB transaction, not after inference or a GitHub request.
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"session_id": id, "duplicate": !accepted})),
    ))
}

pub fn ingest_event(previous: Option<Session>, event: &Webhook) -> Result<Option<Session>> {
    let run = &event.workflow_run;
    if let Some(mut session) = previous {
        // Match immutable identity + exact expected attempt. Old deliveries,
        // unrelated commits and later manual attempts cannot answer this tool.
        if let Some(pending) = &session.pending
            && run.id == session.run_id
            && event.repository.full_name == session.repository
            && run.run_attempt == pending.expected_attempt
            && run.head_sha == session.head_sha
        {
            let text =
                json!({"run_id": run.id, "attempt": run.run_attempt, "conclusion": run.conclusion})
                    .to_string();
            let response = Message::user()
                .with_tool_response(
                    pending.tool_call_id.clone(),
                    Ok(CallToolResult::success(vec![ContentBlock::text(text)])),
                )
                .with_generated_id_if_missing();
            session.conversation.push(response);
            session.trace.push(format!(
                "resume: webhook answered tool {} with attempt {}",
                pending.tool_call_id, run.run_attempt
            ));
            session.pending = None;
            session.error = None;
            session.status = Status::Runnable;
        }
        return Ok(Some(session));
    }
    if run.conclusion.as_deref() != Some("failure") {
        return Ok(None);
    }
    Ok(Some(Session {
        id: event.session_id(), repository: event.repository.full_name.clone(), run_id: run.id,
        original_attempt: run.run_attempt, head_sha: run.head_sha.clone(), status: Status::Runnable,
        conversation: Conversation::new_unvalidated([Message::user().with_text(format!(
            "Investigate this failed GitHub Actions run. Repository: {}; run: {}; attempt: {}; commit: {}. \
             Inspect failed jobs, optionally retry once, and report the outcome.",
            event.repository.full_name, run.id, run.run_attempt, run.head_sha)).with_generated_id_if_missing()]),
        pending: None, rerun_used: false, invocations: 0,
        trace: vec!["webhook: initial failure persisted".into()], usage: vec![], error: None,
    }))
}

fn authorize(app: &App, headers: &HeaderMap) -> ApiResult<()> {
    if app.demo {
        return Ok(());
    }
    if let Some(token) = &app.admin_token {
        // Use HMAC verification for constant-time comparison without leaking tokens.
        let supplied = header(headers, "authorization")
            .strip_prefix("Bearer ")
            .unwrap_or("");
        let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).unwrap();
        mac.update(b"admin");
        let expected = mac.clone().finalize().into_bytes();
        let mut other = Hmac::<Sha256>::new_from_slice(supplied.as_bytes()).unwrap();
        other.update(b"admin");
        // verify_slice compares in constant time.
        if other.verify_slice(&expected).is_ok() {
            return Ok(());
        }
    }
    Err(ApiError(
        StatusCode::UNAUTHORIZED,
        "admin bearer token required".into(),
    ))
}
async fn list_sessions(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authorize(&app, &headers)?;
    let sessions: Vec<_> = app
        .store
        .all()?
        .into_iter()
        .map(|s| {
            json!({
                "id": s.id, "repository": s.repository, "run_id": s.run_id, "status": s.status,
                "invocations": s.invocations, "pending": s.pending, "error": s.error,
            })
        })
        .collect();
    Ok(Json(json!(sessions)))
}
async fn get_session(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Session>> {
    authorize(&app, &headers)?;
    app.store
        .get(&id)?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "session not found".into()))
}

pub async fn serve(app: Arc<App>, bind: SocketAddr) -> Result<()> {
    if app.demo && !bind.ip().is_loopback() {
        bail!("demo mode must bind to a loopback address");
    }
    app.recover_dispatches()?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .context("bind HTTP server")?;
    tracing::info!(%bind, demo = app.demo, repository = app.repository, "CI investigator listening");
    let shutdown = tokio_util::sync::CancellationToken::new();
    let worker_shutdown = shutdown.clone();
    let worker_app = app.clone();
    let worker = tokio::spawn(async move {
        loop {
            if worker_shutdown.is_cancelled() {
                break;
            }
            if let Err(error) = worker_app.work_once().await {
                tracing::error!(%error, "worker error");
            }
            tokio::select! {
                _ = worker_shutdown.cancelled() => break,
                _ = worker_app.wake.notified() => {},
                _ = tokio::time::sleep(Duration::from_millis(250)) => {},
            }
        }
    });
    let signal_shutdown = shutdown.clone();
    let result = axum::serve(listener, router(app))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            signal_shutdown.cancel();
        })
        .await;
    // Finish the current bounded batch rather than dropping a GDK step before
    // its effects/finalization commit. Waiting CI sessions have no active agent.
    shutdown.cancel();
    worker.await?;
    result?;
    Ok(())
}
