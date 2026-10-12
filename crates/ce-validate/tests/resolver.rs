use ce_expr::{Resolver, Value as Ev};
use ce_validate::{
    check_invariants, check_invariants_with, evaluate_computed, evaluate_computed_with,
    unenforced_invariants, Context,
};
use serde_json::{json, Map, Value};
use std::str::FromStr;

fn manifest() -> ce_manifest::Manifest {
    let m = json!({
        "manifest_version": 1,
        "template": {"name": "t", "version": "0.1.0", "title": "T"},
        "entities": {
            "invoice": {
                "fields": {
                    "limit": {"type": "decimal", "precision": 10, "scale": 2, "required": true},
                    "customer": {"type": "ref", "ref": "customer"},
                    "subtotal": {"type": "decimal", "precision": 10, "scale": 2, "computed": "sum(children(line, invoice).amount)"},
                    "lines": {"type": "integer", "computed": "count(children(line, invoice))"},
                    "doubled": {"type": "decimal", "precision": 10, "scale": 2, "computed": "subtotal * 2"}
                },
                "invariants": [
                    {"name": "within_limit", "expr": "subtotal <= limit", "message_code": "invoice.over_limit"},
                    {"name": "customer_open", "expr": "is_null(customer) or ref(customer).state == 'open'"}
                ]
            },
            "line": {"fields": {
                "invoice": {"type": "ref", "ref": "invoice", "required": true},
                "amount": {"type": "decimal", "precision": 10, "scale": 2, "required": true}
            }},
            "customer": {"fields": {"name": {"type": "string"}}}
        },
        "roles": {"admin": {"read": ["*"], "write": ["*"]}},
        "runtime": {"storage": {"relational": true}}
    });
    ce_manifest::validate(m.to_string().as_bytes()).unwrap()
}

struct Fake {
    amounts: Vec<&'static str>,
}

impl Resolver for Fake {
    fn ref_attr(&self, _: &str, _: &str, attr: &str) -> Result<Ev, String> {
        Ok(if attr == "state" {
            Ev::Str("open".into())
        } else {
            Ev::Null
        })
    }
    fn children(&self, entity: &str, field: &str, attr: Option<&str>) -> Result<Vec<Ev>, String> {
        assert_eq!((entity, field), ("line", "invoice"));
        Ok(match attr {
            Some("amount") => self
                .amounts
                .iter()
                .map(|a| Ev::Dec(bigdecimal::BigDecimal::from_str(a).unwrap()))
                .collect(),
            _ => vec![Ev::Bool(true); self.amounts.len()],
        })
    }
}

fn ctx() -> Context {
    Context {
        actor_id: "u".into(),
        actor_role: None,
        now_unix: 1_000,
    }
}

fn rec(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[test]
fn a_resolver_makes_sums_and_counts_computable() {
    let m = manifest();
    let mut r = rec(json!({"limit": "100.00"}));
    // Without a resolver the sum fields are left alone.
    evaluate_computed(&m, "invoice", &mut r, None, &ctx()).unwrap();
    assert!(!r.contains_key("subtotal") && !r.contains_key("doubled"));

    let fake = Fake {
        amounts: vec!["10.10", "20.205"],
    };
    evaluate_computed_with(&m, "invoice", &mut r, None, &ctx(), Some(&fake)).unwrap();
    assert_eq!(r["subtotal"], "30.31"); // half-up to the field scale
    assert_eq!(r["lines"], 2);
    // Fields that read a sum follow it.
    assert_eq!(r["doubled"], "60.62");
}

#[test]
fn a_resolver_turns_skipped_invariants_into_enforced_ones() {
    let m = manifest();
    assert_eq!(unenforced_invariants(&m).len(), 2);
    let mut ok = rec(json!({"limit": "100.00", "customer": "c1"}));
    let fake = Fake {
        amounts: vec!["60", "30"],
    };
    evaluate_computed_with(&m, "invoice", &mut ok, None, &ctx(), Some(&fake)).unwrap();
    check_invariants_with(&m, "invoice", &ok, None, &ctx(), Some(&fake)).unwrap();

    let mut over = rec(json!({"limit": "50.00"}));
    evaluate_computed_with(&m, "invoice", &mut over, None, &ctx(), Some(&fake)).unwrap();
    let errs = check_invariants_with(&m, "invoice", &over, None, &ctx(), Some(&fake)).unwrap_err();
    assert_eq!(errs.len(), 1);
    assert_eq!(
        (errs[0].code, errs[0].detail.as_str()),
        ("invariant_failed", "invoice.over_limit")
    );
    // The same record passes when no resolver is given: the rule is skipped, not failed.
    check_invariants(&m, "invoice", &over, None, &ctx()).unwrap();
}

#[test]
fn a_failing_lookup_fails_the_write_instead_of_passing_it() {
    struct Down;
    impl Resolver for Down {
        fn ref_attr(&self, _: &str, _: &str, _: &str) -> Result<Ev, String> {
            Err("down".into())
        }
        fn children(&self, _: &str, _: &str, _: Option<&str>) -> Result<Vec<Ev>, String> {
            Err("down".into())
        }
    }
    let m = manifest();
    let mut r = rec(json!({"limit": "100.00", "customer": "c1"}));
    let errs =
        evaluate_computed_with(&m, "invoice", &mut r, None, &ctx(), Some(&Down)).unwrap_err();
    assert!(errs.iter().all(|e| e.code == "computed_failed"));
    let mut r = rec(json!({"limit": "100.00", "customer": "c1", "subtotal": "1.00"}));
    assert!(check_invariants_with(&m, "invoice", &r, None, &ctx(), Some(&Down)).is_err());
    // A rule that needs no lookup is unaffected.
    r.remove("customer");
    check_invariants_with(&m, "invoice", &r, None, &ctx(), Some(&Down)).unwrap();
}

/// A state field the template does not declare in `fields` is still readable by expressions.
#[test]
fn an_undeclared_state_field_is_readable_in_an_effect_expression() {
    let m = ce_manifest::validate(
        &std::fs::read(format!(
            "{}/../../spec/examples/approval_workflow.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    )
    .unwrap();
    let rec = json!({"state": "in_review", "title": "x"});
    let ctx = Context {
        actor_id: "u".into(),
        actor_role: None,
        now_unix: 0,
    };
    let run = |src: &str| {
        ce_validate::eval_effect(
            &m,
            "request",
            rec.as_object().unwrap(),
            &ctx,
            &Map::new(),
            &Map::new(),
            &ce_expr::NoResolver,
            src,
            "t",
            &json!({"type": "boolean"}),
        )
    };
    assert_eq!(run("state == 'in_review'"), Ok(Some(json!(true))));
    assert_eq!(run("state == 'draft'"), Ok(Some(json!(false))));
}
