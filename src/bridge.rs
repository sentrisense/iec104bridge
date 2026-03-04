// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sentrisense
////! Converts [`Iec104Message`] values into lib60870 Server calls.
//!
//! The main entry-point is [`dispatch`], which is generic over any [`DataSink`]
//! so that mock sinks can be used in unit tests without spinning up a real
//! IEC-104 server.
//!
//! For production use, wrap the real [`lib60870::server::Server`] in a
//! [`LiveSink`]:
//!
//! ```ignore
//! bridge::dispatch(&LiveSink(&server), &msg, default_ca);
//! ```

use lib60870::server::Server;
use lib60870::types::{CauseOfTransmission, Quality};
use tracing::{debug, warn};

use crate::message::{CotField, DataType, DataValue, Iec104Message, QualityField};

// ─── DataSink trait ───────────────────────────────────────────────────────────

/// Abstraction over anything that can receive IEC-104 data objects.
///
/// The three methods mirror the convenience helpers on [`lib60870::server::Server`].
/// Production code uses [`LiveSink`]; test code uses `CapturingSink` (defined in
/// `#[cfg(test)]`).
pub trait DataSink {
    fn send_single_point(
        &self,
        cot: CauseOfTransmission,
        ca: u16,
        ioa: u32,
        value: bool,
        quality: Quality,
    );

    fn send_measured_float(
        &self,
        cot: CauseOfTransmission,
        ca: u16,
        ioa: u32,
        value: f32,
        quality: Quality,
    );

    fn send_measured_scaled(
        &self,
        cot: CauseOfTransmission,
        ca: u16,
        ioa: u32,
        value: i16,
        quality: Quality,
    );
}

// ─── LiveSink ─────────────────────────────────────────────────────────────────

/// Wraps a real [`Server`] reference and implements [`DataSink`].
///
/// Using a newtype avoids ambiguity between the trait methods and the inherent
/// `send_*` methods on `Server` (which have identical names).
pub struct LiveSink<'a>(pub &'a Server);

impl DataSink for LiveSink<'_> {
    fn send_single_point(
        &self,
        cot: CauseOfTransmission,
        ca: u16,
        ioa: u32,
        value: bool,
        quality: Quality,
    ) {
        self.0.send_single_point(cot, ca, ioa, value, quality);
    }

    fn send_measured_float(
        &self,
        cot: CauseOfTransmission,
        ca: u16,
        ioa: u32,
        value: f32,
        quality: Quality,
    ) {
        self.0.send_measured_float(cot, ca, ioa, value, quality);
    }

    fn send_measured_scaled(
        &self,
        cot: CauseOfTransmission,
        ca: u16,
        ioa: u32,
        value: i16,
        quality: Quality,
    ) {
        self.0.send_measured_scaled(cot, ca, ioa, value, quality);
    }
}

// ─── quality mapping ──────────────────────────────────────────────────────────

/// Convert a JSON-friendly [`QualityField`] to a [`Quality`] bitflag value.
pub fn map_quality(q: QualityField) -> Quality {
    match q {
        QualityField::Good => Quality::GOOD,
        QualityField::Invalid => Quality::INVALID,
        QualityField::NotTopical => Quality::NOT_TOPICAL,
        QualityField::Substituted => Quality::SUBSTITUTED,
        QualityField::Blocked => Quality::BLOCKED,
        QualityField::Overflow => Quality::OVERFLOW,
    }
}

// ─── cause-of-transmission mapping ───────────────────────────────────────────

/// Convert a JSON-friendly [`CotField`] to a [`CauseOfTransmission`].
pub fn map_cot(cot: CotField) -> CauseOfTransmission {
    match cot {
        CotField::Spontaneous => CauseOfTransmission::Spontaneous,
        CotField::Periodic => CauseOfTransmission::Periodic,
        CotField::BackgroundScan => CauseOfTransmission::Background,
        CotField::Interrogated => CauseOfTransmission::InterrogatedByStation,
        CotField::ReturnInfoRemote => CauseOfTransmission::ReturnRemote,
        CotField::ReturnInfoLocal => CauseOfTransmission::ReturnLocal,
    }
}

// ─── type inference ───────────────────────────────────────────────────────────

/// Infer the effective [`DataType`] when the JSON message omits the `type`
/// field.
///
/// Rules:
/// * boolean value → `SinglePoint`
/// * numeric value with a zero fractional part that fits in `i16` → `Scaled`
/// * any other numeric value → `Float`
fn infer_type(value: &DataValue) -> DataType {
    match value {
        DataValue::Bool(_) => DataType::SinglePoint,
        DataValue::Number(n) => {
            if n.fract() == 0.0 && *n >= i16::MIN as f64 && *n <= i16::MAX as f64 {
                DataType::Scaled
            } else {
                DataType::Float
            }
        }
    }
}

// ─── dispatch ─────────────────────────────────────────────────────────────────

/// Translate one [`Iec104Message`] into a [`DataSink`] call.
///
/// Generic over any `DataSink` so that the function can be exercised in unit
/// tests without a real IEC-104 server.  In production, pass a [`LiveSink`].
///
/// The `default_ca` is used when the message does not include a `ca` field.
pub fn dispatch<S: DataSink>(sink: &S, msg: &Iec104Message, default_ca: u16) {
    let ca = msg.ca.unwrap_or(default_ca);
    let ioa = msg.ioa;
    let quality = map_quality(msg.quality);
    let cot = map_cot(msg.cot);
    let data_type = msg.data_type.unwrap_or_else(|| infer_type(&msg.value));

    debug!(ioa, ca, ?data_type, ?cot, ?quality, "dispatching IEC-104 message");

    match (data_type, &msg.value) {
        // ── SinglePoint ───────────────────────────────────────────────────────
        (DataType::SinglePoint, DataValue::Bool(v)) => {
            sink.send_single_point(cot, ca, ioa, *v, quality);
        }
        (DataType::SinglePoint, DataValue::Number(n)) => {
            // Treat any non-zero number as ON.
            sink.send_single_point(cot, ca, ioa, *n != 0.0, quality);
        }

        // ── Float / Normalised (both use the same floating-point ASDU type) ──
        (DataType::Float | DataType::Normalized, DataValue::Number(n)) => {
            sink.send_measured_float(cot, ca, ioa, *n as f32, quality);
        }
        (DataType::Float | DataType::Normalized, DataValue::Bool(b)) => {
            sink.send_measured_float(cot, ca, ioa, if *b { 1.0 } else { 0.0 }, quality);
        }

        // ── Scaled ────────────────────────────────────────────────────────────
        (DataType::Scaled, DataValue::Number(n)) => {
            // Saturate to i16 range.
            let v = n.clamp(i16::MIN as f64, i16::MAX as f64) as i16;
            sink.send_measured_scaled(cot, ca, ioa, v, quality);
        }
        (DataType::Scaled, DataValue::Bool(b)) => {
            sink.send_measured_scaled(cot, ca, ioa, if *b { 1 } else { 0 }, quality);
        }

        // ── DoublePoint (fall back to single-point for now) ───────────────────
        (DataType::DoublePoint, DataValue::Bool(v)) => {
            warn!(
                ioa, ca,
                "DoublePoint not natively supported via convenience API; sending as SinglePoint"
            );
            sink.send_single_point(cot, ca, ioa, *v, quality);
        }
        (DataType::DoublePoint, DataValue::Number(n)) => {
            warn!(
                ioa, ca,
                "DoublePoint not natively supported via convenience API; sending as SinglePoint"
            );
            sink.send_single_point(cot, ca, ioa, *n != 0.0, quality);
        }
    }
}

// ─── test support ─────────────────────────────────────────────────────────────

/// Shared test fixtures, exposed to the whole crate under `#[cfg(test)]`.
///
/// Placing them in a `pub(crate)` module (instead of inside `mod tests`) lets
/// other modules – most notably `e2e_tests` – reuse `CapturingSink` and
/// `SentCall` without duplicating the implementation.
#[cfg(test)]
pub(crate) mod test_support {
    use std::cell::RefCell;

    use lib60870::types::{CauseOfTransmission, Quality};

    use crate::bridge::DataSink;

    /// Records every call made through [`DataSink`] so tests can assert on the
    /// exact sequence and arguments.
    #[derive(Debug, PartialEq)]
    pub enum SentCall {
        SinglePoint {
            cot: CauseOfTransmission,
            ca: u16,
            ioa: u32,
            value: bool,
            quality: Quality,
        },
        MeasuredFloat {
            cot: CauseOfTransmission,
            ca: u16,
            ioa: u32,
            value: f32,
            quality: Quality,
        },
        MeasuredScaled {
            cot: CauseOfTransmission,
            ca: u16,
            ioa: u32,
            value: i16,
            quality: Quality,
        },
    }

    #[derive(Default)]
    pub struct CapturingSink {
        pub calls: RefCell<Vec<SentCall>>,
    }

    impl DataSink for CapturingSink {
        fn send_single_point(
            &self,
            cot: CauseOfTransmission,
            ca: u16,
            ioa: u32,
            value: bool,
            quality: Quality,
        ) {
            self.calls
                .borrow_mut()
                .push(SentCall::SinglePoint { cot, ca, ioa, value, quality });
        }

        fn send_measured_float(
            &self,
            cot: CauseOfTransmission,
            ca: u16,
            ioa: u32,
            value: f32,
            quality: Quality,
        ) {
            self.calls
                .borrow_mut()
                .push(SentCall::MeasuredFloat { cot, ca, ioa, value, quality });
        }

        fn send_measured_scaled(
            &self,
            cot: CauseOfTransmission,
            ca: u16,
            ioa: u32,
            value: i16,
            quality: Quality,
        ) {
            self.calls
                .borrow_mut()
                .push(SentCall::MeasuredScaled { cot, ca, ioa, value, quality });
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use lib60870::types::{CauseOfTransmission, Quality};

    use super::*;
    use super::test_support::{CapturingSink, SentCall};
    use crate::message::{CotField, DataType, DataValue, Iec104Message, QualityField};

    // ── helpers ───────────────────────────────────────────────────────────────

    fn make_msg(
        ioa: u32,
        value: DataValue,
        data_type: Option<DataType>,
        quality: QualityField,
        cot: CotField,
        ca: Option<u16>,
    ) -> Iec104Message {
        Iec104Message { ioa, value, data_type, quality, cot, ca }
    }

    fn simple_float(ioa: u32, v: f64) -> Iec104Message {
        make_msg(ioa, DataValue::Number(v), Some(DataType::Float), QualityField::Good, CotField::Spontaneous, None)
    }

    fn simple_bool(ioa: u32, v: bool) -> Iec104Message {
        make_msg(ioa, DataValue::Bool(v), Some(DataType::SinglePoint), QualityField::Good, CotField::Spontaneous, None)
    }

    // ── map_quality ───────────────────────────────────────────────────────────

    #[test]
    fn map_quality_good() {
        assert_eq!(map_quality(QualityField::Good), Quality::GOOD);
    }

    #[test]
    fn map_quality_invalid() {
        assert_eq!(map_quality(QualityField::Invalid), Quality::INVALID);
    }

    #[test]
    fn map_quality_not_topical() {
        assert_eq!(map_quality(QualityField::NotTopical), Quality::NOT_TOPICAL);
    }

    #[test]
    fn map_quality_substituted() {
        assert_eq!(map_quality(QualityField::Substituted), Quality::SUBSTITUTED);
    }

    #[test]
    fn map_quality_blocked() {
        assert_eq!(map_quality(QualityField::Blocked), Quality::BLOCKED);
    }

    #[test]
    fn map_quality_overflow() {
        assert_eq!(map_quality(QualityField::Overflow), Quality::OVERFLOW);
    }

    // ── map_cot ───────────────────────────────────────────────────────────────

    #[test]
    fn map_cot_spontaneous() {
        assert_eq!(map_cot(CotField::Spontaneous), CauseOfTransmission::Spontaneous);
    }

    #[test]
    fn map_cot_periodic() {
        assert_eq!(map_cot(CotField::Periodic), CauseOfTransmission::Periodic);
    }

    #[test]
    fn map_cot_background_scan() {
        assert_eq!(map_cot(CotField::BackgroundScan), CauseOfTransmission::Background);
    }

    #[test]
    fn map_cot_interrogated() {
        assert_eq!(map_cot(CotField::Interrogated), CauseOfTransmission::InterrogatedByStation);
    }

    #[test]
    fn map_cot_return_info_remote() {
        assert_eq!(map_cot(CotField::ReturnInfoRemote), CauseOfTransmission::ReturnRemote);
    }

    #[test]
    fn map_cot_return_info_local() {
        assert_eq!(map_cot(CotField::ReturnInfoLocal), CauseOfTransmission::ReturnLocal);
    }

    // ── infer_type (tested indirectly via dispatch with data_type: None) ──────

    #[test]
    fn infer_type_bool_yields_single_point() {
        let msg = make_msg(1, DataValue::Bool(true), None, QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::SinglePoint { value: true, .. }));
    }

    #[test]
    fn infer_type_integer_yields_scaled() {
        // 42 has no fractional part and fits in i16 → Scaled
        let msg = make_msg(1, DataValue::Number(42.0), None, QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::MeasuredScaled { value: 42, .. }));
    }

    #[test]
    fn infer_type_float_yields_measured_float() {
        let msg = make_msg(1, DataValue::Number(3.14), None, QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::MeasuredFloat { .. }));
    }

    // ── dispatch – SinglePoint ────────────────────────────────────────────────

    #[test]
    fn dispatch_single_point_bool_true() {
        let msg = simple_bool(100, true);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        assert_eq!(
            sink.calls.borrow()[0],
            SentCall::SinglePoint {
                cot: CauseOfTransmission::Spontaneous,
                ca: 1,
                ioa: 100,
                value: true,
                quality: Quality::GOOD,
            }
        );
    }

    #[test]
    fn dispatch_single_point_bool_false() {
        let msg = simple_bool(200, false);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::SinglePoint { value: false, .. }));
    }

    #[test]
    fn dispatch_single_point_nonzero_number_is_on() {
        let msg = make_msg(1, DataValue::Number(5.0), Some(DataType::SinglePoint), QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::SinglePoint { value: true, .. }));
    }

    #[test]
    fn dispatch_single_point_zero_number_is_off() {
        let msg = make_msg(1, DataValue::Number(0.0), Some(DataType::SinglePoint), QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::SinglePoint { value: false, .. }));
    }

    // ── dispatch – Float ──────────────────────────────────────────────────────

    #[test]
    fn dispatch_float_number() {
        let msg = simple_float(300, 1.5);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::MeasuredFloat { value, .. } if (value - 1.5_f32).abs() < 1e-6));
    }

    #[test]
    fn dispatch_float_bool_true_is_one() {
        let msg = make_msg(1, DataValue::Bool(true), Some(DataType::Float), QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::MeasuredFloat { value, .. } if value == 1.0));
    }

    #[test]
    fn dispatch_float_bool_false_is_zero() {
        let msg = make_msg(1, DataValue::Bool(false), Some(DataType::Float), QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::MeasuredFloat { value, .. } if value == 0.0));
    }

    // ── dispatch – Normalized (same wire type as Float) ───────────────────────

    #[test]
    fn dispatch_normalized_number() {
        let msg = make_msg(1, DataValue::Number(0.75), Some(DataType::Normalized), QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::MeasuredFloat { .. }));
    }

    // ── dispatch – Scaled ─────────────────────────────────────────────────────

    #[test]
    fn dispatch_scaled_number() {
        let msg = make_msg(1, DataValue::Number(1000.0), Some(DataType::Scaled), QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::MeasuredScaled { value: 1000, .. }));
    }

    #[test]
    fn dispatch_scaled_clamps_to_i16_max() {
        let msg = make_msg(1, DataValue::Number(100_000.0), Some(DataType::Scaled), QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::MeasuredScaled { value: i16::MAX, .. }));
    }

    #[test]
    fn dispatch_scaled_clamps_to_i16_min() {
        let msg = make_msg(1, DataValue::Number(-100_000.0), Some(DataType::Scaled), QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::MeasuredScaled { value: i16::MIN, .. }));
    }

    #[test]
    fn dispatch_scaled_bool_true_is_one() {
        let msg = make_msg(1, DataValue::Bool(true), Some(DataType::Scaled), QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::MeasuredScaled { value: 1, .. }));
    }

    // ── dispatch – DoublePoint fallback ───────────────────────────────────────

    #[test]
    fn dispatch_double_point_bool_falls_back_to_single() {
        let msg = make_msg(1, DataValue::Bool(true), Some(DataType::DoublePoint), QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::SinglePoint { value: true, .. }));
    }

    #[test]
    fn dispatch_double_point_number_falls_back_to_single() {
        let msg = make_msg(1, DataValue::Number(1.0), Some(DataType::DoublePoint), QualityField::Good, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::SinglePoint { value: true, .. }));
    }

    // ── dispatch – CA fall-through ────────────────────────────────────────────

    #[test]
    fn dispatch_uses_message_ca_over_default() {
        let msg = make_msg(1, DataValue::Bool(true), Some(DataType::SinglePoint), QualityField::Good, CotField::Spontaneous, Some(42));
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::SinglePoint { ca: 42, .. }));
    }

    #[test]
    fn dispatch_falls_back_to_default_ca() {
        let msg = simple_bool(1, true); // ca = None
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 99);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::SinglePoint { ca: 99, .. }));
    }

    // ── dispatch – quality propagation ───────────────────────────────────────

    #[test]
    fn dispatch_propagates_invalid_quality() {
        let msg = make_msg(1, DataValue::Bool(true), Some(DataType::SinglePoint), QualityField::Invalid, CotField::Spontaneous, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::SinglePoint { quality, .. } if quality == Quality::INVALID));
    }

    // ── dispatch – COT propagation ────────────────────────────────────────────

    #[test]
    fn dispatch_propagates_periodic_cot() {
        let msg = make_msg(1, DataValue::Bool(true), Some(DataType::SinglePoint), QualityField::Good, CotField::Periodic, None);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::SinglePoint { cot: CauseOfTransmission::Periodic, .. }));
    }
}
