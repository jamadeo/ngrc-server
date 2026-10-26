use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use goose_provider_types::{base::Provider, model::ModelConfig};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::{
    agent,
    store::{Session, Status, Store, now_ms},
};

pub struct App {
    pub store: Store,
    pub provider: Arc<dyn Provider>,
    pub model: ModelConfig,
    pub demo: bool,
    pub wake: Notify,
}
impl App {
    /// One worker owns all invocations. A persisted deadline is the timer; there
    /// is no sleeping agent future, per-session timer task, or external service.
    pub async fn work_once(&self) -> Result<()> {
        self.store.expire_timers(now_ms())?;
        for session in self.store.all()? {
            if session.status != Status::Runnable {
                continue;
            }
            if let Err(error) = agent::run(
                &self.store,
                &session.id,
                self.provider.clone(),
                self.model.clone(),
            )
            .await
            {
                tracing::error!(session = session.id, %error, "recipe agent failed");
                self.store.mutate(&session.id, |s| {
                    s.status = Status::Error;
                    s.error = Some(error.to_string());
                    s.trace.push(format!("error: {error}"));
                    Ok(())
                })?;
            }
        }
        Ok(())
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../static/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [("content-type", "text/javascript; charset=utf-8")],
                    include_str!("../static/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [("content-type", "text/css; charset=utf-8")],
                    include_str!("../static/style.css"),
                )
            }),
        )
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .route("/api/config", get(config))
        .route("/api/sessions", post(create).get(list))
        .route("/api/sessions/{id}", get(session))
        .route("/api/sessions/{id}/action", post(action))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn(local_requests))
        .with_state(app)
}

/// Loopback-only UI, no CORS. Reject cross-origin browser mutations and nonlocal
/// Host headers (DNS rebinding). CLI clients without Origin are supported.
async fn local_requests(request: Request<Body>, next: Next) -> Response {
    let host = request
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let local = host
        .parse::<axum::http::uri::Authority>()
        .ok()
        .is_some_and(|a| matches!(a.host(), "localhost" | "127.0.0.1" | "[::1]"));
    if !local {
        return (StatusCode::FORBIDDEN, "Use a localhost or loopback URL").into_response();
    }
    if let Some(origin) = request.headers().get("origin") {
        let same = origin
            .to_str()
            .ok()
            .and_then(|o| o.parse::<axum::http::Uri>().ok())
            .is_some_and(|o| {
                o.scheme_str() == Some("http") && o.authority().is_some_and(|a| a.as_str() == host)
            });
        if !same {
            return (
                StatusCode::FORBIDDEN,
                "Cross-origin requests are not allowed",
            )
                .into_response();
        }
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("cache-control", "no-store".parse().unwrap());
    headers.insert("content-security-policy", "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".parse().unwrap());
    response
}

struct ApiError(StatusCode, String);
type ApiResult<T> = std::result::Result<T, ApiError>;
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        tracing::error!(%error, "request failed");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save or load the recipe session".into(),
        )
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    recipe: String,
    #[serde(default = "default_fast")]
    fast_demo: bool,
}
fn default_fast() -> bool {
    true
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    tool_call_id: String,
}

async fn config(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({"mode": if app.demo { "demo" } else { "live" }, "example": crate::recipe::EXAMPLE}))
}
async fn create(
    State(app): State<Arc<App>>,
    Json(input): Json<Create>,
) -> ApiResult<(StatusCode, Json<Session>)> {
    let session = Session::new(input.recipe, input.fast_demo)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    if app.demo {
        crate::recipe::parse_demo(&session.recipe)
            .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    }
    app.store.create(&session)?;
    app.wake.notify_one();
    Ok((StatusCode::ACCEPTED, Json(session)))
}
async fn list(State(app): State<Arc<App>>) -> ApiResult<Json<Vec<Session>>> {
    Ok(Json(app.store.all()?))
}
async fn session(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<Session>> {
    Ok(Json(app.store.get(&id)?.ok_or_else(|| {
        ApiError(StatusCode::NOT_FOUND, "Recipe session not found".into())
    })?))
}
async fn action(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<Action>,
) -> ApiResult<Json<Session>> {
    if app.store.get(&id)?.is_none() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "Recipe session not found".into(),
        ));
    }
    // Only conflicts are exposed, never a SQL error or provider credential.
    let session = app.store.action(&id, &input.tool_call_id).map_err(|_| {
        ApiError(
            StatusCode::CONFLICT,
            "This session is not waiting for that action. Refresh its state.".into(),
        )
    })?;
    app.wake.notify_one();
    Ok(Json(session))
}

pub async fn serve(app: Arc<App>, bind: SocketAddr) -> Result<()> {
    if !bind.ip().is_loopback() {
        bail!("this local demo must bind to a loopback address, in both demo and live mode");
    }
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(url = %format!("http://{bind}"), demo = app.demo, "Recipe companion ready");
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
                _ = tokio::time::sleep(Duration::from_millis(200)) => {},
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
    shutdown.cancel();
    worker.await?;
    result?;
    Ok(())
}
