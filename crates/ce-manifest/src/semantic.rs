use crate::{issue, Issue};
use ce_expr::{parse, Expr};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

type Obj = Map<String, Value>;

const ROOTS: [&str; 4] = ["old", "actor", "now", "today"];
const FUNCS: [&str; 20] = [
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
    "year",
    "month",
    "day",
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
        for (fname, f) in fields {
            let ty = f["type"].as_str().unwrap_or("");
            if f["searchable"] == true && ty != "string" && ty != "text" {
                out.push(issue(
                    format!("{base}/fields/{fname}/searchable"),
                    "not_searchable",
                    format!(
                        "`{fname}` is a {ty} field; only string and text fields can be searchable"
                    ),
                ));
            }
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
        // Expressions of an action also read the caller's `input.*`.
        let mut expr_names = names.clone();
        expr_names.insert("input");
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
            if let Some(w) = eff.get("when").and_then(Value::as_str) {
                expr(&mut out, &format!("{ep}/when"), w, &expr_names, Site::Plain);
                if eff.as_object().is_some_and(|o| o.len() == 1) {
                    out.push(issue(
                        format!("{ep}/when"),
                        "empty_effect",
                        "`when` needs an effect to guard".to_string(),
                    ));
                }
            }
            if let Some(u) = obj(eff, "update") {
                if let Some(e) = u.get("id").and_then(Value::as_str) {
                    expr(
                        &mut out,
                        &format!("{ep}/update/id"),
                        e,
                        &expr_names,
                        Site::Plain,
                    );
                }
                match u.get("entity").and_then(Value::as_str) {
                    Some(t) if !entities.contains_key(t) => out.push(issue(
                        format!("{ep}/update/entity"),
                        "unknown_entity",
                        format!("no entity `{t}`"),
                    )),
                    Some(t) => {
                        let tf = obj(&entities[t], "fields").unwrap_or(&empty);
                        for (k, v) in u.get("set").and_then(Value::as_object).unwrap_or(&empty) {
                            if !tf.contains_key(k) {
                                out.push(issue(
                                    format!("{ep}/update/set/{k}"),
                                    "unknown_field",
                                    format!("`{t}` has no field `{k}`"),
                                ));
                            } else if tf[k].get("computed").is_some() {
                                out.push(issue(
                                    format!("{ep}/update/set/{k}"),
                                    "computed_field",
                                    format!("`{k}` is computed and cannot be set"),
                                ));
                            }
                            if let Some(e) = v.as_str() {
                                expr(
                                    &mut out,
                                    &format!("{ep}/update/set/{k}"),
                                    e,
                                    &expr_names,
                                    Site::Plain,
                                );
                            }
                        }
                    }
                    None => {}
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
                        expr(
                            &mut out,
                            &format!("{ep}/set/{k}"),
                            e,
                            &expr_names,
                            Site::Plain,
                        );
                    }
                }
            }
            if let Some(c) = obj(eff, "carry") {
                if let Some(req) = c.get("requires").and_then(Value::as_str) {
                    expr(
                        &mut out,
                        &format!("{ep}/carry/requires"),
                        req,
                        &expr_names,
                        Site::Plain,
                    );
                }
                let via = c.get("via").and_then(Value::as_str).unwrap_or("");
                match c.get("entity").and_then(Value::as_str) {
                    Some(t) if !entities.contains_key(t) => out.push(issue(
                        format!("{ep}/carry/entity"),
                        "unknown_entity",
                        format!("no entity `{t}`"),
                    )),
                    Some(t) => {
                        let tf = obj(&entities[t], "fields").unwrap_or(&empty);
                        let points_here = tf
                            .get(via)
                            .is_some_and(|f| f["type"] == "ref" && f["ref"].as_str() == Some(en));
                        if !points_here {
                            out.push(issue(
                                format!("{ep}/carry/via"),
                                "bad_carry_link",
                                format!("`{t}.{via}` must be a ref field pointing at `{en}`"),
                            ));
                        }
                        let mut row_names: BTreeSet<&str> = tf.keys().map(String::as_str).collect();
                        if let Some(sf) = entities[t].get("state").and_then(|s| s["field"].as_str())
                        {
                            row_names.insert(sf);
                        }
                        row_names.insert("input");
                        let values = c.get("values").and_then(Value::as_object);
                        if !values.is_some_and(|v| v.contains_key(via)) {
                            out.push(issue(
                                format!("{ep}/carry/values"),
                                "carry_needs_link",
                                format!(
                                    "`values` must set `{via}` so the copies leave this record"
                                ),
                            ));
                        }
                        for (k, v) in values.unwrap_or(&empty) {
                            if !tf.contains_key(k) {
                                out.push(issue(
                                    format!("{ep}/carry/values/{k}"),
                                    "unknown_field",
                                    format!("`{t}` has no field `{k}`"),
                                ));
                            } else if tf[k].get("computed").is_some() {
                                out.push(issue(
                                    format!("{ep}/carry/values/{k}"),
                                    "computed_field",
                                    format!("`{k}` is computed and cannot be set"),
                                ));
                            }
                            if let Some(e) = v.as_str() {
                                expr(
                                    &mut out,
                                    &format!("{ep}/carry/values/{k}"),
                                    e,
                                    &row_names,
                                    Site::Plain,
                                );
                            }
                        }
                    }
                    None => {}
                }
            }
            if let Some(c) = obj(eff, "create") {
                match c.get("entity").and_then(Value::as_str) {
                    Some(t) if !entities.contains_key(t) => out.push(issue(
                        format!("{ep}/create/entity"),
                        "unknown_entity",
                        format!("no entity `{t}`"),
                    )),
                    Some(t) => {
                        let tf = obj(&entities[t], "fields").unwrap_or(&empty);
                        for (k, v) in c.get("values").and_then(Value::as_object).unwrap_or(&empty) {
                            if !tf.contains_key(k) {
                                out.push(issue(
                                    format!("{ep}/create/values/{k}"),
                                    "unknown_field",
                                    format!("`{t}` has no field `{k}`"),
                                ));
                            }
                            if let Some(e) = v.as_str() {
                                expr(
                                    &mut out,
                                    &format!("{ep}/create/values/{k}"),
                                    e,
                                    &expr_names,
                                    Site::Plain,
                                );
                            }
                        }
                    }
                    None => {}
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
            match crate::projection::filter_json(e) {
                Err(why) => out.push(issue(format!("{pp}/filter"), "unsupported_filter", why)),
                Ok(f) => {
                    if let Err(why) = crate::projection::check_types(&f, fields) {
                        out.push(issue(format!("{pp}/filter"), "invalid_filter", why));
                    }
                }
            }
        }
    }

    projection_roles(&mut out, m);
    guest(&mut out, m, entities);
    subscriptions(&mut out, m, entities);
    deliveries(&mut out, m, entities);
    for (rn, r) in obj(m, "reports").unwrap_or(&empty) {
        check_report(&mut out, rn, r, entities, &empty);
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

fn name_of(a: &Expr) -> Option<&str> {
    match a {
        Expr::Path(p) if p.len() == 1 => Some(p[0].as_str()),
        _ => None,
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
                let (child, field) = match &args[..] {
                    [a, b] => match (name_of(a), name_of(b)) {
                        (Some(c), Some(f)) => (c, f),
                        _ => ("", ""),
                    },
                    _ => ("", ""),
                };
                if child.is_empty() {
                    out.push(issue(
                        path,
                        "bad_children",
                        "children(entity, ref_field) takes exactly two plain names",
                    ));
                    return;
                }
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

/// A report groups one entity's records and computes measures over each group.
fn check_report(out: &mut Vec<Issue>, rn: &str, r: &Value, entities: &Obj, empty: &Obj) {
    let pp = format!("/reports/{rn}");
    let Some(en) = r["entity"].as_str() else {
        return;
    };
    let Some(ent) = entities.get(en) else {
        out.push(issue(
            format!("{pp}/entity"),
            "unknown_entity",
            format!("no entity `{en}`"),
        ));
        return;
    };
    // An engine-managed state field is not declared in `fields` but reports may use it (a string).
    let mut all = obj(ent, "fields").unwrap_or(empty).clone();
    if let Some(sf) = ent["state"]["field"].as_str() {
        all.entry(sf.to_string())
            .or_insert_with(|| serde_json::json!({"type": "string"}));
    }
    let fields = &all;
    let ty = |f: &str| fields.get(f).and_then(|x| x["type"].as_str());
    let pii = |f: &str| fields.get(f).is_some_and(|x| x["pii"] == true);
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for (i, g) in r["group_by"].as_array().into_iter().flatten().enumerate() {
        let g = g.as_str().unwrap_or("");
        match ty(g) {
            None => out.push(issue(
                format!("{pp}/group_by/{i}"),
                "unknown_field",
                format!("no field `{g}`"),
            )),
            Some("json" | "file" | "text") => out.push(issue(
                format!("{pp}/group_by/{i}"),
                "invalid_group",
                format!("`{g}` cannot be grouped by"),
            )),
            Some(_) if pii(g) => out.push(issue(
                format!("{pp}/group_by/{i}"),
                "pii_in_report",
                format!("`{g}` is marked pii"),
            )),
            Some(_) => {
                seen.insert(g);
            }
        }
    }
    for (i, ms) in r["measures"].as_array().into_iter().flatten().enumerate() {
        let mp = format!("{pp}/measures/{i}");
        let name = ms["as"].as_str().unwrap_or("");
        if !seen.insert(name) {
            out.push(issue(
                format!("{mp}/as"),
                "duplicate_name",
                format!("`{name}` is already a column of this report"),
            ));
        }
        let func = ms["fn"].as_str().unwrap_or("");
        match (func, ms["field"].as_str()) {
            ("count", None) => {}
            ("count", Some(f)) | (_, Some(f)) => match ty(f) {
                None => out.push(issue(
                    format!("{mp}/field"),
                    "unknown_field",
                    format!("no field `{f}`"),
                )),
                Some(t) => {
                    let ok = match func {
                        "count" => !matches!(t, "json" | "file"),
                        "sum" | "avg" => matches!(t, "integer" | "decimal"),
                        _ => matches!(t, "integer" | "decimal" | "date" | "datetime"),
                    };
                    if !ok {
                        out.push(issue(
                            format!("{mp}/field"),
                            "invalid_measure",
                            format!("`{func}` cannot be computed over a {t} field"),
                        ));
                    } else if pii(f) {
                        out.push(issue(
                            format!("{mp}/field"),
                            "pii_in_report",
                            format!("`{f}` is marked pii"),
                        ));
                    }
                }
            },
            (_, None) => out.push(issue(
                format!("{mp}/field"),
                "required",
                format!("`{func}` needs a field"),
            )),
        }
    }
    for (i, p) in r["params"].as_array().into_iter().flatten().enumerate() {
        let p = p.as_str().unwrap_or("");
        if p == "format" {
            out.push(issue(
                format!("{pp}/params/{i}"),
                "invalid_param",
                "`format` selects the output (json, csv, html) and cannot be a parameter"
                    .to_string(),
            ));
        } else if !matches!(
            ty(p),
            Some("string" | "integer" | "boolean" | "enum" | "date" | "ref" | "decimal")
        ) {
            out.push(issue(
                format!("{pp}/params/{i}"),
                "invalid_param",
                format!("`{p}` is not a field a report can be narrowed by"),
            ));
        }
    }
    if let Some(e) = r.get("filter").and_then(Value::as_str) {
        let names: BTreeSet<&str> = fields.keys().map(String::as_str).collect();
        expr(out, &format!("{pp}/filter"), e, &names, Site::Filter);
        match crate::projection::filter_json(e) {
            Err(why) => out.push(issue(format!("{pp}/filter"), "unsupported_filter", why)),
            Ok(f) => {
                if let Err(why) = crate::projection::check_types(&f, fields) {
                    out.push(issue(format!("{pp}/filter"), "invalid_filter", why));
                }
            }
        }
    }
}

/// Event names: 2 to 4 dot-separated segments, each `[a-z][a-z0-9_]*`.
fn event_name(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    (2..=4).contains(&parts.len())
        && parts.iter().all(|p| {
            let b = p.as_bytes();
            !b.is_empty()
                && b[0].is_ascii_lowercase()
                && b.iter()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
        })
}

/// Where a subscription may copy a value from: the event's `name`, `subject`, `actor`, `at`, or
/// a path into its `data`.
fn event_source(from: &str) -> bool {
    match from {
        "name" | "subject" | "actor" | "at" => true,
        _ => from.strip_prefix("data.").is_some_and(|rest| {
            !rest.is_empty()
                && rest.split('.').all(|seg| {
                    let b = seg.as_bytes();
                    !b.is_empty()
                        && b[0].is_ascii_lowercase()
                        && b.iter()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
                })
        }),
    }
}

/// Subscriptions create records in an entity the manifest declares. They never target the
/// engine's own server-written entities, so a reaction cannot forge events, audit or changesets.
const SERVER_ENTITIES: &[&str] = &[
    "outbox_event",
    "audit_event",
    "changeset",
    "changeset_op",
    "idempotency_key",
];

fn subscriptions(out: &mut Vec<Issue>, m: &Value, entities: &Obj) {
    for (i, s) in m
        .get("subscriptions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let p = format!("/subscriptions/{i}");
        if let Some(ev) = s["event"].as_str() {
            if !event_name(ev) {
                out.push(issue(
                    format!("{p}/event"),
                    "bad_event_name",
                    "2 to 4 dot-separated segments of lowercase letters, digits and underscores",
                ));
            }
        }
        let create = &s["create"];
        let Some(entity) = create["entity"].as_str() else {
            continue;
        };
        let Some(ent) = entities.get(entity) else {
            out.push(issue(
                format!("{p}/create/entity"),
                "unknown_entity",
                format!("no entity `{entity}`"),
            ));
            continue;
        };
        if SERVER_ENTITIES.contains(&entity) {
            out.push(issue(
                format!("{p}/create/entity"),
                "server_entity",
                format!("`{entity}` is written by the server only"),
            ));
        }
        let fields = ent.get("fields").and_then(Value::as_object);
        for (fname, v) in create["values"].as_object().into_iter().flatten() {
            let vp = format!("{p}/create/values/{fname}");
            match fields.and_then(|f| f.get(fname)) {
                None => out.push(issue(
                    &vp,
                    "unknown_field",
                    format!("`{entity}` has no field `{fname}`"),
                )),
                Some(f) if f["computed"].is_string() => out.push(issue(
                    &vp,
                    "computed_field",
                    "a computed field cannot be set",
                )),
                Some(_) => {}
            }
            if let Some(from) = v["from"].as_str() {
                if !event_source(from) {
                    out.push(issue(
                        format!("{vp}/from"),
                        "bad_event_source",
                        "use name, subject, actor, at, or data.<field>",
                    ));
                }
            }
        }
    }
}

/// A delivery queues records of an entity the manifest declares, never a server-written one.
/// The entity carries the fields the relay writes; `params` read fields of that entity.
fn deliveries(out: &mut Vec<Issue>, m: &Value, entities: &Obj) {
    for (i, d) in m
        .get("deliveries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let p = format!("/deliveries/{i}");
        let Some(entity) = d["entity"].as_str() else {
            continue;
        };
        let Some(ent) = entities.get(entity) else {
            out.push(issue(
                format!("{p}/entity"),
                "unknown_entity",
                format!("no entity `{entity}`"),
            ));
            continue;
        };
        if SERVER_ENTITIES.contains(&entity) {
            out.push(issue(
                format!("{p}/entity"),
                "server_entity",
                format!("`{entity}` is written by the server only"),
            ));
        }
        let fields = ent.get("fields").and_then(Value::as_object);
        for (name, types) in [
            ("status", &["string", "enum"][..]),
            ("attempts", &["integer"][..]),
            ("not_before", &["integer"][..]),
            ("last_error", &["string", "text"][..]),
        ] {
            let t = fields
                .and_then(|f| f.get(name))
                .and_then(|f| f["type"].as_str());
            if !t.is_some_and(|t| types.contains(&t)) {
                out.push(issue(
                    format!("{p}/entity"),
                    "missing_queue_field",
                    format!(
                        "`{entity}` needs a `{name}` field of type {}",
                        types.join(" or ")
                    ),
                ));
            }
        }
        // The relay writes these fields with plain updates, so a state machine must not own
        // them, `status` must allow the four relay values, and the others must be optional.
        let state_field = ent["state"]["field"].as_str();
        for name in ["status", "attempts", "not_before", "last_error", "result"] {
            let f = fields.and_then(|f| f.get(name));
            if f.is_some() && state_field == Some(name) {
                out.push(issue(
                    format!("{p}/entity"),
                    "state_managed_queue_field",
                    format!("`{name}` is owned by the state machine; use a plain field"),
                ));
            }
            if f.is_some_and(|f| f["required"] == true) && name != "status" {
                out.push(issue(
                    format!("{p}/entity"),
                    "required_queue_field",
                    format!("`{name}` must not be required: the relay clears and sets it"),
                ));
            }
        }
        if let Some(values) = fields
            .and_then(|f| f.get("status"))
            .and_then(|f| f["values"].as_array())
        {
            for want in ["queued", "in_flight", "done", "dead"] {
                if !values.iter().any(|v| v == want) {
                    out.push(issue(
                        format!("{p}/entity"),
                        "missing_status_value",
                        format!("`status` must allow `{want}`"),
                    ));
                }
            }
        }
        if let Some(r) = fields.and_then(|f| f.get("result")) {
            if r["type"] != "json" {
                out.push(issue(
                    format!("{p}/entity"),
                    "result_not_json",
                    "`result` keeps the connector's answer and must be json",
                ));
            }
        }
        for (k, v) in d["params"].as_object().into_iter().flatten() {
            if let Some(f) = v["from"].as_str() {
                if f != "id" && !fields.is_some_and(|fs| fs.contains_key(f)) {
                    out.push(issue(
                        format!("{p}/params/{k}/from"),
                        "unknown_field",
                        format!("`{entity}` has no field `{f}`"),
                    ));
                }
            }
        }
    }
}

/// A projection with `roles` is read by signed-in callers holding one of them, so each must exist.
fn projection_roles(out: &mut Vec<Issue>, m: &Value) {
    let empty = Obj::new();
    let roles = obj(m, "roles").unwrap_or(&empty);
    for (pn, p) in obj(m, "projections").unwrap_or(&empty) {
        for (i, r) in p["roles"].as_array().into_iter().flatten().enumerate() {
            let r = r.as_str().unwrap_or("");
            if !roles.contains_key(r) {
                out.push(issue(
                    format!("/projections/{pn}/roles/{i}"),
                    "unknown_role",
                    format!("no role `{r}`"),
                ));
            }
        }
    }
}

/// `eventDate` becomes `event_date`: expression names are lower snake case, form keys often are not.
pub fn snake(key: &str) -> String {
    let mut o = String::new();
    for (i, c) in key.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                o.push('_');
            }
            o.push(c.to_ascii_lowercase());
        } else {
            o.push(c);
        }
    }
    o
}

const GUEST_ROUTES: [&str; 4] = ["signup", "login", "logout", "reset"];

/// The `guest` block: the guest role holds nothing, every name resolves, and what a guest sends
/// can never name a server-owned key (owner, state, note, derived values).
fn guest(out: &mut Vec<Issue>, m: &Value, entities: &Obj) {
    let Some(g) = m.get("guest") else { return };
    let empty = Obj::new();
    let roles = obj(m, "roles").unwrap_or(&empty);
    let role = g["role"].as_str().unwrap_or("");
    match roles.get(role) {
        None => out.push(issue(
            "/guest/role",
            "unknown_role",
            format!("no role `{role}`"),
        )),
        Some(r) => {
            for key in ["read", "write", "actions"] {
                if r[key].as_array().is_some_and(|a| !a.is_empty()) {
                    out.push(issue(
                        format!("/roles/{role}/{key}"),
                        "guest_role_privileged",
                        "the guest role must hold no permissions: guests reach only what `guest` declares",
                    ));
                }
            }
        }
    }
    for kind in ["resources", "public", "feeds"] {
        for name in obj(g, kind).unwrap_or(&empty).keys() {
            if GUEST_ROUTES.contains(&name.as_str()) {
                out.push(issue(
                    format!("/guest/{kind}/{name}"),
                    "reserved_guest_path",
                    format!("`{name}` is a sign-in route"),
                ));
            }
        }
    }
    let kinds = ["resources", "public", "feeds"];
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for kind in kinds {
        for name in obj(g, kind).unwrap_or(&empty).keys() {
            if !seen.insert(name.as_str()) {
                out.push(issue(
                    format!("/guest/{kind}/{name}"),
                    "duplicate_guest_path",
                    format!("`{name}` is declared more than once under `guest`"),
                ));
            }
        }
    }
    for (name, f) in obj(g, "feeds").unwrap_or(&empty) {
        guest_feed(out, name, f, entities, &empty);
    }
    for (name, p) in obj(g, "public").unwrap_or(&empty) {
        let pp = format!("/guest/public/{name}");
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
        for (key, f) in [
            ("match/field", &p["match"]["field"]),
            ("value_field", &p["value_field"]),
        ] {
            let f = f.as_str().unwrap_or("");
            if !fields.contains_key(f) {
                out.push(issue(
                    format!("{pp}/{key}"),
                    "unknown_field",
                    format!("no field `{f}`"),
                ));
            } else if fields[f]["pii"] == true {
                out.push(issue(
                    format!("{pp}/{key}"),
                    "pii_in_public_projection",
                    format!("`{f}` is marked pii"),
                ));
            }
        }
    }
    for (name, r) in obj(g, "resources").unwrap_or(&empty) {
        guest_resource(out, name, r, entities, &empty);
    }
}

fn guest_resource(out: &mut Vec<Issue>, name: &str, r: &Value, entities: &Obj, empty: &Obj) {
    let rp = format!("/guest/resources/{name}");
    let Some(en) = r["entity"].as_str() else {
        return;
    };
    let Some(ent) = entities.get(en) else {
        out.push(issue(
            format!("{rp}/entity"),
            "unknown_entity",
            format!("no entity `{en}`"),
        ));
        return;
    };
    let fields = obj(ent, "fields").unwrap_or(empty);
    let ty = |f: &str| fields.get(f).and_then(|x| x["type"].as_str()).unwrap_or("");
    let known = |out: &mut Vec<Issue>, path: String, f: &str| -> bool {
        if fields.contains_key(f) {
            true
        } else {
            out.push(issue(path, "unknown_field", format!("no field `{f}`")));
            false
        }
    };
    let owner = r["owner_field"].as_str().unwrap_or("");
    if known(out, format!("{rp}/owner_field"), owner) {
        let max = fields[owner]["max_length"].as_u64();
        if ty(owner) != "string" || max.is_some_and(|m| m < 64) {
            out.push(issue(
                format!("{rp}/owner_field"),
                "bad_owner_field",
                "the owner field must be a string of at least 64 characters (it holds `guest:` and a uuid)",
            ));
        }
    }
    for k in obj(r, "fixed").unwrap_or(empty).keys() {
        known(out, format!("{rp}/fixed/{k}"), k);
        if k == owner {
            out.push(issue(
                format!("{rp}/fixed/{k}"),
                "owner_is_fixed",
                "the owner field is set per guest, not fixed",
            ));
        }
    }
    for k in obj(r, "stamp").unwrap_or(empty).keys() {
        if known(out, format!("{rp}/stamp/{k}"), k) && ty(k) != "date" {
            out.push(issue(
                format!("{rp}/stamp/{k}"),
                "bad_stamp_field",
                format!("`{k}` is not a date field"),
            ));
        }
    }
    for key in ["created_at_field", "title_field"] {
        if let Some(f) = r[key].as_str() {
            known(out, format!("{rp}/{key}"), f);
        }
    }
    if r.get("title").is_some() && r.get("title_field").is_none() {
        out.push(issue(
            format!("{rp}/title"),
            "title_needs_field",
            "`title` needs `title_field`",
        ));
    }
    let b = &r["body"];
    let bf = b["field"].as_str().unwrap_or("");
    if known(out, format!("{rp}/body/field"), bf) && ty(bf) != "json" {
        out.push(issue(
            format!("{rp}/body/field"),
            "bad_body_field",
            format!("`{bf}` is not a json field"),
        ));
    }
    let allow: Vec<&str> = b["allow"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut names: BTreeSet<String> = allow.iter().map(|k| snake(k)).collect();
    let snakes: BTreeSet<String> = allow.iter().map(|k| snake(k)).collect();
    if snakes.len() != allow.len() {
        out.push(issue(
            format!("{rp}/body/allow"),
            "ambiguous_keys",
            "two allowed keys become the same expression name",
        ));
    }
    let server_keys: Vec<(&str, Option<&str>)> = vec![
        ("state/key", b["state"]["key"].as_str()),
        ("note_key", b["note_key"].as_str()),
    ];
    for (path, k) in &server_keys {
        if let Some(k) = k {
            if allow.contains(k) {
                out.push(issue(
                    format!("{rp}/body/{path}"),
                    "guest_writable_server_key",
                    format!("`{k}` is owned by the server and cannot be in `allow`"),
                ));
            }
        }
    }
    for k in obj(b, "derive").unwrap_or(empty).keys() {
        if allow.contains(&k.as_str()) {
            out.push(issue(
                format!("{rp}/body/derive/{k}"),
                "guest_writable_server_key",
                format!("`{k}` is derived and cannot be in `allow`"),
            ));
        }
    }
    for (i, k) in b["required"].as_array().into_iter().flatten().enumerate() {
        if !allow.contains(&k.as_str().unwrap_or("")) {
            out.push(issue(
                format!("{rp}/body/required/{i}"),
                "not_allowed_key",
                "a required key must be in `allow`",
            ));
        }
    }
    // Expressions see the allowed keys (snake case) and, for rules after derive, the derived ones.
    let derived: Vec<String> = obj(b, "derive")
        .unwrap_or(empty)
        .keys()
        .map(|k| snake(k))
        .collect();
    names.extend(derived.iter().cloned());
    let view: BTreeSet<&str> = names.iter().map(String::as_str).collect();
    for (k, e) in obj(b, "derive").unwrap_or(empty) {
        if let Some(e) = e.as_str() {
            expr(out, &format!("{rp}/body/derive/{k}"), e, &view, Site::Plain);
        }
    }
    for (i, rule) in b["rules"].as_array().into_iter().flatten().enumerate() {
        if let Some(e) = rule["expr"].as_str() {
            expr(
                out,
                &format!("{rp}/body/rules/{i}/expr"),
                e,
                &view,
                Site::Plain,
            );
        }
        if let Some(f) = rule["field"].as_str() {
            if !allow.contains(&f) {
                out.push(issue(
                    format!("{rp}/body/rules/{i}/field"),
                    "not_allowed_key",
                    "a rule's field must be in `allow`",
                ));
            }
        }
    }
    if let Some(t) = r["title"].as_str() {
        expr(out, &format!("{rp}/title"), t, &view, Site::Plain);
    }
}

fn guest_feed(out: &mut Vec<Issue>, name: &str, f: &Value, entities: &Obj, empty: &Obj) {
    let fp = format!("/guest/feeds/{name}");
    let Some(en) = f["entity"].as_str() else {
        return;
    };
    let Some(ent) = entities.get(en) else {
        out.push(issue(
            format!("{fp}/entity"),
            "unknown_entity",
            format!("no entity `{en}`"),
        ));
        return;
    };
    let fields = obj(ent, "fields").unwrap_or(empty);
    for k in obj(f, "fixed").unwrap_or(empty).keys() {
        if !fields.contains_key(k) {
            out.push(issue(
                format!("{fp}/fixed/{k}"),
                "unknown_field",
                format!("no field `{k}`"),
            ));
        }
    }
    let bf = f["body_field"].as_str().unwrap_or("");
    match fields.get(bf) {
        None => out.push(issue(
            format!("{fp}/body_field"),
            "unknown_field",
            format!("no field `{bf}`"),
        )),
        Some(x) if x["type"] != "json" => out.push(issue(
            format!("{fp}/body_field"),
            "bad_body_field",
            format!("`{bf}` is not a json field"),
        )),
        _ => {}
    }
    let keys: Vec<&str> = f["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let snakes: BTreeSet<String> = keys.iter().map(|k| snake(k)).collect();
    if snakes.len() != keys.len() {
        out.push(issue(
            format!("{fp}/keys"),
            "ambiguous_keys",
            "two keys become the same expression name",
        ));
    }
    let view: BTreeSet<&str> = snakes.iter().map(String::as_str).collect();
    if let Some(w) = f["where"].as_str() {
        expr(out, &format!("{fp}/where"), w, &view, Site::Plain);
    }
    let field_names: Vec<&str> = obj(f, "fields")
        .unwrap_or(empty)
        .keys()
        .map(String::as_str)
        .collect();
    for (k, spec) in obj(f, "fields").unwrap_or(empty) {
        if k == "id" {
            out.push(issue(
                format!("{fp}/fields/{k}"),
                "reserved_field",
                "`id` is always the row's id",
            ));
        }
        if let Some(from) = spec["from"].as_str() {
            if !keys.contains(&from) {
                out.push(issue(
                    format!("{fp}/fields/{k}/from"),
                    "unknown_key",
                    format!("`{from}` is not in `keys`"),
                ));
            }
        }
        if let Some(e) = spec["expr"].as_str() {
            expr(out, &format!("{fp}/fields/{k}/expr"), e, &view, Site::Plain);
        }
    }
    if let Some(o) = f["order_by"].as_str() {
        if !field_names.contains(&o.trim_start_matches('-')) {
            out.push(issue(
                format!("{fp}/order_by"),
                "unknown_field",
                format!("`{o}` is not a declared field"),
            ));
        }
    }
}
