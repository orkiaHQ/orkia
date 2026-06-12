use super::*;
use orkia_shell_types::EventType;

impl State {
    pub(super) fn emit_entry_event(&self, event: &str, entry: &Entry) {
        let Some(tx) = self.journal_tx.as_ref() else {
            return;
        };
        let mut env = JournalEnvelope::now(EventType::Shell);
        env.event = Some(event.into());
        env.source = Some("orkia".into());
        env.job_id = entry.job_id.map(|id| id.0);
        env.agent = Some(entry.agent.clone());
        env.action = Some(entry.id.to_string());
        env.description = Some(truncate(&entry.summary, 200));
        env.target = entry.resource.as_ref().map(|p| p.display().to_string());
        let _ = tx.send(env);
    }

    pub(super) fn emit_row_event(&self, event: &str, row: &AttentionRow) {
        let Some(tx) = self.journal_tx.as_ref() else {
            return;
        };
        let mut env = JournalEnvelope::now(EventType::Shell);
        env.event = Some(event.into());
        env.source = Some("orkia".into());
        env.job_id = row.job_id;
        env.agent = Some(row.agent.clone());
        env.action = Some(row.id.to_string());
        env.description = Some(truncate(&row.summary, 200));
        let _ = tx.send(env);
    }
}
