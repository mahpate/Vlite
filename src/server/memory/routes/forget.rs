//! Forget (contract sections 5 and 8, mapping sections 1 and 6): small
//! exact `ids` targets, `logical_delete` only, never a job or 202.

use crate::memory::{ForgetRequest, ForgetTarget};

use super::*;

/// `MemoryTarget.ids` contract bound.
const MAX_TARGETS: usize = 4_096;

/// Forget field refusals in contract schema field order (`target`,
/// `mode`, `allow_job`), mapping section 1.
fn forget_refusal(body: &Map<String, Value>) -> Option<&'static str> {
    if let Some(target) = body.get("target") {
        if present(target, "series") || present(target, "entity") {
            return Some("forget_target");
        }
        if present(target, "session") || present(target, "scope") {
            return Some("erasure_jobs");
        }
    }
    if body.get("mode") == Some(&json!("expire")) {
        return Some("forget_expire");
    }
    (body.get("allow_job") == Some(&Value::Bool(true))).then_some("erasure_jobs")
}

fn target(value: Value) -> Result<ForgetTarget, Fail> {
    let mut fields = Fields::new(value, "target")?;
    let id = wire::key(fields.require("id")?, "target")?;
    let revision = wire::u64_value(fields.require("revision")?, "target")?;
    fields.finish()?;
    if revision == 0 {
        return Err(Fail::invalid("target"));
    }
    Ok(ForgetTarget {
        id,
        expected_revision: revision,
    })
}

/// The disclosed first-slice notice: `manifest_id` is omitted until the
/// core writes deletion manifests (mapping section 1, E4).
pub(super) fn erasure_notice() -> Value {
    json!({
        "live_state_only": true,
        "secure_erasure": false,
        "external_copies": "operator_managed",
        "restore_requires_manifest": true,
        "backup_history_purge": "not_verified",
    })
}

pub(super) async fn forget(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    Extension(request_id): Extension<RequestId>,
    clock: Option<Extension<Clock>>,
    name: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    bytes: Result<Bytes, BytesRejection>,
) -> Response {
    let call = Call::new(service, role, request_id, clock, "forget");
    // Forget deletes the owned evidence records of its targets.
    let ops = &[Op::Write];
    write(
        call,
        name,
        headers,
        bytes,
        WireOperation::Forget,
        ops,
        forget_in,
    )
    .await
}

fn forget_in(
    db: &mut Database,
    call: &Call,
    bound: &Bound,
    mut fields: Fields,
    scope: MemoryScope,
) -> Result<(Value, String, u64), Fail> {
    writable(db)?;
    bound.covers(db)?;
    refuse(bound, fields.raw(), forget_refusal)?;
    let common = mutation(&mut fields)?;
    let mut target_fields = Fields::new(fields.require("target")?, "target")?;
    let targets = wire::array(target_fields.require("ids")?, "target", 1, MAX_TARGETS)?
        .into_iter()
        .map(target)
        .collect::<Result<Vec<_>, _>>()?;
    target_fields.finish()?;
    if fields.require("mode")? != "logical_delete" {
        return Err(Fail::invalid("mode"));
    }
    // `expires_at_ms` is only for `expire`, which is refused above.
    if fields.take("expires_at_ms")?.is_some() {
        return Err(Fail::invalid("expires_at_ms"));
    }
    wire::boolean(fields.take("allow_job")?, "allow_job")?;
    fields.finish()?;
    let now = call.now();
    let request = ForgetRequest {
        scope,
        idempotency_key: common.key,
        receipt_window_ms: bound.receipt_window_ms,
        now_ms: now,
        deadline_ms: Some(call.deadline(now, common.deadline_ms)),
        targets,
    };
    let result = MemoryMut::bind(db, &bound.graph, bound.evidence.as_deref())
        .and_then(|mut memory| memory.forget(&request))
        .map_err(core_error)?;
    let revision = result.receipt_revision.to_string();
    // Sorted by ID (UTF-8 byte order), on execution and on replay alike.
    let mut deleted: Vec<&String> = result.deleted.iter().collect();
    deleted.sort();
    let outcomes = deleted
        .into_iter()
        .map(|id| json!({"id": id, "action": "deleted", "revision": revision}))
        .collect();
    let receipt = Receipt {
        operation: ReceiptOperation::Forget,
        revision: result.receipt_revision,
        replayed: result.replayed,
    };
    let (mut body, id) = mutation_body(
        db,
        bound,
        &request.scope,
        &request.idempotency_key,
        now,
        &receipt,
        outcomes,
    )?;
    body["state"] = json!("logical");
    body["erasure_notice"] = erasure_notice();
    Ok((body, id, result.receipt_revision))
}
