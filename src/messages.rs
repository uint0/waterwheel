use crate::server::tokens::Token;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::types::Uuid;

#[derive(Serialize, Deserialize, Debug, sqlx::FromRow)]
pub struct TaskDef {
    pub task_id: String,
    pub trigger_datetime: String,
    pub image: String,
    pub args: Vec<String>,
    pub env: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TaskResult {
    pub task_id: String,
    pub trigger_datetime: String,
    pub result: String,
}

impl TaskResult {
    pub fn get_token(&self) -> Result<Token> {
        Ok(Token {
            task_id: Uuid::parse_str(&self.task_id)?,
            trigger_datetime: DateTime::parse_from_rfc3339(&self.trigger_datetime)?
                .with_timezone(&Utc),
        })
    }
}