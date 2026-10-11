use ce_io::*;
use serde_json::json;

fn manifest() -> ce_manifest::Manifest {
    let m = json!({
        "manifest_version": 1,
        "template": {"name": "t", "version": "0.1.0", "title": "T"},
        "entities": {"item": {
            "fields": {
                "name": {"type": "string", "required": true, "max_length": 20},
                "qty": {"type": "integer"},
                "price": {"type": "decimal", "precision": 8, "scale": 2},
                "active": {"type": "boolean"},
                "due": {"type": "date"},
                "meta": {"type": "json"},
                "total": {"type": "decimal", "precision": 8, "scale": 2, "computed": "price * 2"}
            }
        }},
        "roles": {"admin": {"read": ["*"], "write": ["*"]}},
        "runtime": {"storage": {"relational": true}}
    });
    ce_manifest::validate(m.to_string().as_bytes()).unwrap()
}

#[test]
fn export_has_a_header_and_quotes_cells() {
    let m = manifest();
    let recs = vec![
        json!({"id": "1", "name": "Salt, fine", "qty": 3, "price": "1.50", "active": true, "total": "3.00"}),
        json!({"id": "2", "name": "say \"hi\"\nthere"}),
    ];
    let csv = export_csv(&m, "item", &recs).unwrap();
    let mut lines = csv.split("\r\n");
    assert_eq!(
        lines.next().unwrap(),
        "id,active,due,meta,name,price,qty,total"
    );
    assert_eq!(lines.next().unwrap(), "1,true,,,\"Salt, fine\",1.50,3,3.00");
    assert!(csv.contains("\"say \"\"hi\"\"\nthere\""));
    assert_eq!(
        export_csv(&m, "nope", &[]).unwrap_err().code,
        "unknown_entity"
    );
}

#[test]
fn export_guards_formulas_in_text_only() {
    let m = manifest();
    let csv = export_csv(
        &m,
        "item",
        &[json!({"id": "1", "name": "=SUM(A1)", "qty": -4, "price": "-1.00"})],
    )
    .unwrap();
    let row = csv.split("\r\n").nth(1).unwrap();
    assert!(row.contains("'=SUM(A1)"), "{row}");
    // Negative numbers are numbers, not formulas.
    assert!(row.contains(",-1.00,-4,"), "{row}");
}

#[test]
fn import_round_trips_an_export_including_the_guard() {
    let m = manifest();
    let rec = json!({"name": "=SUM(A1)", "qty": 3, "price": "1.50", "active": false, "due": "2026-01-02", "meta": {"a": [1, 2]}});
    let mut with_id = rec.clone();
    with_id["id"] = json!("x");
    let csv = export_csv(&m, "item", &[with_id]).unwrap();
    // `id` and the computed `total` are engine-owned, so a full export cannot be re-imported as is.
    let err = import_csv(&m, "item", &csv).unwrap_err();
    assert!(err
        .iter()
        .any(|e| e.column == "id" && e.code == "reserved_field"));
    assert!(err
        .iter()
        .any(|e| e.column == "total" && e.code == "computed_field"));

    let text = "name,qty,price,active,due,meta\r\n'=SUM(A1),3,1.50,false,2026-01-02,\"{\"\"a\"\":[1,2]}\"\r\n";
    let report = import_csv(&m, "item", text).unwrap();
    let row = &report.rows[0];
    assert!(row.errors.is_empty(), "{:?}", row.errors);
    assert_eq!(serde_json::Value::Object(row.body.clone()), rec);
}

#[test]
fn rows_are_checked_and_reported_by_number() {
    let m = manifest();
    let text = "\u{feff}name,qty,price\n\
                Rice,2,3.5\n\
                ,1,1.00\n\
                Beans,two,1.00\n\
                Oats,1,1.234\n\
                Corn,1\n\
                \n\
                Flour,5,\n";
    let report = import_csv(&m, "item", text).unwrap();
    assert_eq!(report.rows.len(), 6);
    let codes = |i: usize| -> Vec<(&str, &str)> {
        report.rows[i]
            .errors
            .iter()
            .map(|e| (e.column.as_str(), e.code))
            .collect()
    };
    assert!(codes(0).is_empty());
    assert_eq!(codes(1), vec![("name", "required")]);
    assert_eq!(codes(2), vec![("qty", "type_mismatch")]);
    assert_eq!(codes(3), vec![("price", "out_of_range")]);
    assert_eq!(codes(4), vec![("", "column_count")]);
    assert!(codes(5).is_empty());
    assert_eq!(report.rows[5].row, 6);
    assert_eq!(report.accepted().count(), 2);
    assert_eq!(report.rejected().count(), 4);
    assert!(!report.rows[5].body.contains_key("price"));
}

#[test]
fn bad_files_are_refused_whole() {
    let m = manifest();
    let codes = |t: &str| {
        import_csv(&m, "item", t)
            .unwrap_err()
            .into_iter()
            .map(|e| e.code)
            .collect::<Vec<_>>()
    };
    assert_eq!(codes(""), vec!["empty"]);
    assert_eq!(codes("name,name\nA,B\n"), vec!["duplicate_column"]);
    assert_eq!(codes("name,color\nA,B\n"), vec!["unknown_field"]);
    assert_eq!(codes("name,\nA,B\n"), vec!["unnamed_column"]);
    assert_eq!(codes("name\n\"open\n"), vec!["bad_csv"]);
    assert_eq!(
        import_csv(&m, "nope", "a\n1\n").unwrap_err()[0].code,
        "unknown_entity"
    );
    let big = "name\n".to_string() + &"a\n".repeat(MAX_ROWS + 1);
    assert_eq!(codes(&big), vec!["too_many_rows"]);
    let huge = "n".repeat(MAX_BYTES + 1);
    assert_eq!(codes(&huge), vec!["too_large"]);
}

#[test]
fn json_cells_must_parse_and_crlf_is_one_break() {
    let m = manifest();
    let report = import_csv(&m, "item", "name,meta\r\nA,{oops\r\nB,[1]\r\n").unwrap();
    assert_eq!(report.rows.len(), 2);
    assert_eq!(report.rows[0].errors[0].code, "invalid_format");
    assert!(report.rows[1].errors.is_empty());
}
