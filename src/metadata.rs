//! Typed, bounded metadata with strict type matching and conjunction-only filters.

use crate::{Error, Result};
use std::collections::BTreeMap;

pub type Metadata = BTreeMap<String, MetadataValue>;
pub const MAX_METADATA_FIELDS: usize = 64;
pub const MAX_METADATA_BYTES: usize = 65_536;
pub const MAX_FILTER_CONDITIONS: usize = 64;

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "json", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "json",
    serde(
        tag = "type",
        content = "value",
        rename_all = "lowercase",
        deny_unknown_fields
    )
)]
pub enum MetadataValue {
    String(String),
    Integer(#[cfg_attr(feature = "json", serde(with = "integer_json"))] i64),
    Float(f64),
    Boolean(bool),
    GeoPoint { latitude: f64, longitude: f64 },
}

#[cfg(feature = "json")]
mod integer_json {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        let text = String::deserialize(deserializer)?;
        let value: i64 = text.parse().map_err(D::Error::custom)?;
        if value.to_string() != text {
            return Err(D::Error::custom(
                "integer must be a canonical decimal i64 string",
            ));
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "json", derive(serde::Deserialize))]
#[cfg_attr(
    feature = "json",
    serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)
)]
pub enum Condition {
    Eq {
        field: String,
        value: MetadataValue,
    },
    Range {
        field: String,
        min: Option<MetadataValue>,
        max: Option<MetadataValue>,
    },
    GeoRadius {
        field: String,
        latitude: f64,
        longitude: f64,
        radius_meters: f64,
    },
}

fn field_valid(field: &str) -> bool {
    !field.is_empty() && field.len() <= 256
}

fn validate_value(value: &MetadataValue) -> Result<()> {
    match value {
        MetadataValue::Float(value) if !value.is_finite() => {
            Err(Error::InvalidMetadata("float values must be finite"))
        }
        MetadataValue::String(value) if value.len() > 4096 => Err(Error::InvalidMetadata(
            "string values exceed 4096 UTF-8 bytes",
        )),
        MetadataValue::GeoPoint {
            latitude,
            longitude,
        } if !latitude.is_finite()
            || !longitude.is_finite()
            || !(-90.0..=90.0).contains(latitude)
            || !(-180.0..=180.0).contains(longitude) =>
        {
            Err(Error::InvalidMetadata(
                "geo coordinates must be finite latitude/longitude in range",
            ))
        }
        _ => Ok(()),
    }
}

pub(crate) fn contains_geo(metadata: &Metadata) -> bool {
    metadata
        .values()
        .any(|value| matches!(value, MetadataValue::GeoPoint { .. }))
}

pub(crate) fn validate(metadata: &Metadata) -> Result<usize> {
    if metadata.len() > MAX_METADATA_FIELDS {
        return Err(Error::InvalidMetadata("metadata exceeds 64 fields"));
    }
    let mut length = 4;
    for (key, value) in metadata {
        if !field_valid(key) {
            return Err(Error::InvalidMetadata(
                "field names must be 1..256 UTF-8 bytes",
            ));
        }
        validate_value(value)?;
        length += 2
            + key.len()
            + 1
            + match value {
                MetadataValue::String(value) => 4 + value.len(),
                MetadataValue::Integer(_) | MetadataValue::Float(_) => 8,
                MetadataValue::Boolean(_) => 1,
                MetadataValue::GeoPoint { .. } => 16,
            };
        if length > MAX_METADATA_BYTES {
            return Err(Error::InvalidMetadata(
                "encoded metadata exceeds 65536 bytes",
            ));
        }
    }
    Ok(length)
}

pub(crate) fn encode(metadata: &Metadata) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    for (key, value) in metadata {
        bytes.extend_from_slice(&(key.len() as u16).to_le_bytes());
        bytes.extend_from_slice(key.as_bytes());
        match value {
            MetadataValue::String(value) => {
                bytes.push(1);
                bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
                bytes.extend_from_slice(value.as_bytes());
            }
            MetadataValue::Integer(value) => {
                bytes.push(2);
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            MetadataValue::Float(value) => {
                bytes.push(3);
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            MetadataValue::Boolean(value) => {
                bytes.extend_from_slice(&[4, u8::from(*value)]);
            }
            MetadataValue::GeoPoint {
                latitude,
                longitude,
            } => {
                bytes.push(5);
                bytes.extend_from_slice(&latitude.to_le_bytes());
                bytes.extend_from_slice(&longitude.to_le_bytes());
            }
        }
    }
    bytes
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Metadata> {
    fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
        let value = bytes
            .get(..n)
            .ok_or(Error::InvalidMetadata("truncated metadata"))?;
        *bytes = &bytes[n..];
        Ok(value)
    }
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(Error::InvalidMetadata("oversized metadata"));
    }
    let mut remaining = bytes;
    let count = u32::from_le_bytes(take(&mut remaining, 4)?.try_into().unwrap()) as usize;
    if count > MAX_METADATA_FIELDS {
        return Err(Error::InvalidMetadata("too many stored fields"));
    }
    let mut metadata = Metadata::new();
    let mut previous: Option<String> = None;
    for _ in 0..count {
        let length = u16::from_le_bytes(take(&mut remaining, 2)?.try_into().unwrap()) as usize;
        let key = std::str::from_utf8(take(&mut remaining, length)?)
            .map_err(|_| Error::InvalidMetadata("invalid field UTF-8"))?
            .to_owned();
        if previous.as_ref().is_some_and(|previous| previous >= &key) {
            return Err(Error::InvalidMetadata(
                "metadata fields must be strictly sorted and unique",
            ));
        }
        let tag = take(&mut remaining, 1)?[0];
        let value = match tag {
            1 => {
                let length =
                    u32::from_le_bytes(take(&mut remaining, 4)?.try_into().unwrap()) as usize;
                MetadataValue::String(
                    std::str::from_utf8(take(&mut remaining, length)?)
                        .map_err(|_| Error::InvalidMetadata("invalid value UTF-8"))?
                        .to_owned(),
                )
            }
            2 => MetadataValue::Integer(i64::from_le_bytes(
                take(&mut remaining, 8)?.try_into().unwrap(),
            )),
            3 => MetadataValue::Float(f64::from_le_bytes(
                take(&mut remaining, 8)?.try_into().unwrap(),
            )),
            4 => match take(&mut remaining, 1)?[0] {
                0 => MetadataValue::Boolean(false),
                1 => MetadataValue::Boolean(true),
                _ => return Err(Error::InvalidMetadata("invalid boolean")),
            },
            5 => {
                let latitude = f64::from_le_bytes(take(&mut remaining, 8)?.try_into().unwrap());
                let longitude = f64::from_le_bytes(take(&mut remaining, 8)?.try_into().unwrap());
                MetadataValue::GeoPoint {
                    latitude,
                    longitude,
                }
            }
            _ => return Err(Error::InvalidMetadata("unknown metadata type")),
        };
        previous = Some(key.clone());
        metadata.insert(key, value);
    }
    if !remaining.is_empty() {
        return Err(Error::InvalidMetadata("trailing metadata bytes"));
    }
    validate(&metadata)?;
    Ok(metadata)
}

fn numeric_compare(a: &MetadataValue, b: &MetadataValue) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (MetadataValue::Integer(a), MetadataValue::Integer(b)) => Some(a.cmp(b)),
        (MetadataValue::Float(a), MetadataValue::Float(b)) => a.partial_cmp(b),
        _ => None,
    }
}

pub(crate) fn validate_filter(conditions: &[Condition]) -> Result<()> {
    if conditions.len() > MAX_FILTER_CONDITIONS {
        return Err(Error::InvalidFilter("filter exceeds 64 conditions"));
    }
    for condition in conditions {
        let field = match condition {
            Condition::Eq { field, value } => {
                validate_value(value)?;
                field
            }
            Condition::Range { field, min, max } => {
                if min.is_none() && max.is_none() {
                    return Err(Error::InvalidFilter("range requires min or max"));
                }
                for value in min.iter().chain(max) {
                    validate_value(value)?;
                    if !matches!(value, MetadataValue::Integer(_) | MetadataValue::Float(_)) {
                        return Err(Error::InvalidFilter(
                            "ranges require integer or float bounds",
                        ));
                    }
                }
                if let (Some(min), Some(max)) = (min, max)
                    && numeric_compare(min, max).is_none_or(|ordering| ordering.is_gt())
                {
                    return Err(Error::InvalidFilter(
                        "range bounds must have the same numeric type and min <= max",
                    ));
                }
                field
            }
            Condition::GeoRadius {
                field,
                latitude,
                longitude,
                radius_meters,
            } => {
                if !latitude.is_finite()
                    || !(-90.0..=90.0).contains(latitude)
                    || !longitude.is_finite()
                    || !(-180.0..=180.0).contains(longitude)
                    || !radius_meters.is_finite()
                    || *radius_meters < 0.0
                {
                    return Err(Error::InvalidFilter(
                        "geo radius requires valid coordinates and nonnegative finite meters",
                    ));
                }
                field
            }
        };
        if !field_valid(field) {
            return Err(Error::InvalidFilter(
                "field names must be 1..256 UTF-8 bytes",
            ));
        }
    }
    Ok(())
}

pub(crate) fn geo_distance_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_RADIUS_METERS: f64 = 6_371_008.8;
    let radians = std::f64::consts::PI / 180.0;
    let (lat1, lat2) = (lat1 * radians, lat2 * radians);
    let dlat = lat2 - lat1;
    let dlon = (lon2 - lon1) * radians;
    let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_METERS * a.sqrt().atan2((1.0 - a).max(0.0).sqrt())
}

pub(crate) fn matches(metadata: &Metadata, conditions: &[Condition]) -> bool {
    conditions.iter().all(|condition| match condition {
        Condition::Eq { field, value } => metadata.get(field) == Some(value),
        Condition::Range { field, min, max } => metadata.get(field).is_some_and(|value| {
            min.as_ref().is_none_or(|min| {
                numeric_compare(value, min).is_some_and(|ordering| ordering.is_ge())
            }) && max.as_ref().is_none_or(|max| {
                numeric_compare(value, max).is_some_and(|ordering| ordering.is_le())
            })
        }),
        Condition::GeoRadius {
            field,
            latitude,
            longitude,
            radius_meters,
        } => metadata.get(field).is_some_and(|value| {
            if let MetadataValue::GeoPoint {
                latitude: lat,
                longitude: lon,
            } = value
            {
                geo_distance_meters(*latitude, *longitude, *lat, *lon) <= *radius_meters
            } else {
                false
            }
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_duplicate_unsorted_unknown_nonfinite_and_invalid_encoding() {
        let original = BTreeMap::from([
            ("a".into(), MetadataValue::Boolean(true)),
            ("b".into(), MetadataValue::Boolean(false)),
        ]);
        let bytes = encode(&original);
        for bad in [
            vec![],
            vec![65, 0, 0, 0],
            {
                let mut b = bytes.clone();
                b[8] = 2;
                b
            },
            {
                let mut b = bytes.clone();
                b[7] = 99;
                b
            },
            {
                let mut b = bytes.clone();
                b[11] = b'a';
                b
            },
            {
                let mut b = bytes.clone();
                b[6] = 0xff;
                b
            },
            {
                let mut b = bytes.clone();
                b.push(0);
                b
            },
            {
                let mut b = encode(&BTreeMap::from([("a".into(), MetadataValue::Float(1.0))]));
                b[8..16].copy_from_slice(&f64::NAN.to_le_bytes());
                b
            },
        ] {
            assert!(decode(&bad).is_err(), "{bad:?}");
        }
        for end in 0..bytes.len() {
            assert!(decode(&bytes[..end]).is_err());
        }
        assert_eq!(decode(&bytes).unwrap(), original);
    }
}
