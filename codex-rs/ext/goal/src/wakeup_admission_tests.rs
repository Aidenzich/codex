//! Real goal accounting must belong to admitted tasks, never timer reservations.
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use codex_analytics::AnalyticsEventsClient;
use codex_core::TurnInputRequest;
use codex_extension_api::{ExtensionRegistryBuilder, TurnLifecycleContributor};
use codex_protocol::protocol::{EventMsg, SessionSource};
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::test_support::PathExt;
use core_test_support::{responses, skip_if_no_network, wait_for_event_with_timeout};
use tokio::sync::{Notify, oneshot};

use crate::{GoalExtensionConfig, GoalService, install_with_backend};

#[derive(Default)]
struct Barrier {
    start: Mutex<Option<oneshot::Receiver<()>>>,
    stop: Mutex<Option<oneshot::Receiver<()>>>,
    entered_start: Notify,
    entered_stop: Notify,
    exited_start: Notify,
}
impl TurnLifecycleContributor for Barrier {
    fn on_turn_start<'a>(
        &'a self,
        _: codex_extension_api::TurnStartInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let gate = self.start.lock().unwrap().take();
            if let Some(gate) = gate {
                self.entered_start.notify_one();
                let _ = gate.await;
                self.exited_start.notify_one();
            }
        })
    }
    fn on_turn_stop<'a>(
        &'a self,
        _: codex_extension_api::TurnStopInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let gate = self.stop.lock().unwrap().take();
            if let Some(gate) = gate {
                self.entered_stop.notify_one();
                let _ = gate.await;
            }
        })
    }
}
fn input() -> TurnInputRequest {
    TurnInputRequest::user_input(vec![UserInput::Text {
        text: "Schedule a reminder, then finish.".into(),
        text_elements: vec![],
    }])
}
fn answer() -> String {
    responses::sse(vec![
        responses::ev_assistant_message("answer", "Done."),
        responses::ev_completed("done"),
    ])
}
fn schedule() -> String {
    responses::sse(vec![
        responses::ev_function_call_with_namespace(
            "schedule",
            "clock",
            "schedule_wakeup",
            r#"{"duration_ms":1,"instruction":"Report done"}"#,
        ),
        responses::ev_completed("scheduled"),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wakeup_abandoned_admission_does_not_retain_real_goal_accounting() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    // Two detach/reattach cycles, then a real user superseding a reservation.
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            schedule(),
            answer(),
            schedule(),
            answer(),
            schedule(),
            answer(),
            answer(),
            answer(),
        ],
    )
    .await;
    let db_dir = tempfile::TempDir::new()?;
    let db = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(db_dir.path().abs()),
        "test-provider".into(),
    )
    .await?;
    let service = Arc::new(GoalService::new());
    let barrier = Arc::new(Barrier::default());
    let mut extensions = ExtensionRegistryBuilder::new();
    // Install the actual production contributor before the delayed preparation
    // contributor: order previously started accounting on abandoned reservations.
    install_with_backend(
        &mut extensions,
        db,
        AnalyticsEventsClient::disabled(),
        None,
        Weak::new(),
        service.clone(),
        |_| GoalExtensionConfig {
            enabled: true,
            max_goal_token_budget: None,
        },
    );
    extensions.turn_lifecycle_contributor(barrier.clone());
    let test = core_test_support::test_codex::test_codex()
        .with_session_source(SessionSource::Cli)
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    let runtime = test
        .codex
        .thread_extension_data()
        .get::<crate::GoalRuntimeHandle>()
        .expect("real goal runtime");
    let accounting = runtime.accounting_state();
    for cycle in 0..3 {
        let (release_stop, stop) = oneshot::channel();
        *barrier.stop.lock().unwrap() = Some(stop);
        test.codex.start_or_steer_turn(input()).await?;
        tokio::time::timeout(Duration::from_secs(10), barrier.entered_stop.notified()).await?;
        let (release_start, start) = oneshot::channel();
        *barrier.start.lock().unwrap() = Some(start);
        release_stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), barrier.entered_start.notified()).await?;
        // Drain the original completion before waiting on any replacement.
        wait_for_event_with_timeout(
            &test.codex,
            |event| matches!(event, EventMsg::TurnComplete(_)),
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(
            accounting.retained_turn_count(),
            0,
            "reservation {cycle} started goal accounting"
        );
        assert_eq!(mock.requests().len(), (cycle + 1) * 2);
        if cycle < 2 {
            test.codex.set_wakeup_host_attached(false).await;
            release_start.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(10), barrier.exited_start.notified()).await?;
            assert_eq!(mock.requests().len(), (cycle + 1) * 2);
            test.codex.set_wakeup_host_attached(true).await;
        } else {
            let submission = test.codex.start_or_steer_turn(input()).await?;
            let codex_core::TurnInputSubmission::Started { turn_id, .. } = submission else {
                panic!("user must replace the reservation")
            };
            release_start.send(()).unwrap();
            wait_for_event_with_timeout(
                &test.codex,
                |event| matches!(event, EventMsg::TurnComplete(done) if done.turn_id == turn_id),
                Duration::from_secs(10),
            )
            .await;
            // Supersession leaves the unclaimed timer eligible. It must resume
            // exactly once after the replacement user completes.
            wait_for_event_with_timeout(
                &test.codex,
                |event| matches!(event, EventMsg::TurnComplete(done) if done.turn_id != turn_id),
                Duration::from_secs(10),
            )
            .await;
        }
        assert_eq!(accounting.retained_turn_count(), 0);
    }
    test.codex.shutdown_and_wait().await?;
    assert_eq!(accounting.retained_turn_count(), 0);
    assert_eq!(mock.requests().len(), 8);
    Ok(())
}
