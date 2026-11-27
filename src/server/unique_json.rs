use serde::Deserialize;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// JSON inputs whose exact bytes matter must not silently discard duplicate keys.
pub(super) struct UniqueJson(pub(super) Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor;

        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueJson;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON with unique object keys")
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| UniqueJson(Value::Number(number)))
                    .ok_or_else(|| E::custom("invalid number"))
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueJson(value)) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(UniqueJson(Value::Array(values)))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom("duplicate object key"));
                    }
                    let UniqueJson(value) = map.next_value()?;
                    values.insert(key, value);
                }
                Ok(UniqueJson(Value::Object(values)))
            }
        }

        deserializer.deserialize_any(UniqueVisitor)
    }
}
