// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use crossterm::event::{KeyCode, KeyEvent};

use super::TuiRenderer;
use super::modal_routing::KeyOutcome;
use crate::app::View;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonKeyAction {
    Refresh,
    Attach,
    BeginTell,
    Stop,
    ConfirmKill,
    Wait,
    Inspect,
    Logs,
    Gc,
}

impl TuiRenderer {
    pub(super) fn handle_daemon_key(&mut self, key: KeyEvent) -> Option<KeyOutcome> {
        match daemon_key_action(self.app.view, key.code) {
            Some(DaemonKeyAction::Refresh) => {
                self.refresh_daemon_snapshot();
                self.app.status = "daemon jobs refreshed".into();
                Some(KeyOutcome::None)
            }
            Some(DaemonKeyAction::Attach) => Some(self.daemon_command_outcome(
                self.app.selected_attach_command(&self.daemon_jobs),
                "selected daemon row is not attachable",
                false,
            )),
            Some(DaemonKeyAction::BeginTell) => {
                if self
                    .app
                    .selected_tell_command(&self.daemon_jobs, "message")
                    .is_some()
                {
                    self.app.begin_tell();
                } else {
                    self.app.status = "tell requires a daemon stage selection".into();
                }
                Some(KeyOutcome::None)
            }
            Some(DaemonKeyAction::Stop) => Some(self.daemon_command_outcome(
                self.app.selected_stop_command(&self.daemon_jobs),
                "stop requires a daemon job selection",
                true,
            )),
            Some(DaemonKeyAction::ConfirmKill) => {
                if self.app.selected_kill_command(&self.daemon_jobs).is_some() {
                    self.app.begin_kill_confirm();
                } else {
                    self.app.status = "no daemon job or stage selected".into();
                }
                Some(KeyOutcome::None)
            }
            Some(DaemonKeyAction::Wait) => Some(self.daemon_command_outcome(
                self.app.selected_wait_command(&self.daemon_jobs),
                "wait requires a daemon job selection",
                true,
            )),
            Some(DaemonKeyAction::Inspect) => {
                Some(self.app.selected_inspect_command(&self.daemon_jobs).map_or(
                    KeyOutcome::None,
                    |cmd| {
                        self.load_daemon_panel(&cmd, "inspect");
                        KeyOutcome::None
                    },
                ))
            }
            Some(DaemonKeyAction::Logs) => Some(
                self.app
                    .selected_logs_command(&self.daemon_jobs)
                    .map_or(KeyOutcome::None, |cmd| {
                        self.load_daemon_panel(&cmd, "logs");
                        KeyOutcome::None
                    }),
            ),
            Some(DaemonKeyAction::Gc) => {
                Some(self.submit_daemon_command("ps --gc --json".into(), true))
            }
            None => None,
        }
    }

    fn daemon_command_outcome(
        &mut self,
        cmd: Option<String>,
        missing: &str,
        refresh_after: bool,
    ) -> KeyOutcome {
        match cmd {
            Some(cmd) => self.submit_daemon_command(cmd, refresh_after),
            None => {
                self.app.status = missing.into();
                KeyOutcome::None
            }
        }
    }
}

fn daemon_key_action(view: View, code: KeyCode) -> Option<DaemonKeyAction> {
    if view != View::Jobs {
        return None;
    }
    match code {
        KeyCode::Char('r') => Some(DaemonKeyAction::Refresh),
        KeyCode::Char('a') => Some(DaemonKeyAction::Attach),
        KeyCode::Char('t') => Some(DaemonKeyAction::BeginTell),
        KeyCode::Char('s') => Some(DaemonKeyAction::Stop),
        KeyCode::Char('K') => Some(DaemonKeyAction::ConfirmKill),
        KeyCode::Char('w') => Some(DaemonKeyAction::Wait),
        KeyCode::Char('i') => Some(DaemonKeyAction::Inspect),
        KeyCode::Char('l') => Some(DaemonKeyAction::Logs),
        KeyCode::Char('g') => Some(DaemonKeyAction::Gc),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{DaemonKeyAction, daemon_key_action};
    use crate::app::View;
    use crossterm::event::KeyCode;

    #[test]
    fn daemon_keys_only_apply_to_jobs_view() {
        assert_eq!(
            daemon_key_action(View::Jobs, KeyCode::Char('s')),
            Some(DaemonKeyAction::Stop)
        );
        assert_eq!(daemon_key_action(View::Agents, KeyCode::Char('s')), None);
    }

    #[test]
    fn daemon_key_mapping_covers_public_commands() {
        let cases = [
            ('a', DaemonKeyAction::Attach),
            ('t', DaemonKeyAction::BeginTell),
            ('s', DaemonKeyAction::Stop),
            ('K', DaemonKeyAction::ConfirmKill),
            ('w', DaemonKeyAction::Wait),
            ('i', DaemonKeyAction::Inspect),
            ('l', DaemonKeyAction::Logs),
            ('g', DaemonKeyAction::Gc),
            ('r', DaemonKeyAction::Refresh),
        ];
        for (key, action) in cases {
            assert_eq!(
                daemon_key_action(View::Jobs, KeyCode::Char(key)),
                Some(action)
            );
        }
    }
}
