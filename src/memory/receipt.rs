//! Crate-private GetReceipt lookup (`docs/projects/roadmap/gap1-adapter-mapping.md`
//! section 3): a stored forget or consolidate receipt is read by bound graph,
//! scope, operation, idempotency key and captured clock, without the original
//! request. No persisted format changes here; remember lookup is refused by
//! the adapter because its receipts cannot be decoded without the request.

use super::erasure::{ChunkSummary, ErasureJob, lookup_begin, lookup_chunk};
use super::mapping::ReceiptOperation;
use super::{
    LABEL_RECEIPT, MAX_OPS, MemoryError, MemoryRef, MemoryResult, MemoryScope, consolidate,
    decode_outcome, hex, opaque, operation_receipt_key, string_property, unhex,
};
use crate::{MetadataValue, property_graph::NodeView};

/// The stored outcome of a retained receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptOutcome {
    /// Forget: the deleted IDs in stored (ascending UTF-8 byte) order, each
    /// reported at the receipt revision.
    Forget { deleted: Vec<String> },
    /// C1 consolidate: the created summary, always at memory revision 1.
    Consolidate { summary_id: String },
    /// ES: a job-begin receipt, as its begin view (`begin_job_view`).
    BeginErasure { job: ErasureJob },
    /// ES: a resume receipt, as the original view of its chunk.
    ResumeErasure {
        job: ErasureJob,
        chunk: ChunkSummary,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReceiptLookup {
    pub outcome: ReceiptOutcome,
    /// The receipt node revision, equal to the frame's commit sequence.
    pub receipt_revision: u64,
    /// The stored expiry: lookup at or after it returns `None`.
    pub retry_until_ms: u64,
}

impl MemoryRef<'_> {
    /// `None` means 404 `receipt_not_found`: absent, or expired at `now_ms`.
    /// A present receipt decodes strictly whether live or expired. An
    /// erasure barrier does not hide receipts: lookup is a read.
    pub(crate) fn receipt(
        &self,
        operation: ReceiptOperation,
        scope: &MemoryScope,
        idempotency_key: &str,
        now_ms: u64,
    ) -> MemoryResult<Option<ReceiptLookup>> {
        scope.validate()?;
        if !opaque(idempotency_key) {
            return Err(MemoryError::InvalidKey);
        }
        if now_ms > i64::MAX as u64 {
            return Err(MemoryError::InvalidClock);
        }
        let scope_key = scope.key();
        let read = self.graph()?.with_clock(now_ms);
        let scope_node = read
            .get_node(&scope_key)?
            .output
            .ok_or(MemoryError::InvalidScope)?;
        if string_property(&scope_node, "tuple")? != hex(&scope.encode()) {
            return Err(MemoryError::DigestCollision);
        }
        let key = operation_receipt_key(operation.name(), &scope_key, idempotency_key);
        let Some(receipt) = read.get_node(&key)?.output else {
            return Ok(None);
        };
        let (outcome, retry_until_ms) = match operation {
            ReceiptOperation::Forget => {
                let expiry = forget_receipt_expiry(&receipt, &scope_key)?;
                let deleted = decode_deleted(&decode_outcome(&read, &receipt)?)?;
                (ReceiptOutcome::Forget { deleted }, expiry)
            }
            ReceiptOperation::Consolidate => {
                let collection = self.evidence_collection.as_deref().unwrap_or_default();
                let (summary_id, expiry) =
                    consolidate::lookup(&read, &receipt, scope, &scope_key, collection)?;
                (ReceiptOutcome::Consolidate { summary_id }, expiry)
            }
            ReceiptOperation::BeginErasure => {
                let (job, expiry) =
                    lookup_begin(&read, &receipt, scope, self.evidence_collection.as_deref())?;
                (ReceiptOutcome::BeginErasure { job }, expiry)
            }
            ReceiptOperation::ResumeErasure => {
                let (job, chunk, expiry) =
                    lookup_chunk(&read, &receipt, scope, self.evidence_collection.as_deref())?;
                (ReceiptOutcome::ResumeErasure { job, chunk }, expiry)
            }
            // The adapter refuses remember lookup before calling here; a
            // stray call is an internal fault, never a silent not-found.
            ReceiptOperation::Remember => return Err(MemoryError::InvalidBundle),
        };
        if now_ms >= retry_until_ms {
            return Ok(None);
        }
        Ok(Some(ReceiptLookup {
            outcome,
            receipt_revision: receipt.revision,
            retry_until_ms,
        }))
    }
}

impl MemoryRef<'_> {
    /// The stored `retry_until_ms` of the receipt a mutation just wrote or
    /// replayed, for any first-slice operation (remember included): read by
    /// identity alone, without decoding the outcome. `None` when absent.
    pub(crate) fn retry_until(
        &self,
        operation: ReceiptOperation,
        scope: &MemoryScope,
        idempotency_key: &str,
        now_ms: u64,
    ) -> MemoryResult<Option<u64>> {
        let scope_key = scope.key();
        let key = operation_receipt_key(operation.name(), &scope_key, idempotency_key);
        let Some(receipt) = self.graph()?.with_clock(now_ms).get_node(&key)?.output else {
            return Ok(None);
        };
        if string_property(&receipt, "scope")? != scope_key {
            return Err(MemoryError::DigestCollision);
        }
        consolidate::canonical_decimal(string_property(&receipt, "expires_at_ms")?, 20)
            .map(Some)
            .ok_or(MemoryError::InvalidBundle)
    }
}

/// Exactly the A2 forget receipt label and properties; returns the expiry.
fn forget_receipt_expiry(receipt: &NodeView, scope_key: &str) -> MemoryResult<u64> {
    let typed = |name: &str, integer: bool| {
        matches!(
            (receipt.properties.get(name), integer),
            (Some(MetadataValue::String(_)), false) | (Some(MetadataValue::Integer(_)), true)
        )
    };
    if receipt.labels.len() != 1
        || !receipt.labels.contains(LABEL_RECEIPT)
        || receipt.properties.len() != 4
        || !typed("scope", false)
        || !typed("digest", false)
        || !typed("expires_at_ms", false)
        || !typed("outcome_chunks", true)
    {
        return Err(MemoryError::InvalidBundle);
    }
    if string_property(receipt, "scope")? != scope_key {
        return Err(MemoryError::DigestCollision);
    }
    if !consolidate::digest_shaped(string_property(receipt, "digest")?) {
        return Err(MemoryError::InvalidBundle);
    }
    consolidate::canonical_decimal(string_property(receipt, "expires_at_ms")?, 20)
        .ok_or(MemoryError::InvalidBundle)
}

/// `deleted:` plus 1..=4096 strictly ascending, lowercase-hex opaque IDs.
fn decode_deleted(encoded: &str) -> MemoryResult<Vec<String>> {
    let values = encoded
        .strip_prefix("deleted:")
        .ok_or(MemoryError::InvalidBundle)?;
    let mut deleted: Vec<String> = Vec::new();
    for value in values.split(';') {
        let id = String::from_utf8(unhex(value)?).map_err(|_| MemoryError::InvalidBundle)?;
        if !opaque(&id)
            || hex(id.as_bytes()) != value
            || deleted
                .last()
                .is_some_and(|last| last.as_str() >= id.as_str())
            || deleted.len() == MAX_OPS
        {
            return Err(MemoryError::InvalidBundle);
        }
        deleted.push(id);
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests;
