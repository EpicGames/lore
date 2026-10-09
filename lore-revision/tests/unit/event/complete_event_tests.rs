// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::event::LoreCompleteEventData;
use lore_revision::event::LoreErrorDetail;
use lore_revision::event::LoreTraceLocation;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreString;

// Builds a populated error detail with one trace location so the
// serialized form carries non-default values to assert against.
fn populated_detail() -> LoreErrorDetail {
    let location = LoreTraceLocation {
        file: LoreString::from("src/op.rs"),
        line: 11,
        column: 5,
        context: LoreString::from("running op"),
    };

    LoreErrorDetail {
        error_code: 13,
        message: LoreString::from("not found"),
        trace_locations: LoreArray::from_vec(vec![location]),
    }
}

#[test]
fn serializes_error_detail_fields_in_camel_case() {
    let event = LoreCompleteEventData {
        status: 13,
        error: populated_detail(),
    };

    let json: serde_json::Value = serde_json::to_value(&event).unwrap();

    // The `status` field keeps its key and value.
    assert_eq!(json["status"], 13);

    // The appended detail nests under `error` and uses camelCase keys for
    // its own fields.
    let error = &json["error"];
    assert_eq!(error["errorCode"], 13);
    assert_eq!(error["message"], "not found");

    let traces = error["traceLocations"].as_array().unwrap();
    assert_eq!(traces.len(), 1);
    assert_eq!(traces[0]["file"], "src/op.rs");
    assert_eq!(traces[0]["line"], 11);
    assert_eq!(traces[0]["column"], 5);
    assert_eq!(traces[0]["context"], "running op");
}

#[test]
fn legacy_status_field_keeps_its_name_position_and_type() {
    // The `status` field serializes under its existing key.
    let event = LoreCompleteEventData {
        status: 42,
        error: LoreErrorDetail::default(),
    };
    let json: serde_json::Value = serde_json::to_value(&event).unwrap();
    assert_eq!(json["status"], 42);

    // `status` keeps its `i32` type: an `i32` binds directly into the
    // first field, so a change of type or position would fail to compile.
    let status: i32 = -1;
    let by_position = LoreCompleteEventData {
        status,
        error: LoreErrorDetail::default(),
    };
    assert_eq!(by_position.status, status);
}
