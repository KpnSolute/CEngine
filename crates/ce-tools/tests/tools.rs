use ce_tools::{catalog, compose, ToolError};
use serde_json::{json, Value};

fn shell() -> Value {
    json!({
        "manifest_version": 1,
        "template": {"name": "shop", "version": "0.1.0", "title": "Shop"},
        "entities": {"widget": {"fields": {"name": {"type": "string"}}}},
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
    })
}

fn valid(v: &Value) {
    if let Err(e) = ce_manifest::validate(v.to_string().as_bytes()) {
        panic!("{e:#?}");
    }
}

#[test]
fn every_tool_composes_into_a_valid_manifest_alone_and_together() {
    let names: Vec<String> = catalog()
        .iter()
        .map(|t| t["tool"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["customers", "tasks", "notifications", "bookings"]);
    for n in &names {
        valid(&compose(&shell(), &[n.as_str()]).unwrap());
    }
    let all: Vec<&str> = names.iter().map(String::as_str).collect();
    let m = compose(&shell(), &all).unwrap();
    valid(&m);
    for e in [
        "widget",
        "customer",
        "customer_contact",
        "task",
        "notification",
    ] {
        assert!(m["entities"][e].is_object(), "{e}");
    }
    // Roles belong to the template.
    assert_eq!(m["roles"], shell()["roles"]);
}

#[test]
fn tools_work_on_a_template_with_no_entities_or_actions_yet() {
    let bare = json!({
        "manifest_version": 1,
        "template": {"name": "bare", "version": "0.1.0"},
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
    });
    valid(&compose(&bare, &["tasks"]).unwrap());
}

#[test]
fn composition_never_overrides_and_naming_twice_adds_once() {
    let mut t = shell();
    t["entities"]["task"] = json!({"fields": {"x": {"type": "string"}}});
    assert_eq!(
        compose(&t, &["tasks"]),
        Err(ToolError::Clash {
            tool: "tasks".into(),
            kind: "entity",
            name: "task".into()
        })
    );
    let mut t = shell();
    t["actions"] = json!({"start_task": {"entity": "widget", "effect": [{"emit": "x.y"}]}});
    assert!(matches!(
        compose(&t, &["tasks"]),
        Err(ToolError::Clash { kind: "action", .. })
    ));
    let once = compose(&shell(), &["tasks"]).unwrap();
    assert_eq!(compose(&shell(), &["tasks", "tasks"]).unwrap(), once);
    assert_eq!(
        compose(&shell(), &["nope"]),
        Err(ToolError::Unknown("nope".into()))
    );
    assert_eq!(compose(&json!([]), &["tasks"]), Err(ToolError::NotAnObject));
}

#[test]
fn personal_data_is_marked_and_tools_name_no_provider() {
    let m = compose(&shell(), &["customers", "notifications", "bookings"]).unwrap();
    for (e, f) in [
        ("customer", "name"),
        ("customer", "email"),
        ("customer", "phone"),
        ("customer", "address"),
        ("customer_contact", "email"),
        ("notification", "recipient"),
        ("booking", "contact_name"),
        ("booking", "contact_email"),
        ("booking", "contact_phone"),
    ] {
        assert_eq!(m["entities"][e]["fields"][f]["pii"], true, "{e}.{f}");
    }
    let text = serde_json::to_string(&catalog()).unwrap() + &m.to_string();
    for word in [
        "sendgrid", "twilio", "stripe", "password", "secret", "token",
    ] {
        assert!(!text.to_lowercase().contains(word), "{word}");
    }
}
