//! Native wakeups use ordinary turn ownership and retain typed harness attribution.
#[path = "wakeup_admission.rs"]
mod admission;

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::{EventMsg, SessionSource};
use codex_protocol::user_input::UserInput;
use core_test_support::test_codex::test_codex;
use core_test_support::{responses, skip_if_no_network, wait_for_event_with_timeout};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, oneshot};

async fn wait_for_event<F>(codex: &codex_core::CodexThread, predicate: F) -> EventMsg
where
    F: FnMut(&EventMsg) -> bool,
{
    wait_for_event_with_timeout(codex, predicate, Duration::from_secs(10)).await
}

#[derive(Default)]
struct StopBarrier {
    gate: Mutex<Option<oneshot::Receiver<()>>>,
    entered: Notify,
}
impl codex_extension_api::TurnLifecycleContributor for StopBarrier {
    fn on_turn_stop<'a>(
        &'a self,
        _: codex_extension_api::TurnStopInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let gate = self.gate.lock().unwrap().take();
            if let Some(gate) = gate {
                self.entered.notify_one();
                gate.await.unwrap();
            }
        })
    }
}

fn schedule(delay: u64) -> String {
    responses::sse(vec![
        responses::ev_function_call_with_namespace(
            "schedule-1",
            "clock",
            "schedule_wakeup",
            &json!({"duration_ms":delay,"instruction":"Report <done> & 保留上下文"}).to_string(),
        ),
        responses::ev_completed("scheduled"),
    ])
}
fn answer(id: &str, text: &str) -> String {
    responses::sse(vec![
        responses::ev_assistant_message(id, text),
        responses::ev_completed(id),
    ])
}
fn input() -> TurnInputRequest {
    TurnInputRequest::user_input(vec![UserInput::Text {
        text: "Remember the context token ORCHID. Schedule the follow-up, then finish.".into(),
        text_elements: vec![],
    }])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wakeup_idle_deadline_resumes_same_thread_with_history_and_typed_marker() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            schedule(150),
            answer("initial", "Scheduled."),
            answer("followup", "done"),
        ],
    )
    .await;
    let test = test_codex()
        .with_session_source(SessionSource::Cli)
        .build_with_auto_env(&server)
        .await?;
    test.codex.start_or_steer_turn(input()).await?;
    let first = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(first) = first else {
        unreachable!()
    };
    let marker = wait_for_event(&test.codex, |event| matches!(event, EventMsg::ItemCompleted(event) if matches!(event.item, TurnItem::WakeupStarted(_)))).await;
    let EventMsg::ItemCompleted(marker) = marker else {
        unreachable!()
    };
    let TurnItem::WakeupStarted(marker) = marker.item else {
        unreachable!()
    };
    assert_eq!(marker.origin_turn_id, first.turn_id);
    assert_ne!(marker.resumed_turn_id, first.turn_id);
    assert!(!marker.task_id.is_empty());
    let second = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(second) = second else {
        unreachable!()
    };
    assert_eq!(second.turn_id, marker.resumed_turn_id);
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].body_contains_text("ORCHID"));
    assert!(requests[2].body_contains_text(&marker.task_id));
    assert!(requests[2].body_contains_text("Report <done> & 保留上下文"));
    let rollout_path = test.codex.rollout_path().expect("native rollout");
    test.codex.shutdown_and_wait().await?;
    let replay = std::fs::read_to_string(rollout_path)?
        .lines()
        .map(codex_rollout::parse_rollout_line)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|line| match line.item {
            codex_history::RolloutItem::ResponseItem(item) => {
                codex_protocol::items::parse_wakeup_started_message(&item.item)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replay, vec![marker]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wakeup_due_while_finishing_waits_for_safe_boundary_once() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            schedule(30),
            answer("initial", "Scheduled."),
            answer("followup", "done"),
        ],
    )
    .await;
    let barriers = Arc::new(StopBarrier::default());
    let (release, gate) = oneshot::channel();
    *barriers.gate.lock().unwrap() = Some(gate);
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.turn_lifecycle_contributor(barriers.clone());
    let test = test_codex()
        .with_session_source(SessionSource::Cli)
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    test.codex.start_or_steer_turn(input()).await?;
    tokio::time::timeout(Duration::from_secs(10), barriers.entered.notified()).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(mock.requests().len(), 2);
    let mut current_mode = test.codex.config_snapshot().await.collaboration_mode;
    current_mode.mode = codex_protocol::config_types::ModeKind::Plan;
    test.codex
        .update_thread_settings(codex_protocol::protocol::ThreadSettingsOverrides {
            collaboration_mode: Some(current_mode),
            ..Default::default()
        })
        .await?;
    release.send(()).unwrap();
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(mock.requests().len(), 3);
    assert_eq!(
        test.codex.config_snapshot().await.collaboration_mode.mode,
        codex_protocol::config_types::ModeKind::Plan
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wakeup_host_detach_clears_due_jobs_before_current_turn_finishes() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock =
        responses::mount_sse_sequence(&server, vec![schedule(30), answer("initial", "Scheduled.")])
            .await;
    let barriers = Arc::new(StopBarrier::default());
    let (release, gate) = oneshot::channel();
    *barriers.gate.lock().unwrap() = Some(gate);
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.turn_lifecycle_contributor(barriers.clone());
    let test = test_codex()
        .with_session_source(SessionSource::Cli)
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    test.codex.start_or_steer_turn(input()).await?;
    tokio::time::timeout(Duration::from_secs(10), barriers.entered.notified()).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    test.codex.set_wakeup_host_attached(false).await;
    release.send(()).unwrap();
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.codex.set_wakeup_host_attached(true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(mock.requests().len(), 2);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[test_case::test_case(400; "before_deadline")]
#[test_case::test_case(1; "due_but_unclaimed_during_current_turn")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wakeup_cancel_pending_prevents_any_followup_request(delay: u64) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = request_count.clone();
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let body = match n {
                0 => schedule(delay),
                1 => responses::sse(vec![
                    responses::ev_function_call_with_namespace(
                        "list-1",
                        "clock",
                        "list_wakeups",
                        "{}",
                    ),
                    responses::ev_completed("listed"),
                ]),
                2 => {
                    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                    let output = body["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|item| {
                            item["call_id"] == "schedule-1"
                                && item["type"] == "function_call_output"
                        })
                        .unwrap();
                    let receipt: serde_json::Value =
                        serde_json::from_str(output["output"].as_str().unwrap()).unwrap();
                    responses::sse(vec![
                        responses::ev_function_call_with_namespace(
                            "cancel-1",
                            "clock",
                            "cancel_wakeup",
                            &json!({"task_id":receipt["task_id"]}).to_string(),
                        ),
                        responses::ev_completed("cancelled"),
                    ])
                }
                _ => answer("final", "Cancelled."),
            };
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_delay(if n == 1 {
                    Duration::from_millis(5)
                } else {
                    Duration::ZERO
                })
                .set_body_string(body)
        })
        .mount(&server)
        .await;
    let test = test_codex()
        .with_session_source(SessionSource::Cli)
        .build_with_auto_env(&server)
        .await?;
    test.codex.start_or_steer_turn(input()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(request_count.load(std::sync::atomic::Ordering::SeqCst), 4);
    let requests = server.received_requests().await.unwrap();
    let final_body: serde_json::Value = serde_json::from_slice(&requests.last().unwrap().body)?;
    let cancel_output = final_body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["call_id"] == "cancel-1" && item["type"] == "function_call_output")
        .unwrap();
    let receipt: serde_json::Value =
        serde_json::from_str(cancel_output["output"].as_str().unwrap())?;
    assert_eq!(receipt["status"], "cancelled");
    let listed = final_body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["call_id"] == "list-1" && item["type"] == "function_call_output")
        .unwrap();
    let listed: serde_json::Value = serde_json::from_str(listed["output"].as_str().unwrap())?;
    assert_eq!(listed["wakeups"][0]["task_id"], receipt["task_id"]);
    if delay == 1 {
        assert_eq!(listed["wakeups"][0]["status"], "due");
    }

    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wakeup_shutdown_while_pending_prevents_any_followup_request() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![schedule(400), answer("initial", "Scheduled.")],
    )
    .await;
    let test = test_codex()
        .with_session_source(SessionSource::Cli)
        .build_with_auto_env(&server)
        .await?;
    test.codex.start_or_steer_turn(input()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.codex.shutdown_and_wait().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(mock.requests().len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wakeup_expiring_during_unrelated_user_turn_queues_until_it_completes() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            schedule(1500),
            answer("initial", "Scheduled."),
            answer("unrelated", "User work complete."),
            answer("followup", "done"),
        ],
    )
    .await;
    let barriers = Arc::new(StopBarrier::default());
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.turn_lifecycle_contributor(barriers.clone());
    let test = test_codex()
        .with_session_source(SessionSource::Cli)
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    test.codex.start_or_steer_turn(input()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let (release, gate) = oneshot::channel();
    *barriers.gate.lock().unwrap() = Some(gate);
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Finish unrelated work first.".into(),
            text_elements: vec![],
        }]))
        .await?;
    tokio::time::timeout(Duration::from_secs(10), barriers.entered.notified()).await?;
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert_eq!(mock.requests().len(), 3);
    release.send(()).unwrap();
    let user_done = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let reminder = wait_for_event(&test.codex, |event| matches!(event, EventMsg::ItemCompleted(event) if matches!(event.item, TurnItem::WakeupStarted(_)))).await;
    let EventMsg::TurnComplete(user_done) = user_done else {
        unreachable!()
    };
    let EventMsg::ItemCompleted(reminder) = reminder else {
        unreachable!()
    };
    let TurnItem::WakeupStarted(reminder) = reminder.item else {
        unreachable!()
    };
    assert_ne!(reminder.resumed_turn_id, user_done.turn_id);
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(mock.requests().len(), 4);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[derive(Default)]
struct PreparationBarrier {
    gate: Mutex<Option<oneshot::Receiver<()>>>,
    entered: Notify,
}
impl codex_extension_api::TurnLifecycleContributor for PreparationBarrier {
    fn on_turn_start<'a>(
        &'a self,
        _: codex_extension_api::TurnStartInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let gate = self.gate.lock().unwrap().take();
            if let Some(gate) = gate {
                self.entered.notify_one();
                gate.await.unwrap();
            }
        })
    }
}

#[test_case::test_case(false; "detach_during_reserved_preparation")]
#[test_case::test_case(true; "shutdown_during_reserved_preparation")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wakeup_teardown_during_preparation_cannot_recreate_reserved_turn(
    shutdown: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock =
        responses::mount_sse_sequence(&server, vec![schedule(1), answer("initial", "Scheduled.")])
            .await;
    let stop = Arc::new(StopBarrier::default());
    let prep = Arc::new(PreparationBarrier::default());
    let (release_stop, stop_gate) = oneshot::channel();
    *stop.gate.lock().unwrap() = Some(stop_gate);
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.turn_lifecycle_contributor(stop.clone());
    extensions.turn_lifecycle_contributor(prep.clone());
    let test = test_codex()
        .with_session_source(SessionSource::Cli)
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    test.codex.start_or_steer_turn(input()).await?;
    tokio::time::timeout(Duration::from_secs(10), stop.entered.notified()).await?;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let (release_prep, prep_gate) = oneshot::channel();
    *prep.gate.lock().unwrap() = Some(prep_gate);
    release_stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), prep.entered.notified()).await?;
    if shutdown {
        test.codex.shutdown_and_wait().await?;
    } else {
        test.codex.set_wakeup_host_attached(false).await;
    }
    if shutdown {
        // Shutdown drops the in-loop preparation future and its suspended hook.
        let _ = release_prep.send(());
    } else {
        release_prep.send(()).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(mock.requests().len(), 2);
    if !shutdown {
        test.codex.shutdown_and_wait().await?;
    }
    Ok(())
}

#[test_case::test_case(false; "settings_changed_before_expiry")]
#[test_case::test_case(true; "settings_changed_during_reserved_preparation")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wakeup_followup_obeys_current_write_permissions(during_preparation: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            schedule(1),
            answer("initial", "Scheduled."),
            responses::sse(vec![
                responses::ev_apply_patch_custom_tool_call(
                    "forbidden-write",
                    "*** Begin Patch\n*** Add File: forbidden.txt\n+must-not-exist\n*** End Patch",
                ),
                responses::ev_completed("forbidden-attempt"),
            ]),
            answer("done", "The current permissions deny the write."),
        ],
    )
    .await;
    let stop = Arc::new(StopBarrier::default());
    let prep = Arc::new(PreparationBarrier::default());
    let (release_stop, stop_gate) = oneshot::channel();
    *stop.gate.lock().unwrap() = Some(stop_gate);
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.turn_lifecycle_contributor(stop.clone());
    extensions.turn_lifecycle_contributor(prep.clone());
    let test = test_codex()
        .with_session_source(SessionSource::Cli)
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    test.codex.start_or_steer_turn(input()).await?;
    tokio::time::timeout(Duration::from_secs(10), stop.entered.notified()).await?;
    let mut release_stop = Some(release_stop);
    let mut release_prep = None;
    if during_preparation {
        let (release, gate) = oneshot::channel();
        *prep.gate.lock().unwrap() = Some(gate);
        release_prep = Some(release);
        release_stop.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), prep.entered.notified()).await?;
    }
    test.codex
        .update_thread_settings(codex_protocol::protocol::ThreadSettingsOverrides {
            permission_profile: Some(codex_protocol::models::PermissionProfile::read_only()),
            active_permission_profile: Some(
                codex_protocol::models::ActivePermissionProfile::read_only(),
            ),
            approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
            ..Default::default()
        })
        .await?;
    if let Some(release) = release_stop {
        release.send(()).unwrap();
    }
    if let Some(release) = release_prep {
        release.send(()).unwrap();
    }
    for _ in 0..2 {
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }
    assert_eq!(mock.requests().len(), 4);
    let request = mock.last_request().unwrap();
    let (output, _) = request
        .custom_tool_call_output_content_and_success("forbidden-write")
        .expect("actual rejected native patch output");
    let output = output.expect("actual native patch denial text");
    assert!(
        !test.workspace_path("forbidden.txt").exists(),
        "read-only follow-up wrote forbidden.txt; native output: {output}"
    );
    assert!(
        output.contains(
            "patch rejected: writing is blocked by read-only sandbox; rejected by user approval settings"
        ),
        "expected read-only patch denial, got: {output}"
    );
    eprintln!(
        "read-only wakeup (during_preparation={during_preparation}): {output}; forbidden.txt absent"
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

// A consumed timer must never become a retry queue when the ordinary provider turn fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wakeup_terminal_provider_failure_is_visible_once_and_user_can_recover() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            schedule(150),
            answer("initial", "Scheduled."),
            responses::sse_failed(
                "failed-wakeup",
                "insufficient_quota",
                "You exceeded your current quota, please check your plan and billing details.",
            ),
            answer("recovery", "Recovered after the failed wakeup."),
        ],
    )
    .await;
    let test = test_codex()
        .with_session_source(SessionSource::Cli)
        .build_with_auto_env(&server)
        .await?;
    test.codex.start_or_steer_turn(input()).await?;
    let EventMsg::TurnComplete(initial) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await
    else {
        unreachable!()
    };
    let mut marker = None;
    let mut error_count = 0;
    let failed = loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::ItemCompleted(event) => {
                if let TurnItem::WakeupStarted(item) = event.item {
                    assert!(marker.replace(item).is_none(), "one automatic dispatch");
                }
            }
            EventMsg::Error(error) => {
                error_count += 1;
                assert_eq!(
                    error.message,
                    "Quota exceeded. Check your plan and billing details."
                );
                assert_eq!(
                    error.codex_error_info,
                    Some(codex_protocol::protocol::CodexErrorInfo::UsageLimitExceeded)
                );
            }
            EventMsg::TurnComplete(turn) => break turn,
            _ => {}
        }
    };
    let marker = marker.expect("admitted wakeup before provider failure");
    assert_eq!(marker.origin_turn_id, initial.turn_id);
    assert_ne!(marker.resumed_turn_id, initial.turn_id);
    assert_eq!(failed.turn_id, marker.resumed_turn_id);
    let terminal_error = failed
        .error
        .expect("failed outcome on normal turn completion");
    assert_eq!(
        terminal_error.message,
        "Quota exceeded. Check your plan and billing details."
    );
    assert_eq!(
        terminal_error.codex_error_info,
        Some(codex_protocol::protocol::CodexErrorInfo::UsageLimitExceeded)
    );
    assert_eq!(error_count, 1);
    assert_eq!(mock.requests().len(), 3);
    assert!(mock.requests()[2].body_contains_text(&marker.task_id));
    // Observe beyond the original delay again: neither the timer nor the provider retries.
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(mock.requests().len(), 3);
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Recover normally after the failed wakeup.".into(),
            text_elements: vec![],
        }]))
        .await?;
    let mut recovered = false;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::ItemCompleted(event) => assert!(
                !matches!(event.item, TurnItem::WakeupStarted(_)),
                "no timer redispatch during recovery"
            ),
            EventMsg::Error(error) => panic!("recovery failed: {error:?}"),
            EventMsg::AgentMessage(message) => {
                recovered |= message.message == "Recovered after the failed wakeup."
            }
            EventMsg::TurnComplete(turn) => {
                assert_ne!(turn.turn_id, marker.resumed_turn_id);
                assert!(
                    turn.error.is_none(),
                    "ordinary recovery completes successfully"
                );
                break;
            }
            _ => {}
        }
    }
    assert!(recovered);
    assert_eq!(mock.requests().len(), 4);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
