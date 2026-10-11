use crate::{issue, Issue};
use ce_expr::{parse, Expr};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

type Obj = Map<String, Value>;

const ROOTS: [&str; 4] = ["old", "actor", "now", "today"];
const FUNCS: [&str; 17] = [
    "round",
    "floor",
    "ceil",
    "abs",
    "min",
    "max",
    "coalesce",
    "when",
    "len",
    "lower",
    "upper",
    "trim",
    "concat",
    "starts_with",
    "contains",
    "is_null",
    "ref",
];
const AGGS: [&str; 3] = ["sum", "count", "children"];

#[derive(Clone, Copy, PartialEq)]
enum Site {
    Computed,
    Plain,
    Filter,
}

fn obj<'a>(v: &'a Value, k: &str) -> Option<&'a Obj> {
    v.get(k).and_then(Value::as_object)
}

pub fn check(m: &Value) -> Vec<Issue> {
    let mut out = Vec::new();
    let empty = Obj::new();
    let entities = obj(m, "entities").unwrap_or(&empty);
    let actions = obj(m, "actions").unwrap_or(&empty);

    for (en, ent) in entities {
        let base = format!("/entities/{en}");
        let fields = obj(ent, "fields").unwrap_or(&empty);
        let state = ent.get("state");
        let state_field = state.and_then(|s| s["field"].as_str());
        let mut names: BTreeSet<&str> = fields.keys().map(String::as_str).collect();
        if let Some(sf) = state_field {
            names.insert(sf);
        }

        for (fname, f) in fields {
            let fp = format!("{base}/fields/{fname}");
            if let Some(target) = f.get("ref").and_then(Value::as_str) {
                if !entities.contains_key(target) {
                    out.push(issue(
                        format!("{fp}/ref"),
                        "unknown_entity",
                        format!("no entity `{target}`"),
                    ));
                }
            }
            if let Some(e) = f.get("computed").and_then(Value::as_str) {
                if let Ok(ast) = parse(e) {
                    check_children(&mut out, &format!("{fp}/computed"), &ast, entities, en);
                }
                expr(
                    &mut out,
                    &format!("{fp}/computed"),
                    e,
                    &names,
                    Site::Computed,
                );
            }
        }
        for (i, inv) in ent
            .get("invariants")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if let Some(e) = inv["expr"].as_str() {
                expr(
                    &mut out,
                    &format!("{base}/invariants/{i}/expr"),
                    e,
                    &names,
                    Site::Plain,
                );
            }
        }
        if let Some(e) = ent.get("locked_when").and_then(Value::as_str) {
            expr(
                &mut out,
                &format!("{base}/locked_when"),
                e,
                &names,
                Site::Plain,
            );
        }
        for (i, k) in ent
            .get("sort_keys")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let k = k.as_str().unwrap_or("");
            match fields.get(k) {
                None => out.push(issue(
                    format!("{base}/sort_keys/{i}"),
                    "unknown_field",
                    format!("no field `{k}`"),
                )),
                Some(f) if f["indexed"] != true && f["unique"] != true => out.push(issue(
                    format!("{base}/sort_keys/{i}"),
                    "unindexed_sort_key",
                    format!("sort key `{k}` must be indexed or unique"),
                )),
                _ => {}
            }
        }
        if let Some(s) = state {
            state_checks(&mut out, &format!("{base}/state"), s, fields);
        }
        for (i, w) in ent
            .get("freeze_windows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let sp = format!("{base}/freeze_windows/{i}");
            if let Some(f) = w["field"].as_str() {
                if !names.contains(f) {
                    out.push(issue(
                        format!("{sp}/field"),
                        "unknown_field",
                        format!("no field `{f}`"),
                    ));
                }
            }
        }
    }

    for (an, a) in actions {
        let ap = format!("/actions/{an}");
        let Some(en) = a["entity"].as_str() else {
            continue;
        };
        let Some(ent) = entities.get(en) else {
            out.push(issue(
                format!("{ap}/entity"),
                "unknown_entity",
                format!("no entity `{en}`"),
            ));
            continue;
        };
        let fields = obj(ent, "fields").unwrap_or(&empty);
        let state_names: BTreeSet<&str> = ent
            .get("state")
            .and_then(|s| obj(s, "states"))
            .map(|s| s.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let mut names: BTreeSet<&str> = fields.keys().map(String::as_str).collect();
        if let Some(sf) = ent.get("state").and_then(|s| s["field"].as_str()) {
            names.insert(sf);
        }
        for (i, s) in a
            .get("requires_state")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let s = s.as_str().unwrap_or("");
            if !state_names.contains(s) {
                out.push(issue(
                    format!("{ap}/requires_state/{i}"),
                    "unknown_state",
                    format!("`{en}` has no state `{s}`"),
                ));
            }
        }
        for (i, eff) in a
            .get("effect")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let ep = format!("{ap}/effect/{i}");
            if let Some(t) = eff.get("transition").and_then(Value::as_str) {
                if !state_names.contains(t) {
                    out.push(issue(
                        format!("{ep}/transition"),
                        "unknown_state",
                        format!("`{en}` has no state `{t}`"),
                    ));
                }
            }
            if let Some(set) = obj(eff, "set") {
                for (k, v) in set {
                    if !names.contains(k.as_str()) {
                        out.push(issue(
                            format!("{ep}/set/{k}"),
                            "unknown_field",
                            format!("no field `{k}`"),
                        ));
                    }
                    if let Some(e) = v.as_str() {
                        expr(&mut out, &format!("{ep}/set/{k}"), e, &names, Site::Plain);
                    }
                }
            }
            if let Some(c) = obj(eff, "create") {
                match c.get("entity").and_then(Value::as_str) {
                    Some(t) if !entities.contains_key(t) => out.push(issue(
                        format!("{ep}/create/entity"),
                        "unknown_entity",
                        format!("no entity `{t}`"),
                    )),
                    _ => {}
                }
            }
        }
    }

    for (pn, p) in obj(m, "projections").unwrap_or(&empty) {
        let pp = format!("/projections/{pn}");
        let Some(en) = p["entity"].as_str() else {
            continue;
        };
        let Some(ent) = entities.get(en) else {
            out.push(issue(
                format!("{pp}/entity"),
                "unknown_entity",
                format!("no entity `{en}`"),
            ));
            continue;
        };
        let fields = obj(ent, "fields").unwrap_or(&empty);
        let mut names: BTreeSet<&str> = fields.keys().map(String::as_str).collect();
        if let Some(sf) = ent.get("state").and_then(|s| s["field"].as_str()) {
            names.insert(sf);
        }
        for (i, f) in p["fields"].as_array().into_iter().flatten().enumerate() {
            let f = f.as_str().unwrap_or("");
            if !names.contains(f) {
                out.push(issue(
                    format!("{pp}/fields/{i}"),
                    "unknown_field",
                    format!("no field `{f}`"),
                ));
            } else if fields.get(f).is_some_and(|x| x["pii"] == true) {
                out.push(issue(
                    format!("{pp}/fields/{i}"),
                    "pii_in_public_projection",
                    format!("`{f}` is marked pii"),
                ));
            }
        }
        if let Some(e) = p.get("filter").and_then(Value::as_str) {
            expr(&mut out, &format!("{pp}/filter"), e, &names, Site::Filter);
        }
    }

    for (rn, r) in obj(m, "roles").unwrap_or(&empty) {
        for key in ["read", "write"] {
            for (i, e) in r[key].as_array().into_iter().flatten().enumerate() {
                let e = e.as_str().unwrap_or("");
                if e != "*" && !entities.contains_key(e) {
                    out.push(issue(
                        format!("/roles/{rn}/{key}/{i}"),
                        "unknown_entity",
                        format!("no entity `{e}`"),
                    ));
                }
            }
        }
        for (i, a) in r["actions"].as_array().into_iter().flatten().enumerate() {
            let a = a.as_str().unwrap_or("");
            if a != "*" && !actions.contains_key(a) {
                out.push(issue(
                    format!("/roles/{rn}/actions/{i}"),
                    "unknown_action",
                    format!("no action `{a}`"),
                ));
            }
        }
    }

    for (i, s) in m["runtime"]["compute"]["schedules"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let a = s["action"].as_str().unwrap_or("");
        if !actions.contains_key(a) {
            out.push(issue(
                format!("/runtime/compute/schedules/{i}/action"),
                "unknown_action",
                format!("no action `{a}`"),
            ));
        }
    }

    cycles(&mut out, entities);
    out
}

fn state_checks(out: &mut Vec<Issue>, p: &str, s: &Value, fields: &Obj) {
    let states = obj(s, "states").cloned().unwrap_or_default();
    let initial = s["initial"].as_str().unwrap_or("");
    if !states.contains_key(initial) {
        out.push(issue(
            format!("{p}/initial"),
            "unknown_state",
            format!("initial `{initial}` is not a state"),
        ));
    }
    let mut seen = BTreeSet::new();
    for (name, st) in &states {
        if let Some(c) = st["code"].as_i64() {
            if !seen.insert(c) {
                out.push(issue(
                    format!("{p}/states/{name}/code"),
                    "duplicate_state_code",
                    format!("code {c} reused"),
                ));
            }
        }
        for (i, t) in st["to"].as_array().into_iter().flatten().enumerate() {
            let t = t.as_str().unwrap_or("");
            if !states.contains_key(t) {
                out.push(issue(
                    format!("{p}/states/{name}/to/{i}"),
                    "unknown_state",
                    format!("no state `{t}`"),
                ));
            }
        }
    }
    // A declared state field must be an enum whose values match the state names.
    if let Some(f) = s["field"].as_str().and_then(|n| fields.get(n)) {
        let vals: BTreeSet<&str> = f["values"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let names: BTreeSet<&str> = states.keys().map(String::as_str).collect();
        if f["type"] != "enum" || vals != names {
            out.push(issue(
                format!("{p}/field"),
                "state_field_mismatch",
                "declared state field must be an enum whose values equal the state names",
            ));
        }
    }
}

fn expr(out: &mut Vec<Issue>, path: &str, src: &str, names: &BTreeSet<&str>, site: Site) {
    match parse(src) {
        Err(e) => out.push(issue(path, "expr_parse", format!("{e:?}"))),
        Ok(ast) => walk(out, path, &ast, names, site),
    }
}

fn walk(out: &mut Vec<Issue>, p: &str, e: &Expr, names: &BTreeSet<&str>, site: Site) {
    match e {
        Expr::Path(parts) => {
            let root = parts[0].as_str();
            if !names.contains(root) && !ROOTS.contains(&root) {
                out.push(issue(
                    p,
                    "unknown_identifier",
                    format!("`{root}` is not a field or binding"),
                ));
            }
        }
        Expr::Call(f, args) => {
            let f = f.as_str();
            if AGGS.contains(&f) {
                if site != Site::Computed {
                    out.push(issue(
                        p,
                        "forbidden_function",
                        format!("`{f}` is only allowed in computed"),
                    ));
                }
            } else if !FUNCS.contains(&f) {
                out.push(issue(p, "unknown_function", format!("no function `{f}`")));
            } else if f == "ref" && site == Site::Filter {
                out.push(issue(
                    p,
                    "forbidden_function",
                    "`ref` is not allowed in a projection filter",
                ));
            }
            // `children(entity, field)` names an entity and a field of it, not fields of this
            // entity; `check_children` validates those.
            if f != "children" {
                for a in args {
                    walk(out, p, a, names, site);
                }
            }
        }
        Expr::Field(b, _) | Expr::Neg(b) | Expr::Not(b) => walk(out, p, b, names, site),
        Expr::Bin(_, a, b) => {
            walk(out, p, a, names, site);
            walk(out, p, b, names, site);
        }
        Expr::List(v) => v.iter().for_each(|x| walk(out, p, x, names, site)),
        _ => {}
    }
}

fn is_children_chain(e: &Expr) -> bool {
    match e {
        Expr::Call(f, _) => f == "children",
        Expr::Field(b, _) => is_children_chain(b),
        _ => false,
    }
}

/// `children(entity, ref_field)`: the entity must exist and the field must be a `ref` from it
/// back to the entity that holds the computed field.
fn check_children(out: &mut Vec<Issue>, path: &str, e: &Expr, entities: &Obj, owner: &str) {
    match e {
        Expr::Call(f, args) => {
            if f == "children" {
                let names: Vec<&str> = args
                    .iter()
                    .filter_map(|a| match a {
                        Expr::Path(p) if p.len() == 1 => Some(p[0].as_str()),
                        _ => None,
                    })
                    .collect();
                let [child, field] = names[..] else {
                    out.push(issue(
                        path,
                        "bad_children",
                        "children(entity, ref_field) takes two plain names",
                    ));
                    return;
                };
                let Some(ce) = entities.get(child) else {
                    out.push(issue(
                        path,
                        "unknown_entity",
                        format!("no entity `{child}`"),
                    ));
                    return;
                };
                let target = ce["fields"][field]["ref"].as_str();
                if ce["fields"][field]["type"] != "ref" || target != Some(owner) {
                    out.push(issue(
                        path,
                        "bad_children",
                        format!("`{child}.{field}` must be a ref to `{owner}`"),
                    ));
                }
                return;
            }
            args.iter()
                .for_each(|a| check_children(out, path, a, entities, owner));
        }
        Expr::List(v) => v
            .iter()
            .for_each(|a| check_children(out, path, a, entities, owner)),
        Expr::Field(b, name) => {
            // Only one field can be read off the child rows: `children(a, b).value.ghost` has
            // nothing to read from `value`.
            if let Expr::Field(inner, _) = &**b {
                if is_children_chain(inner) {
                    out.push(issue(
                        path,
                        "bad_children",
                        format!("`.{name}` follows a field of the child rows; only one field can be read"),
                    ));
                }
            }
            // `children(line, invoice).line_total` reads a field of the child rows.
            if let Expr::Call(f, args) = &**b {
                if let (true, Some(Expr::Path(p))) = (f == "children", args.first()) {
                    let missing = entities
                        .get(&p[0])
                        .is_some_and(|ce| !ce["fields"][name.as_str()].is_object());
                    if missing {
                        out.push(issue(
                            path,
                            "unknown_field",
                            format!("`{}` has no field `{name}`", p[0]),
                        ));
                    }
                }
            }
            check_children(out, path, b, entities, owner)
        }
        Expr::Neg(b) | Expr::Not(b) => check_children(out, path, b, entities, owner),
        Expr::Bin(_, a, b) => {
            check_children(out, path, a, entities, owner);
            check_children(out, path, b, entities, owner);
        }
        _ => {}
    }
}

/// Computed fields must not depend on each other in a cycle (within one entity).
fn cycles(out: &mut Vec<Issue>, entities: &Obj) {
    for (en, ent) in entities {
        let Some(fields) = obj(ent, "fields") else {
            continue;
        };
        let deps: Vec<(&str, BTreeSet<String>)> = fields
            .iter()
            .filter_map(|(n, f)| {
                let src = f.get("computed")?.as_str()?;
                let mut roots = BTreeSet::new();
                if let Ok(ast) = parse(src) {
                    roots_of(&ast, &mut roots);
                }
                Some((
                    n.as_str(),
                    roots
                        .into_iter()
                        .filter(|r| fields.get(r).is_some_and(|f| f.get("computed").is_some()))
                        .collect(),
                ))
            })
            .collect();
        for (start, _) in &deps {
            let mut stack = vec![*start];
            let mut seen = BTreeSet::new();
            while let Some(cur) = stack.pop() {
                for (n, d) in &deps {
                    if *n == cur {
                        for x in d {
                            if x == start {
                                out.push(issue(
                                    format!("/entities/{en}/fields/{start}/computed"),
                                    "computed_cycle",
                                    format!("`{start}` depends on itself through `{cur}`"),
                                ));
                                return;
                            }
                            if seen.insert(x.clone()) {
                                stack.push(
                                    deps.iter()
                                        .find(|(n2, _)| n2 == x)
                                        .map(|(n2, _)| *n2)
                                        .unwrap_or(""),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

fn roots_of(e: &Expr, out: &mut BTreeSet<String>) {
    match e {
        Expr::Path(p) => {
            out.insert(p[0].clone());
        }
        Expr::Call(f, _) if f == "children" => {}
        Expr::Call(_, a) | Expr::List(a) => a.iter().for_each(|x| roots_of(x, out)),
        Expr::Field(b, _) | Expr::Neg(b) | Expr::Not(b) => roots_of(b, out),
        Expr::Bin(_, a, b) => {
            roots_of(a, out);
            roots_of(b, out);
        }
        _ => {}
    }
}
