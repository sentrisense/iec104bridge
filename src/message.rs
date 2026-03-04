// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sentrisense
//
//! JSON message schema consumed from the NATS JetStream.
//!
//! Every message on the stream must be valid JSON that can be deserialised
//! into [`Iec104Message`].  Fields that are optional have documented defaults
//! so that producers need to send only the minimum required information.
//!
//! ## Minimal example
//! ```json
//! { "ioa": 100, "value": 42.5 }
//! ```
//!
//! ## Full example
//! ```json
//! {
//!   "ioa":     100,
//!   "value":   42.5,
//!   "type":    "float",
//!   "ca":      1,
//!   "quality": "good",
//!   "cot":     "spontaneous"
//! }
//! ```

use serde::Deserialize;

/// Root message structure.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Iec104Message {
    /// Information Object Address (1 – 16 777 215).
    pub ioa: u32,

    /// The process value to transmit.
    pub value: DataValue,

    /// IEC-104 type hint.  When omitted the type is inferred from `value`:
    /// * boolean → `single_point`
    /// * integer → `scaled`
    /// * float   → `float`
    #[serde(default, rename = "type")]
    pub data_type: Option<DataType>,

    /// Common Address (ASDU address).  Falls back to the `IEC104_CA`
    /// environment variable when absent.
    pub ca: Option<u16>,

    /// Quality descriptor sent with the value (default: `good`).
    #[serde(default)]
    pub quality: QualityField,

    /// Cause of Transmission (default: `spontaneous`).
    #[serde(default)]
    pub cot: CotField,
}

// ─── value ────────────────────────────────────────────────────────────────────

/// A flexible value that can be a boolean, integer, or floating-point number.
///
/// JSON booleans map to single-point information; numbers are used for
/// measured / scaled values.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum DataValue {
    Bool(bool),
    /// Covers both integer and floating-point JSON numbers.
    Number(f64),
}

// ─── type ─────────────────────────────────────────────────────────────────────

/// Explicit IEC-104 ASDU type selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    /// M_ME_NC_1 – short floating-point measured value
    Float,
    /// M_ME_NB_1 – scaled measured value (i16)
    Scaled,
    /// M_ME_NA_1 – normalised measured value (−1.0 … +1.0)
    Normalized,
    /// M_SP_NA_1 – single-point information (on/off)
    SinglePoint,
    /// M_DP_NA_1 – double-point information (intermediate/off/on/indeterminate)
    DoublePoint,
}

// ─── quality ──────────────────────────────────────────────────────────────────

/// Quality flags supported in JSON messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityField {
    #[default]
    Good,
    Invalid,
    /// Value is not topical (old data).
    NotTopical,
    /// Value comes from a substituted source.
    Substituted,
    /// Value is blocked (update inhibited).
    Blocked,
    /// Numeric overflow detected.
    Overflow,
}

// ─── cause of transmission ────────────────────────────────────────────────────

/// Cause of Transmission variants that can appear in JSON messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CotField {
    #[default]
    Spontaneous,
    Periodic,
    BackgroundScan,
    /// General interrogation response.
    Interrogated,
    ReturnInfoRemote,
    ReturnInfoLocal,
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Iec104Message {
        serde_json::from_str(json).expect("parse failure")
    }

    // ── minimal message ───────────────────────────────────────────────────────

    #[test]
    fn minimal_float_message() {
        let msg = parse(r#"{"ioa": 100, "value": 42.5}"#);
        assert_eq!(msg.ioa, 100);
        assert_eq!(msg.value, DataValue::Number(42.5));
        assert_eq!(msg.data_type, None);
        assert_eq!(msg.ca, None);
        assert_eq!(msg.quality, QualityField::Good);
        assert_eq!(msg.cot, CotField::Spontaneous);
    }

    #[test]
    fn minimal_bool_message() {
        let msg = parse(r#"{"ioa": 200, "value": true}"#);
        assert_eq!(msg.ioa, 200);
        assert_eq!(msg.value, DataValue::Bool(true));
    }

    // ── explicit type field ───────────────────────────────────────────────────

    #[test]
    fn explicit_type_float() {
        let msg = parse(r#"{"ioa": 1, "value": 1.0, "type": "float"}"#);
        assert_eq!(msg.data_type, Some(DataType::Float));
    }

    #[test]
    fn explicit_type_scaled() {
        let msg = parse(r#"{"ioa": 1, "value": 10, "type": "scaled"}"#);
        assert_eq!(msg.data_type, Some(DataType::Scaled));
    }

    #[test]
    fn explicit_type_normalized() {
        let msg = parse(r#"{"ioa": 1, "value": 0.5, "type": "normalized"}"#);
        assert_eq!(msg.data_type, Some(DataType::Normalized));
    }

    #[test]
    fn explicit_type_single_point() {
        let msg = parse(r#"{"ioa": 1, "value": true, "type": "single_point"}"#);
        assert_eq!(msg.data_type, Some(DataType::SinglePoint));
    }

    #[test]
    fn explicit_type_double_point() {
        let msg = parse(r#"{"ioa": 1, "value": true, "type": "double_point"}"#);
        assert_eq!(msg.data_type, Some(DataType::DoublePoint));
    }

    // ── ca field ──────────────────────────────────────────────────────────────

    #[test]
    fn ca_field_present() {
        let msg = parse(r#"{"ioa": 1, "value": 0, "ca": 5}"#);
        assert_eq!(msg.ca, Some(5));
    }

    #[test]
    fn ca_field_absent() {
        let msg = parse(r#"{"ioa": 1, "value": 0}"#);
        assert_eq!(msg.ca, None);
    }

    // ── quality field ─────────────────────────────────────────────────────────

    #[test]
    fn quality_default_is_good() {
        let msg = parse(r#"{"ioa": 1, "value": 0}"#);
        assert_eq!(msg.quality, QualityField::Good);
    }

    #[test]
    fn quality_invalid() {
        let msg = parse(r#"{"ioa": 1, "value": 0, "quality": "invalid"}"#);
        assert_eq!(msg.quality, QualityField::Invalid);
    }

    #[test]
    fn quality_not_topical() {
        let msg = parse(r#"{"ioa": 1, "value": 0, "quality": "not_topical"}"#);
        assert_eq!(msg.quality, QualityField::NotTopical);
    }

    #[test]
    fn quality_substituted() {
        let msg = parse(r#"{"ioa": 1, "value": 0, "quality": "substituted"}"#);
        assert_eq!(msg.quality, QualityField::Substituted);
    }

    #[test]
    fn quality_blocked() {
        let msg = parse(r#"{"ioa": 1, "value": 0, "quality": "blocked"}"#);
        assert_eq!(msg.quality, QualityField::Blocked);
    }

    #[test]
    fn quality_overflow() {
        let msg = parse(r#"{"ioa": 1, "value": 0, "quality": "overflow"}"#);
        assert_eq!(msg.quality, QualityField::Overflow);
    }

    // ── cot field ─────────────────────────────────────────────────────────────

    #[test]
    fn cot_default_is_spontaneous() {
        let msg = parse(r#"{"ioa": 1, "value": 0}"#);
        assert_eq!(msg.cot, CotField::Spontaneous);
    }

    #[test]
    fn cot_periodic() {
        let msg = parse(r#"{"ioa": 1, "value": 0, "cot": "periodic"}"#);
        assert_eq!(msg.cot, CotField::Periodic);
    }

    #[test]
    fn cot_background_scan() {
        let msg = parse(r#"{"ioa": 1, "value": 0, "cot": "background_scan"}"#);
        assert_eq!(msg.cot, CotField::BackgroundScan);
    }

    #[test]
    fn cot_interrogated() {
        let msg = parse(r#"{"ioa": 1, "value": 0, "cot": "interrogated"}"#);
        assert_eq!(msg.cot, CotField::Interrogated);
    }

    #[test]
    fn cot_return_info_remote() {
        let msg = parse(r#"{"ioa": 1, "value": 0, "cot": "return_info_remote"}"#);
        assert_eq!(msg.cot, CotField::ReturnInfoRemote);
    }

    #[test]
    fn cot_return_info_local() {
        let msg = parse(r#"{"ioa": 1, "value": 0, "cot": "return_info_local"}"#);
        assert_eq!(msg.cot, CotField::ReturnInfoLocal);
    }

    // ── edge cases ────────────────────────────────────────────────────────────

    #[test]
    fn missing_ioa_fails() {
        assert!(serde_json::from_str::<Iec104Message>(r#"{"value": 1.0}"#).is_err());
    }

    #[test]
    fn missing_value_fails() {
        assert!(serde_json::from_str::<Iec104Message>(r#"{"ioa": 1}"#).is_err());
    }

    #[test]
    fn unknown_quality_fails() {
        assert!(
            serde_json::from_str::<Iec104Message>(r#"{"ioa": 1, "value": 0, "quality": "bogus"}"#).is_err()
        );
    }

    #[test]
    fn full_message_round_trip() {
        let msg = parse(
            r#"{"ioa": 100, "value": 42.5, "type": "float", "ca": 1, "quality": "good", "cot": "spontaneous"}"#,
        );
        assert_eq!(msg.ioa, 100);
        assert_eq!(msg.value, DataValue::Number(42.5));
        assert_eq!(msg.data_type, Some(DataType::Float));
        assert_eq!(msg.ca, Some(1));
        assert_eq!(msg.quality, QualityField::Good);
        assert_eq!(msg.cot, CotField::Spontaneous);
    }
}
