"use strict";
const $ = (id) => document.getElementById(id);
let session = null;
let sessionId = localStorage.getItem("recipe-session");
let actionBusy = false;
let polling = false;
let example = "";
let lastRendered = "";
const labels = { runnable: "Agent running", waiting_action: "Agent yielded · waiting for you", waiting_timers: "Agent yielded · waiting for timers", complete: "Recipe complete", error: "Agent stopped" };
async function api(path, data) {
  const response = await fetch(path, data === undefined ? {} : { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(data) });
  if (!response.ok) {
    let message = `Request failed (${response.status})`;
    const text = await response.text();
    try { message = JSON.parse(text).error || message; } catch { /* do not display raw server HTML */ }
    const error = new Error(message); error.status = response.status; throw error;
  }
  return response.json();
}
function showError(message) { $("error").textContent = message; $("error").hidden = !message; }
function duration(seconds) {
  if (seconds % 3600 === 0) return `${seconds / 3600} hr`;
  if (seconds % 60 === 0) return `${seconds / 60} min`;
  return `${seconds} sec`;
}
function countdown() {
  if (session?.pending?.kind !== "timers") return;
  for (const [index, timer] of session.pending.timers.entries()) {
    const seconds = Math.max(0, Math.ceil((timer.deadline_ms - Date.now()) / 1000));
    const node = $("countdown-" + index);
    if (node) node.textContent = seconds ? `${Math.floor(seconds / 3600) ? Math.floor(seconds / 3600) + ":" : ""}${String(Math.floor(seconds / 60) % 60).padStart(2, "0")}:${String(seconds % 60).padStart(2, "0")}` : "Elapsed · waiting for agent";
  }
}
function render(next) {
  session = next;
  const serialized = JSON.stringify(next);
  if (lastRendered === serialized) return;
  lastRendered = serialized;
  $("session").hidden = false;
  $("status").textContent = labels[next.status] || next.status;
  $("entries").textContent = `${next.invocations} fresh invocation${next.invocations === 1 ? "" : "s"}`;
  const plan = next.plan;
  $("recipe-title").textContent = plan?.title || "Planning your recipe…";
  $("progress-label").textContent = plan ? `${next.current_step} of ${plan.steps.length} steps confirmed${next.status === "complete" ? " · enjoy!" : ""}` : "The agent is reading the full recipe and preparing a plan.";
  $("progress").max = plan?.steps.length || 1;
  $("progress").value = next.current_step;
  $("steps").replaceChildren(...(plan?.steps || []).map((step, index) => {
    const item = document.createElement("li");
    item.className = index < next.current_step ? "finished" : index === next.current_step ? "current" : "";
    item.textContent = step.instruction + (step.timers.length ? ` [${step.timers.map((t) => duration(t.seconds)).join(", ")}]` : "");
    return item;
  }));
  const messages = [];
  // Render model/recipe data as text, never HTML. Internal tool messages stay in JSON.
  for (const message of next.conversation) {
    const text = message.content.filter((c) => c.type === "text").map((c) => c.text).join("\n");
    if (!text || message.role === "user") continue;
    const node = document.createElement("div"); node.className = "message";
    const title = document.createElement("strong"); title.textContent = "Cooking companion";
    node.append(title, document.createTextNode(text)); messages.push(node);
  }
  $("messages").replaceChildren(...messages);
  $("messages").scrollTop = $("messages").scrollHeight;
  $("action-panel").hidden = next.status !== "waiting_action";
  $("timer-panel").hidden = next.status !== "waiting_timers";
  $("done-panel").hidden = next.status !== "complete";
  $("action").textContent = next.pending?.kind === "action" ? next.pending.label : "Continue";
  $("action").disabled = actionBusy;
  $("timers").replaceChildren(...(next.pending?.kind === "timers" ? next.pending.timers : []).map((timer, index) => {
    const node = document.createElement("div"); node.className = "timer";
    const label = document.createElement("p"); label.textContent = timer.label;
    const countdown = document.createElement("span"); countdown.id = "countdown-" + index; countdown.className = "countdown"; countdown.setAttribute("role", "timer");
    const time = document.createElement("p"); time.textContent = next.fast_demo ? `Demo: ${duration(timer.actual_seconds)} · Recipe: ${duration(timer.recipe_seconds)} — don't use demo times for cooking` : `Recipe time: ${duration(timer.recipe_seconds)}`;
    node.append(label, countdown, time); return node;
  }));
  $("ingredients").replaceChildren(...(plan?.ingredients || []).map((ingredient) => { const li = document.createElement("li"); li.textContent = ingredient; return li; }));
  $("notes").replaceChildren(...(plan?.notes || []).map((note) => { const p = document.createElement("p"); p.className = "hint"; p.textContent = note; return p; }));
  $("original").textContent = next.recipe;
  $("trace").textContent = next.trace.join("\n");
  $("pending").textContent = next.pending ? `Open tool call: ${next.pending.tool_call_id} (${next.pending.kind}, step ${next.pending.step + 1})` : "No open wait.";
  $("raw").href = `/api/sessions/${encodeURIComponent(next.id)}`;
  $("start").textContent = "Start another recipe →";
  showError(next.error || ""); countdown();
}
async function refresh() {
  if (!sessionId || polling) return;
  const id = sessionId; polling = true;
  try {
    const next = await api(`/api/sessions/${encodeURIComponent(id)}`);
    if (id === sessionId) render(next);
    $("connection").textContent = "Saved locally. Refresh or restart the server without losing your place.";
  } catch (error) {
    if (error.status === 404 && id === sessionId) { localStorage.removeItem("recipe-session"); sessionId = null; session = null; lastRendered = ""; $("session").hidden = true; }
    else { $("connection").textContent = "Server offline? Your progress is saved. Reconnecting automatically…"; }
  } finally { polling = false; }
}
$("start-form").addEventListener("submit", async (event) => {
  event.preventDefault(); $("start").disabled = true; showError("");
  try {
    const next = await api("/api/sessions", { recipe: $("recipe").value, fast_demo: $("fast").checked });
    sessionId = next.id; localStorage.setItem("recipe-session", sessionId); render(next);
  } catch (error) { showError(error.message); }
  finally { $("start").disabled = false; }
});
$("action").addEventListener("click", async () => {
  if (actionBusy || session?.pending?.kind !== "action") return;
  const id = session.id; const call = session.pending.tool_call_id;
  actionBusy = true; $("action").disabled = true;
  try { const next = await api(`/api/sessions/${encodeURIComponent(id)}/action`, { tool_call_id: call }); if (id === sessionId) render(next); }
  catch (error) { showError(error.message); }
  finally { actionBusy = false; $("action").disabled = false; }
});
$("example").addEventListener("click", () => { if (example) { $("recipe").value = example; $("recipe").focus(); } });
async function mode() {
  try {
    const config = await api("/api/config"); example = config.example;
    $("mode").textContent = config.mode === "demo" ? "No-key demo · simple parser" : "Live model";
    $("mode-note").textContent = config.mode === "demo" ? "Simple parser, not AI recipe understanding. Use numbered Instructions; verify detected timers in the plan. Try the bundled example." : "The model reads your full recipe and prepares the step plan. Review it against the original before cooking.";
  } catch { $("mode").textContent = "Reconnecting…"; }
}
mode(); refresh();
setInterval(refresh, 500); setInterval(countdown, 200); setInterval(mode, 5000);
