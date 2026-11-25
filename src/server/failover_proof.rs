#![cfg_attr(not(test), allow(dead_code))]

//! Offline evidence parsing only. No constructor is available outside tests.

use std::collections::{BTreeMap, BTreeSet};

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use sha2::{Digest, Sha256};

const API_VERSION: &str = "2026-07-01";
const BASE: &str = "https://management.azure.com";
const BODY_MAX: usize = 4 * 1024 * 1024;
const MAX_PAGES: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum RevisionMode {
    Single,
    Multiple,
}

/// Trusted retained identity, not a browser-supplied request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct FenceBinding {
    app_resource_id: String,
    environment_resource_id: String,
    app_created_at: String,
    installation_id: String,
    scope_id: String,
    lineage: String,
    authority_store_id: String,
    authority_prefix: String,
    target_id: String,
    route_receipt_digest: String,
    lifecycle_generation: u64,
    revision_mode: RevisionMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProofError {
    Unavailable,
    BindingMismatch,
    Invalid,
    NotStopped,
    Continuation,
    Drift,
}

/// Implementors must supply authenticated, read-only observations. There is
/// intentionally no production implementation, credentials, or network client.
trait AuthenticatedFenceReads {
    fn authority_binding(&mut self) -> Result<FenceBinding, ProofError>;
    fn read(&mut self, resource: &str) -> Result<Vec<u8>, ProofError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WriterFenceProof {
    binding: FenceBinding,
    revisions: BTreeMap<String, Value>,
    observations: Vec<(String, Vec<u8>)>,
    evidence_digest: String,
}

fn app_uri(app: &str) -> String {
    format!("{BASE}{app}?api-version={API_VERSION}")
}

fn collection_uri(app: &str) -> String {
    format!("{BASE}{app}/revisions?api-version={API_VERSION}")
}

fn revision_uri(id: &str) -> String {
    format!("{BASE}{id}?api-version={API_VERSION}")
}

struct UniqueJson(Value);

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

fn read_json(
    port: &mut impl AuthenticatedFenceReads,
    uri: &str,
    observations: &mut Vec<(String, Vec<u8>)>,
) -> Result<Value, ProofError> {
    let bytes = port.read(uri)?;
    if bytes.len() > BODY_MAX {
        return Err(ProofError::Invalid);
    }
    let UniqueJson(value) = serde_json::from_slice(&bytes).map_err(|_| ProofError::Invalid)?;
    if !value.is_object() || value.get("error").is_some() {
        return Err(ProofError::Invalid);
    }
    observations.push((uri.into(), bytes));
    Ok(value)
}

fn check_app(value: &Value, binding: &FenceBinding) -> Result<(), ProofError> {
    let mode: RevisionMode =
        serde_json::from_value(value["properties"]["configuration"]["activeRevisionsMode"].clone())
            .map_err(|_| ProofError::Invalid)?;
    if value["id"].as_str() != Some(&binding.app_resource_id)
        || value["properties"]["environmentId"].as_str() != Some(&binding.environment_resource_id)
        || binding.app_created_at.is_empty()
        || value["systemData"]["createdAt"].as_str() != Some(&binding.app_created_at)
        || mode != binding.revision_mode
    {
        return Err(ProofError::BindingMismatch);
    }
    Ok(())
}

fn check_revision(value: &Value, app: &str) -> Result<String, ProofError> {
    let id = value["id"].as_str().ok_or(ProofError::Invalid)?;
    let name = value["name"].as_str().ok_or(ProofError::Invalid)?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        || id != format!("{app}/revisions/{name}")
    {
        return Err(ProofError::BindingMismatch);
    }
    let active = value["properties"]["active"]
        .as_bool()
        .ok_or(ProofError::Invalid)?;
    let replicas = value["properties"]["replicas"]
        .as_u64()
        .ok_or(ProofError::Invalid)?;
    if active || replicas != 0 {
        return Err(ProofError::NotStopped);
    }
    Ok(id.into())
}

fn check_continuation(uri: &str, app: &str) -> Result<String, ProofError> {
    let collection = format!("{BASE}{app}/revisions");
    // Compare the raw path too: URL normalization must not hide a dot segment,
    // escape, credentials, alternate app, port, or fragment.
    if uri.split('?').next() != Some(collection.as_str()) {
        return Err(ProofError::Continuation);
    }
    let url = reqwest::Url::parse(uri).map_err(|_| ProofError::Continuation)?;
    if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err(ProofError::Continuation);
    }
    let mut keys = BTreeSet::new();
    let mut version = false;
    for (key, value) in url.query_pairs() {
        if !keys.insert(key.to_string()) {
            return Err(ProofError::Continuation);
        }
        match key.as_ref() {
            "api-version" if value == API_VERSION => version = true,
            "$skipToken"
                if !value.is_empty()
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_=".contains(&byte)) => {}
            _ => return Err(ProofError::Continuation),
        }
    }
    if !version {
        return Err(ProofError::Continuation);
    }
    let mut pairs: Vec<_> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    pairs.sort();
    Ok(format!("{collection}?{pairs:?}"))
}

fn inventory(
    port: &mut impl AuthenticatedFenceReads,
    binding: &FenceBinding,
    observations: &mut Vec<(String, Vec<u8>)>,
) -> Result<BTreeMap<String, Value>, ProofError> {
    let mut uri = collection_uri(&binding.app_resource_id);
    let mut seen = BTreeSet::new();
    let mut revisions = BTreeMap::new();
    for _ in 0..MAX_PAGES {
        let canonical = check_continuation(&uri, &binding.app_resource_id)?;
        if !seen.insert(canonical) {
            return Err(ProofError::Continuation);
        }
        let page = read_json(port, &uri, observations)?;
        let values = page["value"].as_array().ok_or(ProofError::Invalid)?;
        for value in values {
            let id = check_revision(value, &binding.app_resource_id)?;
            if revisions.insert(id, value.clone()).is_some() {
                return Err(ProofError::Invalid);
            }
        }
        match page.get("nextLink") {
            None | Some(Value::Null) => {
                if revisions.is_empty() {
                    return Err(ProofError::Invalid);
                }
                return Ok(revisions);
            }
            Some(Value::String(next)) if !next.is_empty() => uri = next.clone(),
            _ => return Err(ProofError::Continuation),
        }
    }
    Err(ProofError::Continuation)
}

#[cfg(test)]
impl WriterFenceProof {
    /// Fixture evidence only, not a qualified reactivation-exclusion proof.
    fn from_recorded_reads(
        port: &mut impl AuthenticatedFenceReads,
        expected: &FenceBinding,
    ) -> Result<Self, ProofError> {
        if port.authority_binding()? != *expected {
            return Err(ProofError::BindingMismatch);
        }
        let mut observations = Vec::new();
        let uri = app_uri(&expected.app_resource_id);
        let app = read_json(port, &uri, &mut observations)?;
        check_app(&app, expected)?;
        let revisions = inventory(port, expected, &mut observations)?;
        for (id, listed) in &revisions {
            let direct = read_json(port, &revision_uri(id), &mut observations)?;
            check_revision(&direct, &expected.app_resource_id)?;
            if direct != *listed {
                return Err(ProofError::Drift);
            }
        }
        if inventory(port, expected, &mut observations)? != revisions {
            return Err(ProofError::Drift);
        }
        let repeated_app = read_json(port, &uri, &mut observations)?;
        check_app(&repeated_app, expected)?;
        if repeated_app != app || port.authority_binding()? != *expected {
            return Err(ProofError::Drift);
        }
        let bytes =
            serde_json::to_vec(&(expected, &observations)).map_err(|_| ProofError::Invalid)?;
        Ok(Self {
            binding: expected.clone(),
            revisions,
            observations,
            evidence_digest: format!("{:x}", Sha256::digest(bytes)),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use serde_json::{Value, json};

    const APP: &str =
        "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.App/containerApps/writer";
    const ENVIRONMENT: &str =
        "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.App/managedEnvironments/env";

    struct RecordedReads {
        binding: FenceBinding,
        responses: VecDeque<(String, Vec<u8>)>,
        reads: Vec<String>,
    }

    impl AuthenticatedFenceReads for RecordedReads {
        fn authority_binding(&mut self) -> Result<FenceBinding, ProofError> {
            Ok(self.binding.clone())
        }

        fn read(&mut self, resource: &str) -> Result<Vec<u8>, ProofError> {
            self.reads.push(resource.into());
            let (expected, bytes) = self.responses.pop_front().ok_or(ProofError::Unavailable)?;
            assert_eq!(resource, expected);
            Ok(bytes)
        }
    }

    fn binding() -> FenceBinding {
        FenceBinding {
            app_resource_id: APP.into(),
            environment_resource_id: ENVIRONMENT.into(),
            app_created_at: "2026-11-18T00:00:00Z".into(),
            installation_id: "installation".into(),
            scope_id: "scope".into(),
            lineage: "0123456789abcdef".into(),
            authority_store_id: "store".into(),
            authority_prefix: "authority/".into(),
            target_id: "dedicated".into(),
            route_receipt_digest: "a".repeat(64),
            lifecycle_generation: 7,
            revision_mode: RevisionMode::Multiple,
        }
    }

    fn app() -> Value {
        json!({
            "id": APP,
            "systemData": {"createdAt": "2026-11-18T00:00:00Z"},
            "properties": {
                "environmentId": ENVIRONMENT,
                "configuration": {"activeRevisionsMode": "Multiple"}
            }
        })
    }

    fn revision(name: &str) -> Value {
        json!({
            "id": format!("{APP}/revisions/{name}"),
            "name": name,
            "properties": {"active": false, "replicas": 0, "runningState": "Stopped"}
        })
    }

    fn recorded(pages: Vec<Value>, direct: Vec<Value>, repeated: Vec<Value>) -> RecordedReads {
        let mut responses = VecDeque::new();
        let mut push = |uri: String, value: Value| {
            responses.push_back((uri, serde_json::to_vec(&value).unwrap()));
        };
        push(app_uri(APP), app());
        for sequence in [&pages, &repeated] {
            for (index, page) in sequence.iter().enumerate() {
                let uri = if index == 0 {
                    collection_uri(APP)
                } else {
                    sequence[index - 1]["nextLink"].as_str().unwrap().into()
                };
                push(uri, page.clone());
            }
            if std::ptr::eq(sequence, &pages) {
                for value in &direct {
                    push(revision_uri(value["id"].as_str().unwrap()), value.clone());
                }
            }
        }
        push(app_uri(APP), app());
        RecordedReads {
            binding: binding(),
            responses,
            reads: Vec::new(),
        }
    }

    fn single(value: Value) -> RecordedReads {
        let page = json!({"value": [value.clone()]});
        recorded(vec![page.clone()], vec![value], vec![page])
    }

    #[test]
    fn writer_fence_requires_exact_app_and_authority_binding() {
        for change in 0..6 {
            let mut reads = single(revision("writer--one"));
            let mut expected = binding();
            match change {
                0 => expected.app_resource_id.push_str("-other"),
                1 => expected.authority_prefix.push_str("other/"),
                2 => expected.lineage.push('0'),
                3 => expected.authority_store_id.push_str("-other"),
                4 => expected.lifecycle_generation += 1,
                _ => expected.environment_resource_id.push_str("-other"),
            }
            assert_eq!(
                WriterFenceProof::from_recorded_reads(&mut reads, &expected),
                Err(ProofError::BindingMismatch)
            );
            assert!(reads.reads.is_empty());
        }
        for field in ["id", "incarnation", "environment"] {
            let mut reads = single(revision("writer--one"));
            let mut metadata = app();
            match field {
                "id" => metadata["id"] = json!(format!("{APP}-other")),
                "incarnation" => metadata["systemData"]["createdAt"] = json!("new-incarnation"),
                _ => metadata["properties"]["environmentId"] = json!("other-environment"),
            }
            reads.responses[0].1 = serde_json::to_vec(&metadata).unwrap();
            assert_eq!(
                WriterFenceProof::from_recorded_reads(&mut reads, &binding()),
                Err(ProofError::BindingMismatch)
            );
        }
        let mut wrong_revision = revision("writer--one");
        wrong_revision["id"] = json!(format!("{APP}-other/revisions/writer--one"));
        let mut reads = single(wrong_revision);
        assert!(WriterFenceProof::from_recorded_reads(&mut reads, &binding()).is_err());
    }

    #[test]
    fn writer_fence_reads_all_pages_and_direct_revisions() {
        let next = format!("{}&$skipToken=page2", collection_uri(APP));
        let pages = vec![
            json!({"value": [revision("writer--one")], "nextLink": next}),
            json!({"value": [revision("writer--two")]}),
        ];
        let mut reads = recorded(
            pages.clone(),
            vec![revision("writer--one"), revision("writer--two")],
            pages,
        );
        let proof = WriterFenceProof::from_recorded_reads(&mut reads, &binding()).unwrap();
        assert_eq!(proof.revisions.len(), 2);
        assert_eq!(proof.binding, binding());
        assert_eq!(proof.observations.len(), 8);
        assert_eq!(proof.evidence_digest.len(), 64);
        assert!(reads.responses.is_empty());
        assert_eq!(reads.reads.iter().filter(|uri| **uri == next).count(), 2);
        for name in ["writer--one", "writer--two"] {
            assert!(
                reads
                    .reads
                    .contains(&revision_uri(&format!("{APP}/revisions/{name}")))
            );
        }
    }

    #[test]
    fn writer_fence_rejects_cross_app_next_link_and_cycles() {
        for next in [
            collection_uri(&format!("{APP}-other")),
            collection_uri(APP),
            format!("https://evil.invalid{APP}/revisions?api-version={API_VERSION}"),
            format!("{}&$skipToken=../other", collection_uri(APP)),
            format!("https://management.azure.com{APP}/revisions/other?api-version={API_VERSION}"),
        ] {
            let pages = vec![json!({"value": [revision("writer--one")], "nextLink": next})];
            let mut reads = recorded(pages.clone(), vec![], pages);
            assert!(WriterFenceProof::from_recorded_reads(&mut reads, &binding()).is_err());
            assert_eq!(
                reads.reads.len(),
                2,
                "must refuse before following a bad continuation"
            );
        }
    }

    #[test]
    fn writer_fence_rejects_missing_null_string_or_fractional_replicas() {
        for replicas in [
            None,
            Some(Value::Null),
            Some(json!("0")),
            Some(json!(0.0)),
            Some(json!(0.5)),
            Some(json!(-1)),
        ] {
            let mut value = revision("writer--one");
            if let Some(replicas) = replicas {
                value["properties"]["replicas"] = replicas;
            } else {
                value["properties"]
                    .as_object_mut()
                    .unwrap()
                    .remove("replicas");
            }
            let mut reads = single(value);
            assert_eq!(
                WriterFenceProof::from_recorded_reads(&mut reads, &binding()),
                Err(ProofError::Invalid)
            );
        }
        let mut reads = single(revision("writer--one"));
        let raw = String::from_utf8(reads.responses[1].1.clone()).unwrap();
        reads.responses[1].1 = raw
            .replace("\"replicas\":0", "\"replicas\":1,\"replicas\":0")
            .into_bytes();
        assert_eq!(
            WriterFenceProof::from_recorded_reads(&mut reads, &binding()),
            Err(ProofError::Invalid)
        );
    }

    #[test]
    fn writer_fence_requires_inactive_and_explicit_zero_on_every_revision() {
        for (active, replicas) in [(true, 0), (false, 1), (true, 1)] {
            let mut second = revision("writer--two");
            second["properties"]["active"] = json!(active);
            second["properties"]["replicas"] = json!(replicas);
            let page = json!({"value": [revision("writer--one"), second.clone()]});
            let mut reads = recorded(
                vec![page.clone()],
                vec![revision("writer--one"), second],
                vec![page],
            );
            assert_eq!(
                WriterFenceProof::from_recorded_reads(&mut reads, &binding()),
                Err(ProofError::NotStopped)
            );
        }
        let mut value = revision("writer--one");
        value["properties"]
            .as_object_mut()
            .unwrap()
            .remove("runningState");
        assert!(WriterFenceProof::from_recorded_reads(&mut single(value), &binding()).is_ok());
        let mut empty = recorded(vec![json!({"value": []})], vec![], vec![]);
        assert_eq!(
            WriterFenceProof::from_recorded_reads(&mut empty, &binding()),
            Err(ProofError::Invalid)
        );
    }

    #[test]
    fn writer_fence_refuses_list_direct_disagreement_and_inventory_drift() {
        let one = revision("writer--one");
        let page = json!({"value": [one.clone()]});
        let mut different = one.clone();
        different["properties"]["runningState"] = json!("Stopping");
        let mut reads = recorded(vec![page.clone()], vec![different], vec![page.clone()]);
        assert_eq!(
            WriterFenceProof::from_recorded_reads(&mut reads, &binding()),
            Err(ProofError::Drift)
        );
        for repeated in [
            json!({"value": [one.clone(), revision("writer--two")]}),
            json!({"value": []}),
            json!({"value": [one.clone(), one.clone()]}),
        ] {
            let mut reads = recorded(vec![page.clone()], vec![one.clone()], vec![repeated]);
            assert!(WriterFenceProof::from_recorded_reads(&mut reads, &binding()).is_err());
        }
        let mut reads = single(one);
        let last = reads.responses.back_mut().unwrap();
        let mut changed = app();
        changed["systemData"]["createdAt"] = json!("recreated");
        last.1 = serde_json::to_vec(&changed).unwrap();
        assert_eq!(
            WriterFenceProof::from_recorded_reads(&mut reads, &binding()),
            Err(ProofError::BindingMismatch)
        );
    }

    #[test]
    fn writer_fence_multiple_mode_probe_fixture_is_not_single_mode_qualification() {
        // Synthetic fixture of the reported probe shape, not live qualification evidence.
        let mut reads = single(revision("writer--one"));
        let proof = WriterFenceProof::from_recorded_reads(&mut reads, &binding()).unwrap();
        assert_eq!(proof.binding.revision_mode, RevisionMode::Multiple);
        let mut single_binding = binding();
        single_binding.revision_mode = RevisionMode::Single;
        let mut reads = single(revision("writer--one"));
        reads.binding = single_binding.clone();
        assert_eq!(
            WriterFenceProof::from_recorded_reads(&mut reads, &single_binding),
            Err(ProofError::BindingMismatch)
        );
    }
}
