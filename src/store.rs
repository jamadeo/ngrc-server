//! Host-owned durable state. Every external event answers the original tool ID.
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use goose_agent::{
    inference::InferenceEffect,
    machine::{EffectHandler, MachineSession, SessionLoader},
    operation::{ConversationEffect, Emitter, MachineEffect},
};
use goose_provider_types::conversation::{
    Conversation, message::Message, token_usage::ProviderUsage,
};
use rmcp::model::{CallToolResult, ContentBlock};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

use crate::recipe::{MAX_RECIPE_BYTES, Plan};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before Unix epoch")
        .as_millis() as u64
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Runnable,
    WaitingAction,
    WaitingTimers,
    Complete,
    Error,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Timer {
    pub label: String,
    pub recipe_seconds: u64,
    pub actual_seconds: u64,
    pub deadline_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pending {
    Action {
        tool_call_id: String,
        step: usize,
        label: String,
    },
    Timers {
        tool_call_id: String,
        step: usize,
        timers: Vec<Timer>,
    },
}
impl Pending {
    pub fn call_id(&self) -> &str {
        match self {
            Self::Action { tool_call_id, .. } | Self::Timers { tool_call_id, .. } => tool_call_id,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub recipe: String,
    pub fast_demo: bool,
    pub plan: Option<Plan>,
    /// Index of the current recipe step. Advancing is always a human action.
    pub current_step: usize,
    pub timed_steps: Vec<usize>,
    pub timer_ready_steps: Vec<usize>,
    pub answered_actions: Vec<String>,
    pub pending: Option<Pending>,
    pub status: Status,
    pub conversation: Conversation,
    pub invocations: u64,
    pub trace: Vec<String>,
    pub usage: Vec<ProviderUsage>,
    pub error: Option<String>,
}
impl Session {
    pub fn new(recipe: String, fast_demo: bool) -> Result<Self> {
        if recipe.trim().is_empty() || recipe.len() > MAX_RECIPE_BYTES {
            bail!("Paste a recipe of 1–32000 bytes");
        }
        Ok(Self {
            id: Uuid::new_v4().to_string(), recipe, fast_demo, plan: None, current_step: 0,
            timed_steps: vec![],
            timer_ready_steps: vec![], answered_actions: vec![], pending: None, status: Status::Runnable,
            conversation: Conversation::new_unvalidated([Message::user().with_text(
                "Guide me through the recipe I pasted, one step at a time. Read the recipe, save a faithful plan, then wait for my confirmations and cooking timers. Do not skip steps."
            ).with_generated_id_if_missing()]), invocations: 0,
            trace: vec!["created: full recipe persisted".into()], usage: vec![], error: None,
        })
    }
    pub fn timer_done(&self) -> bool {
        self.timed_steps.contains(&self.current_step)
    }
    fn answer(&mut self, call_id: &str, text: String) {
        self.conversation.push(
            Message::user()
                .with_tool_response(
                    call_id,
                    Ok(CallToolResult::success(vec![ContentBlock::text(text)])),
                )
                .with_generated_id_if_missing(),
        );
        self.pending = None;
        self.status = Status::Runnable;
        self.error = None;
    }
}
impl MachineSession for Session {
    fn id(&self) -> &str {
        &self.id
    }
    fn conversation(&self) -> Option<&Conversation> {
        Some(&self.conversation)
    }
}

pub enum Effect {
    Conversation(ConversationEffect),
    SavePlan(Plan),
    Wait(Pending),
    Usage(ProviderUsage),
}
impl From<Message> for Effect {
    fn from(message: Message) -> Self {
        Self::Conversation(message.into())
    }
}
impl InferenceEffect for Effect {
    fn record_usage(usage: ProviderUsage) -> Self {
        Self::Usage(usage)
    }
}
impl MachineEffect for Effect {
    fn ensure_message_ids(&mut self) {
        if let Self::Conversation(effect) = self {
            effect.ensure_message_ids();
        }
    }
}

pub struct Store(Mutex<Connection>);
impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS cooking_sessions (id TEXT PRIMARY KEY, body TEXT NOT NULL);",
        )?;
        Ok(Self(Mutex::new(connection)))
    }
    pub fn create(&self, session: &Session) -> Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT INTO cooking_sessions (id,body) VALUES (?1,?2)",
            params![session.id, serde_json::to_string(session)?],
        )?;
        Ok(())
    }
    pub fn get(&self, id: &str) -> Result<Option<Session>> {
        let body: Option<String> = self
            .0
            .lock()
            .unwrap()
            .query_row("SELECT body FROM cooking_sessions WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        body.map(|body| serde_json::from_str(&body).context("decode recipe session"))
            .transpose()
    }
    pub fn all(&self) -> Result<Vec<Session>> {
        let connection = self.0.lock().unwrap();
        let mut statement =
            connection.prepare("SELECT body FROM cooking_sessions ORDER BY rowid DESC")?;
        let bodies = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        bodies
            .into_iter()
            .map(|body| Ok(serde_json::from_str(&body)?))
            .collect()
    }
    /// Reload inside the transaction: don't overwrite an external event with an old snapshot.
    pub fn mutate(&self, id: &str, f: impl FnOnce(&mut Session) -> Result<()>) -> Result<Session> {
        let mut connection = self.0.lock().unwrap();
        let tx = connection.transaction()?;
        let body: String =
            tx.query_row("SELECT body FROM cooking_sessions WHERE id=?1", [id], |r| {
                r.get(0)
            })?;
        let mut session: Session = serde_json::from_str(&body)?;
        f(&mut session)?;
        tx.execute(
            "UPDATE cooking_sessions SET body=?2 WHERE id=?1",
            params![id, serde_json::to_string(&session)?],
        )?;
        tx.commit()?;
        Ok(session)
    }
    /// Retries correlate to the specific wait ID, not a generic 'next' click.
    pub fn action(&self, id: &str, call_id: &str) -> Result<Session> {
        self.mutate(id, |s| {
            if s.answered_actions.iter().any(|id| id == call_id) { return Ok(()); }
            let Some(Pending::Action { tool_call_id, step, .. }) = s.pending.clone() else { bail!("not waiting for an action"); };
            if tool_call_id != call_id || step != s.current_step || s.status != Status::WaitingAction { bail!("wrong action ID"); }
            let plan = s.plan.as_ref().context("missing plan")?;
            let current = plan.steps.get(step).context("missing step")?;
            let start_timer = !current.timers.is_empty() && !s.timer_done();
            if start_timer { s.timer_ready_steps.push(step); } else { s.current_step += 1; }
            s.answered_actions.push(call_id.to_owned());
            s.answer(call_id, if start_timer {
                format!("The user confirmed step {} is ready for timing. Start this step's timers now.", step + 1)
            } else {
                format!("The user confirmed step {} is complete. Proceed to step {} or finish if no steps remain.", step + 1, s.current_step + 1)
            });
            s.trace.push(format!("resume: action answered tool {call_id}")); Ok(())
        })
    }
    /// A timer response never implies food is done: user must check and confirm.
    pub fn expire_timers(&self, now: u64) -> Result<()> {
        for session in self.all()? {
            if !matches!(&session.pending, Some(Pending::Timers { timers, .. }) if timers.iter().all(|t| t.deadline_ms <= now))
            {
                continue;
            }
            self.mutate(&session.id, |s| {
                if let Some(Pending::Timers { tool_call_id, step, timers }) = s.pending.clone()
                    && s.status == Status::WaitingTimers && timers.iter().all(|t| t.deadline_ms <= now) {
                    if !s.timed_steps.contains(&step) { s.timed_steps.push(step); }
                    s.answer(&tool_call_id, format!("Timers for step {} have elapsed. Ask the user to check progress/doneness against the original recipe and confirm completion. Timer expiry is NOT proof of food safety.", step + 1));
                    s.trace.push(format!("resume: timers answered tool {tool_call_id}"));
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}
#[async_trait]
impl SessionLoader<Session> for Store {
    async fn load(&self, id: &str) -> Result<Session> {
        self.get(id)?.context("session not found")
    }
}
#[async_trait]
impl EffectHandler<Session, Effect> for Store {
    async fn apply_effects(
        &self,
        session: &Session,
        effects: &mut [Effect],
        _emit: &Emitter,
    ) -> Result<()> {
        self.mutate(&session.id, |current| {
            for effect in effects {
                match effect {
                    Effect::Conversation(ConversationEffect::AppendMessage(message)) => {
                        current.conversation.push(message.clone())
                    }
                    Effect::SavePlan(plan) => {
                        plan.validate()?;
                        if current.plan.is_some() {
                            bail!("plan already saved");
                        }
                        current.plan = Some(plan.clone());
                        current
                            .trace
                            .push("plan: recipe steps and timers persisted".into());
                    }
                    Effect::Wait(pending) => {
                        current.status = match pending {
                            Pending::Action { .. } => Status::WaitingAction,
                            Pending::Timers { .. } => Status::WaitingTimers,
                        };
                        current.trace.push(format!(
                            "yield: {:?}; tool {} remains unanswered; agent can be dropped",
                            current.status,
                            pending.call_id()
                        ));
                        current.pending = Some(pending.clone());
                    }
                    Effect::Usage(usage) => current.usage.push(usage.clone()),
                    Effect::Conversation(_) => bail!("unsupported conversation effect"),
                }
            }
            Ok(())
        })?;
        Ok(())
    }
}
