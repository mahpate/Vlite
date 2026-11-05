//! GetReceipt (contract section 8 `MemoryReceiptRequest`, mapping section
//! 3): a read of a retained forget or consolidate receipt by bound graph,
//! scope, operation and idempotency key. Remember, correct_facts and
//! resume_erasure lookups are refused before validation.

use crate::memory::{ReceiptLookup, ReceiptOutcome};

use super::*;

pub(super) async fn receipt(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    Extension(request_id): Extension<RequestId>,
    clock: Option<Extension<Clock>>,
    name: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    bytes: Result<Bytes, BytesRejection>,
) -> Response {
    let mut call = Call::new(service.clone(), role.clone(), request_id, clock, "receipt");
    // Receipts contain no evidence content: graph_read only.
    let (bound, name) = match resolve(&service, &role, name, WireOperation::GetReceipt, &[]) {
        Ok(found) => found,
        Err(fail) => return call.finish(Err(fail), None),
    };
    call.binding = Some(name);
    let parsed = body(&headers, bytes).and_then(|mut fields| {
        let scope = wire::scope(fields.require("scope")?, &bound.tenant, "scope")?;
        bound.authorize(&scope)?;
        if let Some(code) = bound.refusal {
            return Err(Fail::wire(code));
        }
        let operation = fields.require("operation")?;
        let operation = match operation.as_str().map(ReceiptOperation::from_wire) {
            Some(Ok(operation)) => operation,
            Some(Err(code)) if code.status == 409 => return Err(Fail::wire(code)),
            _ => return Err(Fail::invalid("operation")),
        };
        let key = wire::key(fields.require("idempotency_key")?, "idempotency_key")?;
        fields.finish()?;
        Ok((scope, operation, key))
    });
    let (scope, operation, key) = match parsed {
        Ok(parsed) => parsed,
        Err(fail) => return call.finish(Err(fail), None),
    };
    let call = Arc::new(call);
    let inner = call.clone();
    let result = service
        .run_read(move |db| Ok(lookup(db, &inner, &bound, &scope, operation, &key)))
        .await
        .map_err(Fail::from)
        .and_then(|result| result);
    call.finish(result, None)
}

fn lookup(
    db: &Database,
    call: &Call,
    bound: &Bound,
    scope: &MemoryScope,
    operation: ReceiptOperation,
    key: &str,
) -> Result<Value, Fail> {
    let now = call.now();
    let found = MemoryRef::bind(db, &bound.graph, bound.evidence.as_deref())
        .and_then(|memory| memory.receipt(operation, scope, key, now))
        .map_err(core_error)?;
    let Some(ReceiptLookup {
        outcome,
        receipt_revision,
        ..
    }) = found
    else {
        return Err(Fail::wire(WireCode {
            status: 404,
            code: "receipt_not_found",
            feature: None,
        }));
    };
    let revision = receipt_revision.to_string();
    let (outcomes, forget): (Vec<Value>, bool) = match outcome {
        ReceiptOutcome::Forget { deleted } => (
            deleted
                .iter()
                .map(|id| json!({"id": id, "action": "deleted", "revision": revision}))
                .collect(),
            true,
        ),
        ReceiptOutcome::Consolidate { summary_id } => (
            vec![json!({"id": summary_id, "action": "created", "revision": "1"})],
            false,
        ),
    };
    let receipt = Receipt {
        operation,
        revision: receipt_revision,
        replayed: true,
    };
    let (mut result, _) = mutation_body(db, bound, scope, key, now, &receipt, outcomes)?;
    if forget {
        result["state"] = json!("logical");
        result["erasure_notice"] = super::forget::erasure_notice();
    }
    Ok(json!({"result": result}))
}
