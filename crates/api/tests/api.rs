//! The generated surfaces agree with each other: one record, two encodings;
//! one method table, three consumers.

use api::{
    Action, ActionRecord, ActionState, Empty, METHODS, Request, Response, RpcError, SendInput,
    Service, SplitDirection, SplitPane, SubmitAction, VersionInfo, action, dispatch,
};
use serde_json::json;

fn submit() -> SubmitAction {
    SubmitAction {
        action: Some(Action {
            kind: Some(action::Kind::SplitPane(SplitPane {
                pane: 3,
                direction: SplitDirection::Vertical as i32,
                percent: Some(30),
                cells: None,
                argv: vec!["htop".into(), "-d".into(), "5".into()],
                cwd: None,
            })),
        }),
        endpoint: None,
        idempotency_key: Some("k1".into()),
        deadline_ms: None,
    }
}

#[test]
fn one_record_round_trips_through_protobuf_and_json() {
    let req = Request::ActionsSubmit(submit());
    let bytes = req.encode();
    assert_eq!(
        Request::decode("actions.submit", &bytes).expect("proto"),
        req
    );
    let value = req.to_json();
    assert_eq!(
        value,
        json!({
            "action": { "kind": { "split_pane": {
                "pane": 3, "direction": "vertical", "percent": 30,
                "argv": ["htop", "-d", "5"]
            }}},
            "idempotency_key": "k1"
        })
    );
    assert_eq!(
        Request::from_json("actions.submit", value).expect("json"),
        req
    );
}

#[test]
fn json_spells_bytes_as_base64_and_accepts_enum_numbers() {
    let input = Request::ActionsSubmit(SubmitAction {
        action: Some(Action {
            kind: Some(action::Kind::SendInput(SendInput {
                pane: 1,
                data: b"ls\r\x1b[A".to_vec(),
                generation: Some(2),
                lease: None,
            })),
        }),
        ..SubmitAction::default()
    });
    let value = input.to_json();
    assert_eq!(value["action"]["kind"]["send_input"]["data"], "bHMNG1tB");
    assert_eq!(
        Request::from_json("actions.submit", value).expect("json"),
        input
    );

    // A newer peer may send a number this build has no name for.
    let rec: ActionRecord =
        serde_json::from_value(json!({ "id": "a", "state": 7 })).expect("number");
    assert_eq!(rec.state, ActionState::OutcomeUnknown as i32);
    assert_eq!(
        serde_json::to_value(&rec).expect("ser")["state"],
        "outcome_unknown"
    );
    let future: ActionRecord = serde_json::from_value(json!({ "state": 99 })).expect("future");
    assert_eq!(serde_json::to_value(&future).expect("ser")["state"], 99);
    assert!(serde_json::from_value::<ActionRecord>(json!({ "state": "nope" })).is_err());
}

#[test]
fn unknown_fields_and_methods_are_handled_explicitly() {
    // A newer client's extra field is ignored, not an error.
    let v: VersionInfo =
        serde_json::from_value(json!({ "protocol_major": 1, "from_the_future": true }))
            .expect("tolerant");
    assert_eq!(v.protocol_major, 1);
    let err = Request::decode("no.such", &[]).expect_err("unknown");
    assert_eq!(RpcError::from(err).code, api::code::METHOD_NOT_FOUND);
    let err = Request::decode("actions.submit", &[0xff, 0xff]).expect_err("garbage");
    assert_eq!(RpcError::from(err).code, api::code::INVALID_PARAMS);
}

struct Fixed;

impl Service for Fixed {
    async fn daemon_version(&self, _: Empty) -> Result<VersionInfo, RpcError> {
        Ok(VersionInfo {
            protocol_major: api::PROTOCOL_MAJOR,
            ..VersionInfo::default()
        })
    }
    async fn daemon_status(&self, _: Empty) -> Result<api::DaemonStatus, RpcError> {
        Err(RpcError::new(api::code::UNAVAILABLE, "test"))
    }
    async fn daemon_shutdown(&self, _: Empty) -> Result<api::ShutdownResult, RpcError> {
        Err(RpcError::new(api::code::UNAVAILABLE, "test"))
    }
    async fn tmux_topology(&self, _: api::TopologyParams) -> Result<api::Topology, RpcError> {
        Err(RpcError::new(api::code::UNAVAILABLE, "test"))
    }
    async fn tmux_size_window(&self, _: api::SizeWindow) -> Result<api::SizeResult, RpcError> {
        Err(RpcError::new(api::code::UNAVAILABLE, "test"))
    }
    async fn tmux_watch(&self, _: api::TopologyParams) -> Result<api::TopologyUpdate, RpcError> {
        Err(RpcError::new(api::code::UNAVAILABLE, "test"))
    }
    async fn actions_submit(&self, p: SubmitAction) -> Result<ActionRecord, RpcError> {
        Ok(ActionRecord {
            action: p.action,
            state: ActionState::Completed as i32,
            ..ActionRecord::default()
        })
    }
    async fn actions_get(&self, _: api::ActionQuery) -> Result<ActionRecord, RpcError> {
        Err(RpcError::new(api::code::NOT_FOUND, "test"))
    }
    async fn actions_list(&self, _: api::ActionListParams) -> Result<api::ActionList, RpcError> {
        Ok(api::ActionList::default())
    }
    async fn terminal_read(&self, _: api::ReadParams) -> Result<api::TerminalText, RpcError> {
        Err(RpcError::new(api::code::NOT_FOUND, "test"))
    }
    async fn terminal_mirrors(&self, _: api::TopologyParams) -> Result<api::MirrorList, RpcError> {
        Ok(api::MirrorList::default())
    }
    async fn terminal_set_tier(&self, _: api::SetTier) -> Result<Empty, RpcError> {
        Ok(Empty {})
    }
    async fn terminal_subscribe(
        &self,
        _: api::TerminalSubscribe,
    ) -> Result<api::Subscription, RpcError> {
        Err(RpcError::new(api::code::NOT_FOUND, "test"))
    }
    async fn terminal_unsubscribe(&self, _: api::Unsubscribe) -> Result<Empty, RpcError> {
        Ok(Empty {})
    }
    async fn terminal_input(&self, _: api::TerminalInput) -> Result<api::InputResult, RpcError> {
        Ok(api::InputResult::default())
    }
    async fn hosts_connect(&self, _: api::HostConnect) -> Result<api::HostStatus, RpcError> {
        Ok(api::HostStatus::default())
    }
    async fn hosts_list(&self, _: Empty) -> Result<api::HostList, RpcError> {
        Ok(api::HostList::default())
    }
    async fn hosts_disconnect(&self, _: api::HostRef) -> Result<api::HostStatus, RpcError> {
        Ok(api::HostStatus::default())
    }
    async fn hosts_reconnect(&self, _: api::HostRef) -> Result<api::HostStatus, RpcError> {
        Ok(api::HostStatus::default())
    }
    async fn hosts_subscribe(&self, _: api::HostSubscribe) -> Result<api::Subscription, RpcError> {
        Ok(api::Subscription::default())
    }
    async fn hosts_watch(&self, _: api::HostRef) -> Result<api::TopologyUpdate, RpcError> {
        Ok(api::TopologyUpdate::default())
    }
    async fn hosts_install_plan(
        &self,
        _: api::InstallPlanParams,
    ) -> Result<api::InstallPlan, RpcError> {
        Ok(api::InstallPlan::default())
    }
    async fn hosts_install(&self, _: api::InstallParams) -> Result<api::InstallResult, RpcError> {
        Ok(api::InstallResult::default())
    }
    async fn hosts_upgrade(&self, _: api::UpgradeParams) -> Result<api::UpgradeResult, RpcError> {
        Ok(api::UpgradeResult::default())
    }
    async fn apps_launch(&self, _: api::LaunchApp) -> Result<api::LaunchedApp, RpcError> {
        Ok(api::LaunchedApp::default())
    }
    async fn apps_register(&self, _: api::RegisterApp) -> Result<api::Registration, RpcError> {
        Ok(api::Registration::default())
    }
    async fn terminal_list(&self, _: api::TopologyParams) -> Result<api::TerminalList, RpcError> {
        Ok(api::TerminalList::default())
    }
    async fn terminal_watch(&self, _: api::TerminalWatch) -> Result<api::WatchResult, RpcError> {
        Ok(api::WatchResult::default())
    }
    async fn terminal_run(&self, _: api::TerminalRun) -> Result<api::RunResult, RpcError> {
        Ok(api::RunResult::default())
    }
    async fn clipboard_read_tmux_buffer(
        &self,
        _: api::TmuxBufferRead,
    ) -> Result<api::TmuxBuffer, RpcError> {
        Ok(api::TmuxBuffer::default())
    }
    async fn clipboard_write_tmux_buffer(
        &self,
        _: api::TmuxBufferWrite,
    ) -> Result<Empty, RpcError> {
        Ok(Empty {})
    }
    async fn approvals_list(&self, _: Empty) -> Result<api::ApprovalList, RpcError> {
        Ok(api::ApprovalList::default())
    }
    async fn approvals_answer(&self, _: api::ApprovalAnswer) -> Result<api::Approval, RpcError> {
        Ok(api::Approval::default())
    }
    async fn drawer_toggle(&self, _: api::DrawerRequest) -> Result<api::DrawerStatus, RpcError> {
        Ok(api::DrawerStatus::default())
    }
    async fn drawer_status(&self, _: api::DrawerRequest) -> Result<api::DrawerStatus, RpcError> {
        Ok(api::DrawerStatus::default())
    }
    async fn events_subscribe(
        &self,
        _: api::EventSubscribe,
    ) -> Result<api::EventSubscription, RpcError> {
        Ok(api::EventSubscription::default())
    }
    async fn events_unsubscribe(&self, _: api::Unsubscribe) -> Result<Empty, RpcError> {
        Ok(Empty {})
    }
    async fn messages_publish(&self, _: api::MessagePublish) -> Result<api::MessageSent, RpcError> {
        Ok(api::MessageSent::default())
    }
    async fn messages_request(
        &self,
        _: api::MessageRequest,
    ) -> Result<api::MessageEnvelope, RpcError> {
        Ok(api::MessageEnvelope::default())
    }
    async fn messages_reply(&self, _: api::MessageReply) -> Result<api::MessageSent, RpcError> {
        Ok(api::MessageSent::default())
    }
    async fn messages_subscribe(
        &self,
        _: api::MessageSubscribe,
    ) -> Result<api::MessageSubscription, RpcError> {
        Ok(api::MessageSubscription::default())
    }
    async fn messages_unsubscribe(&self, _: api::Unsubscribe) -> Result<Empty, RpcError> {
        Ok(Empty {})
    }
    async fn agents_event(&self, _: api::AgentEvent) -> Result<Empty, RpcError> {
        Ok(Empty {})
    }
    async fn agents_runs(&self, _: api::AgentRunQuery) -> Result<api::AgentRunList, RpcError> {
        Ok(api::AgentRunList::default())
    }
    async fn processes_list(&self, _: api::ProcessQuery) -> Result<api::ProcessList, RpcError> {
        Ok(api::ProcessList::default())
    }
    async fn jobs_start(&self, _: api::JobStart) -> Result<api::JobRecord, RpcError> {
        Ok(api::JobRecord::default())
    }
    async fn jobs_get(&self, _: api::JobRef) -> Result<api::JobRecord, RpcError> {
        Ok(api::JobRecord::default())
    }
    async fn jobs_list(&self, _: Empty) -> Result<api::JobList, RpcError> {
        Ok(api::JobList::default())
    }
    async fn jobs_cancel(&self, _: api::JobRef) -> Result<api::JobRecord, RpcError> {
        Ok(api::JobRecord::default())
    }
    async fn jobs_artifacts(&self, _: api::JobRef) -> Result<api::JobArtifacts, RpcError> {
        Ok(api::JobArtifacts::default())
    }
    async fn decorations_list(
        &self,
        _: api::DecorationQuery,
    ) -> Result<api::DecorationList, RpcError> {
        Ok(api::DecorationList::default())
    }
    async fn filters_install(&self, _: api::FilterSpec) -> Result<api::FilterStatus, RpcError> {
        Ok(api::FilterStatus::default())
    }
    async fn filters_list(&self, _: Empty) -> Result<api::FilterList, RpcError> {
        Ok(api::FilterList::default())
    }
    async fn filters_pause(&self, _: api::FilterRef) -> Result<api::FilterStatus, RpcError> {
        Ok(api::FilterStatus::default())
    }
    async fn filters_resume(&self, _: api::FilterRef) -> Result<api::FilterStatus, RpcError> {
        Ok(api::FilterStatus::default())
    }
    async fn filters_remove(&self, _: api::FilterRef) -> Result<api::FilterStatus, RpcError> {
        Ok(api::FilterStatus::default())
    }
    async fn terminal_query(&self, _: api::TerminalQuery) -> Result<api::QueryResult, RpcError> {
        Ok(api::QueryResult::default())
    }
    async fn taps_subscribe(&self, _: api::TapSubscribe) -> Result<api::TapSubscription, RpcError> {
        Ok(api::TapSubscription::default())
    }
    async fn taps_unsubscribe(&self, _: api::Unsubscribe) -> Result<Empty, RpcError> {
        Ok(Empty {})
    }
    async fn collect_create(
        &self,
        _: api::CollectorSpec,
    ) -> Result<api::CollectorStatus, RpcError> {
        Ok(api::CollectorStatus::default())
    }
    async fn collect_list(&self, _: Empty) -> Result<api::CollectList, RpcError> {
        Ok(api::CollectList::default())
    }
    async fn collect_query(&self, _: api::CollectQuery) -> Result<api::CollectResult, RpcError> {
        Ok(api::CollectResult::default())
    }
    async fn collect_stop(&self, _: api::CollectRef) -> Result<api::CollectorStatus, RpcError> {
        Ok(api::CollectorStatus::default())
    }
    async fn scripts_validate(&self, _: api::ScriptSource) -> Result<api::ScriptCheck, RpcError> {
        Ok(api::ScriptCheck::default())
    }
    async fn scripts_load(&self, _: api::ScriptLoad) -> Result<api::ScriptStatus, RpcError> {
        Ok(api::ScriptStatus::default())
    }
    async fn scripts_reload(&self, _: api::ScriptSource) -> Result<api::ScriptStatus, RpcError> {
        Ok(api::ScriptStatus::default())
    }
    async fn scripts_stop(&self, _: api::ScriptRef) -> Result<api::ScriptStatus, RpcError> {
        Ok(api::ScriptStatus::default())
    }
    async fn scripts_list(&self, _: Empty) -> Result<api::ScriptList, RpcError> {
        Ok(api::ScriptList::default())
    }
    async fn extensions_list(&self, _: Empty) -> Result<api::ExtensionList, RpcError> {
        Ok(api::ExtensionList::default())
    }
    async fn extensions_restart(
        &self,
        _: api::ExtensionRef,
    ) -> Result<api::ExtensionStatus, RpcError> {
        Ok(api::ExtensionStatus::default())
    }
    async fn grants_narrow(&self, g: api::Grant) -> Result<api::Grant, RpcError> {
        Ok(g)
    }
    async fn leases_acquire(&self, _: api::LeaseAcquire) -> Result<api::Lease, RpcError> {
        Ok(api::Lease::default())
    }
    async fn leases_release(&self, _: api::LeaseRef) -> Result<Empty, RpcError> {
        Ok(Empty::default())
    }
    async fn grants_current(&self, _: Empty) -> Result<api::Grant, RpcError> {
        Ok(api::Grant::default())
    }
    async fn hosts_call(&self, _: api::HostCall) -> Result<api::HostReply, RpcError> {
        Ok(api::HostReply::default())
    }
}

#[test]
fn every_method_in_the_table_dispatches_to_its_own_result() {
    let rt = futures_lite_block_on;
    for m in METHODS {
        let req = Request::from_json(m.name, json!({})).expect(m.name);
        assert_eq!(req.method(), m.name);
        match rt(dispatch(&Fixed, req)) {
            Ok(resp) => {
                assert_eq!(resp.method(), m.name);
                let bytes = resp.encode();
                assert_eq!(Response::decode(m.name, &bytes).expect("decode"), resp);
            }
            Err(e) => assert!(e.code < 0, "{e}"),
        }
    }
    assert!(
        METHODS
            .iter()
            .any(|m| m.name == "actions.submit" && m.mutation)
    );
    assert!(
        METHODS
            .iter()
            .any(|m| m.name == "tmux.topology" && !m.mutation)
    );
}

/// The dispatcher's futures here never pend, so polling once is enough.
fn futures_lite_block_on<F: std::future::Future>(f: F) -> F::Output {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let mut f = std::pin::pin!(f);
    match f.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => v,
        std::task::Poll::Pending => panic!("test service futures complete immediately"),
    }
}

#[test]
fn the_luau_module_routes_every_method_through_the_host_call() {
    let lua = mlua::Lua::new();
    lua.sandbox(true).expect("sandbox");
    let calls = lua.create_table().expect("table");
    let log = calls.clone();
    let call = lua
        .create_function(move |lua, (method, params): (String, mlua::Table)| {
            log.raw_push(method.clone())?;
            let out = lua.create_table()?;
            out.set("method", method)?;
            out.set("echo", params)?;
            Ok(out)
        })
        .expect("call");
    let factory: mlua::Function = lua
        .load(api::LUAU_MODULE)
        .set_name("dreamer.luau")
        .eval()
        .expect("the generated module compiles");
    let dreamer: mlua::Table = factory.call(call).expect("module");
    let script = r#"
        local dreamer = ...
        local r = dreamer.actions.submit({ action = { kind = { zoom_pane = { pane = 4 } } } })
        assert(r.method == "actions.submit")
        assert(r.echo.action.kind.zoom_pane.pane == 4)
        assert(dreamer.daemon.version().method == "daemon.version")
        assert(not pcall(function() dreamer.daemon.extra = 1 end), "service tables are frozen")
        assert(not pcall(function() dreamer.more = {} end), "the module is frozen")
    "#;
    lua.load(script)
        .call::<()>(dreamer.clone())
        .expect("script");
    let mut seen = Vec::new();
    for m in METHODS {
        let (service, name) = m.name.split_once('.').expect("service.name");
        let f: mlua::Function = dreamer
            .get::<mlua::Table>(service)
            .and_then(|t| t.get(name))
            .unwrap_or_else(|_| panic!("{} missing from the Luau module", m.name));
        let r: mlua::Table = f.call(()).expect("call");
        seen.push(r.get::<String>("method").expect("method"));
    }
    assert_eq!(seen, METHODS.iter().map(|m| m.name).collect::<Vec<_>>());
    assert_eq!(calls.raw_len(), METHODS.len() + 2);
}
