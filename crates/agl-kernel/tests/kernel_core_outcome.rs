use agl_ids::{ExecutionScope, RunId, StepId};
use agl_kernel::{
    ExtensionDescriptor, ExtensionId, ExtensionRegistration, ExtensionSource, ExtensionTrust,
    ExtensionWorkflowFragment, OperationKind, ToolBinding, ToolDeclaration, ToolDelivery,
    ToolDispatchContext, ToolDispatchControl, ToolErrorDeclaration, ToolHandler, ToolHandlerError,
    ToolHandlerFuture, ToolId, ToolInvocation, ToolResult, ToolWorkflowMapping, WorkflowEventId,
};
use agl_kernel::{
    TOOL_OBSERVATION_APPEND_EVENT_ID, ToolAccessMode, ToolCatalog, ToolCatalogError,
    ToolDispatchError, ToolOutcomeStatus, ToolPolicyInput, ToolRuntime,
};
use schemars::JsonSchema;
use serde_json::{Value, json};

#[derive(JsonSchema)]
#[allow(dead_code)]
struct EchoArgs {
    value: String,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct ConflictData {
    expected: String,
    actual: String,
}

fn tool_id() -> ToolId {
    ToolId::new("example.outcome:echo").unwrap()
}

fn descriptor() -> ExtensionDescriptor {
    ExtensionDescriptor::new(
        ExtensionId::new("example.outcome").unwrap(),
        "Outcome fixture",
        "1.0.0",
        ExtensionSource::TestFixture,
        ExtensionTrust::TrustedRegistered,
    )
    .unwrap()
    .with_tool(
        ToolDeclaration::from_schema::<EchoArgs>(tool_id(), "Echo", OperationKind::Read).unwrap(),
    )
}

#[derive(Clone)]
struct ReturningHandler(Result<ToolResult, ToolHandlerError>);

impl ToolHandler for ReturningHandler {
    fn dispatch(&self, _context: ToolDispatchContext) -> ToolHandlerFuture<'_> {
        Box::pin(std::future::ready(self.0.clone()))
    }
}

fn dispatch(
    descriptor: ExtensionDescriptor,
    handler_result: Result<ToolResult, ToolHandlerError>,
) -> Result<agl_kernel::ToolOutcome, ToolDispatchError> {
    let effective =
        ToolPolicyInput::new([descriptor.clone()], [tool_id()], ToolAccessMode::ReadOnly)
            .resolve()
            .unwrap();
    let mut runtime = ToolRuntime::new();
    runtime
        .register_extension(ExtensionRegistration::new(
            descriptor.clone(),
            [ToolBinding::new(
                tool_id(),
                ReturningHandler(handler_result),
            )],
        ))
        .unwrap();
    runtime.dispatch(
        ToolInvocation::new(
            ExecutionScope::builder(
                RunId::parse("run_01890f17-4a00-7000-8000-000000000001").unwrap(),
            )
            .build()
            .unwrap(),
            tool_id(),
            descriptor.id.clone(),
            descriptor.tools[0].digest(),
            effective.policy_hash().clone(),
            json!({"value": "hello"}),
        ),
        &effective,
        ToolDispatchControl::uncancellable(),
    )
}

// Retained AGL-157 invariant. Mutation: serialize Tool data through display text.
#[test]
fn structured_tool_result_is_strict_and_canonical_without_text_conversion() {
    let left: Value =
        serde_json::from_str(r#"{"z":{"b":2,"a":1},"items":[{"d":4,"c":3}],"a":0}"#).unwrap();
    let right: Value =
        serde_json::from_str(r#"{"a":0,"items":[{"c":3,"d":4}],"z":{"a":1,"b":2}}"#).unwrap();
    let expected = r#"{"a":0,"items":[{"c":3,"d":4}],"z":{"a":1,"b":2}}"#;
    assert_eq!(ToolResult::new(left).render_observation(), expected);
    assert_eq!(ToolResult::new(right).render_observation(), expected);

    let result = ToolResult::new(json!({"status": "ok", "count": 2}));
    let encoded = serde_json::to_value(&result).unwrap();
    assert_eq!(
        serde_json::from_value::<ToolResult>(encoded).unwrap(),
        result
    );
    assert!(serde_json::from_value::<ToolResult>(json!({"data": {}, "unknown": true})).is_err());
}

// Retained AGL-157 invariant. Mutation: make a mutating delivery replay-safe or vary the key.
#[test]
fn delivery_and_run_step_idempotency_are_explicit_and_stable() {
    let read = descriptor().tools.remove(0);
    assert_eq!(read.delivery, ToolDelivery::ReplaySafe);

    let write = ToolDeclaration::from_schema::<EchoArgs>(
        ToolId::new("example.outcome:write").unwrap(),
        "Write",
        OperationKind::Write,
    )
    .unwrap()
    .with_state_effects([agl_kernel::EffectId::repo_files()]);
    assert_eq!(write.delivery, ToolDelivery::AtMostOnce);
    assert_eq!(
        write.clone().with_run_step_idempotency().delivery,
        ToolDelivery::IdempotentRunStep
    );
    assert!(read.with_run_step_idempotency().validate().is_err());

    let run_id = RunId::parse("run_01890f17-4a00-7000-8000-000000000001").unwrap();
    let step_id = StepId::parse("step_01890f17-4a00-7000-8000-000000000002").unwrap();
    let invocation = ToolInvocation::new(
        ExecutionScope::builder(run_id.clone())
            .step_id(step_id.clone())
            .build()
            .unwrap(),
        tool_id(),
        descriptor().id,
        descriptor().tools[0].digest(),
        ToolPolicyInput::new([descriptor()], [tool_id()], ToolAccessMode::ReadOnly)
            .resolve()
            .unwrap()
            .policy_hash()
            .clone(),
        json!({"value": "hello"}),
    );
    assert_eq!(
        invocation.run_step_idempotency_key().as_deref(),
        Some(format!("{run_id}:{step_id}").as_str())
    );
}

// Retained AGL-157 invariant. Mutation: accept an undeclared outcome or handler error.
#[test]
fn undeclared_outcome_and_error_codes_fail_closed() {
    assert!(matches!(
        dispatch(
            descriptor(),
            Ok(ToolResult::new(json!({})).with_outcome_code("partial")),
        ),
        Err(ToolDispatchError::UndeclaredOutcome { code, .. }) if code == "partial"
    ));
    assert!(matches!(
        dispatch(
            descriptor(),
            Err(ToolHandlerError::new("conflict", "stale", json!({}))),
        ),
        Err(ToolDispatchError::UndeclaredHandlerError { code, .. }) if code == "conflict"
    ));
}

// Retained AGL-157 invariant. Mutation: collapse recoverable errors into success or terminal failure.
#[test]
fn declared_recoverable_error_is_a_typed_tool_outcome() {
    let mut descriptor = descriptor();
    descriptor.tools[0] = descriptor.tools[0]
        .clone()
        .with_errors([
            ToolErrorDeclaration::recoverable("conflict").with_data_schema::<ConflictData>(),
            ToolErrorDeclaration::terminal("execution_failed"),
        ])
        .unwrap();
    let outcome = dispatch(
        descriptor,
        Err(ToolHandlerError::new(
            "conflict",
            "stale",
            json!({"expected": "one", "actual": "two"}),
        )),
    )
    .unwrap();
    assert_eq!(outcome.status, ToolOutcomeStatus::RecoverableError);
    assert_eq!(outcome.outcome_code, "conflict");
    assert_eq!(outcome.error.unwrap().code, "conflict");
}

// Retained AGL-157 invariant. Mutation: accept an undeclared runtime event mapping.
#[test]
fn workflow_mapping_accepts_only_declared_kernel_events() {
    let valid =
        descriptor().with_workflow(ExtensionWorkflowFragment::new([ToolWorkflowMapping::new(
            tool_id(),
            "success",
            WorkflowEventId::new(TOOL_OBSERVATION_APPEND_EVENT_ID).unwrap(),
        )]));
    let outcome = dispatch(valid, Ok(ToolResult::new(json!({"echo": "hello"})))).unwrap();
    assert_eq!(
        outcome.workflow_event.as_ref().map(WorkflowEventId::as_str),
        Some(TOOL_OBSERVATION_APPEND_EVENT_ID)
    );

    let invalid =
        descriptor().with_workflow(ExtensionWorkflowFragment::new([ToolWorkflowMapping::new(
            tool_id(),
            "success",
            WorkflowEventId::new("third-party:take_over_fsm").unwrap(),
        )]));
    assert!(matches!(
        ToolCatalog::new().register(invalid),
        Err(ToolCatalogError::UnknownWorkflowEvent { event_id })
            if event_id.as_str() == "third-party:take_over_fsm"
    ));
}
