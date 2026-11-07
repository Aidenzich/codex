//! Bounded, volatile one-shot wakeups. The session admission lock owns claiming.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::Notify;
use tokio::time::Instant;
use uuid::Uuid;

use super::Session;
use super::turn_context::TurnContext;
use codex_features::Feature;
use codex_protocol::protocol::{SessionSource, ThreadSource};

pub(crate) const MAX_WAKEUP_DELAY_MS: u64 = 43_200_000;
pub(crate) const MAX_WAKEUP_INSTRUCTION_BYTES: usize = 16 * 1024;
pub(crate) const MAX_PENDING_WAKEUPS: usize = 64;

/// The one exposure predicate, also enforced on direct tool invocation.
pub(crate) fn wakeup_tools_allowed(turn: &TurnContext) -> bool {
    // ManagedFeatures normalizes pinned values before can_set validates them.
    // Read back a private clone after enabling to distinguish RequiredOff.
    let mut candidate = turn.config.features.clone();
    let managed_allows = candidate
        .set_enabled(Feature::CurrentTimeReminder, true)
        .is_ok()
        && candidate.enabled(Feature::CurrentTimeReminder);
    wakeup_root_allowed(
        &turn.session_source,
        turn.turn_metadata_state.thread_source(),
        turn.root_capabilities_restricted,
    ) && managed_allows
}

fn wakeup_root_allowed(
    source: &SessionSource,
    thread_source: Option<&ThreadSource>,
    restricted: bool,
) -> bool {
    !restricted
        && matches!(source, SessionSource::Cli | SessionSource::VSCode)
        && matches!(thread_source, None | Some(ThreadSource::User))
}

#[derive(Clone, Debug)]
pub(crate) struct PendingWakeup {
    pub(crate) task_id: String,
    pub(crate) instruction: String,
    pub(crate) originating_turn_id: String,
    pub(crate) deadline: Instant,
    scheduled_at: DateTime<Utc>,
    due_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct WakeupReceipt {
    pub(crate) task_id: String,
    pub(crate) instruction: String,
    pub(crate) originating_turn_id: String,
    pub(crate) scheduled_at: DateTime<Utc>,
    pub(crate) due_at: DateTime<Utc>,
    pub(crate) status: &'static str,
}

impl PendingWakeup {
    fn receipt(&self, now: Instant) -> WakeupReceipt {
        WakeupReceipt {
            task_id: self.task_id.clone(),
            instruction: self.instruction.clone(),
            originating_turn_id: self.originating_turn_id.clone(),
            scheduled_at: self.scheduled_at,
            due_at: self.due_at,
            status: if self.deadline <= now {
                "due"
            } else {
                "scheduled"
            },
        }
    }
}

#[derive(Default)]
pub(crate) struct WakeupRegistry {
    pending: BTreeMap<String, PendingWakeup>,
    attached: bool,
    pub(crate) notify: Arc<Notify>,
}

impl WakeupRegistry {
    pub(crate) fn attached(&self) -> bool {
        self.attached
    }

    pub(crate) fn set_attached(&mut self, attached: bool) {
        self.attached = attached;
        if !attached {
            self.pending.clear();
        }
        self.notify.notify_one();
    }

    pub(crate) fn clear(&mut self) {
        self.pending.clear();
        self.notify.notify_one();
    }

    pub(crate) fn schedule(
        &mut self,
        duration_ms: u64,
        instruction: String,
        originating_turn_id: String,
    ) -> Result<WakeupReceipt, String> {
        if !(1..=MAX_WAKEUP_DELAY_MS).contains(&duration_ms) {
            return Err(format!(
                "duration_ms must be between 1 and {MAX_WAKEUP_DELAY_MS}"
            ));
        }
        if instruction.trim().is_empty() || instruction.len() > MAX_WAKEUP_INSTRUCTION_BYTES {
            return Err(format!(
                "instruction must be nonblank and at most {MAX_WAKEUP_INSTRUCTION_BYTES} UTF-8 bytes"
            ));
        }
        if !self.attached {
            return Err("Wakeups require an attached live session".into());
        }
        if self.pending.len() >= MAX_PENDING_WAKEUPS {
            return Err(format!(
                "At most {MAX_PENDING_WAKEUPS} wakeups may be pending; cancel one first"
            ));
        }
        let now = Instant::now();
        let scheduled_at = Utc::now();
        let entry = PendingWakeup {
            task_id: Uuid::new_v4().to_string(),
            instruction,
            originating_turn_id,
            deadline: now + Duration::from_millis(duration_ms),
            scheduled_at,
            due_at: scheduled_at + chrono::Duration::milliseconds(duration_ms as i64),
        };
        let receipt = entry.receipt(now);
        self.pending.insert(entry.task_id.clone(), entry);
        self.notify.notify_one();
        Ok(receipt)
    }

    pub(crate) fn list(&self) -> Vec<WakeupReceipt> {
        let now = Instant::now();
        self.pending
            .values()
            .map(|entry| entry.receipt(now))
            .collect()
    }

    pub(crate) fn cancel(&mut self, task_id: &str) -> bool {
        let removed = self.pending.remove(task_id).is_some();
        if removed {
            self.notify.notify_one();
        }
        removed
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if !self.attached {
            return None;
        }
        self.pending.values().map(|entry| entry.deadline).min()
    }

    /// Call only while holding active_turn then state, at final turn admission.
    pub(crate) fn claim_due(&mut self, now: Instant) -> Option<PendingWakeup> {
        if !self.attached {
            return None;
        }
        let id = self
            .pending
            .values()
            .filter(|entry| entry.deadline <= now)
            .min_by_key(|entry| (entry.deadline, entry.task_id.clone()))?
            .task_id
            .clone();
        self.pending.remove(&id)
    }
}

/// Every committed settings update replaces the retained StepSettings Arc, even
/// when only permissions or environments changed. Compare that existing identity
/// rather than keeping a second configuration generation counter.
pub(crate) fn wakeup_configuration_is_current(
    turn: &TurnContext,
    state: &crate::state::SessionState,
) -> bool {
    std::ptr::eq(
        turn.initial_settings.selected(),
        state.session_configuration.step_settings.as_ref(),
    )
}

impl Session {
    pub(crate) async fn schedule_wakeup(
        &self,
        duration_ms: u64,
        instruction: String,
        originating_turn_id: String,
    ) -> Result<WakeupReceipt, String> {
        let mut state = self.state.lock().await;
        if state.shutting_down {
            return Err("Session is shutting down".into());
        }
        state
            .wakeups
            .schedule(duration_ms, instruction, originating_turn_id)
    }

    pub(crate) async fn list_wakeups(&self) -> Vec<WakeupReceipt> {
        self.state.lock().await.wakeups.list()
    }
    pub(crate) async fn cancel_wakeup(&self, task_id: &str) -> bool {
        self.state.lock().await.wakeups.cancel(task_id)
    }
    pub(crate) async fn next_wakeup_deadline(&self) -> Option<Instant> {
        let state = self.state.lock().await;
        if state.shutting_down {
            None
        } else {
            state.wakeups.next_deadline()
        }
    }
    pub(crate) async fn wakeup_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.state.lock().await.wakeups.notify)
    }
    pub(crate) async fn set_wakeup_attached(&self, attached: bool) {
        // Serialize the host transition with final task registration. A detach
        // acknowledged after this lock wins cannot race an unstarted claim.
        let _active = self.active_turn.lock().await;
        let mut state = self.state.lock().await;
        let attached = attached && !state.shutting_down;
        state.wakeups.set_attached(attached);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_registry() -> WakeupRegistry {
        let mut registry = WakeupRegistry::default();
        registry.set_attached(true);
        registry
    }

    #[tokio::test]
    async fn wakeup_schedule_returns_without_waiting() {
        let (session, _) = crate::session::tests::make_session_and_context().await;
        session.set_wakeup_attached(true).await;
        let receipt = tokio::time::timeout(
            Duration::from_secs(1),
            session.schedule_wakeup(MAX_WAKEUP_DELAY_MS, "continue".into(), "origin".into()),
        )
        .await
        .unwrap()
        .unwrap();
        Uuid::parse_str(&receipt.task_id).unwrap();
        assert_eq!(receipt.status, "scheduled");
        assert_eq!(
            receipt.due_at - receipt.scheduled_at,
            chrono::Duration::milliseconds(MAX_WAKEUP_DELAY_MS as i64)
        );
        assert_eq!(session.list_wakeups().await, vec![receipt]);
    }

    #[tokio::test(start_paused = true)]
    async fn wakeup_list_cancel_pending_and_due() {
        let mut registry = live_registry();
        let first = registry
            .schedule(10, "first".into(), "turn-a".into())
            .unwrap();
        let second = registry
            .schedule(20, "second".into(), "turn-b".into())
            .unwrap();
        assert_ne!(first.task_id, second.task_id);
        assert_eq!(registry.list().len(), 2);
        assert!(registry.cancel(&first.task_id));
        tokio::time::advance(Duration::from_millis(20)).await;
        assert_eq!(registry.list()[0].status, "due");
        assert!(registry.cancel(&second.task_id));
        assert!(registry.claim_due(Instant::now()).is_none());
        assert!(!registry.cancel(&first.task_id));
        assert!(registry.list().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn wakeup_claim_is_one_shot_and_session_isolated() {
        let mut first = live_registry();
        let mut other = live_registry();
        let receipt = first
            .schedule(1, "only once".into(), "origin".into())
            .unwrap();
        assert!(!other.cancel(&receipt.task_id));
        assert!(first.claim_due(Instant::now()).is_none());
        tokio::time::advance(Duration::from_millis(1)).await;
        let claim = first.claim_due(Instant::now()).unwrap();
        assert_eq!(claim.task_id, receipt.task_id);
        assert_eq!(claim.instruction, "only once");
        assert_eq!(claim.originating_turn_id, "origin");
        assert!(first.claim_due(Instant::now()).is_none());
        assert!(!first.cancel(&receipt.task_id));
        assert!(first.list().is_empty());
    }

    #[test]
    fn wakeup_invalid_bounds_and_capacity_never_mutate() {
        let mut registry = live_registry();
        for (delay, instruction) in [
            (0, "x".into()),
            (MAX_WAKEUP_DELAY_MS + 1, "x".into()),
            (u64::MAX, "x".into()),
            (1, " \n".into()),
            (1, "字".repeat(MAX_WAKEUP_INSTRUCTION_BYTES / 3 + 1)),
        ] {
            assert!(
                registry
                    .schedule(delay, instruction, "origin".into())
                    .is_err()
            );
            assert!(registry.list().is_empty());
        }
        for _ in 0..MAX_PENDING_WAKEUPS {
            registry.schedule(1, "x".into(), "origin".into()).unwrap();
        }
        assert!(
            registry
                .schedule(1, "overflow".into(), "origin".into())
                .is_err()
        );
        assert_eq!(registry.list().len(), MAX_PENDING_WAKEUPS);
        registry.set_attached(false);
        assert!(registry.list().is_empty());
        assert!(registry.next_deadline().is_none());
        assert!(
            registry
                .schedule(1, "detached".into(), "origin".into())
                .is_err()
        );
        registry.set_attached(true);
        assert!(registry.list().is_empty());
    }

    #[tokio::test]
    async fn wakeup_shutdown_rejects_registration() {
        let (session, _) = crate::session::tests::make_session_and_context().await;
        session.set_wakeup_attached(true).await;
        session.state.lock().await.shutting_down = true;
        assert!(
            session
                .schedule_wakeup(1, "x".into(), "origin".into())
                .await
                .is_err()
        );
        session.set_wakeup_attached(true).await;
        assert!(!session.state.lock().await.wakeups.attached());
        assert!(session.list_wakeups().await.is_empty());
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[tokio::test]
    async fn wakeup_managed_required_off_overrides_only_wakeup_eligibility() {
        let (_, mut turn) = crate::session::tests::make_session_and_context().await;
        turn.session_source = SessionSource::Cli;
        let config = Arc::make_mut(&mut turn.config);
        config
            .features
            .disable(Feature::CurrentTimeReminder)
            .unwrap();
        assert!(wakeup_tools_allowed(&turn));
        let config = Arc::make_mut(&mut turn.config);
        config.features = crate::config::ManagedFeatures::from_configured(
            config.features.get().clone(),
            Some(codex_config::Sourced::new(
                codex_config::FeatureRequirementsToml {
                    entries: BTreeMap::from([("current_time_reminder".into(), false)]),
                    runtime_auth_mode: None,
                },
                codex_config::RequirementSource::Unknown,
            )),
        )
        .unwrap();
        assert!(!wakeup_tools_allowed(&turn));
    }

    #[test]
    fn wakeup_root_identity_matrix() {
        for source in [SessionSource::Cli, SessionSource::VSCode] {
            assert!(wakeup_root_allowed(&source, None, false));
            assert!(wakeup_root_allowed(
                &source,
                Some(&ThreadSource::User),
                false
            ));
            assert!(!wakeup_root_allowed(
                &source,
                Some(&ThreadSource::User),
                true
            ));
            for thread_source in [
                ThreadSource::Subagent,
                ThreadSource::GuardianReview,
                ThreadSource::MemoryConsolidation,
                ThreadSource::Feature("review".into()),
            ] {
                assert!(!wakeup_root_allowed(&source, Some(&thread_source), false));
            }
        }
        for source in [
            SessionSource::Exec,
            SessionSource::Mcp,
            SessionSource::Unknown,
            SessionSource::Custom("custom".into()),
            SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::Review),
        ] {
            assert!(!wakeup_root_allowed(&source, None, false));
        }
    }
}

#[cfg(test)]
mod concurrency_tests {
    use super::*;
    use tokio::sync::{Barrier, Mutex};

    #[tokio::test]
    async fn wakeup_cancel_and_claim_have_one_winner() {
        let mut registry = WakeupRegistry::default();
        registry.set_attached(true);
        let receipt = registry
            .schedule(1, "race".into(), "origin".into())
            .unwrap();
        let deadline = registry.next_deadline().unwrap();
        let registry = Arc::new(Mutex::new(registry));
        let barrier = Arc::new(Barrier::new(2));
        let cancel = {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                registry.lock().await.cancel(&receipt.task_id)
            })
        };
        let claim = {
            let registry = Arc::clone(&registry);
            tokio::spawn(async move {
                barrier.wait().await;
                registry.lock().await.claim_due(deadline).is_some()
            })
        };
        let cancelled = cancel.await.unwrap();
        let claimed = claim.await.unwrap();
        assert_ne!(cancelled, claimed);
        assert!(registry.lock().await.list().is_empty());
    }
}
