use anyhow::{anyhow, Result};

use super::Backend;
use crate::model::{Session, Transcript};

#[derive(Clone, Debug, Default)]
pub struct OpenCodeBackend;

impl Backend for OpenCodeBackend {
    fn harness(&self) -> &'static str {
        "opencode"
    }

    fn available(&self) -> bool {
        false
    }

    fn list(&self, _limit: usize) -> Result<Vec<Session>> {
        Ok(Vec::new())
    }

    fn transcript(&self, id: &str, _tail: usize) -> Result<Transcript> {
        Err(anyhow!("opencode session {id} is unavailable"))
    }
}
