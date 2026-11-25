//! design-tenant-provisioning EPIC-008: the configuration digest shared by
//! the deployment profile and the ARM adapter.
//!
//! What ARM returns for a resource is not the body that was sent: it adds
//! fields (`id`, `systemData`, `provisioningState`, defaults), may change
//! the case of resource IDs, spells the top-level `location` as a display
//! name and may return an integer as a float. A desired body is therefore
//! compared only on what it states: the observed body is projected onto the
//! desired body's shape (every stated key, `null` when absent; arrays
//! element-wise only at equal length), both sides are normalized the same
//! way and hashed as canonical JSON with sorted keys. Every other string,
//! enum casing included, compares exactly, and a stated field ARM never
//! echoes (a write-only secret) projects to `null`, so it can never match:
//! unproven, never assumed.
use serde_json::Value;

/// Separates these digests from content digests (SHA-256 of retained bytes).
const DOMAIN: &[u8] = b"vlite.arm-config.v1\n";
const RESOURCE_ID: &str = "/subscriptions/";
/// 2^53: beyond it an `f64` no longer holds every integer exactly.
const EXACT: f64 = 9_007_199_254_740_992.0;

/// The digest a desired ARM body is deployed and verified under.
pub(super) fn config_digest(desired: &Value) -> String {
    digest(desired)
}

/// The digest of `observed` as far as `desired` states it: equal to
/// [`config_digest`] exactly when every stated field is observed with the
/// same normalized value.
pub(super) fn observed_digest(desired: &Value, observed: &Value) -> String {
    digest(&project(desired, observed))
}

fn digest(value: &Value) -> String {
    let mut bytes = DOMAIN.to_vec();
    canonical(&normalize(value, true), &mut bytes);
    super::sha256_hex(&bytes)
}

fn project(desired: &Value, observed: &Value) -> Value {
    match (desired, observed) {
        (Value::Object(desired), Value::Object(observed)) => Value::Object(
            desired
                .iter()
                .map(|(key, d)| {
                    let o = observed.get(key).map_or(Value::Null, |o| project(d, o));
                    (key.clone(), o)
                })
                .collect(),
        ),
        (Value::Array(desired), Value::Array(observed)) if desired.len() == observed.len() => {
            Value::Array(
                desired
                    .iter()
                    .zip(observed)
                    .map(|(d, o)| project(d, o))
                    .collect(),
            )
        }
        _ => observed.clone(),
    }
}

fn normalize(value: &Value, top: bool) -> Value {
    match value {
        Value::String(id)
            if id
                .get(..RESOURCE_ID.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(RESOURCE_ID)) =>
        {
            Value::String(id.to_ascii_lowercase())
        }
        Value::Number(number) => match number.as_f64() {
            Some(float) if number.is_f64() && float.fract() == 0.0 && float.abs() < EXACT => {
                if float < 0.0 {
                    Value::from(float as i64)
                } else {
                    Value::from(float as u64)
                }
            }
            _ => value.clone(),
        },
        Value::Array(items) => Value::Array(items.iter().map(|v| normalize(v, false)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, v)| match v {
                    // ARM answers `westus2` with "West US 2".
                    Value::String(location) if top && key == "location" => {
                        let name: String =
                            location.chars().filter(|c| !c.is_whitespace()).collect();
                        (key.clone(), Value::String(name.to_ascii_lowercase()))
                    }
                    v => (key.clone(), normalize(v, false)),
                })
                .collect(),
        ),
        Value::Null | Value::Bool(_) | Value::String(_) => value.clone(),
    }
}

/// Compact JSON with object keys sorted at every level, independent of
/// how `serde_json` orders a map.
fn canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            out.push(b'{');
            for (i, (key, value)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(Value::from(key.as_str()).to_string().as_bytes());
                out.push(b':');
                canonical(value, out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                canonical(item, out);
            }
            out.push(b']');
        }
        scalar => out.extend_from_slice(scalar.to_string().as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn desired() -> Value {
        json!({
            "location": "westus2",
            "properties": {
                "environmentId": "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg/providers/Microsoft.App/managedEnvironments/env",
                "configuration": {"activeRevisionsMode": "Single"},
                "template": {
                    "containers": [{"name": "vlite", "image": "ghcr.io/o/vlite@sha256:00", "resources": {"cpu": 0.5, "memory": "1Gi"}}],
                    "scale": {"minReplicas": 1, "maxReplicas": 1}
                }
            }
        })
    }

    /// What ARM returns for [`desired`]: added fields, other resource-ID
    /// casing, a display-name location and an integral float.
    fn observed() -> Value {
        json!({
            "id": "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg/providers/Microsoft.App/containerApps/app",
            "name": "app",
            "location": "West US 2",
            "tags": {"vlite-tenant": "vlite-tenant:00"},
            "systemData": {"createdBy": "someone"},
            "properties": {
                "provisioningState": "Succeeded",
                "environmentId": "/subscriptions/00000000-0000-0000-0000-000000000000/resourcegroups/RG/providers/microsoft.app/managedEnvironments/env",
                "configuration": {"activeRevisionsMode": "Single", "ingress": null, "maxInactiveRevisions": 100},
                "template": {
                    "containers": [{"name": "vlite", "image": "ghcr.io/o/vlite@sha256:00", "resources": {"cpu": 0.5, "memory": "1Gi", "ephemeralStorage": "2Gi"}, "probes": []}],
                    "scale": {"minReplicas": 1.0, "maxReplicas": 1, "rules": null}
                }
            }
        })
    }

    #[test]
    fn an_observed_body_matches_on_what_the_desired_body_states() {
        assert_eq!(
            observed_digest(&desired(), &observed()),
            config_digest(&desired())
        );
    }

    #[test]
    fn every_stated_difference_is_a_mismatch() {
        let want = config_digest(&desired());
        let changes: [fn(&mut Value); 6] = [
            |o| o["properties"]["template"]["containers"][0]["image"] = json!("ghcr.io/o/x"),
            // Enum strings compare exactly.
            |o| o["properties"]["configuration"]["activeRevisionsMode"] = json!("single"),
            |o| o["properties"]["template"]["scale"]["maxReplicas"] = json!(1.5),
            |o| o["properties"]["template"]["containers"] = json!([{}, {}]),
            // A stated field ARM does not echo, like a write-only secret.
            |o| {
                o["properties"]
                    .as_object_mut()
                    .unwrap()
                    .remove("environmentId");
            },
            |o| o["location"] = json!("East US"),
        ];
        for change in changes {
            let mut observed = observed();
            change(&mut observed);
            assert_ne!(observed_digest(&desired(), &observed), want);
        }
    }

    #[test]
    fn the_canonical_form_sorts_keys_and_normalizes_numbers_ids_and_location() {
        let value = json!({
            "b": [true, null, 2.0, -3.0, 0.5, "/SUBSCRIPTIONS/X/rg", "/subscription€x"],
            "a": {"c": "Mixed", "location": "West US"},
            "location": "West US 2"
        });
        let mut bytes = Vec::new();
        canonical(&normalize(&value, true), &mut bytes);
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"a":{"c":"Mixed","location":"West US"},"b":[true,null,2,-3,0.5,"/subscriptions/x/rg","/subscription€x"],"location":"westus2"}"#
        );
        assert!(normalize(&json!(9_007_199_254_740_992.0), false).is_f64());
    }
}
