use ce_manifest::{validate, Manifest};
use ce_state::*;
use serde_json::Value;

fn load(name: &str) -> Manifest {
    let path = format!(
        "{}/../../spec/examples/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    validate(std::fs::read_to_string(path).unwrap().as_bytes()).expect("example is valid")
}

fn approval() -> Manifest {
    load("approval_workflow")
}

#[test]
fn machine_reports_initial_codes_and_graph() {
    let m = approval();
    let mc = Machine::new(&m, "request").unwrap();
    assert_eq!(mc.initial(), "submitted");
    assert_eq!(mc.code("in_review").unwrap(), 120);
    assert_eq!(
        mc.allowed_from("submitted").unwrap(),
        ["in_review", "withdrawn"]
    );
    assert!(mc.is_terminal("closed").unwrap());
    assert!(!mc.is_terminal("approved").unwrap());
    assert_eq!(mc.class("approved").unwrap(), Some(StateClass::Active));
    assert_eq!(mc.class("closed").unwrap(), Some(StateClass::Terminal));
    assert_eq!(mc.class("rejected").unwrap(), Some(StateClass::Failed));
}

#[test]
fn transition_checks_the_graph() {
    let m = approval();
    let mc = Machine::new(&m, "request").unwrap();
    let mv = mc.transition("submitted", "in_review").unwrap();
    assert_eq!((mv.from_code, mv.to_code), (100, 120));
    match mc.transition("submitted", "approved") {
        Err(StateError::InvalidTransition { allowed, .. }) => {
            assert_eq!(allowed, ["in_review", "withdrawn"])
        }
        other => panic!("{other:?}"),
    }
    assert!(mc.transition("closed", "submitted").is_err());
    assert_eq!(
        mc.transition("submitted", "nope"),
        Err(StateError::UnknownState("nope".into()))
    );
}

#[test]
fn unknown_entity_and_missing_state_model() {
    let m = approval();
    assert!(matches!(
        Machine::new(&m, "ghost"),
        Err(StateError::UnknownEntity(_))
    ));
    let inv = load("inventory");
    let entity = inv.value["entities"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, e)| e.get("state").is_none())
        .map(|(k, _)| k.clone());
    if let Some(name) = entity {
        assert!(matches!(
            Machine::new(&inv, &name),
            Err(StateError::NoStateModel(_))
        ));
    }
}

#[test]
fn plan_action_checks_required_state_and_collects_effects() {
    let m = approval();
    let p = plan_action(&m, "approve", "in_review").unwrap();
    assert_eq!(p.entity, "request");
    assert_eq!(p.moves.len(), 1);
    assert_eq!(
        (p.moves[0].from.as_str(), p.moves[0].to.as_str()),
        ("in_review", "approved")
    );
    assert_eq!(p.sets, [("decided_by".to_string(), "actor.id".to_string())]);
    assert_eq!(p.emits, ["request.approved"]);
    assert!(!p.is_async);

    match plan_action(&m, "approve", "submitted") {
        Err(StateError::WrongState {
            required, current, ..
        }) => {
            assert_eq!(required, ["in_review"]);
            assert_eq!(current, "submitted");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        plan_action(&m, "nope", "submitted"),
        Err(StateError::UnknownAction(_))
    ));
    assert!(matches!(
        plan_action(&m, "approve", "ghost"),
        Err(StateError::UnknownState(_))
    ));
}

fn edited(f: impl FnOnce(&mut Value)) -> Manifest {
    let mut v = approval().value.clone();
    f(&mut v);
    validate(serde_json::to_vec(&v).unwrap().as_slice()).expect("edited manifest is valid")
}

#[test]
fn action_that_jumps_outside_the_graph_is_refused() {
    // submitted -> approved is not in the graph, even though both states exist.
    let m = edited(|v| {
        v["actions"]["start_review"]["effect"] = serde_json::json!([{"transition": "approved"}]);
    });
    assert!(matches!(
        plan_action(&m, "start_review", "submitted"),
        Err(StateError::InvalidTransition { .. })
    ));
}

#[test]
fn multi_step_actions_chain_from_the_state_left_behind() {
    // submitted -> in_review -> approved is legal as two moves in one action.
    let m = edited(|v| {
        v["actions"]["start_review"]["effect"] =
            serde_json::json!([{"transition": "in_review"}, {"transition": "approved"}]);
    });
    let p = plan_action(&m, "start_review", "submitted").unwrap();
    assert_eq!(p.moves.len(), 2);
    assert_eq!(p.moves[1].from, "in_review");
    assert_eq!(p.moves[1].to_code, 200);
    // A chain that breaks midway fails as a whole.
    let m = edited(|v| {
        v["actions"]["start_review"]["effect"] =
            serde_json::json!([{"transition": "in_review"}, {"transition": "closed"}]);
    });
    assert!(plan_action(&m, "start_review", "submitted").is_err());
}

#[test]
fn state_class_covers_the_registry_ranges() {
    assert_eq!(StateClass::of(100), Some(StateClass::Pending));
    assert_eq!(StateClass::of(310), Some(StateClass::Limited));
    assert_eq!(StateClass::of(510), Some(StateClass::WindingDown));
    assert_eq!(StateClass::of(690), Some(StateClass::Terminal));
    assert_eq!(StateClass::of(50), None);
}
