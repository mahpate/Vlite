//! Property validation and encoding.
//!
//! The byte layout and every rule are identical to the existing record
//! metadata encoding (`src\metadata.rs` `validate`/`encode`/`decode` in
//! `vlite`), so graph entries reuse the metadata encoding without a code
//! dependency:
//!
//! `count u32`, then per field in ascending byte order of the name:
//! `name_len u16`, `name`, `type u8`, value — `1` String (`len u32`, UTF-8),
//! `2` Integer (`i64`), `3` Float (`f64`), `4` Boolean (`u8` 0 or 1),
//! `5` GeoPoint (`latitude f64`, `longitude f64`). All integers little-endian.

use super::error::GraphError;
use super::limits::{
    MAX_FIELD_NAME_BYTES, MAX_PROPERTY_BYTES, MAX_PROPERTY_FIELDS, MAX_STRING_VALUE_BYTES,
};
use super::model::{Properties, PropertyValue};

fn field_name_valid(field: &str) -> bool {
    (1..=MAX_FIELD_NAME_BYTES).contains(&field.len())
}

pub(crate) fn validate_field_name(field: &str) -> Result<(), GraphError> {
    if field_name_valid(field) {
        Ok(())
    } else {
        Err(GraphError::InvalidProperties(
            "field names must be 1..256 UTF-8 bytes",
        ))
    }
}

pub(crate) fn validate_value(value: &PropertyValue) -> Result<(), GraphError> {
    match value {
        PropertyValue::Float(value) if !value.is_finite() => {
            Err(GraphError::InvalidProperties("float values must be finite"))
        }
        PropertyValue::String(value) if value.len() > MAX_STRING_VALUE_BYTES => Err(
            GraphError::InvalidProperties("string values exceed 4096 UTF-8 bytes"),
        ),
        PropertyValue::GeoPoint {
            latitude,
            longitude,
        } if !latitude.is_finite()
            || !longitude.is_finite()
            || !(-90.0..=90.0).contains(latitude)
            || !(-180.0..=180.0).contains(longitude) =>
        {
            Err(GraphError::InvalidProperties(
                "geo coordinates must be finite latitude/longitude in range",
            ))
        }
        _ => Ok(()),
    }
}

/// Validates `properties` and returns its encoded length.
pub fn validate(properties: &Properties) -> Result<usize, GraphError> {
    if properties.len() > MAX_PROPERTY_FIELDS {
        return Err(GraphError::InvalidProperties("properties exceed 64 fields"));
    }
    let mut length = 4;
    for (key, value) in properties {
        validate_field_name(key)?;
        validate_value(value)?;
        length += 2
            + key.len()
            + 1
            + match value {
                PropertyValue::String(value) => 4 + value.len(),
                PropertyValue::Integer(_) | PropertyValue::Float(_) => 8,
                PropertyValue::Boolean(_) => 1,
                PropertyValue::GeoPoint { .. } => 16,
            };
        if length > MAX_PROPERTY_BYTES {
            return Err(GraphError::InvalidProperties(
                "encoded properties exceed 65536 bytes",
            ));
        }
    }
    Ok(length)
}

/// Appends the encoding of already validated `properties` to `bytes`.
pub(crate) fn encode_into(properties: &Properties, bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&(properties.len() as u32).to_le_bytes());
    for (key, value) in properties {
        bytes.extend_from_slice(&(key.len() as u16).to_le_bytes());
        bytes.extend_from_slice(key.as_bytes());
        match value {
            PropertyValue::String(value) => {
                bytes.push(1);
                bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
                bytes.extend_from_slice(value.as_bytes());
            }
            PropertyValue::Integer(value) => {
                bytes.push(2);
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            PropertyValue::Float(value) => {
                bytes.push(3);
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            PropertyValue::Boolean(value) => bytes.extend_from_slice(&[4, u8::from(*value)]),
            PropertyValue::GeoPoint {
                latitude,
                longitude,
            } => {
                bytes.push(5);
                bytes.extend_from_slice(&latitude.to_le_bytes());
                bytes.extend_from_slice(&longitude.to_le_bytes());
            }
        }
    }
}

/// Validates and encodes `properties`.
pub fn encode(properties: &Properties) -> Result<Vec<u8>, GraphError> {
    let mut bytes = Vec::with_capacity(validate(properties)?);
    encode_into(properties, &mut bytes);
    Ok(bytes)
}

/// Decodes exactly `bytes`; rejects truncation, trailing bytes, unsorted or
/// duplicate names and every value the validator rejects.
pub fn decode(bytes: &[u8]) -> Result<Properties, GraphError> {
    fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], GraphError> {
        let (value, rest) = bytes
            .split_at_checked(n)
            .ok_or(GraphError::InvalidProperties("truncated properties"))?;
        *bytes = rest;
        Ok(value)
    }
    fn array<const N: usize>(bytes: &mut &[u8]) -> Result<[u8; N], GraphError> {
        let mut value = [0; N];
        value.copy_from_slice(take(bytes, N)?);
        Ok(value)
    }
    if bytes.len() > MAX_PROPERTY_BYTES {
        return Err(GraphError::InvalidProperties("oversized properties"));
    }
    let mut remaining = bytes;
    let count = u32::from_le_bytes(array(&mut remaining)?) as usize;
    if count > MAX_PROPERTY_FIELDS {
        return Err(GraphError::InvalidProperties("too many stored fields"));
    }
    let mut properties = Properties::new();
    let mut previous: Option<&str> = None;
    for _ in 0..count {
        let length = u16::from_le_bytes(array(&mut remaining)?) as usize;
        let key = std::str::from_utf8(take(&mut remaining, length)?)
            .map_err(|_| GraphError::InvalidProperties("invalid field UTF-8"))?;
        if previous.is_some_and(|previous| previous >= key) {
            return Err(GraphError::InvalidProperties(
                "property fields must be strictly sorted and unique",
            ));
        }
        let value = match take(&mut remaining, 1)?[0] {
            1 => {
                let length = u32::from_le_bytes(array(&mut remaining)?) as usize;
                PropertyValue::String(
                    std::str::from_utf8(take(&mut remaining, length)?)
                        .map_err(|_| GraphError::InvalidProperties("invalid value UTF-8"))?
                        .to_owned(),
                )
            }
            2 => PropertyValue::Integer(i64::from_le_bytes(array(&mut remaining)?)),
            3 => PropertyValue::Float(f64::from_le_bytes(array(&mut remaining)?)),
            4 => match take(&mut remaining, 1)?[0] {
                0 => PropertyValue::Boolean(false),
                1 => PropertyValue::Boolean(true),
                _ => return Err(GraphError::InvalidProperties("invalid boolean")),
            },
            5 => PropertyValue::GeoPoint {
                latitude: f64::from_le_bytes(array(&mut remaining)?),
                longitude: f64::from_le_bytes(array(&mut remaining)?),
            },
            _ => return Err(GraphError::InvalidProperties("unknown property type")),
        };
        previous = Some(key);
        properties.insert(key.to_owned(), value);
    }
    if !remaining.is_empty() {
        return Err(GraphError::InvalidProperties("trailing property bytes"));
    }
    validate(&properties)?;
    Ok(properties)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_types() -> Properties {
        Properties::from([
            ("b".into(), PropertyValue::Boolean(true)),
            ("f".into(), PropertyValue::Float(1.5)),
            (
                "g".into(),
                PropertyValue::GeoPoint {
                    latitude: -90.0,
                    longitude: 180.0,
                },
            ),
            ("i".into(), PropertyValue::Integer(-2)),
            ("s".into(), PropertyValue::String("hé".into())),
        ])
    }

    #[test]
    fn empty_properties_encode_as_a_zero_count() {
        assert_eq!(encode(&Properties::new()).unwrap(), [0, 0, 0, 0]);
        assert_eq!(decode(&[0, 0, 0, 0]).unwrap(), Properties::new());
    }

    /// Golden bytes written from the `src\metadata.rs` layout by hand.
    #[test]
    fn encoding_matches_the_record_metadata_layout() {
        let mut expected = vec![5, 0, 0, 0];
        expected.extend_from_slice(&[1, 0, b'b', 4, 1]);
        expected.extend_from_slice(&[1, 0, b'f', 3]);
        expected.extend_from_slice(&1.5f64.to_le_bytes());
        expected.extend_from_slice(&[1, 0, b'g', 5]);
        expected.extend_from_slice(&(-90.0f64).to_le_bytes());
        expected.extend_from_slice(&180.0f64.to_le_bytes());
        expected.extend_from_slice(&[1, 0, b'i', 2]);
        expected.extend_from_slice(&(-2i64).to_le_bytes());
        expected.extend_from_slice(&[1, 0, b's', 1, 3, 0, 0, 0, b'h', 0xC3, 0xA9]);
        let encoded = encode(&all_types()).unwrap();
        assert_eq!(encoded, expected);
        assert_eq!(validate(&all_types()).unwrap(), expected.len());
        assert_eq!(decode(&encoded).unwrap(), all_types());
        let mut boolean_false = vec![1, 0, 0, 0, 1, 0, b'b', 4, 0];
        assert_eq!(
            decode(&boolean_false).unwrap()["b"],
            PropertyValue::Boolean(false)
        );
        boolean_false[8] = 2;
        assert_eq!(
            decode(&boolean_false),
            Err(GraphError::InvalidProperties("invalid boolean"))
        );
    }

    #[test]
    fn every_truncation_and_trailing_byte_is_rejected() {
        let encoded = encode(&all_types()).unwrap();
        for length in 0..encoded.len() {
            assert!(decode(&encoded[..length]).is_err(), "prefix {length}");
        }
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            decode(&trailing),
            Err(GraphError::InvalidProperties("trailing property bytes"))
        );
    }

    #[test]
    fn unsorted_duplicate_and_unknown_fields_are_rejected() {
        let unsorted = [2, 0, 0, 0, 1, 0, b'b', 4, 1, 1, 0, b'a', 4, 1];
        let duplicate = [2, 0, 0, 0, 1, 0, b'a', 4, 1, 1, 0, b'a', 4, 1];
        for bytes in [&unsorted[..], &duplicate[..]] {
            assert_eq!(
                decode(bytes),
                Err(GraphError::InvalidProperties(
                    "property fields must be strictly sorted and unique"
                ))
            );
        }
        assert_eq!(
            decode(&[1, 0, 0, 0, 1, 0, b'a', 6]),
            Err(GraphError::InvalidProperties("unknown property type"))
        );
        assert_eq!(
            decode(&[1, 0, 0, 0, 1, 0, 0xFF, 4, 1]),
            Err(GraphError::InvalidProperties("invalid field UTF-8"))
        );
        assert_eq!(
            decode(&[1, 0, 0, 0, 1, 0, b'a', 1, 1, 0, 0, 0, 0xFF]),
            Err(GraphError::InvalidProperties("invalid value UTF-8"))
        );
        assert_eq!(
            decode(&[0, 0, 0, 0, 0, 0, b'a', 4, 1][..]),
            Err(GraphError::InvalidProperties("trailing property bytes"))
        );
        // A zero-length field name is decodable but invalid.
        assert_eq!(
            decode(&[1, 0, 0, 0, 0, 0, 4, 1]),
            Err(GraphError::InvalidProperties(
                "field names must be 1..256 UTF-8 bytes"
            ))
        );
    }

    #[test]
    fn value_rules_match_metadata() {
        for value in [
            PropertyValue::Float(f64::NAN),
            PropertyValue::Float(f64::INFINITY),
            PropertyValue::String("x".repeat(4097)),
            PropertyValue::GeoPoint {
                latitude: 90.5,
                longitude: 0.0,
            },
            PropertyValue::GeoPoint {
                latitude: 0.0,
                longitude: -180.5,
            },
            PropertyValue::GeoPoint {
                latitude: f64::NAN,
                longitude: 0.0,
            },
        ] {
            let properties = Properties::from([("a".into(), value)]);
            assert!(
                matches!(validate(&properties), Err(GraphError::InvalidProperties(_))),
                "{properties:?}"
            );
            assert!(encode(&properties).is_err());
        }
        let ok = Properties::from([("a".into(), PropertyValue::String("x".repeat(4096)))]);
        assert_eq!(validate(&ok).unwrap(), 4 + 2 + 1 + 1 + 4 + 4096);
        // Non-finite floats smuggled through bytes are rejected by decode.
        let mut nan = vec![1, 0, 0, 0, 1, 0, b'a', 3];
        nan.extend_from_slice(&f64::NAN.to_le_bytes());
        assert_eq!(
            decode(&nan),
            Err(GraphError::InvalidProperties("float values must be finite"))
        );
    }

    #[test]
    fn field_and_size_maxima() {
        let name = "n".repeat(256);
        assert!(validate(&Properties::from([(name, PropertyValue::Boolean(true))])).is_ok());
        let long = "n".repeat(257);
        assert!(validate(&Properties::from([(long, PropertyValue::Boolean(true))])).is_err());
        assert!(
            validate(&Properties::from([(
                String::new(),
                PropertyValue::Boolean(true)
            )]))
            .is_err()
        );

        let sixty_four: Properties = (0..64)
            .map(|i| (format!("f{i:02}"), PropertyValue::Integer(i)))
            .collect();
        assert_eq!(decode(&encode(&sixty_four).unwrap()).unwrap(), sixty_four);
        let mut sixty_five = sixty_four.clone();
        sixty_five.insert("z".into(), PropertyValue::Integer(0));
        assert_eq!(
            validate(&sixty_five),
            Err(GraphError::InvalidProperties("properties exceed 64 fields"))
        );
        let mut count_too_large = encode(&sixty_four).unwrap();
        count_too_large[..4].copy_from_slice(&65u32.to_le_bytes());
        assert_eq!(
            decode(&count_too_large),
            Err(GraphError::InvalidProperties("too many stored fields"))
        );

        // Exactly 65,536 encoded bytes is accepted; one more byte is not.
        // 16 strings of 4,096 bytes plus fixed overhead exceed the limit, so
        // size the last string to land exactly on it.
        let mut exact = Properties::new();
        let mut length = 4;
        for i in 0..15 {
            exact.insert(format!("s{i:02}"), PropertyValue::String("v".repeat(4096)));
            length += 2 + 3 + 1 + 4 + 4096;
        }
        let last = MAX_PROPERTY_BYTES - length - (2 + 3 + 1 + 4);
        exact.insert("s15".into(), PropertyValue::String("v".repeat(last)));
        assert_eq!(validate(&exact).unwrap(), MAX_PROPERTY_BYTES);
        assert_eq!(decode(&encode(&exact).unwrap()).unwrap(), exact);
        exact.insert("s15".into(), PropertyValue::String("v".repeat(last + 1)));
        assert_eq!(
            validate(&exact),
            Err(GraphError::InvalidProperties(
                "encoded properties exceed 65536 bytes"
            ))
        );
        assert_eq!(
            decode(&vec![0; MAX_PROPERTY_BYTES + 1]),
            Err(GraphError::InvalidProperties("oversized properties"))
        );
    }
}
