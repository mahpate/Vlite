//! Consolidate (contract sections 5 and 8, mapping sections 1 and 6): C1,
//! retain-only, commit-only. One `created` outcome for the summary at
//! revision 1; the output `source.transformation_id` is the C1
//! transformation and `max_references` the C1 input budget.

use crate::memory::{ConsolidateInput, ConsolidateRequest, ConsolidateSummary};

use super::*;

/// The serving cap on summary sources (the fixture
/// `consolidate-65-inputs-serving-provenance-cap`): at most
/// `min(max_references, 64)` inputs.
const MAX_INPUTS: u64 = 64;
/// `MemoryConsolidateRequest.inputs` contract bound.
const MAX_WIRE_INPUTS: usize = 256;

/// Consolidate field refusals in contract schema field order (`inputs`,
/// `output`, `evidence`, `dry_run`), mapping section 1.
fn consolidate_refusal(body: &Map<String, Value>) -> Option<&'static str> {
    let delete = body
        .get("inputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|input| input.get("disposition") == Some(&json!("logical_delete")));
    if delete {
        return Some("consolidate_delete");
    }
    if let Some(output) = body.get("output") {
        if matches!(
            output.get("kind").and_then(Value::as_str),
            Some("fact" | "relation")
        ) {
            return Some("fact_kinds");
        }
        if present(output, "confidence") {
            return Some("confidence");
        }
        // C1 plans no output references or merge (no first-slice mapping).
        if output
            .get("references")
            .and_then(Value::as_array)
            .is_some_and(|references| !references.is_empty())
        {
            return Some("references");
        }
        for field in ["evidence", "metadata", "assertion", "expected"] {
            if present(output, field) {
                return Some(field);
            }
        }
        if output.get("merge").is_some_and(|merge| merge != "none") {
            return Some("merge");
        }
    }
    if body.contains_key("evidence") {
        return Some("evidence");
    }
    (body.get("dry_run") == Some(&Value::Bool(true))).then_some("consolidate_plan")
}

fn input(value: Value) -> Result<ConsolidateInput, Fail> {
    let mut fields = Fields::new(value, "inputs")?;
    let id = wire::key(fields.require("id")?, "inputs")?;
    let revision = wire::u64_value(fields.require("revision")?, "inputs")?;
    // Refusals ran first: only `retain` remains acceptable.
    if let Some(disposition) = fields.take("disposition")?
        && disposition != "retain"
    {
        return Err(Fail::invalid("inputs"));
    }
    fields.finish()?;
    if revision == 0 {
        return Err(Fail::invalid("inputs"));
    }
    Ok(ConsolidateInput {
        id,
        expected_memory_revision: Some(revision),
        retain: true,
    })
}

/// The output item: an `episodic` `MemoryItem` whose source names its
/// transformation. Returns the summary and the transformation.
fn output(value: Value, max_references: u64) -> Result<(ConsolidateSummary, String), Fail> {
    let transformation = value
        .get("source")
        .and_then(|source| source.get("transformation_id"))
        .cloned();
    let item = remember_item(value, &Default::default(), max_references)?;
    let transformation = wire::key(
        transformation.ok_or_else(|| Fail::invalid("output"))?,
        "output",
    )?;
    if item.kind != MemoryKind::Episodic {
        return Err(Fail::invalid("output"));
    }
    let summary = ConsolidateSummary {
        id: item.id,
        content: item.content,
        source: item.source,
        observed_at_ms: item.observed_at_ms,
        importance: item.importance,
        expiry_ms: item.expiry_ms,
        expiry_valid: item.expiry_valid,
        provenance: item.provenance,
    };
    Ok((summary, transformation))
}

pub(super) async fn consolidate(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    Extension(request_id): Extension<RequestId>,
    clock: Option<Extension<Clock>>,
    name: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    bytes: Result<Bytes, BytesRejection>,
) -> Response {
    let call = Call::new(service, role, request_id, clock, "consolidate");
    // C1 writes the bound graph only; evidence attachment is refused.
    let ops = &[];
    let operation = WireOperation::Consolidate;
    write(call, name, headers, bytes, operation, ops, consolidate_in).await
}

fn consolidate_in(
    db: &mut Database,
    call: &Call,
    bound: &Bound,
    mut fields: Fields,
    scope: MemoryScope,
) -> Result<(Value, String, u64), Fail> {
    writable(db)?;
    bound.covers(db)?;
    fields.drop_empty_evidence();
    refuse(bound, fields.raw(), consolidate_refusal)?;
    let common = mutation(&mut fields)?;
    let inputs = wire::array(fields.require("inputs")?, "inputs", 1, MAX_WIRE_INPUTS)?;
    if inputs.len() as u64 > common.max_references.min(MAX_INPUTS) {
        return Err(Fail::invalid("inputs"));
    }
    let inputs = inputs
        .into_iter()
        .map(input)
        .collect::<Result<Vec<_>, _>>()?;
    let (summary, transformation) = output(fields.require("output")?, common.max_references)?;
    wire::boolean(fields.take("dry_run")?, "dry_run")?;
    fields.finish()?;
    let now = call.now();
    let request = ConsolidateRequest {
        scope,
        idempotency_key: common.key,
        now_ms: now,
        clock_valid: true,
        receipt_window_ms: bound.receipt_window_ms,
        deadline_ms: Some(call.deadline(now, common.deadline_ms)),
        input_budget: Some(common.max_references),
        inputs,
        summary,
        transformation,
        evidence: None,
        allow_future_observations: false,
    };
    let result = MemoryMut::bind(db, &bound.graph, bound.evidence.as_deref())
        .and_then(|mut memory| memory.consolidate(&request))
        .map_err(core_error)?;
    let outcomes = vec![json!({
        "id": result.summary_id,
        "action": "created",
        "revision": result.summary_revision.to_string(),
    })];
    let receipt = Receipt {
        operation: ReceiptOperation::Consolidate,
        revision: result.receipt_revision,
        replayed: result.replayed,
    };
    let (body, id) = mutation_body(
        db,
        bound,
        &request.scope,
        &request.idempotency_key,
        now,
        &receipt,
        outcomes,
    )?;
    Ok((body, id, result.receipt_revision))
}
