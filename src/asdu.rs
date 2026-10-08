// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sentrisense

use lib60870::sys;
use lib60870::time::Timestamp;
use lib60870::types::Quality;

use crate::bridge::TimedDispatch;
use crate::message::{DataType, DataValue};

/// Self-referencing heap copy; `lib60870::Asdu` pointers dangle after its move.
pub struct PinnedAsdu(Box<sys::sCS101_StaticASDU>);

impl PinnedAsdu {
    pub fn copy_of(asdu: &lib60870::Asdu) -> Self {
        let mut inner =
            Box::new(unsafe { std::ptr::read(asdu.as_ptr() as *const sys::sCS101_StaticASDU) });
        let base = inner.encodedData.as_mut_ptr();
        inner.asdu = base;
        inner.payload = unsafe { base.add(inner.asduHeaderLength as usize) };
        Self(inner)
    }

    pub fn as_ptr(&self) -> sys::CS101_ASDU {
        &*self.0 as *const sys::sCS101_StaticASDU as sys::CS101_ASDU
    }
}

/// Untimed value already converted to its IEC-104 encoding.
pub enum UntimedValue {
    Single(bool),
    Float(f32),
    Scaled(i16),
}

pub fn enqueue_timed_asdu(message: TimedDispatch<'_>) -> bool {
    let Some(server_ptr) = message.server_ptr else {
        return false;
    };
    let al_params = unsafe { sys::CS104_Slave_getAppLayerParameters(server_ptr) };
    let Some(asdu) = (unsafe { timed_asdu(al_params, &message) }) else {
        return false;
    };
    unsafe {
        sys::CS104_Slave_enqueueASDU(server_ptr, asdu);
        sys::CS101_ASDU_destroy(asdu);
    }
    true
}

/// Send a timed point on one master connection, ordered with its ACT_CON/ACT_TERM.
pub fn send_timed_to_connection(conn: sys::IMasterConnection, message: &TimedDispatch<'_>) -> bool {
    let al_params = unsafe { sys::IMasterConnection_getApplicationLayerParameters(conn) };
    let Some(asdu) = (unsafe { timed_asdu(al_params, message) }) else {
        return false;
    };
    send_to_connection(conn, asdu)
}

/// Send an untimed point on one master connection, ordered with its ACT_CON/ACT_TERM.
pub fn send_untimed_to_connection(
    conn: sys::IMasterConnection,
    cot: lib60870::types::CauseOfTransmission,
    ca: u16,
    ioa: u32,
    value: UntimedValue,
    quality: Quality,
) -> bool {
    let al_params = unsafe { sys::IMasterConnection_getApplicationLayerParameters(conn) };
    let io = unsafe {
        match value {
            UntimedValue::Single(v) => sys::SinglePointInformation_create(
                std::ptr::null_mut(),
                ioa as i32,
                v,
                quality.bits(),
            ) as sys::InformationObject,
            UntimedValue::Float(v) => {
                sys::MeasuredValueShort_create(std::ptr::null_mut(), ioa as i32, v, quality.bits())
                    as sys::InformationObject
            }
            UntimedValue::Scaled(v) => sys::MeasuredValueScaled_create(
                std::ptr::null_mut(),
                ioa as i32,
                i32::from(v),
                quality.bits(),
            ) as sys::InformationObject,
        }
    };
    let Some(asdu) = (unsafe { single_object_asdu(al_params, cot, ca, io) }) else {
        return false;
    };
    send_to_connection(conn, asdu)
}

fn send_to_connection(conn: sys::IMasterConnection, asdu: sys::CS101_ASDU) -> bool {
    unsafe {
        let sent = sys::IMasterConnection_sendASDU(conn, asdu);
        sys::CS101_ASDU_destroy(asdu);
        sent
    }
}

unsafe fn timed_asdu(
    al_params: sys::CS101_AppLayerParameters,
    message: &TimedDispatch<'_>,
) -> Option<sys::CS101_ASDU> {
    let io = unsafe {
        create_information_object(
            message.ioa,
            message.value,
            message.data_type,
            message.quality,
            message.timestamp,
        )
    };
    unsafe { single_object_asdu(al_params, message.cot, message.ca, io) }
}

/// Wrap one information object in a new ASDU; the caller destroys the ASDU.
unsafe fn single_object_asdu(
    al_params: sys::CS101_AppLayerParameters,
    cot: lib60870::types::CauseOfTransmission,
    ca: u16,
    io: sys::InformationObject,
) -> Option<sys::CS101_ASDU> {
    if io.is_null() {
        return None;
    }
    let asdu = unsafe {
        sys::CS101_ASDU_create(al_params, false, cot.as_raw(), 0, ca as i32, false, false)
    };
    if asdu.is_null() {
        unsafe { sys::InformationObject_destroy(io) };
        return None;
    }
    unsafe {
        sys::CS101_ASDU_addInformationObject(asdu, io);
        sys::InformationObject_destroy(io);
    }
    Some(asdu)
}

unsafe fn create_information_object(
    ioa: u32,
    value: &DataValue,
    data_type: DataType,
    quality: Quality,
    timestamp: &Timestamp,
) -> sys::InformationObject {
    let ts = timestamp.as_raw() as *const _ as sys::CP56Time2a;
    match data_type {
        DataType::SinglePoint => unsafe {
            sys::SinglePointWithCP56Time2a_create(
                std::ptr::null_mut(),
                ioa as i32,
                matches!(value, DataValue::Bool(true))
                    || matches!(value, DataValue::Number(number) if *number != 0.0),
                quality.bits(),
                ts,
            ) as sys::InformationObject
        },
        DataType::DoublePoint => unsafe {
            sys::DoublePointWithCP56Time2a_create(
                std::ptr::null_mut(),
                ioa as i32,
                if matches!(value, DataValue::Bool(true))
                    || matches!(value, DataValue::Number(number) if *number != 0.0)
                {
                    sys::DoublePointValue_IEC60870_DOUBLE_POINT_ON
                } else {
                    sys::DoublePointValue_IEC60870_DOUBLE_POINT_OFF
                },
                quality.bits(),
                ts,
            ) as sys::InformationObject
        },
        DataType::Scaled => unsafe {
            sys::MeasuredValueScaledWithCP56Time2a_create(
                std::ptr::null_mut(),
                ioa as i32,
                match value {
                    DataValue::Bool(true) => 1,
                    DataValue::Bool(false) => 0,
                    DataValue::Number(number) => {
                        number.clamp(i16::MIN as f64, i16::MAX as f64) as i32
                    }
                },
                quality.bits(),
                ts,
            ) as sys::InformationObject
        },
        DataType::Float => unsafe {
            sys::MeasuredValueShortWithCP56Time2a_create(
                std::ptr::null_mut(),
                ioa as i32,
                match value {
                    DataValue::Bool(true) => 1.0,
                    DataValue::Bool(false) => 0.0,
                    DataValue::Number(number) => *number as f32,
                },
                quality.bits(),
                ts,
            ) as sys::InformationObject
        },
        DataType::Normalized => unsafe {
            sys::MeasuredValueNormalizedWithCP56Time2a_create(
                std::ptr::null_mut(),
                ioa as i32,
                match value {
                    DataValue::Bool(true) => 1.0,
                    DataValue::Bool(false) => 0.0,
                    DataValue::Number(number) => *number as f32,
                },
                quality.bits(),
                ts,
            ) as sys::InformationObject
        },
    }
}
