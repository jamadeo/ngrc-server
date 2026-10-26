use crate::{
    agent,
    recipe::{self, Plan, RecipeStep, TimerSpec},
    server::{self, App},
    store::{Pending, Session, Status, Store, now_ms},
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use goose_provider_types::{
    conversation::message::{Message, MessageContent},
    model::ModelConfig,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tokio::sync::Notify;
use tower::ServiceExt;

const RECIPE: &str = "A quick bowl\nIngredients\n1 cup oats\n1 cup water\nInstructions\n1. Mix oats and water.\n2. Simmer for 2 minutes.\n3. Stir and serve.";
fn app(path: impl AsRef<Path>) -> App {
    App {
        store: Store::open(path).unwrap(),
        provider: Arc::new(agent::DemoProvider),
        model: ModelConfig::new("demo"),
        demo: true,
        wake: Notify::new(),
    }
}
fn create(app: &App, recipe: &str, fast: bool) -> String {
    let s = Session::new(recipe.into(), fast).unwrap();
    app.store.create(&s).unwrap();
    s.id
}
fn get(app: &App, id: &str) -> Session {
    app.store.get(id).unwrap().unwrap()
}
async fn click(app: &App, id: &str) {
    let s = get(app, id);
    assert_eq!(s.status, Status::WaitingAction);
    app.store.action(id, s.pending.unwrap().call_id()).unwrap();
    app.work_once().await.unwrap();
}
fn expire(app: &App, id: &str) {
    let s = get(app, id);
    let Some(Pending::Timers { timers, .. }) = s.pending else {
        panic!("not timers")
    };
    app.store
        .expire_timers(timers.iter().map(|t| t.deadline_ms).max().unwrap())
        .unwrap();
}
fn requests(s: &Session) -> Vec<String> {
    s.conversation
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(MessageContent::as_tool_request)
        .map(|r| r.id.clone())
        .collect()
}
fn responses(s: &Session) -> Vec<String> {
    s.conversation
        .iter()
        .flat_map(|m| m.get_tool_response_ids().into_iter().map(str::to_owned))
        .collect()
}
async fn http(app: Arc<App>, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost:3000")
        .header("content-type", "application/json")
        .body(
            body.map(|v| Body::from(v.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let res = server::router(app).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes))),
    )
}

#[tokio::test]
async fn guides_every_step_and_timer_across_restarts() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("recipe.db");
    let id;
    {
        let app = app(&path);
        id = create(&app, RECIPE, true);
        app.work_once().await.unwrap();
        let s = get(&app, &id);
        assert_eq!(s.status, Status::WaitingAction);
        assert_eq!(s.current_step, 0);
        assert_eq!(s.invocations, 1);
        assert_eq!(s.plan.as_ref().unwrap().steps.len(), 3);
        assert_eq!(requests(&s).len(), 3);
        assert_eq!(responses(&s).len(), 2);
        app.work_once().await.unwrap();
        assert_eq!(get(&app, &id).invocations, 1);
    }
    let deadline;
    {
        let app = app(&path);
        click(&app, &id).await;
        assert_eq!(get(&app, &id).current_step, 1); // step 2 preparation
        click(&app, &id).await;
        let s = get(&app, &id);
        assert_eq!(s.status, Status::WaitingTimers);
        assert_eq!(s.current_step, 1);
        assert_eq!(s.invocations, 3);
        let Some(Pending::Timers { timers, .. }) = s.pending else {
            panic!()
        };
        deadline = timers[0].deadline_ms;
        assert_eq!(timers[0].recipe_seconds, 120);
        assert_eq!(timers[0].actual_seconds, 5);
    }
    {
        let app = app(&path);
        app.store.expire_timers(deadline - 1).unwrap();
        assert_eq!(get(&app, &id).status, Status::WaitingTimers);
        app.store.expire_timers(deadline).unwrap();
        app.work_once().await.unwrap();
        let s = get(&app, &id);
        assert_eq!(s.status, Status::WaitingAction);
        assert_eq!(s.current_step, 1);
        assert_eq!(s.invocations, 4);
        assert!(
            s.conversation
                .last()
                .unwrap()
                .as_concat_text()
                .contains("Check the food")
        );
        click(&app, &id).await;
        assert_eq!(get(&app, &id).current_step, 2);
        click(&app, &id).await;
        let s = get(&app, &id);
        assert_eq!(s.status, Status::Complete);
        assert_eq!(s.current_step, 3);
        assert_eq!(s.invocations, 6);
        assert!(s.pending.is_none());
        assert_eq!(requests(&s).len(), responses(&s).len());
    }
}
#[tokio::test]
async fn retries_of_old_and_current_actions_never_skip_steps_or_restart_timers() {
    let app = app(":memory:");
    let id = create(&app, RECIPE, true);
    app.work_once().await.unwrap();
    let first = get(&app, &id).pending.unwrap().call_id().to_owned();
    for _ in 0..3 {
        app.store.action(&id, &first).unwrap();
    }
    app.work_once().await.unwrap();
    assert_eq!(get(&app, &id).current_step, 1);
    let second = get(&app, &id).pending.unwrap().call_id().to_owned();
    app.store.action(&id, &first).unwrap();
    assert_eq!(get(&app, &id).pending.unwrap().call_id(), second);
    app.store.action(&id, &second).unwrap();
    app.work_once().await.unwrap();
    let timer = get(&app, &id).pending.unwrap();
    app.store.action(&id, &second).unwrap();
    assert_eq!(
        serde_json::to_value(get(&app, &id).pending.unwrap()).unwrap(),
        serde_json::to_value(timer).unwrap()
    );
    expire(&app, &id);
    expire_timers_again(&app);
    app.work_once().await.unwrap();
    assert_eq!(get(&app, &id).current_step, 1);
    click(&app, &id).await;
    click(&app, &id).await;
    app.store.action(&id, &first).unwrap();
    app.work_once().await.unwrap();
    assert_eq!(get(&app, &id).invocations, 6);
}
fn expire_timers_again(app: &App) {
    app.store.expire_timers(u64::MAX).unwrap();
}
#[tokio::test]
async fn wrong_and_early_clicks_do_not_mutate() {
    let app = app(":memory:");
    let id = create(&app, RECIPE, true);
    assert!(app.store.action(&id, "anything").is_err());
    app.work_once().await.unwrap();
    let before = serde_json::to_value(get(&app, &id)).unwrap();
    assert!(app.store.action(&id, "wrong").is_err());
    assert_eq!(serde_json::to_value(get(&app, &id)).unwrap(), before);
}
#[tokio::test]
async fn overdue_timers_resume_automatically_after_restart() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("recipe.db");
    let id;
    {
        let app = app(&path);
        id = create(&app, "Instructions\n1. Rest for 10 seconds.", true);
        app.work_once().await.unwrap();
        click(&app, &id).await;
        app.store
            .mutate(&id, |s| {
                if let Some(Pending::Timers { timers, .. }) = &mut s.pending {
                    for t in timers {
                        t.deadline_ms = now_ms() - 1;
                    }
                }
                Ok(())
            })
            .unwrap();
    }
    let app = app(&path);
    app.work_once().await.unwrap();
    assert_eq!(get(&app, &id).status, Status::WaitingAction);
    assert!(get(&app, &id).timer_done());
    assert_eq!(get(&app, &id).current_step, 0);
}
#[tokio::test]
async fn multiple_timers_in_one_step_are_concurrent_and_all_must_expire() {
    let app = app(":memory:");
    let id = create(&app, RECIPE, false);
    let plan = Plan {
        title: "Two pans".into(),
        ingredients: vec![],
        notes: vec![],
        steps: vec![RecipeStep {
            instruction: "Cook both pans and check doneness.".into(),
            timers: vec![
                TimerSpec {
                    label: "Pan A".into(),
                    seconds: 20,
                },
                TimerSpec {
                    label: "Pan B".into(),
                    seconds: 40,
                },
            ],
        }],
    };
    app.store
        .mutate(&id, |s| {
            s.plan = Some(plan);
            s.timer_ready_steps.push(0);
            Ok(())
        })
        .unwrap();
    app.work_once().await.unwrap();
    let s = get(&app, &id);
    let Some(Pending::Timers { timers, .. }) = s.pending else {
        panic!()
    };
    assert_eq!(timers.len(), 2);
    assert_eq!(timers[1].deadline_ms - timers[0].deadline_ms, 20000);
    app.store.expire_timers(timers[0].deadline_ms).unwrap();
    assert_eq!(get(&app, &id).status, Status::WaitingTimers);
    app.store.expire_timers(timers[1].deadline_ms).unwrap();
    assert_eq!(get(&app, &id).status, Status::Runnable);
    app.work_once().await.unwrap();
    assert_eq!(get(&app, &id).status, Status::WaitingAction);
}
#[tokio::test]
async fn normal_mode_uses_original_recipe_seconds() {
    let app = app(":memory:");
    let id = create(&app, "Instructions\n1. Rest for 90 seconds.", false);
    app.work_once().await.unwrap();
    let before = now_ms();
    click(&app, &id).await;
    let Some(Pending::Timers { timers, .. }) = get(&app, &id).pending else {
        panic!()
    };
    assert_eq!(timers[0].actual_seconds, 90);
    assert!(timers[0].deadline_ms >= before + 90000);
}
#[tokio::test]
async fn unknown_and_premature_timer_tools_are_answered_without_starting_a_timer() {
    let app = app(":memory:");
    let id = create(&app, RECIPE, true);
    app.store
        .mutate(&id, |s| {
            s.conversation.push(
                Message::assistant()
                    .with_tool_request(
                        "unknown",
                        Ok(rmcp::model::CallToolRequestParams::new("unknown")),
                    )
                    .with_tool_request(
                        "early",
                        Ok(rmcp::model::CallToolRequestParams::new("start_step_timers")),
                    ),
            );
            Ok(())
        })
        .unwrap();
    app.work_once().await.unwrap();
    let s = get(&app, &id);
    assert_eq!(s.status, Status::WaitingAction);
    assert_eq!(s.current_step, 0);
    assert!(responses(&s).contains(&"early".into()));
    assert!(responses(&s).contains(&"unknown".into()));
}
#[tokio::test]
async fn independent_recipes_keep_their_own_progress() {
    let app = app(":memory:");
    let a = create(&app, RECIPE, true);
    let b = create(&app, RECIPE, true);
    app.work_once().await.unwrap();
    click(&app, &a).await;
    assert_eq!(get(&app, &a).current_step, 1);
    assert_eq!(get(&app, &b).current_step, 0);
    assert_eq!(get(&app, &b).invocations, 1);
}

#[test]
fn parser_preserves_recipe_and_extracts_explicit_durations() {
    let plan = recipe::parse_demo(RECIPE).unwrap();
    assert_eq!(plan.title, "A quick bowl");
    assert_eq!(plan.ingredients, ["1 cup oats", "1 cup water"]);
    assert_eq!(plan.steps.len(), 3);
    assert_eq!(plan.steps[1].timers[0].seconds, 120);
    for (text, seconds) in [
        ("Rest for 90 seconds.", 90),
        ("Bake for 1 hour and 20 minutes.", 4800),
        ("Wait five minutes.", 300),
        ("Rest 0.5 minutes.", 30),
        ("Bake for 10–15 minutes, then check.", 600),
    ] {
        let plan = recipe::parse_demo(&format!("Instructions\n1. {text}")).unwrap();
        assert_eq!(plan.steps[0].timers[0].seconds, seconds, "{text}");
    }
    let example = recipe::parse_demo(recipe::EXAMPLE).unwrap();
    assert_eq!(example.steps.len(), 4);
    assert_eq!(example.steps[1].timers[0].seconds, 300);
}
#[test]
fn ambiguous_sequential_durations_and_unknown_waits_are_not_guessed() {
    for text in [
        "Cook for 5 minutes, then stir for 2 minutes.",
        "Chill overnight.",
        "Cook until done.",
        "Rest for 0 seconds.",
        "Wait for 48 hours.",
    ] {
        let plan = recipe::parse_demo(&format!("Instructions\n1. {text}")).unwrap();
        assert!(plan.steps[0].timers.is_empty(), "{text}");
    }
}
#[test]
fn plan_validation_bounds_steps_and_timers() {
    let mut plan = recipe::parse_demo(RECIPE).unwrap();
    plan.steps[1].timers[0].seconds = 0;
    assert!(plan.validate().is_err());
    plan.steps[1].timers[0].seconds = 86401;
    assert!(plan.validate().is_err());
    plan.steps.clear();
    assert!(plan.validate().is_err());
    assert!(recipe::parse_demo("").is_err());
    assert!(Session::new(" ".into(), true).is_err());
    assert!(Session::new("x".repeat(32001), true).is_err());
    assert!(recipe::parse_demo("Ingredients\n1 cup oats").is_err());
}
#[tokio::test]
async fn http_validates_recipe_and_acknowledges_before_inference() {
    let app = Arc::new(app(":memory:"));
    for bad in [
        json!({}),
        json!({"recipe":""}),
        json!({"recipe":"Instructions"}),
        json!({"recipe":RECIPE,"seconds":10}),
        json!({"recipe":"x".repeat(32001)}),
    ] {
        assert_eq!(
            http(app.clone(), "POST", "/api/sessions", Some(bad))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert!(app.store.all().unwrap().is_empty());
    let (status, s) = http(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(json!({"recipe":RECIPE})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(s["status"], "runnable");
    assert_eq!(s["invocations"], 0);
    assert_eq!(s["fast_demo"], true);
    let id = s["id"].as_str().unwrap();
    app.work_once().await.unwrap();
    let path = format!("/api/sessions/{id}/action");
    assert_eq!(
        http(
            app.clone(),
            "POST",
            &path,
            Some(json!({"tool_call_id":"wrong"}))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let call = get(&app, id).pending.unwrap().call_id().to_owned();
    assert_eq!(
        http(
            app.clone(),
            "POST",
            &path,
            Some(json!({"tool_call_id":call}))
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        http(app.clone(), "GET", "/api/sessions/missing", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        http(
            app,
            "POST",
            "/api/sessions/missing/action",
            Some(json!({"tool_call_id":"x"}))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn live_mode_accepts_unstructured_text_without_demo_parsing() {
    let mut app = app(":memory:");
    app.demo = false;
    let app = Arc::new(app);
    assert_eq!(
        http(
            app,
            "POST",
            "/api/sessions",
            Some(json!({"recipe":"Ingredients: oats and water. Simmer, stir, serve."}))
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
}
#[tokio::test]
async fn local_browser_policy_rejects_cross_origin_and_nonlocal_hosts() {
    let app = Arc::new(app(":memory:"));
    for (host, origin, expected) in [
        (
            "localhost:3000",
            "http://evil.example",
            StatusCode::FORBIDDEN,
        ),
        ("evil.example", "http://evil.example", StatusCode::FORBIDDEN),
        (
            "localhost:3000",
            "http://localhost:3001",
            StatusCode::FORBIDDEN,
        ),
        (
            "localhost:3000",
            "http://localhost:3000",
            StatusCode::ACCEPTED,
        ),
    ] {
        let req = Request::post("/api/sessions")
            .header("host", host)
            .header("origin", origin)
            .header("content-type", "application/json")
            .body(Body::from(json!({"recipe":RECIPE}).to_string()))
            .unwrap();
        assert_eq!(
            server::router(app.clone())
                .oneshot(req)
                .await
                .unwrap()
                .status(),
            expected
        );
    }
}
#[tokio::test]
async fn embedded_ui_assets_and_example_are_served() {
    let app = Arc::new(app(":memory:"));
    for (path, content) in [
        ("/", "One step at a time"),
        ("/app.js", "waiting_timers"),
        ("/style.css", "color-scheme"),
    ] {
        let res = server::router(app.clone())
            .oneshot(
                Request::get(path)
                    .header("host", "localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().contains_key("content-security-policy"));
        assert_eq!(res.headers()["x-content-type-options"], "nosniff");
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&bytes).contains(content));
    }
    let (_, config) = http(app, "GET", "/api/config", None).await;
    assert_eq!(config["example"], recipe::EXAMPLE);
}
#[tokio::test]
async fn oversized_request_is_rejected() {
    let app = Arc::new(app(":memory:"));
    assert_eq!(
        http(
            app,
            "POST",
            "/api/sessions",
            Some(json!({"recipe":"x".repeat(70000)}))
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}
#[test]
fn failed_mutation_rolls_back() {
    let app = app(":memory:");
    let id = create(&app, RECIPE, true);
    let before = serde_json::to_value(get(&app, &id)).unwrap();
    assert!(
        app.store
            .mutate(&id, |s| {
                s.current_step = 99;
                anyhow::bail!("failure")
            })
            .is_err()
    );
    assert_eq!(serde_json::to_value(get(&app, &id)).unwrap(), before);
}

#[test]
fn parser_handles_wrapped_numbered_and_plain_instruction_lines() {
    let plan = recipe::parse_demo(
        "Soup\nIngredients\n1 cup water\nInstructions\nHeat the water.\nAdd herbs.\nServe.",
    )
    .unwrap();
    assert_eq!(plan.steps.len(), 3);
    let plan =
        recipe::parse_demo("Soup\nInstructions\n1. Heat the water\nand stir gently.\n2. Serve.")
            .unwrap();
    assert_eq!(plan.steps.len(), 2);
    assert!(plan.steps[0].instruction.contains("and stir gently"));
}

#[test]
fn unsupported_fractions_do_not_create_a_misleading_timer() {
    for text in ["Rest for 1/2 hour.", "Rest for ½ hour."] {
        let plan = recipe::parse_demo(&format!("Instructions\n1. {text}")).unwrap();
        assert!(plan.steps[0].timers.is_empty());
    }
}
