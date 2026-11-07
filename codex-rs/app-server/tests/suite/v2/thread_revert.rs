use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::create_mock_responses_server_sequence;
use app_test_support::create_request_user_input_sse_response;
use app_test_support::write_models_cache_with_models;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItemsListCursor;
use codex_app_server_protocol::ThreadItemsListParams;
use codex_app_server_protocol::ThreadItemsListResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadRevertParams;
use codex_app_server_protocol::ThreadRevertResponse;
use codex_app_server_protocol::ThreadRevertedNotification;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::Settings;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_rollout::RolloutItem;
use codex_rollout::read_session_meta_line;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::load_default_config_for_test;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[test_case::test_case(1; "one_retained_turn")]
#[test_case::test_case(101; "cross_page_oldest_origin")]
#[tokio::test]
async fn wakeup_revert_preserves_retained_job_and_discards_reverted_origin(
    retained_turns: usize,
) -> Result<()> {
    use serde_json::json;
    use std::time::Duration;

    fn call(id: &str, name: &str, arguments: Value) -> String {
        responses::sse(vec![
            responses::ev_function_call_with_namespace(id, "clock", name, &arguments.to_string()),
            responses::ev_completed(id),
        ])
    }
    fn answer(id: &str) -> String {
        responses::sse(vec![
            responses::ev_assistant_message(id, id),
            responses::ev_completed(id),
        ])
    }
    let model = responses::start_mock_server().await;
    let delay_ms = 30_000;
    let mut sequence = vec![
        call(
            "keep",
            "schedule_wakeup",
            json!({"duration_ms":delay_ms,"instruction":"retained ORCHID"}),
        ),
        answer("scheduled-a"),
    ];
    sequence.extend((1..retained_turns).map(|_| answer("retained-padding")));
    sequence.extend([
        call(
            "discard",
            "schedule_wakeup",
            json!({"duration_ms":delay_ms,"instruction":"reverted LOTUS"}),
        ),
        answer("scheduled-b"),
        call("list", "list_wakeups", json!({})),
        answer("listed"),
        answer("autonomous"),
    ]);
    let mock = responses::mount_sse_sequence(&model, sequence).await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&model.uri())
        .enable_feature(Feature::Sqlite)
        .write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_json_logging(
            "warn,codex_app_server::request_processors::thread_processor=debug,codex_thread_store::local::thread_history=debug,codex_rollout::compression=debug,codex_state::runtime::threads=debug",
        )
        .build()
        .await?;
    initialize_experimental(&mut app).await?;
    let started = app
        .start_thread(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?;
    let thread_id = started.thread.id;
    let input = |text: &str| TurnStartParams {
        thread_id: thread_id.clone(),
        input: vec![UserInput::Text {
            text: text.into(),
            text_elements: vec![],
        }],
        ..Default::default()
    };
    let first = app
        .start_turn_and_wait_for_completion(input("Schedule retained ORCHID"))
        .await?;
    for _ in 1..retained_turns {
        app.start_turn_and_wait_for_completion(input("Retained padding turn"))
            .await?;
    }
    let second = app
        .start_turn_and_wait_for_completion(input("Schedule reverted LOTUS"))
        .await?;
    let _: ThreadRevertResponse = app
        .request(|request_id| ClientRequest::ThreadRevert {
            request_id,
            params: ThreadRevertParams {
                thread_id: thread_id.clone(),
                before_turn_id: second.turn.id.clone(),
            },
        })
        .await?;
    let listed = app
        .start_turn_and_wait_for_completion(input("List pending wakeups"))
        .await?;
    // Completion notifications contain a summary, not the full turn transcript.
    let notification = timeout(
        Duration::from_secs(45),
        app.read_stream_until_matching_notification("native wakeup item", |notification| {
            notification.method == "item/completed"
                && notification
                    .params
                    .as_ref()
                    .and_then(|params| params.pointer("/item/type"))
                    .and_then(Value::as_str)
                    == Some("wakeupStarted")
        }),
    )
    .await??;
    let marker_event: codex_app_server_protocol::ItemCompletedNotification =
        serde_json::from_value(notification.params.expect("wakeup item parameters"))?;
    let followup: TurnCompletedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        app.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(followup.thread_id, thread_id);
    assert_eq!(followup.turn.status, TurnStatus::Completed);
    assert!(followup.turn.error.is_none());
    assert_ne!(followup.turn.id, first.turn.id);
    assert_ne!(followup.turn.id, second.turn.id);
    assert_ne!(followup.turn.id, listed.turn.id);

    let requests = mock.requests();
    let outputs: Vec<Value> = requests
        .iter()
        .flat_map(|request| request.body_json()["input"].as_array().unwrap().clone())
        .filter(|item| item["type"] == "function_call_output")
        .filter_map(|item| {
            item["output"]
                .as_str()
                .and_then(|text| serde_json::from_str(text).ok())
        })
        .collect();
    let original = outputs
        .iter()
        .find(|output| output["instruction"] == "retained ORCHID")
        .expect("schedule receipt");
    let pending = outputs
        .iter()
        .find_map(|output| output["wakeups"].as_array())
        .expect("list receipt");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["task_id"], original["task_id"]);
    assert_eq!(pending[0]["due_at"], original["due_at"]);
    assert_eq!(pending[0]["originating_turn_id"], first.turn.id);
    assert_eq!(marker_event.thread_id, thread_id);
    assert_eq!(marker_event.turn_id, followup.turn.id);
    let marker = match &marker_event.item.payload {
        codex_app_server_protocol::ThreadItemPayload::WakeupStarted {
            task_id,
            origin_turn_id,
            resumed_turn_id,
            instruction,
            ..
        } => (task_id, origin_turn_id, resumed_turn_id, instruction),
        _ => panic!("same-thread native wakeup marker"),
    };
    assert_eq!(marker.0, original["task_id"].as_str().unwrap());
    assert_eq!(marker.1, &first.turn.id);
    assert_eq!(marker.2, &followup.turn.id);
    assert_eq!(marker.3, "retained ORCHID");
    let body = requests.last().unwrap().body_json().to_string();
    assert!(body.contains("retained ORCHID"));
    assert!(!body.contains("reverted LOTUS"));
    assert!(body.contains(original["task_id"].as_str().unwrap()));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        mock.requests().len(),
        retained_turns + 6,
        "one autonomous claim, no reverted-origin claim"
    );
    let events = app.json_log_events()?;
    let scan_events: Vec<_> = events
        .iter()
        .filter(|event| {
            event["spans"].as_array().is_some_and(|spans| {
                spans
                    .iter()
                    .any(|span| span["name"] == "wakeup_origin_scan")
            })
        })
        .collect();
    let count = |name: &str| {
        scan_events
            .iter()
            .filter(|event| event["fields"]["event.name"] == name)
            .count()
    };
    let pages = retained_turns.div_ceil(100);
    assert_eq!(
        count("wakeup_origin_scan_completed"),
        1,
        "one isolated origin scan: {scan_events:?}"
    );
    assert_eq!(
        count("thread_history_turn_page_read"),
        pages,
        "one store page operation per chunk: {scan_events:?}"
    );
    // Revert creates an empty replacement rollout and one retained ancestor.
    // Each page validates metadata and resolves the current rollout once. Its
    // header is opened by resolution, then each of the two lineage headers is
    // opened. The first page queries the empty leaf and retained ancestor;
    // a subsequent cursor starts directly in that ancestor.
    assert_eq!(
        count("state_thread_query_completed"),
        pages * 2,
        "actual metadata SQL queries per page: {scan_events:?}"
    );
    assert_eq!(
        count("thread_history_turn_query_completed"),
        pages + 1,
        "actual SQL queries for first and subsequent lineage pages: {scan_events:?}"
    );
    assert_eq!(
        count("rollout_reader_opened"),
        pages * 3,
        "actual current and lineage header opens per page: {scan_events:?}"
    );
    for event in scan_events
        .iter()
        .filter(|event| event["fields"]["event.name"] == "thread_history_turn_page_read")
    {
        assert_eq!(
            event["fields"]["lineage_segments"], 2,
            "observed revert lineage"
        );
    }
    let returned_rows: u64 = scan_events
        .iter()
        .filter(|event| event["fields"]["event.name"] == "thread_history_turn_page_read")
        .map(|event| {
            event["fields"]["rows"]
                .as_u64()
                .expect("observed store rows")
        })
        .sum();
    assert_eq!(returned_rows, retained_turns as u64);
    println!(
        "wakeup revert origin scan: retained={retained_turns} pages={pages} returned_rows={returned_rows} metadata_sql={} turn_sql={} rollout_opens={}",
        count("state_thread_query_completed"),
        count("thread_history_turn_query_completed"),
        count("rollout_reader_opened"),
    );
    app.shutdown_gracefully().await?;
    Ok(())
}

#[test_case::test_case(None; "default_paginated")]
#[test_case::test_case(Some(ThreadHistoryMode::Legacy); "explicit_legacy")]
#[tokio::test]
async fn thread_revert_rejects_ephemeral_without_losing_context(
    history_mode: Option<ThreadHistoryMode>,
) -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Remembered answer").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    initialize_experimental(&mut mcp).await?;
    let ThreadStartResponse { thread, .. } = mcp
        .start_thread(ThreadStartParams {
            ephemeral: Some(true),
            history_mode,
            ..Default::default()
        })
        .await?;
    let completed = mcp
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Remember this task".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let request_id = mcp
        .send_request(
            "thread/revert",
            Some(serde_json::to_value(ThreadRevertParams {
                thread_id: thread.id.clone(),
                before_turn_id: completed.turn.id,
            })?),
        )
        .await?;
    let error = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        error.error,
        codex_app_server_protocol::JSONRPCErrorError {
            code: -32600,
            message: "ephemeral threads do not support thread/revert".to_string(),
            data: None,
        }
    );
    mcp.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id,
        input: vec![UserInput::Text {
            text: "Continue".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    let requests = server.received_requests().await.expect("response requests");
    let followup = requests
        .iter()
        .rev()
        .find(|request| request.url.path().ends_with("/responses"))
        .expect("followup request")
        .body_json::<Value>()?;
    let input = followup["input"].as_array().expect("model input");
    for (role, text) in [
        ("user", "Remember this task"),
        ("assistant", "Remembered answer"),
    ] {
        assert!(
            input.iter().any(|item| item["role"] == role
                && item["content"]
                    .as_array()
                    .is_some_and(|content| content.iter().any(|part| part["text"] == text))),
            "missing prior {role} message from followup: {followup}"
        );
    }
    Ok(())
}

#[test_case::test_case(false; "live_reload")]
#[test_case::test_case(true; "cold_resume")]
#[tokio::test]
async fn thread_revert_preserves_model_selected_multi_agent_version(restart: bool) -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .disable_feature(Feature::MultiAgentV2)
        .write(codex_home.path())?;
    let config = load_default_config_for_test(&codex_home).await;
    let mut model = codex_core::test_support::construct_model_info_offline("mock-model", &config);
    model.multi_agent_version = Some(MultiAgentVersion::V2);
    write_models_cache_with_models(codex_home.path(), vec![model]).await?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    initialize_experimental(&mut mcp).await?;
    let ThreadStartResponse { thread, .. } = mcp
        .start_thread(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?;
    let completed = mcp
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "First message".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: ThreadRevertResponse = mcp
        .request(|request_id| ClientRequest::ThreadRevert {
            request_id,
            params: ThreadRevertParams {
                thread_id: thread.id.clone(),
                before_turn_id: completed.turn.id,
            },
        })
        .await?;
    if restart {
        // Restart before another turn can persist a replacement TurnContext.
        mcp.shutdown_gracefully().await?;
        mcp = TestAppServer::builder()
            .with_codex_home(codex_home.path())
            .build()
            .await?;
        initialize_experimental(&mut mcp).await?;
        let _: ThreadResumeResponse = mcp
            .request(|request_id| ClientRequest::ThreadResume {
                request_id,
                params: ThreadResumeParams {
                    thread_id: thread.id.clone(),
                    exclude_turns: true,
                    ..Default::default()
                },
            })
            .await?;
    }
    mcp.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id,
        input: vec![UserInput::Text {
            text: "Edited first message".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;

    let requests = server.received_requests().await.expect("response requests");
    let mut multi_agent_namespaces = Vec::new();
    for request in requests
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
    {
        let body = request.body_json::<Value>()?;
        multi_agent_namespaces.push(
            body["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .filter(|name| matches!(*name, "collaboration" | "multi_agent_v1"))
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(
        multi_agent_namespaces,
        vec![vec!["collaboration"], vec!["collaboration"]]
    );
    Ok(())
}

#[tokio::test]
async fn thread_revert_preserves_fork_cutoff_after_cold_resume() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    let updated_workspace = TempDir::new()?;
    let saved_cwd = AbsolutePathBuf::from_absolute_path(updated_workspace.path().canonicalize()?)?
        .into_path_buf();
    let extra_workspace = TempDir::new()?;
    let saved_roots = vec![
        AbsolutePathBuf::from_absolute_path(&saved_cwd)?,
        AbsolutePathBuf::from_absolute_path(extra_workspace.path().canonicalize()?)?,
    ];
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    // This fixture checks host-native cwd and workspace restoration across fork and revert.
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    initialize_experimental(&mut mcp).await?;
    let ThreadStartResponse { thread: parent, .. } = mcp
        .request(|request_id| ClientRequest::ThreadStart {
            request_id,
            params: ThreadStartParams {
                history_mode: Some(ThreadHistoryMode::Paginated),
                ..Default::default()
            },
        })
        .await?;
    let mut parent_turns = Vec::new();
    for text in ["parent first", "parent second"] {
        let completed = mcp
            .start_turn_and_wait_for_completion(TurnStartParams {
                thread_id: parent.id.clone(),
                cwd: Some(parent.cwd.as_path().to_path_buf()),
                input: vec![UserInput::Text {
                    text: text.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            })
            .await?;
        parent_turns.push(completed.turn.id);
    }
    let ThreadForkResponse { thread: child, .. } = mcp
        .request(|request_id| ClientRequest::ThreadFork {
            request_id,
            params: ThreadForkParams {
                thread_id: parent.id.clone(),
                cwd: Some(codex_home.path().to_string_lossy().into_owned()),
                ..Default::default()
            },
        })
        .await?;
    let child_meta = read_session_meta_line(child.path.as_ref().expect("child rollout"))
        .await?
        .meta;
    let fork_cutoff = child_meta
        .history_base
        .expect("fork history base")
        .end_ordinal_exclusive;
    assert_eq!(child_meta.forked_from_ordinal_exclusive, Some(fork_cutoff));
    let inherited_revert_cutoff =
        std::fs::read_to_string(parent.path.as_ref().expect("parent rollout"))?
            .lines()
            .map(codex_rollout::parse_rollout_line)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find_map(|line| match line.item {
                RolloutItem::EventMsg(EventMsg::TurnStarted(turn))
                    if turn.turn_id == parent_turns[1] =>
                {
                    line.ordinal
                }
                _ => None,
            })
            .expect("inherited turn start ordinal");
    let mut child_turns = Vec::new();
    for (text, runtime_workspace_roots) in [
        ("child first", None),
        ("child second", Some(saved_roots.clone())),
    ] {
        let completed = mcp
            .start_turn_and_wait_for_completion(TurnStartParams {
                thread_id: child.id.clone(),
                cwd: Some(saved_cwd.clone()),
                runtime_workspace_roots,
                input: vec![UserInput::Text {
                    text: text.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            })
            .await?;
        child_turns.push(completed.turn.id);
    }

    // First revert within the child, then revert into its inherited parent history.
    for (before_turn_id, expected_cutoff) in [
        (child_turns[1].clone(), fork_cutoff),
        (parent_turns[1].clone(), inherited_revert_cutoff),
    ] {
        let ThreadRevertResponse {
            thread: reverted, ..
        } = mcp
            .request(|request_id| ClientRequest::ThreadRevert {
                request_id,
                params: ThreadRevertParams {
                    thread_id: child.id.clone(),
                    before_turn_id,
                },
            })
            .await?;
        let meta = read_session_meta_line(reverted.path.as_ref().expect("reverted rollout"))
            .await?
            .meta;
        assert_eq!(meta.forked_from_ordinal_exclusive, Some(expected_cutoff));
        if expected_cutoff == fork_cutoff {
            assert!(
                meta.history_base
                    .expect("child revert base")
                    .end_ordinal_exclusive
                    > fork_cutoff
            );
        }

        mcp.shutdown_gracefully().await?;
        mcp = TestAppServer::builder()
            .with_codex_home(codex_home.path())
            .without_auto_env()
            .build()
            .await?;
        initialize_experimental(&mut mcp).await?;
        let ThreadResumeResponse {
            cwd,
            runtime_workspace_roots,
            ..
        } = mcp
            .request(|request_id| ClientRequest::ThreadResume {
                request_id,
                params: ThreadResumeParams {
                    thread_id: child.id.clone(),
                    ..Default::default()
                },
            })
            .await?;
        assert_eq!(
            (cwd.as_path(), runtime_workspace_roots),
            (saved_cwd.as_path(), saved_roots.clone())
        );
        mcp.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: child.id.clone(),
            input: vec![UserInput::Text {
                text: "continue after revert and cold resume".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
        let requests = server.received_requests().await.expect("response requests");
        let body = requests
            .iter()
            .rev()
            .find(|request| request.url.path().ends_with("/responses"))
            .expect("resumed model request")
            .body_json::<Value>()?;
        let metadata: Value = serde_json::from_str(
            body["client_metadata"]["x-codex-turn-metadata"]
                .as_str()
                .expect("turn metadata"),
        )?;
        assert_eq!(
            (
                metadata["forked_from_thread_id"].as_str(),
                metadata["forked_from_ordinal_exclusive"].as_u64()
            ),
            (Some(parent.id.as_str()), Some(expected_cutoff))
        );
    }
    Ok(())
}

#[tokio::test]
async fn thread_revert_replaces_paginated_history_before_turn() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock =
        responses::mount_sse_sequence(&server, vec![responses::sse_completed("done"); 4]).await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_model("gpt-5.5")
        .enable_feature(Feature::FastMode)
        .write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    initialize_experimental(&mut mcp).await?;

    let ThreadStartResponse { thread, .. } = mcp
        .start_thread(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Paginated),
            service_tier: Some(Some("priority".to_string())),
            personality: Some(Personality::Friendly),
            ..Default::default()
        })
        .await?;
    let stale_rollout_path = thread.path.clone().expect("thread rollout path");
    let mut turn_ids = Vec::new();
    for text in ["first", "second"] {
        let completed = mcp
            .start_turn_and_wait_for_completion(TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: text.to_string(),
                    text_elements: Vec::new(),
                }],
                model: (text == "second").then_some("gpt-daybreak-blue-latest".to_string()),
                personality: (text == "second").then_some(Personality::None),
                ..Default::default()
            })
            .await?;
        turn_ids.push(completed.turn.id);
    }

    let ThreadRevertResponse {
        thread: reverted_thread,
        turns_backwards_cursor,
        items_backwards_cursor,
    } = mcp
        .request(|request_id| ClientRequest::ThreadRevert {
            request_id,
            params: ThreadRevertParams {
                thread_id: thread.id.clone(),
                before_turn_id: turn_ids[1].clone(),
            },
        })
        .await?;
    let reverted: ThreadRevertedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_notification("thread/reverted"),
    )
    .await??;
    assert_eq!(reverted.thread_id, thread.id);

    assert_eq!(reverted_thread.id, thread.id);
    assert!(reverted_thread.turns.is_empty());
    assert!(items_backwards_cursor.is_some());
    assert_eq!(
        turn_ids_from_cursor(
            &mut mcp,
            &thread.id,
            turns_backwards_cursor,
            /*sort_direction*/ None,
        )
        .await?,
        turn_ids[..1]
    );
    let ThreadItemsListResponse {
        data: reverted_items,
        ..
    } = mcp
        .request(|request_id| ClientRequest::ThreadItemsList {
            request_id,
            params: ThreadItemsListParams {
                thread_id: thread.id.clone(),
                turn_id: None,
                cursor: items_backwards_cursor.map(ThreadItemsListCursor::Opaque),
                limit: None,
                sort_direction: None,
                defer_output: None,
            },
        })
        .await?;
    assert!(!reverted_items.is_empty());
    assert!(
        reverted_items
            .iter()
            .all(|item| item.turn_id == turn_ids[0])
    );

    mcp.shutdown_gracefully().await?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    initialize_experimental(&mut mcp).await?;
    let stale_resume_id = mcp
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id.clone(),
            path: Some(stale_rollout_path),
            ..Default::default()
        })
        .await?;
    let stale_resume_error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(stale_resume_id)),
    )
    .await??;
    assert!(
        stale_resume_error.error.message.contains("stale path")
            && stale_resume_error
                .error
                .message
                .contains("omit path and resume by thread id"),
        "unexpected resume error: {}",
        stale_resume_error.error.message,
    );
    let resume_id = mcp
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id.clone(),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(resume_id)).await??;
    let invalid_revert_id = mcp
        .send_raw_request(
            "thread/revert",
            Some(serde_json::to_value(ThreadRevertParams {
                thread_id: thread.id.clone(),
                before_turn_id: "missing-turn".to_string(),
            })?),
        )
        .await?;
    let invalid_revert_error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(invalid_revert_id)),
    )
    .await??;
    assert_eq!(
        invalid_revert_error.error.message,
        "turn not found: missing-turn"
    );

    let third_turn = mcp
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "third".to_string(),
                text_elements: Vec::new(),
            }],
            model: Some("gpt-5.5".to_string()),
            ..Default::default()
        })
        .await?;
    let body = response_mock
        .last_request()
        .expect("third turn request")
        .body_json();
    assert_eq!(body["service_tier"], "priority");
    assert!(
        !body["instructions"]
            .as_str()
            .is_some_and(|instructions| { instructions.contains("# Personality") })
    );
    let model_input = body["input"].clone();
    let model_input = serde_json::to_string(&model_input)?;
    assert!(model_input.contains(r#""text":"first""#));
    assert!(!model_input.contains(r#""text":"second""#));
    assert!(model_input.contains(r#""text":"third""#));
    assert_eq!(
        turn_ids_from_cursor(
            &mut mcp,
            &thread.id,
            /*cursor*/ None,
            Some(SortDirection::Asc),
        )
        .await?,
        vec![turn_ids[0].clone(), third_turn.turn.id]
    );
    Ok(())
}

#[tokio::test]
async fn thread_revert_interrupts_active_turn_and_keeps_thread_loaded() -> Result<()> {
    let home = TempDir::new()?;
    let server = create_mock_responses_server_sequence(vec![
        create_final_assistant_message_sse_response("first")?,
        create_request_user_input_sse_response("call_blocked")?,
        create_final_assistant_message_sse_response("third")?,
    ])
    .await;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(home.path())
        .build()
        .await?;
    initialize_experimental(&mut mcp).await?;

    let ThreadStartResponse { thread, .. } = mcp
        .start_thread(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?;
    let first_turn = mcp
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "first".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;

    let TurnStartResponse { turn: active_turn } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "sleep".to_string(),
                    text_elements: Vec::new(),
                }],
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Plan,
                    settings: Settings {
                        model: "mock-model".to_string(),
                        reasoning_effort: Some(ReasoningEffort::Medium),
                        developer_instructions: None,
                    },
                }),
                approval_policy: Some(AskForApproval::Never),
                ..Default::default()
            },
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_request_message(),
    )
    .await??;

    let ThreadRevertResponse {
        thread: reverted_thread,
        turns_backwards_cursor,
        items_backwards_cursor,
    } = mcp
        .request(|request_id| ClientRequest::ThreadRevert {
            request_id,
            params: ThreadRevertParams {
                thread_id: thread.id.clone(),
                before_turn_id: active_turn.id.clone(),
            },
        })
        .await?;
    let completed: TurnCompletedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(completed.thread_id, thread.id);
    assert_eq!(completed.turn.status, TurnStatus::Interrupted);
    assert!(reverted_thread.turns.is_empty());
    assert!(items_backwards_cursor.is_some());
    assert_eq!(
        turn_ids_from_cursor(
            &mut mcp,
            &thread.id,
            turns_backwards_cursor,
            /*sort_direction*/ None,
        )
        .await?,
        vec![first_turn.turn.id]
    );

    let resumed: ThreadResumeResponse = mcp
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: thread.id.clone(),
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(resumed.approval_policy, AskForApproval::Never);

    mcp.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id,
        input: vec![UserInput::Text {
            text: "third".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    Ok(())
}

async fn turn_ids_from_cursor(
    mcp: &mut TestAppServer,
    thread_id: &str,
    cursor: Option<String>,
    sort_direction: Option<SortDirection>,
) -> Result<Vec<String>> {
    let ThreadTurnsListResponse { data, .. } = mcp
        .request(|request_id| ClientRequest::ThreadTurnsList {
            request_id,
            params: ThreadTurnsListParams {
                thread_id: thread_id.to_string(),
                cursor,
                limit: None,
                sort_direction,
                items_view: None,
            },
        })
        .await?;
    Ok(data.into_iter().map(|turn| turn.id).collect())
}

async fn initialize_experimental(mcp: &mut TestAppServer) -> Result<()> {
    let initialized = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.initialize_with_capabilities(
            ClientInfo {
                name: "test-client".to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                required_human_approval: false,
                explicit_gateway_oauth: false,
                experimental_api: true,
                request_attestation: false,
                opt_out_notification_methods: None,
                mcp_server_openai_form_elicitation: false,
                extensions: None,
            }),
        ),
    )
    .await??;
    assert!(matches!(initialized, JSONRPCMessage::Response(_)));
    Ok(())
}
