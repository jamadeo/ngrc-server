# One step at a time 🥣

Paste a whole cooking recipe and a small GDK agent guides you through it: one
step, confirmation, and cooking timer at a time. A useful, self-contained demo
of **re-entrant agents**, with no external integrations to configure.

The default is a **credential-free demo** with simple recipe parsing and scripted
model replies. Optional live-model mode interprets less structured recipes. In
both modes, the GDK loop, tool dispatch, human confirmations, timers, HTTP server,
and SQLite persistence are real.

## Try it

Requires Cargo/Rust (tested with 1.96.0; GDK crates declare 1.94.1 minimum).

```sh
cargo run --locked
```

Open **http://127.0.0.1:3000**. Click **Try an example** for a complete lemon
couscous recipe, or paste your own title, ingredients, and instructions.

1. Click **Let's cook**. The agent saves an ordered plan, then guides step 1.
2. Click **Done — next step** after performing an untimed step.
3. For a timed step, perform the preparation, then click **Ready — start cooking
   timer(s)** when the timed action has actually begun.
4. The agent starts the timer(s), saves their deadlines, and yields. No agent task
   is waiting in memory.
5. When they expire, the agent resumes and asks you to **check the food**. Confirm
   only when that step is complete. A timer does not automatically advance a step.
6. Repeat until all steps are confirmed. Enjoy what you've made!

**Fast demo** is on by default: every timer is capped at **5 seconds**. These are
**not real cooking times**. Uncheck it when using the recipe's actual times.
Original recipe times remain visible beside the demo countdowns.

The page shows recipe progress, ingredients, plan notes, original text, agent
messages, open tool calls and the persisted execution trace. Review the plan and
timers against your original recipe before cooking. The app is an instructional
prototype, not a food-safety authority.

## Recipe interpretation

### No-key demo

A small parser (`src/recipe.rs`) turns numbered directions into steps and detects
explicit durations. The provider then emits deterministic tool calls through the
real GDK `InferenceRunner`.

Best input format:

```text
A quick bowl

Ingredients
1 cup oats
1 cup water

Instructions
1. Combine the oats and water in a saucepan.
2. Simmer for 2 minutes, stirring and checking the texture.
3. Check that the oats are cooked to your preference, then serve.
```

Supported duration examples: `90 seconds`, `5 minutes`, `five minutes`,
`0.5 minutes`, `1 hour and 20 minutes`. Ranges such as `10–15 minutes` use the
lower end as a **progress check**, not guaranteed doneness.

This is **not semantic recipe understanding**. Ambiguous multiple waits in one
step, fractions, `overnight`, preheating, and doneness-only instructions need
manual timing. Split sequential timed actions into separate numbered steps.
Ingredients and step wording are retained, but always verify the detected plan.
At most 40 steps, 4 timers per step, and 24 hours per timer are accepted. Timers
within one step run simultaneously, and the agent resumes after all have elapsed.
The demo parser conservatively omits ambiguous multiple durations; live-model
plans can represent concurrent timers explicitly.

### Live model

Set `APP_MODE=live`, `OPENAI_API_KEY`, and `AGENT_MODEL` to an available,
tool-capable OpenAI model ID, then run the same server. The host explicitly
constructs `goose-providers`' `OpenAiProvider`; it does not load the goose
CLI/desktop configuration or automatically read `.env` files.

The model reads the full pasted recipe and submits a plan through
`save_recipe_plan`. It is instructed to preserve quantities, temperatures,
ordering, warnings and doneness criteria, and not invent timings. The host
validates the plan's shape and bounds, **not semantic correctness**. Review it
against the original. Actual paid model calls have not been exercised locally.

## Re-entrancy

```text
paste recipe → fresh machine: read recipe, save plan, guide step 1
                                      │
                              wait_for_action → YIELD
                                      │
                           human button supplies tool result
                                      │
                  fresh machine: guide next step / start timers
                                      │
                             start_step_timers → YIELD
                                      │
                        host finds saved deadlines have elapsed
                                      │
                     fresh machine: ask user to check the food
                                      │
                              wait_for_action → YIELD
                                      │
                  repeat for the remaining steps → final message
```

An asynchronous operation commits a wait **without answering the model's tool
call**. The machine and operations are dropped. A later button click or timer
answers the original tool-call ID. A **new** machine continues the same saved
conversation. This is reconstruction, not resuming an in-memory coroutine.

Stop/restart during either wait. The browser reconnects automatically; refreshing
restores the current session from local storage. Overdue timers are processed on
startup. The app cannot notify you while the server is off. Starting another
recipe does not cancel a previous session's timers.

### GDK assembly

- `ToolOperation` + `RecipeContext`: an ordinary immediate `read_recipe` tool.
- `RecipeFlow`: `save_recipe_plan` applies a custom plan effect;
  `wait_for_action` and `start_step_timers` use `yielded_with` to persist a wait.
- `OpenCallGuard`: prevents inference past unanswered calls; unknown tools receive
  error responses rather than silently wedging the loop.
- `InferenceRunner`: calls the selected provider, with tools and prompt parts
  contributed by the operations. Prompt progress is regenerated from persisted state.
- Host runtime: `MachineSession`, `SessionLoader`, `EffectHandler`, and custom
  `MachineEffect` / `InferenceEffect` persist conversation, plan, waits and usage.
- Explicit `step` / `apply` driving provides a readable trace and execution budget;
  `finalize` is called on exit, including interrupted inference.

GDK dependencies are pinned to `0.1.0-alpha.13`. Runtime effects, not emitted debug
events, are the durable source of truth. The host enforces step order, user
confirmation before timers, and user confirmation after timer expiry. Recipe
content is treated as data in the model prompt, and rendered as text, never HTML.

## API

| Endpoint | Purpose |
| --- | --- |
| `GET /` | Embedded single-page UI |
| `GET /health` | Liveness |
| `GET /api/config` | Demo/live mode and bundled example recipe |
| `POST /api/sessions` | `{"recipe":"<full text>","fast_demo":true}`; persist before inference and return `202` |
| `GET /api/sessions` | Saved sessions, newest first |
| `GET /api/sessions/{id}` | Recipe, plan, current step, pending wait, conversation, usage, trace and error |
| `POST /api/sessions/{id}/action` | `{"tool_call_id":"<pending.tool_call_id>"}` answers the current human wait |

Recipes are limited to 32,000 bytes. `fast_demo` defaults to `true`. Unknown
fields and invalid plans are rejected. Retrying an action ID that was already
answered is harmless, even after moving to later steps; an old click cannot
accidentally skip the new step or restart a timer.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `APP_MODE` | `demo` | `demo` or `live` |
| `DATABASE_PATH` | `recipe-agent.sqlite3` | Durable SQLite filename |
| `BIND_ADDR` | `127.0.0.1:3000` | Loopback only in both modes |
| `OPENAI_API_KEY` | — | Required for live mode |
| `AGENT_MODEL` | — | Required tool-capable model ID for live mode |
| `RUST_LOG` | `ngrc_server=info` | Set debug for GDK events |

Recipe data uses its own table and default filename, separate from the earlier
tea/CI demos.

## Files

| File | Responsibility |
| --- | --- |
| `src/agent.rs` | GDK pipeline, plan/wait operations, guarded tools, driver and scripted provider |
| `src/recipe.rs` | Plan types, validation, simple no-key parser, bundled example |
| `src/store.rs` | SQLite runtime, step progress, idempotent actions, durable deadlines |
| `src/server.rs` | HTTP API, embedded assets, local request policy, worker |
| `src/main.rs` | Explicit provider and host configuration |
| `static/` | Dependency-free UI; no frontend build or CDN |
| `src/tests.rs` | Restart, action/timer, parser, bounds, idempotency and HTTP tests |
| `scripts/demo.py` | End-to-end HTTP demo with two hard process kills/restarts |

## Boundaries

- This is a local, single-process/single-worker demo. No multi-instance ownership
  leases, authentication, chat/questions, timer extensions, cancellation or retention.
  Do not expose it publicly.
- Local Host and Origin checks reject nonlocal/cross-origin browser requests.
  Assets use a restrictive content security policy. This is not a replacement for
  authentication on a real deployment.
- Timer deadlines use wall time and are checked every 200 ms between worker
  batches. System clock changes or another session's long live inference can
  delay resumption. There are no OS notifications or background alerts while off.
- At most 24 machine steps and 120 seconds per invocation. Invalid model behavior
  produces an inspectable error; it is not automatically retried. Inference can
  replay after a hard crash before effects commit, adding model cost.
- Timer completion is a reminder, never a food-safety or doneness guarantee. You
  must check the original recipe and confirm each step. Demo-mode timers must not
  be used for actual cooking.

## Checks

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
python3 scripts/demo.py
```

The Python script needs only the standard library. It pastes the bundled recipe,
hard-kills and restarts while waiting for a human action, starts a timer, kills
the process again, lets the deadline pass offline, then restarts and confirms
all remaining steps. It verifies **seven fresh invocations** for the example.

### Verification completed

GDK interfaces were checked against published alpha.13 crate source and compiled:

- [goose-agent README](https://docs.rs/crate/goose-agent/0.1.0-alpha.13) and [API](https://docs.rs/goose-agent/0.1.0-alpha.13/goose_agent/): state machine, operations, yielding, tools, runtime traits, inference and finalization.
- [goose-provider-types API](https://docs.rs/goose-provider-types/0.1.0-alpha.13/goose_provider_types/): provider contract, conversation, messages, model config and usage.
- [goose-providers README](https://docs.rs/crate/goose-providers/0.1.0-alpha.13): provider implementation and TLS feature.
- [Official provider documentation](https://goose-docs.ai/docs/getting-started/providers): OpenAI support and `OPENAI_API_KEY`.

“Recipe” in this demo means cooking instructions; it is not a goose YAML recipe.
