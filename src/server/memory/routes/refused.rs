//! The explicit first-slice capability refusals (mapping section 1): facts
//! and erasure-job routes are mounted so that authentication, binding and
//! exact-scope authorization (and the writable precheck for writes) run in
//! the shared order, then refuse 409 `memory_capability_unavailable` before
//! any further request validation.

use super::*;

/// Steps 1-4 of the check order, then the binding's refusal for
/// `operation` (`binding_retired`, `facts` or `erasure_jobs`).
async fn refused(
    mut call: Call,
    name: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    bytes: Result<Bytes, BytesRejection>,
    operation: WireOperation,
    evidence_ops: &[Op],
    write: bool,
) -> Response {
    let service = call.service.clone();
    let (bound, name) = match resolve(&service, &call.role, name, operation, evidence_ops) {
        Ok(found) => found,
        Err(fail) => return call.finish(Err(fail), None),
    };
    call.binding = Some(name);
    let authorized = body(&headers, bytes).and_then(|mut fields| {
        let scope = wire::scope(fields.require("scope")?, &bound.tenant, "scope")?;
        bound.authorize(&scope)
    });
    if let Err(fail) = authorized {
        return call.finish(Err(fail), None);
    }
    if write {
        let checked = service
            .run_read(|db| Ok(writable(db)))
            .await
            .map_err(Fail::from)
            .and_then(|result| result);
        if let Err(fail) = checked {
            return call.finish(Err(fail), None);
        }
    }
    let refusal = bound.refusal.map(Fail::wire).unwrap_or_else(internal);
    call.finish(Err(refusal), None)
}

pub(super) async fn find_facts(
    State(service): State<Service>,
    caller: Caller,
    Extension(request_id): Extension<RequestId>,
    clock: Option<Extension<Clock>>,
    name: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    bytes: Result<Bytes, BytesRejection>,
) -> Response {
    let call = Call::new(service, caller, request_id, clock, "find_facts");
    let ops = &[Op::Read];
    refused(
        call,
        name,
        headers,
        bytes,
        WireOperation::FindFacts,
        ops,
        false,
    )
    .await
}

pub(super) async fn correct_facts(
    State(service): State<Service>,
    caller: Caller,
    Extension(request_id): Extension<RequestId>,
    clock: Option<Extension<Clock>>,
    name: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    bytes: Result<Bytes, BytesRejection>,
) -> Response {
    let call = Call::new(service, caller, request_id, clock, "correct_facts");
    let ops = &[Op::Write];
    refused(
        call,
        name,
        headers,
        bytes,
        WireOperation::CorrectFacts,
        ops,
        true,
    )
    .await
}

pub(super) async fn get_erasure(
    State(service): State<Service>,
    caller: Caller,
    Extension(request_id): Extension<RequestId>,
    clock: Option<Extension<Clock>>,
    name: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    bytes: Result<Bytes, BytesRejection>,
) -> Response {
    let call = Call::new(service, caller, request_id, clock, "get_erasure");
    // Job status contains no evidence content: graph_read only.
    refused(
        call,
        name,
        headers,
        bytes,
        WireOperation::GetErasure,
        &[],
        false,
    )
    .await
}

pub(super) async fn resume_erasure(
    State(service): State<Service>,
    caller: Caller,
    Extension(request_id): Extension<RequestId>,
    clock: Option<Extension<Clock>>,
    name: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    bytes: Result<Bytes, BytesRejection>,
) -> Response {
    let call = Call::new(service, caller, request_id, clock, "resume_erasure");
    let ops = &[Op::Write];
    refused(
        call,
        name,
        headers,
        bytes,
        WireOperation::ResumeErasure,
        ops,
        true,
    )
    .await
}
