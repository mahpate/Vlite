//! Crate-private adapter-to-core mapping for the first Gap 1 slice D adapter.
//! Contract: `docs/projects/roadmap/gap1-adapter-mapping.md` sections 1-3.
//! Nothing here is a public API, wire route or public `Error` variant.

use sha2::{Digest, Sha256};

use super::ranking::RankingError;
use super::recall::RecallError;
use super::{MemoryError, MemoryResult, MemoryScope, hex, opaque, operation_receipt_key};

/// The nine wire operations of the serving contract (decision 67).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WireOperation {
    Remember,
    Recall,
    FindFacts,
    CorrectFacts,
    Consolidate,
    Forget,
    GetReceipt,
    GetErasure,
    ResumeErasure,
}

/// Operations whose receipts the first slice can look up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptOperation {
    Remember,
    Forget,
    Consolidate,
}

/// One wire status and code, with the optional `details.feature` context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WireCode {
    pub status: u16,
    pub code: &'static str,
    pub feature: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WireMapping {
    Code(WireCode),
    /// `Storage(e)` other than `InvalidQuery`: the existing server-private
    /// mapper owns the code.
    Delegate,
    /// Cancellation has no wire code and is never a success or a partial.
    Cancelled,
}

const fn code(status: u16, code: &'static str) -> WireMapping {
    WireMapping::Code(WireCode {
        status,
        code,
        feature: None,
    })
}

pub(crate) const fn unavailable(feature: &'static str) -> WireCode {
    WireCode {
        status: 409,
        code: "memory_capability_unavailable",
        feature: Some(feature),
    }
}

impl WireOperation {
    /// The explicit first-slice refusal, or `None` when the core serves it.
    pub(crate) fn refusal(self) -> Option<WireCode> {
        match self {
            Self::Remember | Self::Recall | Self::Consolidate | Self::Forget | Self::GetReceipt => {
                None
            }
            Self::FindFacts | Self::CorrectFacts => Some(unavailable("facts")),
            Self::GetErasure | Self::ResumeErasure => Some(unavailable("erasure_jobs")),
        }
    }

    /// Whether a grant's read-only scopes may authorize the operation. The
    /// match is exhaustive on purpose: a new operation has to be classified,
    /// and anything that writes or touches a receipt must answer `false`.
    pub(crate) fn accepts_read_scopes(self) -> bool {
        match self {
            Self::Recall | Self::FindFacts => true,
            Self::Remember
            | Self::CorrectFacts
            | Self::Consolidate
            | Self::Forget
            | Self::GetReceipt
            | Self::GetErasure
            | Self::ResumeErasure => false,
        }
    }
}

impl ReceiptOperation {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Remember => "remember",
            Self::Forget => "forget",
            Self::Consolidate => "consolidate",
        }
    }

    /// Maps a wire `MemoryOperation` for GetReceipt: unknown names are
    /// `invalid_input`, operations without first-slice lookup are refused.
    /// Remember receipts need the request to decode (mapping section 3).
    pub(crate) fn from_wire(operation: &str) -> Result<Self, WireCode> {
        match operation {
            "remember" => Err(unavailable("remember_receipt_lookup")),
            "forget" => Ok(Self::Forget),
            "consolidate" => Ok(Self::Consolidate),
            "correct_facts" => Err(unavailable("facts")),
            "resume_erasure" => Err(unavailable("erasure_jobs")),
            _ => Err(WireCode {
                status: 422,
                code: "invalid_input",
                feature: None,
            }),
        }
    }
}

/// The stable opaque receipt ID (mapping section 3).
pub(crate) fn receipt_id(
    operation: ReceiptOperation,
    scope: &MemoryScope,
    idempotency_key: &str,
    receipt_revision: u64,
) -> MemoryResult<String> {
    scope.validate()?;
    if !opaque(idempotency_key) {
        return Err(MemoryError::InvalidKey);
    }
    if receipt_revision == 0 {
        return Err(MemoryError::InvalidBundle);
    }
    let key = operation_receipt_key(operation.name(), &scope.key(), idempotency_key);
    let mut input = b"vlite.memory.receipt.id.v1\0".to_vec();
    input.extend_from_slice(&(key.len() as u32).to_be_bytes());
    input.extend_from_slice(key.as_bytes());
    input.extend_from_slice(&receipt_revision.to_be_bytes());
    Ok(format!("rcpt:v1:{}", hex(&Sha256::digest(input))))
}

pub(crate) fn memory_error(error: &MemoryError) -> WireMapping {
    use MemoryError::*;
    match error {
        InvalidKey
        | InvalidExpiry
        | InvalidEvidence
        | DuplicateItem
        | OperationLimit
        | BodyLimit
        | InvalidIdentity
        | InvalidContent
        | InvalidImportance
        | InvalidObservation
        | FutureObservation
        | InvalidProvenance
        | ProvenanceLimit
        | InvalidReference
        | CrossScope
        | ExpectedRevisionRequired
        | InvalidTarget
        | DuplicateTarget => code(422, "invalid_input"),
        // PROVISIONAL: a scope the adapter could not validate is unprovisioned.
        InvalidScope => code(409, "memory_not_configured"),
        UnsupportedKind => WireMapping::Code(unavailable("fact_kinds")),
        MissingMemory | MissingReference => code(404, "memory_not_found"),
        IdempotencyConflict => code(409, "idempotency_conflict"),
        RevisionConflict | IdentityConflict | MergeIdentityConflict => {
            code(412, "precondition_failed")
        }
        RecordAlreadyAttached | ForeignAttachment => code(409, "memory_attachment_conflict"),
        DeadlineExceeded => code(504, "deadline_exceeded"),
        InvalidClock | InvalidReceiptWindow | InvalidBundle | DigestCollision => {
            code(500, "internal_error")
        }
        DeletionPending => code(409, "memory_deletion_pending"),
        // PROVISIONAL: a second erasure begin is a job generation mismatch;
        // unreachable in the first slice, which refuses erasure jobs.
        JobConflict => code(412, "precondition_failed"),
        // The graph's binding record no longer matches the binding served.
        BindingGeneration => WireMapping::Code(unavailable("binding_generation")),
        Storage(error) => storage_error(error),
    }
}

/// The memory core raises `InvalidQuery` only for a missing or mistyped
/// stored property once the adapter has validated the request against the
/// contract bounds: an integrity fault, 500 rather than the mapper's 422
/// (mapping section 2). **PROVISIONAL.**
fn storage_error(error: &crate::Error) -> WireMapping {
    match error {
        crate::Error::InvalidQuery(_) => code(500, "internal_error"),
        _ => WireMapping::Delegate,
    }
}

pub(crate) fn ranking_error(error: RankingError) -> WireMapping {
    use RankingError::*;
    match error {
        Weights | HalfLifeRequired | HalfLife | Limit | AgeOverflow | FutureObservation => {
            code(422, "invalid_input")
        }
        CandidateCount | Scope | Importance | Rank | DuplicateRank | DuplicateIdentity
        | DenseRanks | SegmentBounds | NonFiniteScore => code(500, "internal_error"),
    }
}

pub(crate) fn recall_error(error: &RecallError) -> WireMapping {
    match error {
        RecallError::Memory(error) => memory_error(error),
        RecallError::Storage(error) => storage_error(error),
        RecallError::Ranking(error) => ranking_error(*error),
        RecallError::InvalidLimits
        | RecallError::InvalidScopes
        | RecallError::InvalidQuery
        | RecallError::Budget(_) => code(422, "invalid_input"),
        RecallError::UnsupportedPlan => code(422, "memory_bounded_plan_unsupported"),
        RecallError::Cancelled => WireMapping::Cancelled,
        RecallError::Deadline => code(504, "deadline_exceeded"),
    }
}

#[cfg(test)]
mod tests;
