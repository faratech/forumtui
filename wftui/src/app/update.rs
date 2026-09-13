//! Self-update wiring (#722): the background check, the manual `g u` /
//! palette trigger, and what the header chip says. The work itself —
//! feed, verify, stage, swap — is `common::update`; this file only decides
//! *when* to run it and *how* to tell the reader.
//!
//! The check is not session work: it must survive a sign-out and run on the
//! sign-in screen, where a stale build most often sits. So its task is a
//! plain `tokio::spawn` with its own generation stamp (the login flow's
//! pattern), never wrapped in `session_msg`.

use std::time::Duration;

use common::update::{self, Outcome};

use super::{App, Msg};
use crate::overlay;

/// What the reader has been told about updates this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Failed(String),
    /// Downloaded and verified; the next start applies it.
    Staged { version: String },
    /// Newer, but this binary is not replaceable in place (MSIX); the
    /// release page is the way.
    Available { version: String, html_url: String },
    /// Staged, but the install dir is not this user's: `command` is the
    /// install-then-rename line to run by hand.
    Manual { version: String, command: String },
    /// This start swapped the binary. The process is still the old image,
    /// so it neither re-checks nor pretends to be current.
    JustApplied { version: String },
}

impl App {
    /// The once-per-start background check, a few seconds after the first
    /// paint so it never competes with the bootstrap fetches. A session that
    /// just applied an update skips it: the running image is the old one and
    /// would only re-download what is already installed.
    pub(super) fn schedule_startup_update_check(&mut self) {
        if self.update_cfg.disabled || matches!(self.update, UpdateState::JustApplied { .. }) {
            return;
        }
        self.spawn_update_check(false, update::STARTUP_DELAY);
    }

    /// `g u` and the palette row. Acts on what is already known before it
    /// asks again: a staged or applied update needs a restart, not a check;
    /// an MSIX install opens the release page.
    pub(super) fn check_for_updates_now(&mut self) {
        if self.update_cfg.disabled {
            self.set_status("Updates are disabled (WFTUI_NO_UPDATE).");
            return;
        }
        match self.update.clone() {
            UpdateState::JustApplied { version } => {
                self.set_hint(format!("Update v{version} applied — restart wftui to run it."));
            }
            UpdateState::Staged { version } => {
                self.set_hint(format!("Update v{version} downloaded — restart wftui to apply it."));
            }
            UpdateState::Manual { version, command } => {
                self.set_hint(format!("Update v{version} downloaded; install it with: {command}"));
            }
            UpdateState::Available { html_url, .. } => self.open_url(&html_url),
            UpdateState::Checking => self.set_status("Already checking for updates…"),
            _ => {
                self.set_status("Checking for updates…");
                self.spawn_update_check(true, Duration::ZERO);
            }
        }
    }

    fn spawn_update_check(&mut self, forced: bool, delay: Duration) {
        if let Some(task) = self.update_task.take() {
            task.abort();
        }
        self.update_generation += 1;
        let generation = self.update_generation;
        self.update = UpdateState::Checking;
        let cfg = self.update_cfg.clone();
        let tx = self.tx.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let result = update::check_and_stage(&cfg, forced)
                .await
                .map_err(|e| e.to_string());
            tx.send(Msg::UpdateChecked { generation, forced, result }).ok();
        });
        self.update_task = Some(task.abort_handle());
    }

    /// The check's verdict. A stale generation (a manual check superseded
    /// the one that answered) is dropped. The automatic check is quiet
    /// unless there is something to act on; a manual one always answers.
    pub(super) fn handle_update_checked(
        &mut self,
        generation: u64,
        forced: bool,
        result: Result<Outcome, String>,
    ) {
        if generation != self.update_generation {
            return;
        }
        self.update_task = None;
        match result {
            Ok(Outcome::Skipped(why)) => {
                self.update = UpdateState::Idle;
                if forced {
                    self.set_status(format!("Updates unavailable: {why}."));
                }
            }
            Ok(Outcome::Throttled) => self.update = UpdateState::Idle,
            Ok(Outcome::UpToDate { .. }) => {
                self.update = UpdateState::UpToDate;
                if forced {
                    self.set_status(format!("wftui v{} is up to date.", self.update_cfg.current_version));
                }
            }
            Ok(Outcome::Staged { version, .. }) => {
                self.set_hint(format!("Update v{version} downloaded — restart wftui to apply it."));
                self.update = UpdateState::Staged { version };
            }
            Ok(Outcome::Available { version, html_url, why }) => {
                self.set_hint(format!("wftui v{version} is available ({why}) — g u opens the release page."));
                self.update = UpdateState::Available { version, html_url };
            }
            Ok(Outcome::ManualInstall { version, command, .. }) => {
                self.set_hint(format!("Update v{version} downloaded; install it with: {command}"));
                self.update = UpdateState::Manual { version, command };
            }
            Err(e) => {
                if forced {
                    self.set_status(format!("Update check failed: {e}"));
                } else {
                    tracing::warn!("update check failed: {e}");
                }
                self.update = UpdateState::Failed(e);
            }
        }
    }

    /// The header chip, present only while something waits on the reader.
    pub(super) fn update_chip(&self) -> Option<String> {
        match &self.update {
            UpdateState::Staged { version } => Some(format!("update v{version} ready")),
            UpdateState::JustApplied { version } => Some(format!("v{version} applied, restart")),
            UpdateState::Available { version, .. } => Some(format!("update v{version}")),
            UpdateState::Manual { version, .. } => Some(format!("update v{version} (manual)")),
            _ => None,
        }
    }

    /// The palette's row, worded for the state so the row itself says what
    /// pressing it will do.
    pub(super) fn update_palette_item(&self) -> overlay::Item {
        let title = match &self.update {
            UpdateState::Staged { version } | UpdateState::JustApplied { version } => {
                format!("Update v{version} ready, restart to apply")
            }
            UpdateState::Available { version, .. } => format!("Update v{version}, open release page"),
            UpdateState::Manual { version, .. } => format!("Update v{version}, install by hand"),
            _ => "Check for updates".to_string(),
        };
        overlay::Item::action(title, "gu", overlay::Target::Update)
    }
}
