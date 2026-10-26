use std::{path::Path, sync::Arc};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use goose_provider_types::{
    conversation::message::{Message, MessageContent},
    model::ModelConfig,
};
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::Notify;
use tower::ServiceExt;

use crate::{
    agent,
    github::{GitHub, Webhook},
    server::{self, App},
    store::{Session, Status, Store},
};

const ID: &str = "acme:widgets:42";
fn app(path: impl AsRef<Path>) -> App {
    App {
        store: Store::open(path).unwrap(),
        github: GitHub::new(None).unwrap(),
        provider: Arc::new(agent::DemoProvider),
        model: ModelConfig::new("demo"),
        repository: "acme/widgets".into(),
        webhook_secret: None,
        admin_token: None,
        demo: true,
        wake: Notify::new(),
    }
}
fn payload(attempt: u64, conclusion: &str) -> Value {
    json!({"action": "completed", "repository": {"full_name": "acme/widgets"},
        "workflow_run": {"id": 42, "run_attempt": attempt, "head_sha": "abc123", "conclusion": conclusion}})
}
fn ingest(app: &App, delivery: &str, body: Value) -> bool {
    let event: Webhook = serde_json::from_value(body).unwrap();
    app.store
        .ingest(delivery, &event.session_id(), |previous| {
            server::ingest_event(previous, &event)
        })
        .unwrap()
}
fn requests(session: &Session) -> Vec<String> {
    session
        .conversation
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(MessageContent::as_tool_request)
        .map(|request| request.id.clone())
        .collect()
}
fn responses(session: &Session) -> Vec<String> {
    session
        .conversation
        .iter()
        .flat_map(|m| m.get_tool_response_ids().into_iter().map(str::to_owned))
        .collect()
}
async fn waiting(app: &App) {
    ingest(app, "initial-failure", payload(1, "failure"));
    app.work_once().await.unwrap(); // inference -> inspect -> inference -> yield
    assert_eq!(
        app.store.get(ID).unwrap().unwrap().status,
        Status::RerunQueued
    );
    app.work_once().await.unwrap(); // durable intent -> external dispatch
    assert_eq!(
        app.store.get(ID).unwrap().unwrap().status,
        Status::WaitingCi
    );
}
fn sign(body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}
async fn post(app: Arc<App>, delivery: &str, body: Value, signed: bool) -> (StatusCode, Value) {
    let bytes = serde_json::to_vec(&body).unwrap();
    let mut request = Request::post("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "workflow_run")
        .header("x-github-delivery", delivery);
    if signed {
        request = request.header("x-hub-signature-256", sign(&bytes));
    }
    let response = server::router(app)
        .oneshot(request.body(Body::from(bytes)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn agent_yields_with_an_open_tool_and_resumes_after_restart() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.db");
    {
        let app = app(&path);
        waiting(&app).await;
        let session = app.store.get(ID).unwrap().unwrap();
        assert_eq!(session.invocations, 1);
        assert_eq!(requests(&session), ["demo-inspect", "demo-rerun"]);
        assert_eq!(responses(&session), ["demo-inspect"]);
        assert_eq!(session.pending.as_ref().unwrap().expected_attempt, 2);
        assert!(
            session
                .trace
                .iter()
                .any(|line| line.contains("rerun_and_wait -> Yielded"))
        );
    } // All runtime state, agent operations, provider and DB connection dropped.
    {
        let app = app(&path);
        app.recover_dispatches().unwrap();
        app.work_once().await.unwrap(); // Nothing should execute while waiting.
        assert_eq!(app.store.get(ID).unwrap().unwrap().invocations, 1);
        ingest(&app, "rerun-passed", payload(2, "success"));
        app.work_once().await.unwrap();
        let session = app.store.get(ID).unwrap().unwrap();
        assert_eq!(session.status, Status::Complete);
        assert_eq!(session.invocations, 2);
        assert!(session.pending.is_none());
        assert_eq!(requests(&session), ["demo-inspect", "demo-rerun"]);
        assert_eq!(responses(&session), ["demo-inspect", "demo-rerun"]);
        let report = session.conversation.last().unwrap().as_concat_text();
        assert!(report.contains("success"), "{report}");
        assert!(report.contains("synthetic demo"));
        assert!(
            session
                .trace
                .iter()
                .any(|line| line.contains("resume: webhook answered tool"))
        );
        assert_eq!(session.usage.len(), 3);
    }
}

#[tokio::test]
async fn repeated_failure_finishes_without_an_infinite_rerun_loop() {
    let app = app(":memory:");
    waiting(&app).await;
    ingest(&app, "still-failing", payload(2, "failure"));
    app.work_once().await.unwrap();
    let session = app.store.get(ID).unwrap().unwrap();
    assert_eq!(session.status, Status::Complete);
    assert_eq!(requests(&session).len(), 2);
    assert!(
        session
            .conversation
            .last()
            .unwrap()
            .as_concat_text()
            .contains("failure")
    );
    ingest(&app, "manual-attempt", payload(3, "failure"));
    app.work_once().await.unwrap();
    assert_eq!(app.store.get(ID).unwrap().unwrap().invocations, 2);
}

#[tokio::test]
async fn stale_wrong_commit_and_wrong_attempt_events_cannot_resume() {
    let app = app(":memory:");
    waiting(&app).await;
    ingest(&app, "old", payload(1, "failure"));
    ingest(&app, "future", payload(3, "success"));
    let mut wrong_commit = payload(2, "success");
    wrong_commit["workflow_run"]["head_sha"] = json!("different");
    ingest(&app, "wrong-commit", wrong_commit);
    app.work_once().await.unwrap();
    let session = app.store.get(ID).unwrap().unwrap();
    assert_eq!(session.status, Status::WaitingCi);
    assert_eq!(session.invocations, 1);
    assert_eq!(responses(&session), ["demo-inspect"]);
}

#[tokio::test]
async fn duplicate_deliveries_and_semantic_duplicates_are_idempotent() {
    let app = app(":memory:");
    waiting(&app).await;
    assert!(!ingest(&app, "initial-failure", payload(1, "failure")));
    assert!(ingest(&app, "done", payload(2, "success")));
    assert!(!ingest(&app, "done", payload(2, "success")));
    assert!(ingest(
        &app,
        "same-event-new-delivery",
        payload(2, "success")
    ));
    app.work_once().await.unwrap();
    assert_eq!(
        responses(&app.store.get(ID).unwrap().unwrap()),
        ["demo-inspect", "demo-rerun"]
    );
    assert_eq!(app.store.get(ID).unwrap().unwrap().invocations, 2);
}

#[tokio::test]
async fn success_without_an_investigation_is_ignored() {
    let app = app(":memory:");
    ingest(&app, "success", payload(1, "success"));
    app.work_once().await.unwrap();
    assert!(app.store.all().unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_is_a_valid_result_of_a_rerun() {
    let app = app(":memory:");
    waiting(&app).await;
    ingest(&app, "cancelled", payload(2, "cancelled"));
    app.work_once().await.unwrap();
    let session = app.store.get(ID).unwrap().unwrap();
    assert_eq!(session.status, Status::Complete);
    assert!(
        session
            .conversation
            .last()
            .unwrap()
            .as_concat_text()
            .contains("cancelled")
    );
}

#[tokio::test]
async fn ambiguous_dispatch_is_not_retried_but_completion_can_recover_it() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.db");
    {
        let app = app(&path);
        ingest(&app, "failure", payload(1, "failure"));
        app.work_once().await.unwrap();
        app.store
            .mutate(ID, |s| {
                s.status = Status::Dispatching;
                Ok(())
            })
            .unwrap();
    }
    let app = app(&path);
    app.recover_dispatches().unwrap();
    app.work_once().await.unwrap();
    let session = app.store.get(ID).unwrap().unwrap();
    assert_eq!(session.status, Status::Error);
    assert!(session.error.unwrap().contains("outcome unknown"));
    ingest(&app, "completed", payload(2, "success"));
    app.work_once().await.unwrap();
    assert_eq!(app.store.get(ID).unwrap().unwrap().status, Status::Complete);
}

#[tokio::test]
async fn queued_intent_is_dispatched_after_restart() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.db");
    {
        let app = app(&path);
        ingest(&app, "failure", payload(1, "failure"));
        app.work_once().await.unwrap();
        assert_eq!(
            app.store.get(ID).unwrap().unwrap().status,
            Status::RerunQueued
        );
    }
    let app = app(&path);
    app.recover_dispatches().unwrap();
    app.work_once().await.unwrap();
    assert_eq!(
        app.store.get(ID).unwrap().unwrap().status,
        Status::WaitingCi
    );
    assert_eq!(app.store.get(ID).unwrap().unwrap().invocations, 1);
}

#[tokio::test]
async fn completion_before_dispatch_skips_unnecessary_post() {
    let app = app(":memory:");
    ingest(&app, "failure", payload(1, "failure"));
    app.work_once().await.unwrap();
    ingest(&app, "very-fast-completion", payload(2, "success"));
    app.work_once().await.unwrap();
    let session = app.store.get(ID).unwrap().unwrap();
    assert_eq!(session.status, Status::Complete);
    assert!(
        !session
            .trace
            .iter()
            .any(|line| line.starts_with("dispatch:"))
    );
}

#[test]
fn signature_verification_uses_exact_raw_bytes() {
    let body = br#"{"a":1}"#;
    assert!(server::verify_signature("secret", &sign(body), body));
    assert!(!server::verify_signature("wrong", &sign(body), body));
    assert!(!server::verify_signature(
        "secret",
        &sign(body),
        br#"{"a": 1}"#
    ));
    for bad in ["", "sha1=abc", "sha256=zz", "sha256=", "sha256=ab"] {
        assert!(!server::verify_signature("secret", bad, body));
    }
}

#[tokio::test]
async fn http_ack_is_durable_before_agent_work_and_duplicates_are_reported() {
    let app = Arc::new(app(":memory:"));
    let (status, body) = post(app.clone(), "delivery", payload(1, "failure"), false).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["session_id"], ID);
    assert_eq!(body["duplicate"], false);
    let session = app.store.get(ID).unwrap().unwrap();
    assert_eq!(session.status, Status::Runnable);
    assert_eq!(session.invocations, 0);
    let (_, body) = post(app, "delivery", payload(1, "failure"), false).await;
    assert_eq!(body["duplicate"], true);
}

#[tokio::test]
async fn live_webhooks_require_signature_and_repository_allowlist() {
    let mut app = app(":memory:");
    app.demo = false;
    app.webhook_secret = Some("secret".into());
    let app = Arc::new(app);
    assert_eq!(
        post(app.clone(), "unsigned", payload(1, "failure"), false)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mut body = payload(1, "failure");
    body["repository"]["full_name"] = json!("other/repo");
    assert_eq!(
        post(app.clone(), "wrong-repo", body, true).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(app.clone(), "signed", payload(1, "failure"), true)
            .await
            .0,
        StatusCode::ACCEPTED
    );
    assert_eq!(app.store.all().unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_events_do_not_create_sessions() {
    let app = Arc::new(app(":memory:"));
    assert_eq!(
        post(app.clone(), "", payload(1, "failure"), false).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(app.clone(), "bad-attempt", payload(0, "failure"), false)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut body = payload(1, "failure");
    body["workflow_run"]["conclusion"] = Value::Null;
    assert_eq!(
        post(app.clone(), "no-conclusion", body, false).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert!(app.store.all().unwrap().is_empty());
}

#[tokio::test]
async fn live_session_reads_require_admin_bearer_token() {
    let mut app = app(":memory:");
    app.demo = false;
    app.admin_token = Some("admin-secret".into());
    let app = Arc::new(app);
    for (token, expected) in [
        ("", StatusCode::UNAUTHORIZED),
        ("Bearer wrong", StatusCode::UNAUTHORIZED),
        ("Bearer admin-secret", StatusCode::OK),
    ] {
        let request = Request::get("/sessions")
            .header("authorization", token)
            .body(Body::empty())
            .unwrap();
        let response = server::router(app.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), expected);
    }
}

#[test]
fn failed_event_transaction_does_not_consume_delivery_id() {
    let app = app(":memory:");
    let error = app
        .store
        .ingest("retry", ID, |_| anyhow::bail!("simulated error"));
    assert!(error.is_err());
    assert!(ingest(&app, "retry", payload(1, "failure")));
    assert!(app.store.get(ID).unwrap().is_some());
}

#[test]
fn repository_validation_rejects_path_and_url_injection() {
    for name in ["acme/widgets", "a-b/c.d_e"] {
        assert!(crate::github::valid_repository(name));
    }
    for name in [
        "../widgets",
        "a/..",
        "a/b/c",
        "a/b?x=y",
        "https://evil",
        "/widgets",
        "a/%2f",
    ] {
        assert!(!crate::github::valid_repository(name));
    }
}

#[tokio::test]
async fn unknown_tool_gets_an_error_response_instead_of_hanging() {
    let app = app(":memory:");
    ingest(&app, "failure", payload(1, "failure"));
    app.store
        .mutate(ID, |s| {
            s.conversation.push(Message::assistant().with_tool_request(
                "unknown",
                Ok(rmcp::model::CallToolRequestParams::new("unknown")),
            ));
            Ok(())
        })
        .unwrap();
    app.work_once().await.unwrap();
    let session = app.store.get(ID).unwrap().unwrap();
    assert!(responses(&session).contains(&"unknown".to_owned()));
    assert_eq!(session.status, Status::RerunQueued);
}
