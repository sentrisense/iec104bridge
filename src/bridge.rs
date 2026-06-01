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
use lib60870::time::Timestamp;
use lib60870::types::{CauseOfTransmission, Quality};
use tracing::{debug, warn};

use crate::asdu;
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

    fn enqueue_timed(&self, message: TimedDispatch<'_>);
}

pub struct TimedDispatch<'a> {
    pub server_ptr: Option<lib60870::sys::CS104_Slave>,
    pub cot: CauseOfTransmission,
    pub ca: u16,
    pub ioa: u32,
    pub value: &'a DataValue,
    pub data_type: DataType,
    pub quality: Quality,
    pub timestamp: &'a Timestamp,
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

    fn enqueue_timed(&self, message: TimedDispatch<'_>) {
        let message = TimedDispatch {
            server_ptr: Some(self.0.as_ptr()),
            ..message
        };
        let _ = asdu::enqueue_timed_asdu(message);
    }
}

// ─── quality mapping ──────────────────────────────────────────────────────────

const QUALITY_MAP: [Quality; 6] = [
    Quality::GOOD,
    Quality::INVALID,
    Quality::NOT_TOPICAL,
    Quality::SUBSTITUTED,
    Quality::BLOCKED,
    Quality::OVERFLOW,
];

const COT_MAP: [CauseOfTransmission; 6] = [
    CauseOfTransmission::Spontaneous,
    CauseOfTransmission::Periodic,
    CauseOfTransmission::Background,
    CauseOfTransmission::InterrogatedByStation,
    CauseOfTransmission::ReturnRemote,
    CauseOfTransmission::ReturnLocal,
];

/// Convert a JSON-friendly [`QualityField`] to a [`Quality`] bitflag value.
pub fn map_quality(q: QualityField) -> Quality {
    QUALITY_MAP[q as usize]
}

// ─── cause-of-transmission mapping ───────────────────────────────────────────

/// Convert a JSON-friendly [`CotField`] to a [`CauseOfTransmission`].
pub fn map_cot(cot: CotField) -> CauseOfTransmission {
    COT_MAP[cot as usize]
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
        DataValue::Number(n) if is_scaled_number(*n) => DataType::Scaled,
        DataValue::Number(_) => DataType::Float,
    }
}

fn is_scaled_number(value: f64) -> bool {
    value.fract() == 0.0 && value >= i16::MIN as f64 && value <= i16::MAX as f64
}

// ─── dispatch ─────────────────────────────────────────────────────────────────

/// Translate one [`Iec104Message`] into a [`DataSink`] call.
///
/// Generic over any `DataSink` so that the function can be exercised in unit
/// tests without a real IEC-104 server.  In production, pass a [`LiveSink`].
///
/// The `default_ca` is used when the message does not include a `ca` field.
pub fn dispatch<S: DataSink>(sink: &S, msg: &Iec104Message, default_ca: u16) {
    let context = DispatchContext::from_message(msg, default_ca);

    debug!(
        ioa = context.ioa,
        ca = context.ca,
        ?context.data_type,
        ?context.cot,
        ?context.quality,
        "dispatching IEC-104 message"
    );

    match context.data_type {
        DataType::SinglePoint => dispatch_single_point(sink, &context, &msg.value),
        DataType::Float | DataType::Normalized => dispatch_float_like(sink, &context, &msg.value),
        DataType::Scaled => dispatch_scaled(sink, &context, &msg.value),
        DataType::DoublePoint => dispatch_double_point(sink, &context, &msg.value),
    }
}

struct DispatchContext {
    ca: u16,
    ioa: u32,
    quality: Quality,
    cot: CauseOfTransmission,
    data_type: DataType,
    timestamp: Option<Timestamp>,
}

impl DispatchContext {
    fn from_message(msg: &Iec104Message, default_ca: u16) -> Self {
        Self {
            ca: msg.ca.unwrap_or(default_ca),
            ioa: msg.ioa,
            quality: map_quality(msg.quality),
            cot: map_cot(msg.cot),
            data_type: msg.data_type.unwrap_or_else(|| infer_type(&msg.value)),
            timestamp: msg.timestamp.map(offset_datetime_to_cp56_timestamp),
        }
    }
}

fn offset_datetime_to_cp56_timestamp(timestamp: time::OffsetDateTime) -> Timestamp {
    Timestamp::from_ms(timestamp.unix_timestamp_nanos() as u64 / 1_000_000)
}

fn dispatch_single_point<S: DataSink>(sink: &S, context: &DispatchContext, value: &DataValue) {
    if let Some(timestamp) = context.timestamp.as_ref() {
        sink.enqueue_timed(TimedDispatch {
            server_ptr: None,
            cot: context.cot,
            ca: context.ca,
            ioa: context.ioa,
            value,
            data_type: DataType::SinglePoint,
            quality: context.quality,
            timestamp,
        });
        return;
    }

    sink.send_single_point(
        context.cot,
        context.ca,
        context.ioa,
        as_single_point_value(value),
        context.quality,
    );
}

fn dispatch_float_like<S: DataSink>(sink: &S, context: &DispatchContext, value: &DataValue) {
    if let Some(timestamp) = context.timestamp.as_ref() {
        sink.enqueue_timed(TimedDispatch {
            server_ptr: None,
            cot: context.cot,
            ca: context.ca,
            ioa: context.ioa,
            value,
            data_type: context.data_type,
            quality: context.quality,
            timestamp,
        });
        return;
    }

    sink.send_measured_float(
        context.cot,
        context.ca,
        context.ioa,
        as_float_value(value),
        context.quality,
    );
}

fn dispatch_scaled<S: DataSink>(sink: &S, context: &DispatchContext, value: &DataValue) {
    if let Some(timestamp) = context.timestamp.as_ref() {
        sink.enqueue_timed(TimedDispatch {
            server_ptr: None,
            cot: context.cot,
            ca: context.ca,
            ioa: context.ioa,
            value,
            data_type: DataType::Scaled,
            quality: context.quality,
            timestamp,
        });
        return;
    }

    sink.send_measured_scaled(
        context.cot,
        context.ca,
        context.ioa,
        as_scaled_value(value),
        context.quality,
    );
}

fn dispatch_double_point<S: DataSink>(sink: &S, context: &DispatchContext, value: &DataValue) {
    warn!(
        ioa = context.ioa,
        ca = context.ca,
        "DoublePoint not natively supported via convenience API; sending as SinglePoint"
    );
    dispatch_single_point(sink, context, value);
}

fn as_single_point_value(value: &DataValue) -> bool {
    match value {
        DataValue::Bool(value) => *value,
        DataValue::Number(value) => *value != 0.0,
    }
}

fn as_float_value(value: &DataValue) -> f32 {
    match value {
        DataValue::Bool(value) => {
            if *value {
                1.0
            } else {
                0.0
            }
        }
        DataValue::Number(value) => *value as f32,
    }
}

fn as_scaled_value(value: &DataValue) -> i16 {
    match value {
        DataValue::Bool(value) => {
            if *value {
                1
            } else {
                0
            }
        }
        DataValue::Number(value) => value.clamp(i16::MIN as f64, i16::MAX as f64) as i16,
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

    use lib60870::types::{CauseOfTransmission, Quality, TypeId};

    use crate::bridge::{DataSink, TimedDispatch};
    use crate::message::{DataType, DataValue};

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
        Timed {
            cot: CauseOfTransmission,
            ca: u16,
            ioa: u32,
            data_type: TypeId,
            value: DataValue,
            quality: Quality,
            timestamp_ms: u64,
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
            self.calls.borrow_mut().push(SentCall::SinglePoint {
                cot,
                ca,
                ioa,
                value,
                quality,
            });
        }

        fn send_measured_float(
            &self,
            cot: CauseOfTransmission,
            ca: u16,
            ioa: u32,
            value: f32,
            quality: Quality,
        ) {
            self.calls.borrow_mut().push(SentCall::MeasuredFloat {
                cot,
                ca,
                ioa,
                value,
                quality,
            });
        }

        fn send_measured_scaled(
            &self,
            cot: CauseOfTransmission,
            ca: u16,
            ioa: u32,
            value: i16,
            quality: Quality,
        ) {
            self.calls.borrow_mut().push(SentCall::MeasuredScaled {
                cot,
                ca,
                ioa,
                value,
                quality,
            });
        }

        fn enqueue_timed(&self, message: TimedDispatch<'_>) {
            let type_id = match message.data_type {
                DataType::SinglePoint => TypeId::SinglePointTime,
                DataType::DoublePoint => TypeId::DoublePointTime,
                DataType::Scaled => TypeId::MeasuredScaledTime,
                DataType::Float | DataType::Normalized => TypeId::MeasuredFloatTime,
            };
            self.calls.borrow_mut().push(SentCall::Timed {
                cot: message.cot,
                ca: message.ca,
                ioa: message.ioa,
                data_type: type_id,
                value: message.value.clone(),
                quality: message.quality,
                timestamp_ms: message.timestamp.as_ms(),
            });
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use lib60870::types::{CauseOfTransmission, Quality, TypeId};

    use super::test_support::{CapturingSink, SentCall};
    use super::*;
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
        Iec104Message {
            ioa,
            value,
            data_type,
            quality,
            cot,
            ca,
            timestamp: None,
        }
    }

    fn simple_float(ioa: u32, v: f64) -> Iec104Message {
        make_msg(
            ioa,
            DataValue::Number(v),
            Some(DataType::Float),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        )
    }

    fn simple_bool(ioa: u32, v: bool) -> Iec104Message {
        make_msg(
            ioa,
            DataValue::Bool(v),
            Some(DataType::SinglePoint),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        )
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
        assert_eq!(
            map_cot(CotField::Spontaneous),
            CauseOfTransmission::Spontaneous
        );
    }

    #[test]
    fn map_cot_periodic() {
        assert_eq!(map_cot(CotField::Periodic), CauseOfTransmission::Periodic);
    }

    #[test]
    fn map_cot_background_scan() {
        assert_eq!(
            map_cot(CotField::BackgroundScan),
            CauseOfTransmission::Background
        );
    }

    #[test]
    fn map_cot_interrogated() {
        assert_eq!(
            map_cot(CotField::Interrogated),
            CauseOfTransmission::InterrogatedByStation
        );
    }

    #[test]
    fn map_cot_return_info_remote() {
        assert_eq!(
            map_cot(CotField::ReturnInfoRemote),
            CauseOfTransmission::ReturnRemote
        );
    }

    #[test]
    fn map_cot_return_info_local() {
        assert_eq!(
            map_cot(CotField::ReturnInfoLocal),
            CauseOfTransmission::ReturnLocal
        );
    }

    // ── infer_type (tested indirectly via dispatch with data_type: None) ──────

    #[test]
    fn infer_type_bool_yields_single_point() {
        let msg = make_msg(
            1,
            DataValue::Bool(true),
            None,
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(
            calls[0],
            SentCall::SinglePoint { value: true, .. }
        ));
    }

    #[test]
    fn infer_type_integer_yields_scaled() {
        // 42 has no fractional part and fits in i16 → Scaled
        let msg = make_msg(
            1,
            DataValue::Number(42.0),
            None,
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(
            calls[0],
            SentCall::MeasuredScaled { value: 42, .. }
        ));
    }

    #[test]
    fn infer_type_float_yields_measured_float() {
        let msg = make_msg(
            1,
            DataValue::Number(1.5),
            None,
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
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
        assert!(matches!(
            calls[0],
            SentCall::SinglePoint { value: false, .. }
        ));
    }

    #[test]
    fn dispatch_single_point_nonzero_number_is_on() {
        let msg = make_msg(
            1,
            DataValue::Number(5.0),
            Some(DataType::SinglePoint),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(
            calls[0],
            SentCall::SinglePoint { value: true, .. }
        ));
    }

    #[test]
    fn dispatch_single_point_zero_number_is_off() {
        let msg = make_msg(
            1,
            DataValue::Number(0.0),
            Some(DataType::SinglePoint),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(
            calls[0],
            SentCall::SinglePoint { value: false, .. }
        ));
    }

    // ── dispatch – Float ──────────────────────────────────────────────────────

    #[test]
    fn dispatch_float_number() {
        let msg = simple_float(300, 1.5);
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(
            matches!(calls[0], SentCall::MeasuredFloat { value, .. } if (value - 1.5_f32).abs() < 1e-6)
        );
    }

    #[test]
    fn dispatch_float_bool_true_is_one() {
        let msg = make_msg(
            1,
            DataValue::Bool(true),
            Some(DataType::Float),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::MeasuredFloat { value, .. } if value == 1.0));
    }

    #[test]
    fn dispatch_float_bool_false_is_zero() {
        let msg = make_msg(
            1,
            DataValue::Bool(false),
            Some(DataType::Float),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::MeasuredFloat { value, .. } if value == 0.0));
    }

    // ── dispatch – Normalized (same wire type as Float) ───────────────────────

    #[test]
    fn dispatch_normalized_number() {
        let msg = make_msg(
            1,
            DataValue::Number(0.75),
            Some(DataType::Normalized),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(calls[0], SentCall::MeasuredFloat { .. }));
    }

    // ── dispatch – Scaled ─────────────────────────────────────────────────────

    #[test]
    fn dispatch_scaled_number() {
        let msg = make_msg(
            1,
            DataValue::Number(1000.0),
            Some(DataType::Scaled),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(
            calls[0],
            SentCall::MeasuredScaled { value: 1000, .. }
        ));
    }

    #[test]
    fn dispatch_scaled_clamps_to_i16_max() {
        let msg = make_msg(
            1,
            DataValue::Number(100_000.0),
            Some(DataType::Scaled),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(
            calls[0],
            SentCall::MeasuredScaled {
                value: i16::MAX,
                ..
            }
        ));
    }

    #[test]
    fn dispatch_scaled_clamps_to_i16_min() {
        let msg = make_msg(
            1,
            DataValue::Number(-100_000.0),
            Some(DataType::Scaled),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(
            calls[0],
            SentCall::MeasuredScaled {
                value: i16::MIN,
                ..
            }
        ));
    }

    #[test]
    fn dispatch_scaled_bool_true_is_one() {
        let msg = make_msg(
            1,
            DataValue::Bool(true),
            Some(DataType::Scaled),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(
            calls[0],
            SentCall::MeasuredScaled { value: 1, .. }
        ));
    }

    // ── dispatch – DoublePoint fallback ───────────────────────────────────────

    #[test]
    fn dispatch_double_point_bool_falls_back_to_single() {
        let msg = make_msg(
            1,
            DataValue::Bool(true),
            Some(DataType::DoublePoint),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(
            calls[0],
            SentCall::SinglePoint { value: true, .. }
        ));
    }

    #[test]
    fn dispatch_double_point_number_falls_back_to_single() {
        let msg = make_msg(
            1,
            DataValue::Number(1.0),
            Some(DataType::DoublePoint),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(
            calls[0],
            SentCall::SinglePoint { value: true, .. }
        ));
    }

    // ── dispatch – CA fall-through ────────────────────────────────────────────

    #[test]
    fn dispatch_uses_message_ca_over_default() {
        let msg = make_msg(
            1,
            DataValue::Bool(true),
            Some(DataType::SinglePoint),
            QualityField::Good,
            CotField::Spontaneous,
            Some(42),
        );
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
        let msg = make_msg(
            1,
            DataValue::Bool(true),
            Some(DataType::SinglePoint),
            QualityField::Invalid,
            CotField::Spontaneous,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(
            matches!(calls[0], SentCall::SinglePoint { quality, .. } if quality == Quality::INVALID)
        );
    }

    // ── dispatch – COT propagation ────────────────────────────────────────────

    #[test]
    fn dispatch_propagates_periodic_cot() {
        let msg = make_msg(
            1,
            DataValue::Bool(true),
            Some(DataType::SinglePoint),
            QualityField::Good,
            CotField::Periodic,
            None,
        );
        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);
        let calls = sink.calls.borrow();
        assert!(matches!(
            calls[0],
            SentCall::SinglePoint {
                cot: CauseOfTransmission::Periodic,
                ..
            }
        ));
    }

    #[test]
    fn dispatch_timestamped_float_uses_timed_type() {
        let mut msg = simple_float(10, 42.5);
        msg.timestamp = Some(time::OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(1));

        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);

        assert!(matches!(
            sink.calls.borrow()[0],
            SentCall::Timed {
                ioa: 10,
                data_type: TypeId::MeasuredFloatTime,
                ..
            }
        ));
    }

    #[test]
    fn dispatch_timestamped_scaled_uses_timed_type() {
        let mut msg = make_msg(
            11,
            DataValue::Number(5.0),
            Some(DataType::Scaled),
            QualityField::Good,
            CotField::Spontaneous,
            None,
        );
        msg.timestamp = Some(time::OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(2));

        let sink = CapturingSink::default();
        dispatch(&sink, &msg, 1);

        assert!(matches!(
            sink.calls.borrow()[0],
            SentCall::Timed {
                ioa: 11,
                data_type: TypeId::MeasuredScaledTime,
                ..
            }
        ));
    }

    #[test]
    fn dispatch_timestamp_offsets_for_same_instant_produce_same_iec_timestamp() {
        let mut utc_message = simple_float(12, 1.5);
        utc_message.timestamp = Some(
            time::OffsetDateTime::parse(
                "2026-06-01T12:34:56.789Z",
                &time::format_description::well_known::Rfc3339,
            )
            .unwrap(),
        );

        let mut offset_message = simple_float(12, 1.5);
        offset_message.timestamp = Some(
            time::OffsetDateTime::parse(
                "2026-06-01T14:34:56.789+02:00",
                &time::format_description::well_known::Rfc3339,
            )
            .unwrap(),
        );

        let utc_sink = CapturingSink::default();
        dispatch(&utc_sink, &utc_message, 1);
        let offset_sink = CapturingSink::default();
        dispatch(&offset_sink, &offset_message, 1);

        let utc_timestamp_ms = match utc_sink.calls.borrow()[0] {
            SentCall::Timed { timestamp_ms, .. } => timestamp_ms,
            ref call => panic!("expected timed dispatch, got {call:?}"),
        };
        let offset_timestamp_ms = match offset_sink.calls.borrow()[0] {
            SentCall::Timed { timestamp_ms, .. } => timestamp_ms,
            ref call => panic!("expected timed dispatch, got {call:?}"),
        };

        assert_eq!(utc_timestamp_ms, offset_timestamp_ms);
    }
}
