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
    assert_eq!(
        names,
        [
            "customers",
            "tasks",
            "notifications",
            "bookings",
            "invoicing",
            "audit",
            "changesets"
        ]
    );
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
        "invoice",
        "invoice_line",
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

#[test]
fn invoicing_keeps_totals_computed_and_checks_the_rest() {
    let m = compose(&shell(), &["invoicing"]).unwrap();
    let inv = &m["entities"]["invoice"]["invariants"];
    let names: Vec<&str> = inv
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["non_negative", "currency_code", "due_after_issue"]);
    assert_eq!(
        m["entities"]["invoice"]["fields"]["bill_to_email"]["pii"],
        true
    );
    // Totals are derived, never typed in: lines compute their total, the invoice sums them.
    let f = &m["entities"]["invoice"]["fields"];
    assert_eq!(
        f["subtotal"]["computed"],
        "sum(children(invoice_line, invoice).line_total)"
    );
    assert_eq!(f["total"]["computed"], "subtotal + tax");
    assert_eq!(
        m["entities"]["invoice_line"]["fields"]["line_total"]["computed"],
        "round(quantity * unit_price, 2)"
    );
}

#[test]
fn invoice_rules_are_enforced_by_the_checker() {
    let m = ce_manifest::validate(
        compose(&shell(), &["invoicing"])
            .unwrap()
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    let ctx = ce_validate::Context {
        actor_id: "u".into(),
        actor_role: None,
        now_unix: 0,
    };
    let run = |entity: &str, v: Value| {
        ce_validate::check_invariants(&m, entity, v.as_object().unwrap(), None, &ctx)
            .map_err(|e| e.into_iter().map(|x| x.detail).collect::<Vec<_>>())
    };
    let inv = |sub: &str, tax: &str, total: &str, due: &str| json!({"subtotal": sub, "tax": tax, "total": total, "currency": "USD", "issue_date": "2026-11-01", "due_date": due});
    assert_eq!(
        run("invoice", inv("100.00", "8.25", "108.25", "2026-12-01")),
        Ok(())
    );
    assert_eq!(
        run("invoice", inv("100.00", "8.25", "108.25", "2026-10-01")),
        Err(vec!["invoice.due_before_issue".to_string()])
    );
    let mut bad = inv("100.00", "8.25", "108.25", "2026-12-01");
    for code in ["", "US", "usd"] {
        bad["currency"] = json!(code);
        assert_eq!(
            run("invoice", bad.clone()),
            Err(vec!["invoice.bad_currency".to_string()]),
            "{code}"
        );
    }
    let line = |q: &str, p: &str| json!({"quantity": q, "unit_price": p});
    assert_eq!(run("invoice_line", line("2.5", "10.00")), Ok(()));
    assert_eq!(
        run("invoice_line", line("0", "10.00")),
        Err(vec!["invoice.bad_quantity".to_string()])
    );
}
