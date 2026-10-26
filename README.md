# CI investigator: a re-entrant GDK agent server

A small Rust server demonstrating **an agent tool call that spans two process lifetimes**.
A failed GitHub Actions run starts an investigation. The agent inspects failed jobs,
optionally requests **one** rerun, and **yields**. A later GitHub webhook answers the
open tool call. A newly constructed agent continues the same conversation and writes
a report.

This is deliberately not an autonomous coding bot: it doesn't clone repositories,
execute repository code, push fixes, or post comments. Its only GitHub write is a
single rerun of failed jobs. Reports are available through the session API.

## The demonstration

```text
GitHub: attempt 1 failed
          │
          ▼
Webhook ──► SQLite ──► fresh StateMachine #1
                         │
                         ├─ InferenceRunner: ask to inspect_failed_jobs
                         ├─ ToolOperation: return failed job/step metadata
                         ├─ InferenceRunner: ask to rerun_failed_jobs
                         └─ RerunAndWait: persist intent + yielded_with(...)
                                             │
                               NO TOOL RESPONSE YET
                                             │
                              worker dispatches GitHub POST
                              state = waiting_ci
                              agent instance is gone
                                             │
                    ┌── server can stop/restart here ──┐
                    └───────────────────────────────────┘
                                             │
GitHub: attempt 2 completed                   │
          │                                  │
          ▼                                  ▼
Webhook ──► append tool response to original call ID
          └─► SQLite ──► fresh StateMachine #2
                            └─ InferenceRunner: final report
```

**Re-entry is not an in-memory coroutine resuming.** The durable conversation and
pending tool-call ID are sufficient to reconstruct what happens next. Nothing
waits in an agent task while CI runs, and the GDK reloads the session between steps.
The host's small worker stays available to process new durable inputs.

## Run the credential-free demo

Requires Cargo/Rust and Python 3. Tested with Rust 1.96.0; the GDK crates declare
Rust 1.94.1 as their minimum. The first build downloads crates and compiles SQLite.

```sh
python3 scripts/demo.py
```

The script builds the binary, starts it on a temporary loopback port, sends a
failure webhook, waits for the agent to yield, **stops and restarts the server**,
and sends a success webhook. It prints the persisted step trace and final report,
then cleans up its temporary database. It asserts that exactly two agent
invocations occurred.

Both inference and GitHub responses are **synthetic** in demo mode. The real GDK
`StateMachine`, `InferenceRunner`, and `ToolOperation`, the real HTTP endpoints,
and SQLite persistence are exercised. No model or GitHub credentials are used.

## Run the server yourself

```sh
cargo run --locked
```

Defaults: demo mode, `127.0.0.1:3000`, repository `acme/widgets`, database
`./ci-agent.sqlite3`. Demo mode refuses non-loopback bind addresses.

Send an initial failure:

```sh
curl -fsS http://127.0.0.1:3000/webhooks/github \
  -H 'Content-Type: application/json' \
  -H 'X-GitHub-Event: workflow_run' \
  -H 'X-GitHub-Delivery: local-failure-42' \
  -d '{"action":"completed","repository":{"full_name":"acme/widgets"},"workflow_run":{"id":42,"run_attempt":1,"head_sha":"abc123","conclusion":"failure"}}'
```

Inspect the pending call, conversation, usage, and execution trace:

```sh
curl -fsS http://127.0.0.1:3000/sessions/acme:widgets:42
```

Wait until `status` is `waiting_ci`. Restart the server if desired, then send:

```sh
curl -fsS http://127.0.0.1:3000/webhooks/github \
  -H 'Content-Type: application/json' \
  -H 'X-GitHub-Event: workflow_run' \
  -H 'X-GitHub-Delivery: local-success-42' \
  -d '{"action":"completed","repository":{"full_name":"acme/widgets"},"workflow_run":{"id":42,"run_attempt":2,"head_sha":"abc123","conclusion":"success"}}'
```

Read the session again: `status` becomes `complete`, `invocations` becomes `2`,
and the last assistant message is the report. Use `failure` or `cancelled` instead
of `success` to demonstrate another outcome. Use a fresh run ID for a new
investigation; each repository/run pair has one investigation.

## Live GitHub + model mode

Live mode uses `goose-providers`' `OpenAiProvider`. The host constructs the provider
explicitly; it does **not** load your desktop/CLI goose configuration.

Set these variables in your shell or secret manager (no `.env` auto-loading):

| Variable | Meaning |
| --- | --- |
| `APP_MODE=live` | Enable actual model calls and GitHub writes |
| `GITHUB_REPOSITORY` | Exact allowlisted `owner/repo` |
| `GITHUB_TOKEN` | Fine-grained PAT or GitHub App installation token for that repository; Actions **read/write** access |
| `GITHUB_WEBHOOK_SECRET` | Shared secret configured on the repository webhook |
| `OPENAI_API_KEY` | API key for the GDK OpenAI provider |
| `AGENT_MODEL` | Your available tool-capable OpenAI model ID; no implicit model choice |
| `ADMIN_TOKEN` | Bearer token protecting session inspection endpoints |
| `DATABASE_PATH` | Optional SQLite filename; default `ci-agent.sqlite3` |
| `BIND_ADDR` | Optional socket address; default `127.0.0.1:3000` |
| `RUST_LOG` | Optional logging filter, e.g. `ngrc_server=info` |

Then start with `cargo run --locked`. Configure a repository webhook with:

- Payload URL: your HTTPS ingress URL + `/webhooks/github`.
- Content type: `application/json`.
- Secret: the same value as `GITHUB_WEBHOOK_SECRET`.
- Event: **Workflow runs** (`workflow_run`); only `action=completed` is handled.

Use a TLS reverse proxy or tunnel to reach the loopback server; do not expose demo
mode. Query live sessions with `Authorization: Bearer <ADMIN_TOKEN>`.

The live agent receives **failed job and step names, not raw logs**. It can inspect,
choose to retry once, and report what the retry actually returned. It may decide
against retrying and finish in one invocation. No claim of root-cause certainty is
made from job names alone. A pass suggests a transient failure, not proof.

**Use a disposable development repository.** Rerunning CI can execute privileged
jobs or deployment steps. A signed webhook authenticates GitHub's payload, not the
trustworthiness of job names or repository content. The prompt treats those as
untrusted data, and host-side policy limits reruns to one per investigation. This
is not a substitute for approval rules or workflow-level allowlists.

Live mode is compiled and covered by webhook/auth tests, but actual GitHub writes
and paid model calls have not been exercised by this project's local test run.

## API

| Endpoint | Purpose |
| --- | --- |
| `GET /health` | Unauthenticated liveness check |
| `POST /webhooks/github` | Persist webhook input, then acknowledge with `202`; no inference inside HTTP handling |
| `GET /sessions` | Investigation summaries |
| `GET /sessions/{id}` | Full persisted session, conversation, trace, pending call, usage, and error |

Session IDs look like `owner:repo:run_id`. Session reads require the admin bearer
token in live mode. Webhooks require `X-GitHub-Delivery`; live mode also requires
valid `X-Hub-Signature-256` over the **raw body bytes**. Bodies are limited to 1 MiB.
Duplicate delivery IDs are acknowledged without applying the event twice.

## Where the GDK is demonstrated

All three direct GDK dependencies are pinned to `0.1.0-alpha.13`.

| File | Responsibility |
| --- | --- |
| `src/agent.rs` | Fresh pipeline construction, custom yielding `Operation`, open-call guard, bounded `step` / `apply` driver, `finalize`, deterministic demo `Provider` |
| `src/store.rs` | SQLite host runtime implementing `MachineSession`, `SessionLoader`, `EffectHandler`; custom `MachineEffect` / `InferenceEffect` |
| `src/github.rs` | Fixed-host GitHub API client and typed webhook payload |
| `src/server.rs` | Webhook ingestion, deduplication, correlation, durable dispatch, worker, inspection API |
| `src/main.rs` | Explicit provider and host configuration |
| `src/tests.rs` | Restart/resume, duplicate delivery, correlation, auth, recovery, and loop-boundary tests |
| `scripts/demo.py` | Actual HTTP/process-restart demonstration |

Pipeline ordering matters:

1. `ToolOperation` handles ordinary immediate tools, including `inspect_failed_jobs`.
2. `RerunAndWait` handles the long-lived tool: commit a pending intent and yield,
   leaving the model's original tool request open.
3. `OpenCallGuard` prevents inference with unanswered calls. Unknown tools get an
   error response, rather than silently wedging the conversation.
4. `InferenceRunner` calls the provider with tools and prompt parts collected from
   the operations. When the matching completion webhook supplies a tool response,
   it can infer again.

`Effect::Wait` persists the intent separately from dispatching the GitHub request.
This avoids performing the external write in a GDK operation's cancellable future.
The custom effect is not a GDK built-in: it illustrates host-owned application state
alongside conversation and usage effects. Emitted `AgentEvent`s are debug logging;
only applied effects are the durable source of truth.

## Reliability boundaries

- **Single process, single worker.** Do not run multiple server instances against
  this database. A production deployment needs ownership leases / fencing.
- SQLite transactions atomically apply webhook inputs with delivery deduplication.
  Agent effects reload the current row before mutation so they don't overwrite a
  concurrent webhook's state with a stale session snapshot.
- A completion answers the tool only for the same repository, run ID, commit SHA,
  and **exact expected attempt**. Repeated completion events cannot answer twice.
- Pending CI waits survive restart. Runnable sessions and queued dispatch intents
  are picked up by the worker. No durable task queue service is required here.
- **No exactly-once promise for external writes.** GitHub's rerun POST has no
  idempotency key. Before POSTing, the host persists `dispatching`. If a network
  error or restart makes the result uncertain, it marks the session `error` and
  does **not** automatically POST again. Check GitHub manually. A matching
  completion webhook can still resolve the open call and resume the session.
- Ordinary read tools / inference may replay after a hard crash before their
  effects commit. A repeated inference can incur extra model cost.
- Missing completion events leave the session waiting. There is no expiry,
  reconciliation poller, or manual-resume API. Out-of-order completions arriving
  **before the pending intent exists** are not buffered/reconciled; redelivery or
  a production event inbox would be needed.
- Invocations are limited to 24 machine steps and 120 seconds; failures are
  inspectable as `error`. These are per-invocation, not lifetime spending quotas.
- Ctrl-C stops accepting HTTP and allows the current bounded worker batch to
  finish. A hard kill still has the external-POST ambiguity described above.
- Session history / delivery IDs have no retention policy. The JSON-blob SQLite
  store uses synchronous short transactions and scans sessions; it is intentionally
  instructional, not a production-scale scheduler or public multi-tenant API.

## Checks

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
python3 scripts/demo.py
```

## Verification completed

GDK APIs were checked against the published alpha.13 crate source/READMEs and
compiled, rather than inferred from the high-level goose app configuration:

- [goose-agent README](https://docs.rs/crate/goose-agent/0.1.0-alpha.13): state-machine model and explicit yielding.
- [goose-agent API](https://docs.rs/goose-agent/0.1.0-alpha.13/goose_agent/): `StateMachine`, operations, runtime traits, inference, tools, and cancellation finalization.
- [goose-provider-types API](https://docs.rs/goose-provider-types/0.1.0-alpha.13/goose_provider_types/): conversation, messages, provider contract, model config, and usage.
- [goose-providers README](https://docs.rs/crate/goose-providers/0.1.0-alpha.13): provider implementations and `rustls-tls` feature.
- [Official supported provider documentation](https://goose-docs.ai/docs/getting-started/providers): OpenAI support and `OPENAI_API_KEY`.
- [Official session documentation](https://goose-docs.ai/docs/guides/sessions/session-management): session lifecycle context. This demo implements its own GDK session store, not the app's session management API.
- [GitHub workflow runs API](https://docs.github.com/en/rest/actions/workflow-runs#re-run-failed-jobs-from-a-workflow-run): failed-job rerun endpoint and Actions write permission.
- [GitHub workflow jobs API](https://docs.github.com/en/rest/actions/workflow-jobs#list-jobs-for-a-workflow-run-attempt): attempt-scoped job metadata endpoint.
