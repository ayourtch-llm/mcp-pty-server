use std::collections::HashMap;

use crate::session::{PtyError, PtySession, Result, SessionId, SessionMetadata};

const DEFAULT_MAX_SESSIONS: usize = 32;

pub struct SessionManager {
    sessions: HashMap<SessionId, PtySession>,
    next_id: u64,
    max_sessions: usize,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            next_id: 1,
            max_sessions: DEFAULT_MAX_SESSIONS,
        }
    }

    pub fn generate_id(&mut self) -> SessionId {
        let id = format!("pty-{}", self.next_id);
        self.next_id += 1;
        id
    }

    pub fn insert(&mut self, session: PtySession) -> Result<SessionId> {
        if self.sessions.len() >= self.max_sessions {
            return Err(PtyError::MaxSessions(self.max_sessions));
        }
        let id = session.id.clone();
        self.sessions.insert(id.clone(), session);
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Result<&PtySession> {
        self.sessions
            .get(id)
            .ok_or_else(|| PtyError::NotFound(id.to_string()))
    }

    pub fn get_mut(&mut self, id: &str) -> Result<&mut PtySession> {
        self.sessions
            .get_mut(id)
            .ok_or_else(|| PtyError::NotFound(id.to_string()))
    }

    pub fn remove(&mut self, id: &str) -> Result<PtySession> {
        self.sessions
            .remove(id)
            .ok_or_else(|| PtyError::NotFound(id.to_string()))
    }

    pub fn list(&self) -> Vec<SessionMetadata> {
        let mut out: Vec<_> = self.sessions.values().map(|s| s.metadata()).collect();
        out.sort_by(|a, b| {
            let na = a.id.strip_prefix("pty-").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let nb = b.id.strip_prefix("pty-").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            na.cmp(&nb)
        });
        out
    }

    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.sessions.len()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_ids_monotonic() {
        let mut m = SessionManager::new();
        assert_eq!(m.generate_id(), "pty-1");
        assert_eq!(m.generate_id(), "pty-2");
    }

    #[test]
    fn empty_list_and_get() {
        let m = SessionManager::new();
        assert_eq!(m.count(), 0);
        assert!(m.list().is_empty());
        assert!(m.get("nope").is_err());
    }
}
