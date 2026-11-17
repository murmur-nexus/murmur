use serde_json::json;

use super::*;

fn contracts_of(wasm: &str) -> murmur_artifact::WitContracts {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../murmur-cli/tests/fixtures/bus-probe/tool")
        .join(wasm);
    murmur_artifact::extract_wit_contracts(&std::fs::read(path).unwrap())
        .unwrap()
        .expect("a component")
}

#[test]
fn bus_repl_imports_the_bus_and_bus_echo_does_not() {
    assert!(contracts_of("bus-repl.wasm")
        .imports
        .contains(&BUS_IFACE.to_string()));
    assert!(imports_bus(&contracts_of("bus-repl.wasm").imports));
    assert!(!imports_bus(&contracts_of("bus-echo.wasm").imports));
}

#[test]
fn imports_bus_matches_the_package_at_any_version() {
    let names = |name: &str| vec![name.to_string()];
    assert!(imports_bus(&names("murmur:bus/bus@0.1.0")));
    assert!(imports_bus(&names("murmur:bus/bus@9.9.9")));
    assert!(imports_bus(&names("murmur:bus/other")));
    assert!(!imports_bus(&names("murmur:bush/bus@0.1.0")));
    assert!(!imports_bus(&names("murmur:process/process@0.2.0")));
}

/// The staging arms' check: a bus-importing tool not named by `code_mode.repl`, a driver, a hook
/// and a script capsule root (each of which passes no `repl`) are refused; the named tool passes.
#[test]
fn bus_import_check_refuses_every_artifact_but_the_named_tool() {
    let imports = contracts_of("bus-repl.wasm").imports;
    let refused =
        |artifact: &str, repl: Option<&str>| match refuse_bus_import(artifact, &imports, repl) {
            Err(crate::errors::RuntimeError::BusImportRefused { artifact: named }) => named,
            other => panic!("{artifact}: {other:?}"),
        };
    assert_eq!(
        refused("local/bus-repl-copy", Some("local/bus-repl")),
        "local/bus-repl-copy"
    );
    assert_eq!(refused("local/bus-repl", None), "local/bus-repl");
    for arm in ["local/a-driver", "local/a-hook", "local/script-root"] {
        assert_eq!(refused(arm, None), arm);
    }
    assert!(refuse_bus_import("local/bus-repl", &imports, Some("local/bus-repl")).is_ok());
    let echo = contracts_of("bus-echo.wasm").imports;
    assert!(refuse_bus_import("local/bus-echo", &echo, None).is_ok());
    assert_eq!(
        crate::errors::RuntimeError::BusImportRefused {
            artifact: "local/x".to_string()
        }
        .to_string(),
        "artifact 'local/x' imports murmur:bus but is not the artifact code_mode.repl names"
    );
}

#[test]
fn operations_leave_out_the_code_mode_tool_and_the_unserved_tools() {
    let inventory = vec![
        json!({"name": "bus-echo", "description": "Echoes.", "parameters": {"type": "object"}}),
        json!({"name": "bus-repl", "description": "The REPL.", "parameters": {}}),
        json!({"name": "pull-artifact", "parameters": {}}),
        json!({"name": "remove-artifact", "parameters": {}}),
        json!({"name": "session-status", "parameters": {"type": "object"}}),
        json!({"name": "submit-plan", "parameters": {}}),
    ];
    let operations = operations(&inventory, "bus-repl");
    let names: Vec<&str> = operations.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["bus-echo", "session-status"]);
    assert_eq!(operations[0].description, "Echoes.");
    assert_eq!(operations[0].input_schema, r#"{"type":"object"}"#);
    assert_eq!(
        operations[1].description, "",
        "no description is an empty string"
    );
}

#[test]
fn refusal_texts_are_the_documented_sentences() {
    assert_eq!(
        not_listed_text("nope"),
        "'nope' is not an operation this program may call; list() returns every operation it \
         may call"
    );
    assert_eq!(
        code_mode_tool_text("bus-repl"),
        "'bus-repl' is the code mode tool this program runs in, which a program cannot call"
    );
    assert_eq!(
        not_served_text("submit-plan"),
        "'submit-plan' is not served to a code mode program"
    );
    let error = serde_json::from_str::<serde_json::Value>("{not json").unwrap_err();
    assert!(input_not_json_text("bus-echo", &error)
        .starts_with("the input for 'bus-echo' is not JSON: "));
    let detached = OutsideFrame::detached("bus-repl", "wrk_abc");
    assert_eq!(detached.reason, refusal::DETACHED);
    assert_eq!(
        detached.call_text("bus-echo"),
        "'bus-echo' was not called: the 'bus-repl' call this program runs in was moved to the \
         background as wrk_abc, and the bus answers only while that call is in the foreground"
    );
}

/// The plan step's and the process transport's wording, and `list`'s, which drops the leading
/// `'{name}' was not called: `.
#[test]
fn not_answered_names_the_call_it_is() {
    let plan = OutsideFrame::unanswered("bus-repl", Unanswered::PlanStep);
    assert_eq!(plan.reason, refusal::NOT_ANSWERED);
    assert_eq!(
        plan.call_text("bus-echo"),
        "'bus-echo' was not called: the bus answers only while the model's own 'bus-repl' call \
         is in progress on inference.transport: http, and this 'bus-repl' call is a plan step's"
    );
    let bridged = OutsideFrame::unanswered("bus-repl", Unanswered::ProcessTransport);
    assert_eq!(
        bridged.call_text("bus-echo"),
        "'bus-echo' was not called: the bus answers only while the model's own 'bus-repl' call \
         is in progress on inference.transport: http, and this 'bus-repl' call is made through a \
         process-transport harness"
    );
    assert_eq!(
        bridged.tail,
        "the bus answers only while the model's own 'bus-repl' call is in progress on \
         inference.transport: http, and this 'bus-repl' call is made through a process-transport \
         harness"
    );
}

/// A link with no code mode call behind it — every call's but the code mode instance's — answers
/// every function with a refusal and carries no frame.
#[tokio::test]
async fn a_link_without_a_code_mode_call_refuses_everything() {
    let link = BusLink::none();
    assert!(!link.has_frame());
    assert!(matches!(
        link.answer(BusCall::List).await,
        BusReply::List(Err(_))
    ));
    match link
        .answer(BusCall::Batch(vec![
            CallRequest {
                name: "a".to_string(),
                input: "{}".to_string(),
            },
            CallRequest {
                name: "b".to_string(),
                input: "{}".to_string(),
            },
        ]))
        .await
    {
        BusReply::Batch(answers) => {
            assert_eq!(answers.len(), 2);
            assert!(answers
                .iter()
                .all(|answer| matches!(answer, Err(CallError::Refused(_)))));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_call_origin_names_its_code_mode_call() {
    assert_eq!(CallOrigin::Model.repl_call_id(), None);
    assert_eq!(
        CallOrigin::CodeMode {
            repl_call_id: Some("toolu_1".to_string())
        }
        .repl_call_id(),
        Some("toolu_1")
    );
}
