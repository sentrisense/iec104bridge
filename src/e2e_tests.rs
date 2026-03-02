// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2024 Sentrisense
//
//! End-to-end tests that exercise the full pipeline:
//!
//! **JSON string → `serde_json` parse → `IterSource` stream → `dispatch` → `CapturingSink`**
//!
//! These tests deliberately avoid the real NATS / IEC-104 stack and instead
//! combine the testable building blocks that were introduced specifically for
//! this purpose:
//!
//! * [`crate::source::IterSource`] — feeds a `Vec<Iec104Message>` as a stream.
//! * [`crate::bridge::test_support::CapturingSink`] — records every `DataSink`
//!   call so assertions can be made on the exact type, value, CA, IOA, quality,
//!   and COT.

use futures::StreamExt as _;
use lib60870::types::{CauseOfTransmission, Quality};

use crate::bridge::test_support::{CapturingSink, SentCall};
use crate::bridge::dispatch;
use crate::message::Iec104Message;
use crate::source::{IterSource, MessageSource};

// ─── helpers ──────────────────────────────────────────────────────────────────

/// Parse a JSON string into an [`Iec104Message`], panicking on failure.
fn parse(json: &str) -> Iec104Message {
    serde_json::from_str(json).unwrap_or_else(|e| panic!("JSON parse failed: {e}\nInput: {json}"))
}

/// Collect every message from a source, dispatch it through a `CapturingSink`,
/// and return the sink's recorded calls.
///
/// `default_ca` is forwarded to [`dispatch`] to mirror how `main` uses the
/// `IEC104_CA` env-var default.
async fn run(msgs: Vec<Iec104Message>, default_ca: u16) -> Vec<SentCall> {
    let sink = CapturingSink::default();
    let source: Box<dyn MessageSource> = Box::new(IterSource::new(msgs));
    let mut stream = source.into_messages();

    while let Some(Ok(msg)) = stream.next().await {
        dispatch(&sink, &msg, default_ca);
    }

    sink.calls.into_inner()
}

// ─── single-message round-trips ───────────────────────────────────────────────

#[tokio::test]
async fn float_message_dispatches_as_measured_float() {
    let calls = run(vec![parse(r#"{"ioa": 100, "value": 42.5, "type": "float"}"#)], 1).await;
    assert_eq!(calls.len(), 1);
    assert!(
        matches!(&calls[0], SentCall::MeasuredFloat { ioa: 100, value, ca: 1, .. } if (*value - 42.5_f32).abs() < 1e-5)
    );
}

#[tokio::test]
async fn scaled_message_dispatches_as_measured_scaled() {
    let calls = run(vec![parse(r#"{"ioa": 200, "value": 1000, "type": "scaled"}"#)], 1).await;
    assert_eq!(calls.len(), 1);
    assert!(matches!(&calls[0], SentCall::MeasuredScaled { ioa: 200, value: 1000, ca: 1, .. }));
}

#[tokio::test]
async fn normalized_message_dispatches_as_measured_float() {
    let calls = run(vec![parse(r#"{"ioa": 300, "value": 0.75, "type": "normalized"}"#)], 1).await;
    assert_eq!(calls.len(), 1);
    assert!(matches!(&calls[0], SentCall::MeasuredFloat { ioa: 300, ca: 1, .. }));
}

#[tokio::test]
async fn single_point_true_dispatches_as_on() {
    let calls = run(vec![parse(r#"{"ioa": 400, "value": true, "type": "single_point"}"#)], 1).await;
    assert_eq!(calls.len(), 1);
    assert!(matches!(&calls[0], SentCall::SinglePoint { ioa: 400, value: true, ca: 1, .. }));
}

#[tokio::test]
async fn single_point_false_dispatches_as_off() {
    let calls = run(vec![parse(r#"{"ioa": 401, "value": false, "type": "single_point"}"#)], 1).await;
    assert_eq!(calls.len(), 1);
    assert!(matches!(&calls[0], SentCall::SinglePoint { ioa: 401, value: false, ca: 1, .. }));
}

#[tokio::test]
async fn double_point_falls_back_to_single_point() {
    let calls = run(vec![parse(r#"{"ioa": 500, "value": true, "type": "double_point"}"#)], 1).await;
    assert_eq!(calls.len(), 1);
    // DoublePoint has no native convenience API; the bridge falls back to SinglePoint.
    assert!(matches!(&calls[0], SentCall::SinglePoint { ioa: 500, value: true, ca: 1, .. }));
}

// ─── type inference from JSON value ──────────────────────────────────────────

#[tokio::test]
async fn bool_without_type_field_inferred_as_single_point() {
    // No "type" key → infer from value.
    let calls = run(vec![parse(r#"{"ioa": 1, "value": true}"#)], 1).await;
    assert_eq!(calls.len(), 1);
    assert!(matches!(&calls[0], SentCall::SinglePoint { value: true, .. }));
}

#[tokio::test]
async fn integer_without_type_field_inferred_as_scaled() {
    let calls = run(vec![parse(r#"{"ioa": 2, "value": 42}"#)], 1).await;
    assert_eq!(calls.len(), 1);
    assert!(matches!(&calls[0], SentCall::MeasuredScaled { value: 42, .. }));
}

#[tokio::test]
async fn fractional_number_without_type_field_inferred_as_float() {
    let calls = run(vec![parse(r#"{"ioa": 3, "value": 3.14}"#)], 1).await;
    assert_eq!(calls.len(), 1);
    assert!(matches!(&calls[0], SentCall::MeasuredFloat { .. }));
}

// ─── CA resolution ────────────────────────────────────────────────────────────

#[tokio::test]
async fn message_ca_overrides_default_ca() {
    let calls = run(
        vec![parse(r#"{"ioa": 1, "value": true, "ca": 42}"#)],
        99, // default that should be replaced
    )
    .await;
    assert_eq!(calls.len(), 1);
    assert!(matches!(&calls[0], SentCall::SinglePoint { ca: 42, .. }));
}

#[tokio::test]
async fn missing_ca_falls_back_to_default() {
    let calls = run(
        vec![parse(r#"{"ioa": 1, "value": true}"#)],
        7,
    )
    .await;
    assert_eq!(calls.len(), 1);
    assert!(matches!(&calls[0], SentCall::SinglePoint { ca: 7, .. }));
}

// ─── quality propagation ──────────────────────────────────────────────────────

#[tokio::test]
async fn good_quality_propagated() {
    let calls = run(vec![parse(r#"{"ioa": 1, "value": 0.0, "type": "float", "quality": "good"}"#)], 1).await;
    assert!(matches!(&calls[0], SentCall::MeasuredFloat { quality, .. } if *quality == Quality::GOOD));
}

#[tokio::test]
async fn invalid_quality_propagated() {
    let calls = run(vec![parse(r#"{"ioa": 1, "value": 0.0, "type": "float", "quality": "invalid"}"#)], 1).await;
    assert!(matches!(&calls[0], SentCall::MeasuredFloat { quality, .. } if *quality == Quality::INVALID));
}

#[tokio::test]
async fn not_topical_quality_propagated() {
    let calls = run(vec![parse(r#"{"ioa": 1, "value": 0.0, "type": "float", "quality": "not_topical"}"#)], 1).await;
    assert!(matches!(&calls[0], SentCall::MeasuredFloat { quality, .. } if *quality == Quality::NOT_TOPICAL));
}

#[tokio::test]
async fn substituted_quality_propagated() {
    let calls = run(vec![parse(r#"{"ioa": 1, "value": 0.0, "type": "float", "quality": "substituted"}"#)], 1).await;
    assert!(matches!(&calls[0], SentCall::MeasuredFloat { quality, .. } if *quality == Quality::SUBSTITUTED));
}

#[tokio::test]
async fn default_quality_is_good_when_absent() {
    let calls = run(vec![parse(r#"{"ioa": 1, "value": 0.0, "type": "float"}"#)], 1).await;
    assert!(matches!(&calls[0], SentCall::MeasuredFloat { quality, .. } if *quality == Quality::GOOD));
}

// ─── COT propagation ──────────────────────────────────────────────────────────

#[tokio::test]
async fn periodic_cot_propagated() {
    let calls = run(
        vec![parse(r#"{"ioa": 1, "value": 1.0, "type": "float", "cot": "periodic"}"#)],
        1,
    )
    .await;
    assert!(matches!(&calls[0], SentCall::MeasuredFloat { cot: CauseOfTransmission::Periodic, .. }));
}

#[tokio::test]
async fn background_scan_cot_propagated() {
    let calls = run(
        vec![parse(r#"{"ioa": 1, "value": 1.0, "type": "float", "cot": "background_scan"}"#)],
        1,
    )
    .await;
    assert!(matches!(&calls[0], SentCall::MeasuredFloat { cot: CauseOfTransmission::Background, .. }));
}

#[tokio::test]
async fn default_cot_is_spontaneous_when_absent() {
    let calls = run(vec![parse(r#"{"ioa": 1, "value": 0.0, "type": "float"}"#)], 1).await;
    assert!(
        matches!(&calls[0], SentCall::MeasuredFloat { cot: CauseOfTransmission::Spontaneous, .. })
    );
}

// ─── full message schema ──────────────────────────────────────────────────────

#[tokio::test]
async fn full_schema_float_message() {
    let json = r#"{
        "ioa":     150,
        "value":   -7.25,
        "type":    "float",
        "ca":      3,
        "quality": "good",
        "cot":     "spontaneous"
    }"#;
    let calls = run(vec![parse(json)], 1).await;
    assert_eq!(calls.len(), 1);
    assert!(matches!(
        &calls[0],
        SentCall::MeasuredFloat {
            ioa: 150,
            ca: 3,
            cot: CauseOfTransmission::Spontaneous,
            quality,
            value,
        } if *quality == Quality::GOOD && (*value - -7.25_f32).abs() < 1e-4
    ));
}

#[tokio::test]
async fn full_schema_scaled_message() {
    let json = r#"{
        "ioa":     250,
        "value":   -500,
        "type":    "scaled",
        "ca":      2,
        "quality": "substituted",
        "cot":     "periodic"
    }"#;
    let calls = run(vec![parse(json)], 1).await;
    assert_eq!(calls.len(), 1);
    assert!(matches!(
        &calls[0],
        SentCall::MeasuredScaled {
            ioa: 250,
            ca: 2,
            value: -500,
            cot: CauseOfTransmission::Periodic,
            quality,
        } if *quality == Quality::SUBSTITUTED
    ));
}

#[tokio::test]
async fn full_schema_single_point_message() {
    let json = r#"{
        "ioa":     99,
        "value":   true,
        "type":    "single_point",
        "ca":      5,
        "quality": "good",
        "cot":     "interrogated"
    }"#;
    let calls = run(vec![parse(json)], 1).await;
    assert_eq!(calls.len(), 1);
    assert!(matches!(
        &calls[0],
        SentCall::SinglePoint {
            ioa: 99,
            ca: 5,
            value: true,
            cot: CauseOfTransmission::InterrogatedByStation,
            quality,
        } if *quality == Quality::GOOD
    ));
}

// ─── multiple messages ────────────────────────────────────────────────────────

#[tokio::test]
async fn multiple_messages_dispatched_in_order() {
    let msgs = vec![
        parse(r#"{"ioa": 1, "value": 1.0, "type": "float"}"#),
        parse(r#"{"ioa": 2, "value": true, "type": "single_point"}"#),
        parse(r#"{"ioa": 3, "value": 300, "type": "scaled"}"#),
    ];
    let calls = run(msgs, 1).await;

    assert_eq!(calls.len(), 3);
    assert!(matches!(&calls[0], SentCall::MeasuredFloat  { ioa: 1, .. }));
    assert!(matches!(&calls[1], SentCall::SinglePoint    { ioa: 2, .. }));
    assert!(matches!(&calls[2], SentCall::MeasuredScaled { ioa: 3, .. }));
}

#[tokio::test]
async fn mixed_ca_messages_resolved_independently() {
    let msgs = vec![
        parse(r#"{"ioa": 10, "value": 1.0, "type": "float", "ca": 1}"#),
        parse(r#"{"ioa": 20, "value": 2.0, "type": "float"}"#),        // → default_ca = 9
        parse(r#"{"ioa": 30, "value": 3.0, "type": "float", "ca": 5}"#),
    ];
    let calls = run(msgs, 9).await;

    assert!(matches!(&calls[0], SentCall::MeasuredFloat { ioa: 10, ca:  1, .. }));
    assert!(matches!(&calls[1], SentCall::MeasuredFloat { ioa: 20, ca:  9, .. }));
    assert!(matches!(&calls[2], SentCall::MeasuredFloat { ioa: 30, ca:  5, .. }));
}

// ─── scaled saturation ────────────────────────────────────────────────────────

#[tokio::test]
async fn scaled_value_clamped_to_i16_max() {
    let calls = run(
        vec![parse(r#"{"ioa": 1, "value": 999999, "type": "scaled"}"#)],
        1,
    )
    .await;
    assert!(matches!(&calls[0], SentCall::MeasuredScaled { value: i16::MAX, .. }));
}

#[tokio::test]
async fn scaled_value_clamped_to_i16_min() {
    let calls = run(
        vec![parse(r#"{"ioa": 1, "value": -999999, "type": "scaled"}"#)],
        1,
    )
    .await;
    assert!(matches!(&calls[0], SentCall::MeasuredScaled { value: i16::MIN, .. }));
}

// ─── empty source ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn empty_source_produces_no_calls() {
    let calls = run(vec![], 1).await;
    assert!(calls.is_empty());
}
