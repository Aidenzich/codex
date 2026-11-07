#![cfg(test)]

//! Checks runtime release and notification delivery around listener shutdown.

use super::super::thread_goal_processor::ThreadGoalRequestProcessor;
use super::super::thread_processor::ThreadRequestProcessor;
use super::super::thread_processor::ThreadResumeTarget;
use super::*;
use crate::config_manager::ConfigManager;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingMessage;
use crate::thread_sample::SampleResponseRegistry;
use crate::thread_state::ConnectionCapabilities;
use crate::thread_state::PendingNetworkApproval;
use crate::thread_state::resolve_server_request_on_thread_listener;
use anyhow::Context;
use anyhow::Result;
use codex_analytics::AnalyticsEventsClient;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::ThreadUnsubscribeStatus;
use codex_arg0::Arg0DispatchPaths;
use codex_goal_extension::GoalService;
use codex_login::AuthManager;
use codex_thread_store::PersistContext;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use std::collections::HashSet;
use test_case::test_case;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

#[test_case(true; "listener attached before shutdown")]
#[test_case(false; "listener attached after shutdown")]
#[tokio::test]
async fn listener_shutdown_releases_runtime_and_preserves_other_subscribers(
    attach_before_shutdown: bool,
) -> Result<()> {
    let server = wiremock::MockServer::start().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let thread_id = test.session_configured.thread_id;
    test.thread_store
        .persist_thread(thread_id, PersistContext::Standard)
        .await?;
    let thread = test.codex;
    let runtime = Arc::downgrade(&thread);
    let config = Arc::new(test.config.clone());
    let auth =
        AuthManager::shared_from_config(config.as_ref(), /*enable_codex_api_key_env*/ false)
            .await?;
    let config_manager =
        ConfigManager::without_managed_config_for_tests(config.codex_home.to_path_buf());
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(/*buffer*/ 16);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        outgoing_tx,
        AnalyticsEventsClient::disabled(),
    ));
    let thread_state_manager = ThreadStateManager::new();
    let thread_watch_manager = ThreadWatchManager::new_with_outgoing(outgoing.clone());
    let skills_watcher = SkillsWatcher::new(
        test.thread_manager.skills_service(),
        &config.codex_home,
        outgoing.clone(),
    );
    let thread_goal_processor = ThreadGoalRequestProcessor::new(
        test.thread_manager.clone(),
        outgoing.clone(),
        config.clone(),
        thread_state_manager.clone(),
        /*state_db*/ None,
        Arc::new(GoalService::new()),
        config_manager.clone(),
    );
    let processor = ThreadRequestProcessor::new(
        auth,
        test.thread_manager.clone(),
        outgoing,
        Arg0DispatchPaths::default(),
        config,
        config_manager,
        test.thread_store.clone(),
        Default::default(),
        thread_state_manager.clone(),
        thread_watch_manager.clone(),
        Arc::new(Semaphore::new(/*permits*/ 1)),
        thread_goal_processor,
        /*state_db*/ None,
        /*log_db*/ None,
        skills_watcher.clone(),
        /*turn_cost_worker*/ None,
        Vec::new(),
        Arc::new(SampleResponseRegistry::default()),
    );
    let connection_id = ConnectionId(1);
    let disconnecting_connection_id = ConnectionId(2);
    let remaining_connection_id = ConnectionId(3);
    let connection_ids = vec![
        connection_id,
        disconnecting_connection_id,
        remaining_connection_id,
    ];
    for &connection_id in &connection_ids {
        processor
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
    }
    let network_request_id = RequestId::Integer(4);
    if attach_before_shutdown {
        processor
            .try_attach_thread_listener(thread_id, Arc::clone(&thread), connection_ids.clone())
            .await;
        // Leave the prompt outstanding so shutdown runs before the proxy waiter resolves it.
        let permission_guard = thread_watch_manager
            .note_permission_requested(&thread_id.to_string())
            .await;
        thread_state_manager
            .thread_state(thread_id)
            .await
            .lock()
            .await
            .network_approvals
            .insert(
                "network-approval".to_string(),
                PendingNetworkApproval {
                    request_id: network_request_id.clone(),
                    permission_guard,
                },
            );
    }
    thread.shutdown_and_wait().await?;
    let shutdown_result = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
        let resume = processor
            .thread_resume(
                ThreadResumeTarget::Client(ConnectionRequestId {
                    connection_id,
                    request_id: RequestId::Integer(1),
                }),
                ThreadResumeParams {
                    thread_id: thread_id.to_string(),
                    exclude_turns: true,
                    ..Default::default()
                },
                /*app_server_client_name*/ None,
                /*app_server_client_version*/ None,
                /*client_mcp_extensions*/ Default::default(),
            )
            .await;
        test.thread_manager.remove_thread(&thread_id).await;
        if !attach_before_shutdown {
            processor
                .try_attach_thread_listener(thread_id, Arc::clone(&thread), connection_ids)
                .await;
        }
        let mut network_resolved = false;
        loop {
            match outgoing_rx.recv().await.expect("outgoing channel closed") {
                OutgoingEnvelope::ToConnection {
                    connection_id: recipient,
                    message: OutgoingMessage::AppServerNotification(notification),
                    ..
                } if recipient == connection_id
                    && matches!(
                        &notification.notification,
                        ServerNotification::ServerRequestResolved(resolved)
                            if resolved.request_id == network_request_id
                    ) =>
                {
                    assert!(!network_resolved, "duplicate network prompt resolution");
                    network_resolved = true;
                }
                OutgoingEnvelope::Broadcast {
                    message: OutgoingMessage::AppServerNotification(notification),
                } if matches!(
                    &notification.notification,
                    ServerNotification::ThreadStatusChanged(status)
                        if status.status == ThreadStatus::NotLoaded
                ) =>
                {
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(network_resolved, attach_before_shutdown);
        drop(thread);
        while runtime.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
        let Some(ClientResponsePayload::ThreadUnsubscribe(unsubscribe)) = processor
            .thread_unsubscribe(
                &ConnectionRequestId {
                    connection_id,
                    request_id: RequestId::Integer(2),
                },
                ThreadUnsubscribeParams {
                    thread_id: thread_id.to_string(),
                },
            )
            .await
            .expect("unsubscribe failed")
        else {
            panic!("unsubscribe response missing");
        };
        assert_eq!(
            unsubscribe,
            ThreadUnsubscribeResponse {
                status: ThreadUnsubscribeStatus::NotLoaded,
            },
        );
        assert_eq!(
            thread_state_manager
                .subscribed_connection_ids(thread_id)
                .await
                .into_iter()
                .collect::<HashSet<_>>(),
            HashSet::from([disconnecting_connection_id, remaining_connection_id]),
        );
        processor
            .connection_closed(disconnecting_connection_id)
            .await;
        assert_eq!(
            thread_state_manager
                .subscribed_connection_ids(thread_id)
                .await,
            vec![remaining_connection_id],
        );
        resolve_server_request_on_thread_listener(
            &thread_state_manager.thread_state(thread_id).await,
            RequestId::Integer(3),
        )
        .await;
        loop {
            if let OutgoingEnvelope::ToConnection {
                connection_id: recipient,
                message: OutgoingMessage::AppServerNotification(notification),
                ..
            } = outgoing_rx.recv().await.expect("outgoing channel closed")
                && recipient == remaining_connection_id
                && matches!(
                    &notification.notification,
                    ServerNotification::ServerRequestResolved(resolved)
                        if resolved.request_id == RequestId::Integer(3)
                )
            {
                break;
            }
        }
        resume
    })
    .await;
    thread_state_manager.clear_all_listeners().await;
    skills_watcher.shutdown();

    let resume = shutdown_result.context("terminal listener did not release or relay")?;
    assert_eq!(
        resume.expect_err("resume accepted a terminated runtime"),
        invalid_request(format!(
            "thread {thread_id} is closing; retry thread/resume after the thread is closed"
        )),
    );
    Ok(())
}

#[test_case(false, false; "last_unsubscribe_clears_before_unload")]
#[test_case(true, false; "last_disconnect_clears_before_unload")]
#[test_case(false, true; "one_of_two_clients_preserves_pending")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wakeup_actual_subscriber_transitions_revoke_only_after_last_detach(
    disconnect: bool,
    keep_second: bool,
) -> Result<()> {
    use codex_core::TurnInputRequest;
    use codex_protocol::protocol::{EventMsg, SessionSource};
    use codex_protocol::user_input::UserInput;
    use core_test_support::{responses, wait_for_event};
    use serde_json::json;
    let server = responses::start_mock_server().await;
    let schedule = responses::sse(vec![
        responses::ev_function_call_with_namespace(
            "schedule",
            "clock",
            "schedule_wakeup",
            &json!({"duration_ms":250,"instruction":"subscriber follow-up"}).to_string(),
        ),
        responses::ev_completed("scheduled"),
    ]);
    let answer = |id: &str| {
        responses::sse(vec![
            responses::ev_assistant_message(id, "done"),
            responses::ev_completed(id),
        ])
    };
    let list = responses::sse(vec![
        responses::ev_function_call_with_namespace("list", "clock", "list_wakeups", "{}"),
        responses::ev_completed("listed"),
    ]);
    let sequence = if keep_second {
        vec![schedule, answer("initial"), answer("resumed")]
    } else {
        vec![schedule, answer("initial"), list, answer("list-answer")]
    };
    let mock = responses::mount_sse_sequence(&server, sequence).await;
    let test = test_codex()
        .with_session_source(SessionSource::VSCode)
        .build_with_auto_env(&server)
        .await?;
    let thread_id = test.session_configured.thread_id;
    let manager = ThreadStateManager::new();
    let first = ConnectionId(71);
    let second = ConnectionId(72);
    manager
        .connection_initialized(first, ConnectionCapabilities::default())
        .await;
    manager
        .connection_initialized(second, ConnectionCapabilities::default())
        .await;
    let candidate = manager.created_listener_state(thread_id).await;
    let candidate_lock = candidate.state().lock().await;
    let attached = if keep_second {
        vec![first, second]
    } else {
        vec![first]
    };
    manager
        .subscribe_and_install_listener(thread_id, &candidate, &test.codex, &attached, |_| {
            Ok::<_, anyhow::Error>(())
        })
        .await?
        .context("listener install")?;
    drop(candidate_lock);
    let input = || {
        TurnInputRequest::user_input(vec![UserInput::Text {
            text: "schedule and finish".into(),
            text_elements: vec![],
        }])
    };
    test.codex.start_or_steer_turn(input()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let receipt = mock
        .function_call_output_text("schedule")
        .context("native schedule receipt")?;
    let receipt: serde_json::Value = serde_json::from_str(&receipt)?;
    assert!(receipt["task_id"].as_str().is_some_and(|id| !id.is_empty()));
    if disconnect {
        manager.remove_connection(first).await;
    } else {
        assert!(
            manager
                .unsubscribe_connection_from_thread(thread_id, first)
                .await
        );
    }
    if keep_second {
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        assert_eq!(mock.requests().len(), 3);
    } else {
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(mock.requests().len(), 2);
        // A later attachment enables new work but cannot restore the cleared job.
        assert!(
            manager
                .try_add_connection_to_thread(thread_id, second)
                .await
        );
        test.codex.start_or_steer_turn(input()).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        let output: serde_json::Value = serde_json::from_str(
            &mock
                .function_call_output_text("list")
                .context("native pending read-back")?,
        )?;
        assert_eq!(output["wakeups"], json!([]));
        assert_eq!(mock.requests().len(), 4);
    }
    manager.clear_all_listeners().await;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
