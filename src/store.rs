//! The host owns persistence. The GDK only needs SessionLoader + EffectHandler.
use std::{path::Path, sync::Mutex};

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
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Runnable,
    RerunQueued,
    Dispatching,
    WaitingCi,
    Complete,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingRerun {
    pub tool_call_id: String,
    pub expected_attempt: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub repository: String,
    pub run_id: u64,
    pub original_attempt: u64,
    pub head_sha: String,
    pub status: Status,
    pub conversation: Conversation,
    pub pending: Option<PendingRerun>,
    pub rerun_used: bool,
    pub invocations: u64,
    pub trace: Vec<String>,
    pub usage: Vec<ProviderUsage>,
    pub error: Option<String>,
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
    Wait(PendingRerun),
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
            CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS deliveries (id TEXT PRIMARY KEY);",
        )?;
        Ok(Self(Mutex::new(connection)))
    }

    pub fn get(&self, id: &str) -> Result<Option<Session>> {
        let connection = self.0.lock().unwrap();
        let body: Option<String> = connection
            .query_row("SELECT body FROM sessions WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        body.map(|body| serde_json::from_str(&body).context("decode session"))
            .transpose()
    }

    pub fn all(&self) -> Result<Vec<Session>> {
        let connection = self.0.lock().unwrap();
        let mut statement = connection.prepare("SELECT body FROM sessions ORDER BY id")?;
        let bodies = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        bodies
            .into_iter()
            .map(|body| Ok(serde_json::from_str(&body)?))
            .collect()
    }

    /// Every mutation reloads the latest row inside a transaction, so a webhook
    /// cannot be overwritten by an agent's older session snapshot.
    pub fn mutate(&self, id: &str, f: impl FnOnce(&mut Session) -> Result<()>) -> Result<Session> {
        let mut connection = self.0.lock().unwrap();
        let tx = connection.transaction()?;
        let body: String =
            tx.query_row("SELECT body FROM sessions WHERE id=?1", [id], |r| r.get(0))?;
        let mut session: Session = serde_json::from_str(&body)?;
        f(&mut session)?;
        tx.execute(
            "UPDATE sessions SET body=?2 WHERE id=?1",
            params![id, serde_json::to_string(&session)?],
        )?;
        tx.commit()?;
        Ok(session)
    }

    /// Deduplication and event application commit together. No network work in here.
    pub fn ingest(
        &self,
        delivery: &str,
        id: &str,
        f: impl FnOnce(Option<Session>) -> Result<Option<Session>>,
    ) -> Result<bool> {
        let mut connection = self.0.lock().unwrap();
        let tx = connection.transaction()?;
        if tx.execute(
            "INSERT OR IGNORE INTO deliveries (id) VALUES (?1)",
            [delivery],
        )? == 0
        {
            return Ok(false);
        }
        let body: Option<String> = tx
            .query_row("SELECT body FROM sessions WHERE id=?1", [id], |r| r.get(0))
            .optional()?;
        let previous = body.map(|body| serde_json::from_str(&body)).transpose()?;
        if let Some(session) = f(previous)? {
            tx.execute(
                "INSERT INTO sessions (id,body) VALUES (?1,?2)
                ON CONFLICT(id) DO UPDATE SET body=excluded.body",
                params![id, serde_json::to_string(&session)?],
            )?;
        }
        tx.commit()?;
        Ok(true)
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
                        current.conversation.push(message.clone());
                    }
                    Effect::Wait(pending) => {
                        current.pending = Some(pending.clone());
                        current.rerun_used = true;
                        current.status = Status::RerunQueued;
                        current.trace.push(format!(
                            "yield: tool {} stays open; waiting for CI attempt {}",
                            pending.tool_call_id, pending.expected_attempt
                        ));
                    }
                    Effect::Usage(usage) => current.usage.push(usage.clone()),
                    // This demo's pipeline produces only AppendMessage, Wait, and Usage.
                    Effect::Conversation(_) => bail!("unsupported conversation effect"),
                }
            }
            Ok(())
        })?;
        Ok(())
    }
}
