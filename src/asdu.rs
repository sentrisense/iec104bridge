// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sentrisense

use lib60870::sys;
use lib60870::time::Timestamp;
use lib60870::types::Quality;

use crate::bridge::TimedDispatch;
use crate::message::{DataType, DataValue};

pub fn enqueue_timed_asdu(message: TimedDispatch<'_>) -> bool {
    let Some(server_ptr) = message.server_ptr else {
        return false;
    };
    let app_layer_params = unsafe { sys::CS104_Slave_getAppLayerParameters(server_ptr) };
    let asdu = unsafe {
        sys::CS101_ASDU_create(
            app_layer_params,
            false,
            message.cot.as_raw(),
            0,
            message.ca as i32,
            false,
            false,
        )
    };
    if asdu.is_null() {
        return false;
    }

    let io = unsafe {
        create_information_object(
            message.ioa,
            message.value,
            message.data_type,
            message.quality,
            message.timestamp,
        )
    };
    if io.is_null() {
        unsafe { sys::CS101_ASDU_destroy(asdu) };
        return false;
    }

    unsafe {
        sys::CS101_ASDU_addInformationObject(asdu, io);
        sys::InformationObject_destroy(io);
        sys::CS104_Slave_enqueueASDU(server_ptr, asdu);
        sys::CS101_ASDU_destroy(asdu);
    }

    true
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
