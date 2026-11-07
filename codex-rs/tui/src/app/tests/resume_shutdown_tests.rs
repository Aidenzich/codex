//! A delayed close must not shut down a thread that has already been reloaded.

use super::disconnect::serve_reconnect_requests;
use super::*;
use crate::app_server_session::ThreadParamsMode;
use codex_app_server_client::AppServerEvent;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::net::TcpListener;

#[tokio::test]
async fn wakeup_new_and_resume_routes_unsubscribe_previous_conversation() -> Result<()> {
    use super::session_lifecycle_requests::recorded_params;
    use super::session_lifecycle_requests::start_recording_app_server;
    let (mut app, _events, _commands) = make_test_app_with_channels().await;
    let (mut server, requests, proxy) = start_recording_app_server(&app.config, None, None).await?;
    let original = server.start_thread(&app.config).await?;
    let original_id = original.session.thread_id;
    app.enqueue_primary_thread_session(original.session, original.turns)
        .await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    requests.lock().unwrap().clear();
    app.start_fresh_session(&mut tui, &mut server, None, None, None)
        .await;
    let fresh_id = app.chat_widget.thread_id().expect("fresh conversation");
    assert_ne!(fresh_id, original_id);
    assert!(
        recorded_params(&requests, "thread/unsubscribe")
            .iter()
            .any(|params| params["threadId"] == original_id.to_string())
    );
    let resumed = server
        .resume_thread(
            &app.local_settings,
            app.config.clone(),
            original_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    let prepared = super::super::resume_preparation::PreparedResume {
        backend_id: server.backend_id(),
        config: app.config.clone(),
        local_settings: app.local_settings.clone(),
        thread: resumed,
        read_only: false,
        history_notice: None,
        provider_error: None,
    };
    requests.lock().unwrap().clear();
    app.activate_prepared_session(&mut tui, &mut server, prepared)
        .await?;
    assert_eq!(app.chat_widget.thread_id(), Some(original_id));
    assert!(
        recorded_params(&requests, "thread/unsubscribe")
            .iter()
            .any(|params| params["threadId"] == fresh_id.to_string())
    );
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn wakeup_buffer_reselection_and_history_refresh_render_started_once() -> Result<()> {
    let (mut app, mut events, _commands) = make_test_app_with_channels().await;
    let thread_id = ThreadId::new();
    let session = test_thread_session(thread_id, app.config.cwd.to_path_buf());
    let marker = ThreadItemPayload::from(codex_protocol::items::TurnItem::WakeupStarted(
        codex_protocol::items::WakeupStartedItem {
            id: "started-item".into(),
            task_id: "pending-task".into(),
            origin_turn_id: "origin".into(),
            resumed_turn_id: "followup".into(),
            instruction: "Check the build result.".into(),
        },
    ));
    app.thread_event_channels.insert(
        thread_id,
        ThreadEventChannel::new_with_session(
            THREAD_EVENT_CHANNEL_CAPACITY,
            session.clone(),
            Vec::new(),
        ),
    );
    {
        let channel = app.thread_event_channels.get(&thread_id).unwrap();
        let mut store = channel.store.lock().await;
        store.push_notification(ServerNotification::ItemCompleted(
            codex_app_server_protocol::ItemCompletedNotification::new(
                thread_id.to_string(),
                "followup".into(),
                marker.clone(),
                None,
                0,
            ),
        ));
    }
    let mut tui = crate::tui::test_support::make_test_tui()?;
    for refresh_history in [false, true] {
        if refresh_history {
            let channel = app.thread_event_channels.get(&thread_id).unwrap();
            let mut store = channel.store.lock().await;
            store.set_session(
                session.clone(),
                vec![test_turn(
                    "followup",
                    TurnStatus::Completed,
                    vec![marker.clone()],
                )],
            );
            store.rebase_buffer_after_session_refresh();
        }
        app.reset_for_thread_switch(&mut tui)?;
        while events.try_recv().is_ok() {}
        let snapshot = app
            .thread_event_channels
            .get(&thread_id)
            .unwrap()
            .store
            .lock()
            .await
            .snapshot();
        app.replay_thread_snapshot(snapshot, false);
        let text = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| {
                if let AppEvent::InsertHistoryCell(cell) = event {
                    Some(lines_to_single_string(&cell.display_lines(80)))
                } else {
                    None
                }
            })
            .collect::<String>();
        assert_eq!(text.matches("Wakeup started").count(), 1, "{text}");
        assert!(text.contains("pending-task"));
        assert!(text.contains("Check the build result."));
    }
    Ok(())
}

#[tokio::test]
async fn wakeup_native_navigation_retains_side_and_reply_then_exit_unsubscribes() -> Result<()> {
    let (mut app, _events, _commands) = make_test_app_with_channels().await;
    let original = ThreadId::new();
    let destination = ThreadId::new();
    let side = ThreadId::new();
    app.enqueue_primary_thread_session(
        test_thread_session(original, app.config.cwd.to_path_buf()),
        Vec::new(),
    )
    .await?;
    app.ensure_thread_channel(destination);
    app.ensure_thread_channel(side);
    app.side_threads
        .insert(side, SideThreadState::new(original));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
    let remote = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        serve_reconnect_requests(
            tokio_tungstenite::accept_async(stream).await?,
            move |request| {
                let method = request.method.clone();
                let params = request.params.clone();
                requests_tx
                    .send((method.clone(), params))
                    .expect("request receiver");
                async move {
                    assert_eq!(method, "thread/unsubscribe");
                    Some(json!({"result": {"status": "unsubscribed"}}))
                }
            },
        )
        .await
    });
    let mut server = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        crate::connect_remote_app_server(endpoint).await?,
        ThreadParamsMode::Remote,
    );
    // Finishing the initial reply leaves the native subscription alive.
    app.chat_widget
        .handle_server_notification(turn_started_notification(original, "initial-turn"), None);
    app.chat_widget.handle_server_notification(
        turn_completed_notification(original, "initial-turn", TurnStatus::Completed),
        None,
    );
    assert!(requests_rx.try_recv().is_err());
    app.detach_current_thread_for_navigation(&mut server, Some(destination))
        .await;
    let (method, params) = requests_rx.try_recv().expect("navigation unsubscribe");
    assert_eq!(method, "thread/unsubscribe");
    assert_eq!(params.unwrap()["threadId"], original.to_string());
    assert!(
        requests_rx.try_recv().is_err(),
        "destination and retained side stay subscribed"
    );
    app.active_thread_id = Some(destination);
    let control = app
        .handle_exit_mode(&mut server, ExitMode::ShutdownFirst)
        .await;
    assert!(matches!(
        control,
        AppRunControl::Exit(ExitReason::UserRequested)
    ));
    let (_, params) = requests_rx.try_recv().expect("side unsubscribe");
    assert_eq!(params.unwrap()["threadId"], side.to_string());
    assert!(!app.side_threads.contains_key(&side));
    let remaining: Vec<_> = std::iter::from_fn(|| requests_rx.try_recv().ok())
        .map(|(_, params)| params.unwrap()["threadId"].as_str().unwrap().to_owned())
        .collect();
    assert!(remaining.contains(&destination.to_string()));
    server.shutdown().await?;
    tokio::time::timeout(Duration::from_secs(5), remote).await???;
    Ok(())
}

#[tokio::test]
async fn thread_close_exits_only_when_displayed_thread_is_unloaded() -> Result<()> {
    let (mut app, mut events, _commands) = make_test_app_with_channels().await;
    std::fs::write(
        app.config.codex_home.join("config.toml"),
        "thread_unload_delay_secs = 0\n",
    )?;
    let mut server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
    let started = server.start_thread(&app.config).await?;
    let thread_id = started.session.thread_id;
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.drain_active_thread_events(&mut tui).await?;

    // The old runtime's notification can arrive after this thread ID is loaded again.
    app.handle_app_server_event(
        &server,
        AppServerEvent::ServerNotification(Box::new(thread_closed_notification(thread_id))),
    )
    .await;
    app.drain_active_thread_events(&mut tui).await?;
    assert!(
        !std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, AppEvent::Exit(_)))
    );

    // A genuine close must still exit.
    server.thread_unsubscribe(thread_id).await?;
    let closed = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        while let Some(event) = server.next_event().await {
            if matches!(&event.event,
                AppServerEvent::ServerNotification(notification)
                    if matches!(notification.as_ref(), ServerNotification::ThreadClosed(closed)
                        if closed.thread_id == thread_id.to_string()))
            {
                return event;
            }
        }
        panic!("server event stream ended before the thread closed");
    })
    .await?;
    app.handle_backend_event(&server, closed).await;
    app.drain_active_thread_events(&mut tui).await?;
    assert!(
        std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, AppEvent::Exit(ExitMode::Immediate)))
    );
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn failed_close_status_read_reconnects_remote_or_closes_embedded() -> Result<()> {
    for (failure, expected_reads, offline) in [
        ("disconnect", 1, true),
        ("timeout", 1, true),
        ("transient", 2, false),
        ("persistent", 2, true),
        ("embedded", 2, false),
        ("unsupported", 1, false),
    ] {
        let (mut app, mut events, _commands) = make_test_app_with_channels().await;
        let id = ThreadId::new();
        app.enqueue_primary_thread_session(
            test_thread_session(id, app.config.cwd.to_path_buf()),
            Vec::new(),
        )
        .await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
        if failure != "embedded" {
            app.app_server_target = AppServerTarget::Remote {
                endpoint: endpoint.clone(),
            };
        }
        let reads = Arc::new(AtomicUsize::new(/*v*/ 0));
        let server_reads = reads.clone();
        let cwd = app.config.cwd.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            serve_reconnect_requests(
                tokio_tungstenite::accept_async(stream).await?,
                move |request| {
                    assert_eq!(request.method, "thread/read");
                    let attempt = server_reads.fetch_add(/*val*/ 1, Ordering::SeqCst);
                    let response = match failure {
                        "disconnect" | "timeout" => None,
                        "unsupported" => {
                            Some(json!({"error": {"code": -32601, "message": "method not found"}}))
                        }
                        "transient" if attempt == 1 => Some(json!({"result": {"thread": {
                            "id": id, "sessionId": id, "preview": "", "ephemeral": false,
                            "modelProvider": "test-provider", "createdAt": 1, "updatedAt": 2,
                            "status": {"type": "idle"}, "cwd": cwd, "cliVersion": "0.0.0",
                            "source": "cli", "turns": []
                        }}})),
                        _ => {
                            Some(json!({"error": {"code": -32603, "message": "temporary failure"}}))
                        }
                    };
                    async move {
                        if failure == "timeout" {
                            std::future::pending::<()>().await;
                        }
                        response
                    }
                },
            )
            .await
        });
        let session = AppServerSession::new(
            crate::backend_id::BackendId::new(),
            crate::connect_remote_app_server(endpoint).await?,
            ThreadParamsMode::Remote,
        );
        tokio::time::timeout(
            Duration::from_secs(/*secs*/ 10),
            app.handle_app_server_event(
                &session,
                AppServerEvent::ServerNotification(Box::new(thread_closed_notification(id))),
            ),
        )
        .await?;
        let mut tui = crate::tui::test_support::make_test_tui()?;
        app.drain_active_thread_events(&mut tui).await?;
        assert_eq!(
            (reads.load(Ordering::SeqCst), app.reconnect.offline),
            (expected_reads, offline),
            "{failure}"
        );
        assert_eq!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| matches!(event, AppEvent::Exit(_) | AppEvent::FatalExitRequest(_))),
            matches!(failure, "embedded" | "unsupported"),
            "{failure}"
        );
        server.abort();
        let _ = server.await;
        session.shutdown().await?;
    }
    Ok(())
}
