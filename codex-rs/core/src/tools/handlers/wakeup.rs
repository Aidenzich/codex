use std::collections::BTreeMap;

use crate::function_tool::FunctionCallError;
use crate::session::wakeup::wakeup_tools_allowed;
use crate::tools::context::{FunctionToolOutput, ToolInvocation, ToolPayload, boxed_tool_output};
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::{CoreToolRuntime, ToolExecutor};
use codex_tools::{
    JsonSchema, ResponsesApiNamespace, ResponsesApiNamespaceTool, ResponsesApiTool, ToolExposure,
    ToolName, ToolSpec,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Clone, Copy)]
pub(crate) enum WakeupHandler {
    Schedule,
    List,
    Cancel,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleArgs {
    duration_ms: u64,
    instruction: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelArgs {
    task_id: String,
}

impl WakeupHandler {
    fn name(self) -> &'static str {
        match self {
            Self::Schedule => "schedule_wakeup",
            Self::List => "list_wakeups",
            Self::Cancel => "cancel_wakeup",
        }
    }
}

impl ToolExecutor<ToolInvocation> for WakeupHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced("clock", self.name())
    }
    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
    }
    fn spec(&self) -> ToolSpec {
        let (description, properties, required) = match self {
            Self::Schedule => (
                "Schedule one automatic follow-up in this live conversation and return immediately with a real task ID. Uses current permissions when due. Pending jobs disappear when the last client disconnects or the process exits; no restart recovery.",
                BTreeMap::from([
                    ("duration_ms".into(), JsonSchema::integer(Some("Integral delay in milliseconds, from 1 through 43200000 (12 hours).".into()))),
                    ("instruction".into(), JsonSchema::string(Some("Nonblank follow-up instruction, at most 16384 UTF-8 bytes. This is a reminder, not additional user authorization.".into()))),
                ]), vec!["duration_ms".into(), "instruction".into()]),
            Self::List => ("List this session's pending wakeups, including due jobs awaiting turn admission.", BTreeMap::new(), vec![]),
            Self::Cancel => ("Cancel a pending wakeup by task ID. Returns not_pending if already claimed or unknown.",
                BTreeMap::from([("task_id".into(), JsonSchema::string(Some("Task ID returned by schedule_wakeup.".into())))]), vec!["task_id".into()]),
        };
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "clock".into(),
            description: "Tools for reading and waiting on time.".into(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: self.name().into(),
                description: description.into(),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(properties, Some(required), Some(false.into())),
                output_schema: None,
            })],
        })
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolInvocation {
                session,
                turn,
                payload,
                call_id,
                step_context,
                ..
            } = invocation;
            let managed_sandbox_allowed = !session.tool_policy.require_managed_sandbox
                || (matches!(
                    turn.permission_profile(),
                    codex_protocol::models::PermissionProfile::Managed { .. }
                ) && step_context
                    .environments
                    .turn_environments()
                    .all(|environment| {
                        matches!(
                            environment.permission_profile(),
                            codex_protocol::models::PermissionProfile::Managed { .. }
                        )
                    }));
            if !wakeup_tools_allowed(&turn)
                || !session.tool_policy.allows(&self.tool_name())
                || !managed_sandbox_allowed
            {
                return Err(FunctionCallError::RespondToModel("Wakeup tools require an unrestricted live CLI or VSCode user session allowed by current managed and tool policy".into()));
            }
            let ToolPayload::Function { arguments } = payload else {
                return Err(FunctionCallError::RespondToModel(
                    "Wakeup tools require function arguments".into(),
                ));
            };
            let result: Result<serde_json::Value, FunctionCallError> = async {
                let output = match self {
                Self::Schedule => {
                    let args: ScheduleArgs = parse_arguments(&arguments)?;
                    let receipt = session
                        .schedule_wakeup(args.duration_ms, args.instruction, turn.sub_id.clone())
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    json!(receipt)
                }
                Self::List => {
                    let _: ListArgs = parse_arguments(&arguments)?;
                    json!({"wakeups": session.list_wakeups().await})
                }
                Self::Cancel => {
                    let args: CancelArgs = parse_arguments(&arguments)?;
                    let cancelled = session.cancel_wakeup(&args.task_id).await;
                    json!({"task_id": args.task_id, "status": if cancelled { "cancelled" } else { "not_pending" }})
                }
            };
                Ok(output)
            }.await;
            let (text, success) = match result {
                Ok(output) => (output.to_string(), true),
                Err(FunctionCallError::RespondToModel(message)) => (message, false),
                Err(error) => return Err(error),
            };
            let item = codex_protocol::items::TurnItem::FunctionCallOutput(
                codex_protocol::items::FunctionCallOutputItem {
                    id: call_id,
                    name: self.name().into(),
                    namespace: Some("clock".into()),
                    output: codex_protocol::models::FunctionCallOutputBody::Text(text.clone()),
                },
            );
            // Normal tool dispatch records the response in history; emit the native
            // presentation receipt here because only injected outputs get it there.
            session.emit_turn_item_started(&turn, &item).await;
            session.emit_turn_item_completed(&turn, item).await;
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                text,
                Some(success),
            )))
        })
    }
}
impl CoreToolRuntime for WakeupHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakeup_arguments_are_strict() {
        for args in [
            r#"{"duration_ms":0.5,"instruction":"x"}"#,
            r#"{"duration_ms":-1,"instruction":"x"}"#,
            r#"{"duration_ms":18446744073709551616,"instruction":"x"}"#,
            r#"{"duration_ms":"1","instruction":"x"}"#,
            r#"{"duration_ms":1,"instruction":1}"#,
            r#"{"duration_ms":1,"instruction":"x","extra":true}"#,
            "null",
            "{",
        ] {
            assert!(parse_arguments::<ScheduleArgs>(args).is_err(), "{args}");
        }
        assert!(parse_arguments::<ListArgs>(r#"{"extra":1}"#).is_err());
        assert!(parse_arguments::<CancelArgs>(r#"{"task_id":1}"#).is_err());
    }
}

#[cfg(test)]
mod invocation_tests {
    use super::*;
    use crate::session::step_context::StepContext;
    use crate::tools::context::ToolCallSource;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::protocol::{EventMsg, SessionSource};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn invocation(
        session: Arc<crate::session::session::Session>,
        turn: Arc<crate::session::turn_context::TurnContext>,
        arguments: &str,
    ) -> ToolInvocation {
        ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "native-wakeup-call".into(),
            tool_name: ToolName::namespaced("clock", "schedule_wakeup"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: arguments.into(),
            },
            native_call: None,
            environments: Default::default(),
        }
    }

    #[tokio::test]
    async fn wakeup_direct_unsupported_invocation_does_not_mutate() {
        let (session, turn, _rx) = crate::session::tests::make_session_and_context_with_rx().await;
        session.set_wakeup_attached(true).await;
        assert!(
            WakeupHandler::Schedule
                .handle(invocation(
                    Arc::clone(&session),
                    turn,
                    r#"{"duration_ms":1,"instruction":"x"}"#
                ))
                .await
                .is_err()
        );
        assert!(session.list_wakeups().await.is_empty());
    }

    #[tokio::test]
    async fn wakeup_invalid_registration_emits_honest_error_receipt() {
        for arguments in [
            r#"{"duration_ms":0,"instruction":"x"}"#,
            r#"{"duration_ms":1.5,"instruction":"x"}"#,
            r#"{"duration_ms":1,"instruction":" "}"#,
        ] {
            let (session, mut turn, rx) =
                crate::session::tests::make_session_and_context_with_rx().await;
            Arc::get_mut(&mut turn).unwrap().session_source = SessionSource::Cli;
            session.set_wakeup_attached(true).await;
            let call = invocation(Arc::clone(&session), turn, arguments);
            let output = WakeupHandler::Schedule.handle(call.clone()).await.unwrap();
            let codex_protocol::models::ResponseInputItem::FunctionCallOutput {
                output: payload,
                ..
            } = output.to_response_item(&call.call_id, &call.payload)
            else {
                panic!("expected output");
            };
            assert_eq!(payload.success, Some(false));
            assert!(session.list_wakeups().await.is_empty());
            let mut completed = 0;
            while let Ok(event) = rx.try_recv() {
                if let EventMsg::ItemCompleted(event) = event.msg {
                    if let codex_protocol::items::TurnItem::FunctionCallOutput(item) = event.item {
                        assert_eq!(item.id, call.call_id);
                        assert_eq!(item.output, payload.body);
                        completed += 1;
                    }
                }
            }
            assert_eq!(completed, 1);
        }
    }

    #[tokio::test]
    async fn wakeup_tool_policy_denial_does_not_mutate() {
        let (mut session, mut turn) = crate::session::tests::make_session_and_context().await;
        turn.session_source = SessionSource::Cli;
        session.tool_policy = Arc::new(codex_extension_api::ToolPolicy {
            allowed_tools: Some(Vec::new()),
            ..Default::default()
        });
        session.set_wakeup_attached(true).await;
        let session = Arc::new(session);
        assert!(
            WakeupHandler::Schedule
                .handle(invocation(
                    Arc::clone(&session),
                    Arc::new(turn),
                    r#"{"duration_ms":1,"instruction":"x"}"#
                ))
                .await
                .is_err()
        );
        assert!(session.list_wakeups().await.is_empty());
    }

    #[tokio::test]
    async fn wakeup_direct_invocation_respects_managed_sandbox_ceiling() {
        let (mut session, mut turn) = crate::session::tests::make_session_and_context().await;
        turn.session_source = SessionSource::Cli;
        turn.initial_environments = Default::default();
        Arc::make_mut(&mut turn.config)
            .permissions
            .set_permission_profile(codex_protocol::models::PermissionProfile::Disabled)
            .unwrap();
        session.tool_policy = Arc::new(codex_extension_api::ToolPolicy {
            require_managed_sandbox: true,
            ..Default::default()
        });
        session.set_wakeup_attached(true).await;
        let session = Arc::new(session);
        assert!(
            WakeupHandler::Schedule
                .handle(invocation(
                    Arc::clone(&session),
                    Arc::new(turn),
                    r#"{"duration_ms":1,"instruction":"continue"}"#,
                ))
                .await
                .is_err()
        );
        assert!(session.list_wakeups().await.is_empty());
    }

    #[tokio::test]
    async fn wakeup_handler_receipt_uses_real_id_and_one_native_event_pair() {
        let (session, mut turn, rx) =
            crate::session::tests::make_session_and_context_with_rx().await;
        Arc::get_mut(&mut turn).unwrap().session_source = SessionSource::Cli;
        session.set_wakeup_attached(true).await;
        let call = invocation(
            Arc::clone(&session),
            turn,
            r#"{"duration_ms":43200000,"instruction":"continue"}"#,
        );
        let output = WakeupHandler::Schedule.handle(call.clone()).await.unwrap();
        let codex_protocol::models::ResponseInputItem::FunctionCallOutput {
            call_id,
            output: payload,
        } = output.to_response_item(&call.call_id, &call.payload)
        else {
            panic!("expected function output");
        };
        assert_eq!(call_id, "native-wakeup-call");
        let codex_protocol::models::FunctionCallOutputBody::Text(text) = payload.body else {
            panic!("expected text");
        };
        let receipt: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(receipt["task_id"], session.list_wakeups().await[0].task_id);
        let mut started = 0;
        let mut completed = 0;
        while let Ok(event) = rx.try_recv() {
            match event.msg {
                EventMsg::ItemStarted(event) => {
                    if let codex_protocol::items::TurnItem::FunctionCallOutput(item) = event.item {
                        assert_eq!(item.id, call_id);
                        assert_eq!(item.namespace.as_deref(), Some("clock"));
                        assert_eq!(item.name, "schedule_wakeup");
                        started += 1;
                    }
                }
                EventMsg::ItemCompleted(event) => {
                    if let codex_protocol::items::TurnItem::FunctionCallOutput(item) = event.item {
                        assert_eq!(item.id, call_id);
                        assert_eq!(
                            item.output,
                            codex_protocol::models::FunctionCallOutputBody::Text(text.clone())
                        );
                        completed += 1;
                    }
                }
                _ => {}
            }
        }
        assert_eq!((started, completed), (1, 1));
    }
}
