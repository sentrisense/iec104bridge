// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sentrisense
//
//! Shared semantic validation for incoming IEC-104 messages.

use crate::message::{DataType, DataValue, Iec104Message};

pub fn validate_message(msg: &Iec104Message) -> anyhow::Result<()> {
    validate_ioa(msg.ioa)?;
    validate_ca(msg.ca)?;
    validate_normalized_value(msg.data_type, &msg.value)?;
    validate_boolean_point_value(msg.data_type, &msg.value)?;

    Ok(())
}

fn validate_ioa(ioa: u32) -> anyhow::Result<()> {
    if !(1..=16_777_215).contains(&ioa) {
        anyhow::bail!("ioa must be in the range 1..=16777215");
    }

    Ok(())
}

fn validate_ca(ca: Option<u16>) -> anyhow::Result<()> {
    if let Some(ca) = ca
        && !(1..=65_534).contains(&ca)
    {
        anyhow::bail!("ca must be in the range 1..=65534");
    }

    Ok(())
}

fn validate_normalized_value(data_type: Option<DataType>, value: &DataValue) -> anyhow::Result<()> {
    if data_type != Some(DataType::Normalized) {
        return Ok(());
    }

    match value {
        DataValue::Number(value) if (-1.0..=1.0).contains(value) => Ok(()),
        DataValue::Number(_) => anyhow::bail!("normalized values must be in the range -1.0..=1.0"),
        DataValue::Bool(_) => anyhow::bail!("normalized values must be numeric"),
    }
}

fn validate_boolean_point_value(
    data_type: Option<DataType>,
    value: &DataValue,
) -> anyhow::Result<()> {
    if !matches!(
        data_type,
        Some(DataType::SinglePoint | DataType::DoublePoint)
    ) {
        return Ok(());
    }

    if matches!(value, DataValue::Bool(_)) {
        return Ok(());
    }

    anyhow::bail!("single_point and double_point values must be boolean")
}

#[cfg(test)]
mod tests {
    use super::validate_message;
    use crate::message::{CotField, DataType, DataValue, Iec104Message, QualityField};

    fn base_message() -> Iec104Message {
        Iec104Message {
            ioa: 1,
            value: DataValue::Number(42.0),
            data_type: Some(DataType::Float),
            ca: Some(1),
            quality: QualityField::Good,
            cot: CotField::Spontaneous,
        }
    }

    #[test]
    fn accepts_valid_message() {
        assert!(validate_message(&base_message()).is_ok());
    }

    #[test]
    fn rejects_zero_ioa() {
        let mut msg = base_message();
        msg.ioa = 0;
        assert!(validate_message(&msg).is_err());
    }

    #[test]
    fn rejects_invalid_ca() {
        let mut msg = base_message();
        msg.ca = Some(0);
        assert!(validate_message(&msg).is_err());
    }

    #[test]
    fn rejects_out_of_range_normalized_value() {
        let mut msg = base_message();
        msg.data_type = Some(DataType::Normalized);
        msg.value = DataValue::Number(1.5);
        assert!(validate_message(&msg).is_err());
    }

    #[test]
    fn rejects_numeric_single_point() {
        let mut msg = base_message();
        msg.data_type = Some(DataType::SinglePoint);
        assert!(validate_message(&msg).is_err());
    }
}
