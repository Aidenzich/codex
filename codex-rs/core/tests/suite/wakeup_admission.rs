use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wakeup_commits_actual_trigger_in_request_and_saved_turn_metadata() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            schedule(100),
            answer("initial", "Scheduled."),
            answer("followup", "done"),
        ],
    )
    .await;
    let test = test_codex()
        .with_session_source(SessionSource::Cli)
        .build_with_auto_env(&server)
        .await?;
    let mut request = input();
    // Use a real originating trigger: copying this write-once value into the
    // reserved context used to hide the later wakeup trigger.
    request.start.turn_trigger = Some("user".into());
    test.codex.start_or_steer_turn(request).await?;
    let first = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(first) = first else {
        unreachable!()
    };
    let second = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(second) = second else {
        unreachable!()
    };
    assert_ne!(first.turn_id, second.turn_id);
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    for (request, turn_id, trigger) in [
        (&requests[0], &first.turn_id, "user"),
        (&requests[2], &second.turn_id, "wakeup"),
    ] {
        let metadata: serde_json::Value = serde_json::from_str(
            &request
                .header("x-codex-turn-metadata")
                .expect("native request metadata"),
        )?;
        assert_eq!(metadata["turn_id"], *turn_id);
        assert_eq!(metadata["turn_trigger"], trigger);
    }
    let path = test.codex.rollout_path().expect("native rollout");
    test.codex.shutdown_and_wait().await?;
    let started = std::fs::read_to_string(path)?
        .lines()
        .map(codex_rollout::parse_rollout_line)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|line| match line.item {
            codex_history::RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => Some(event),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(started.len(), 2);
    for (event, turn_id, trigger) in [
        (&started[0], &first.turn_id, "user"),
        (&started[1], &second.turn_id, "wakeup"),
    ] {
        let attribution = event.turn_attribution.as_ref().expect("saved attribution");
        assert_eq!(&attribution.turn_id, turn_id);
        assert_eq!(attribution.turn_trigger.as_deref(), Some(trigger));
    }
    Ok(())
}
