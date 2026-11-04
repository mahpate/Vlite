using System.Text.Json.Serialization;

namespace VLite;

// Agent-memory wire types (serving contract section 8). MemoryU64/MemoryI64 values are canonical decimal
// strings in every response record. Request records take ulong/long and write the canonical string; the shared
// records MemoryRef, MemoryInterval and the Integer/Revision union variants hold the canonical string and also
// accept a number at construction. Strings are checked when a request is written, never silently repaired.

/// <summary>Kind of a memory item.</summary>
[JsonConverter(typeof(MemoryEnumConverter<MemoryKind>))]
public enum MemoryKind { Entity, Episodic, Fact, Relation }

/// <summary>Mutating operation named by a receipt.</summary>
[JsonConverter(typeof(MemoryEnumConverter<MemoryOperation>))]
public enum MemoryOperation { Remember, CorrectFacts, Consolidate, Forget, ResumeErasure }

[JsonConverter(typeof(MemoryEnumConverter<MemoryConditionOp>))]
public enum MemoryConditionOp { Eq, Lt, Lte, Gt, Gte, Exists }

[JsonConverter(typeof(MemoryEnumConverter<MemoryDisagreement>))]
public enum MemoryDisagreement { Reject, Disputed }

[JsonConverter(typeof(MemoryEnumConverter<MemoryMerge>))]
public enum MemoryMerge { None, MergeProvenance }

[JsonConverter(typeof(MemoryEnumConverter<MemorySelection>))]
public enum MemorySelection { Weighted, Exact, Mmr }

[JsonConverter(typeof(MemoryEnumConverter<MemoryFutureObservations>))]
public enum MemoryFutureObservations { Reject, Clamp }

[JsonConverter(typeof(MemoryEnumConverter<MemoryRecallMode>))]
public enum MemoryRecallMode { Exact, Approximate }

[JsonConverter(typeof(MemoryEnumConverter<MemoryCorrectionAction>))]
public enum MemoryCorrectionAction { Close, Replace, Remove }

[JsonConverter(typeof(MemoryEnumConverter<MemoryDisposition>))]
public enum MemoryDisposition { Retain, LogicalDelete }

[JsonConverter(typeof(MemoryEnumConverter<MemoryForgetMode>))]
public enum MemoryForgetMode { LogicalDelete, Expire }

[JsonConverter(typeof(MemoryEnumConverter<MemoryOutcomeAction>))]
public enum MemoryOutcomeAction { Created, Merged, Noop, Corrected, Deleted, Expired, RetainedShared }

/// <summary>Result state of Forget and ResumeErasure; Pending is the HTTP 202 outcome.</summary>
[JsonConverter(typeof(MemoryEnumConverter<MemoryMutationState>))]
public enum MemoryMutationState { Logical, Pending, RetentionSet }

[JsonConverter(typeof(MemoryEnumConverter<MemoryErasureState>))]
public enum MemoryErasureState { Pending, Logical }

[JsonConverter(typeof(MemoryEnumConverter<MemoryStatus>))]
public enum MemoryStatus { Accepted, Disputed }

// ---------------------------------------------------------------------------------------------- shared records

/// <summary>Request scope: agent, optionally user, optionally user and session.</summary>
public sealed record MemoryScope(
    [property: JsonPropertyName("agent")] string Agent,
    [property: JsonPropertyName("user")] string? User = null,
    [property: JsonPropertyName("session")] string? Session = null);

/// <summary>Scope as resolved by the server, with the tenant it derived from the credential.</summary>
public sealed record MemoryEffectiveScope(
    [property: JsonPropertyName("tenant")] string Tenant,
    [property: JsonPropertyName("agent")] string Agent,
    [property: JsonPropertyName("user")] string? User = null,
    [property: JsonPropertyName("session")] string? Session = null);

/// <summary>Reference to a memory (or series) at an exact revision; revision 0 names an item of the same request.</summary>
[JsonConverter(typeof(MemoryRefConverter))]
public sealed record MemoryRef(string Id, string Revision)
{
    public MemoryRef(string id, ulong revision) : this(id, MemoryNumber.FormatU64(revision)) { }
}

/// <summary>Half-open validity interval; a null endpoint is unbounded. Both endpoints are always sent.</summary>
[JsonConverter(typeof(MemoryIntervalConverter))]
public sealed record MemoryInterval(string? ValidFromMs, string? ValidToMs)
{
    public static MemoryInterval Unbounded { get; } = new((string?)null, null);
    public static MemoryInterval From(long validFromMs) => new(MemoryNumber.FormatI64(validFromMs), null);
    public static MemoryInterval Until(long validToMs) => new(null, MemoryNumber.FormatI64(validToMs));
    public static MemoryInterval Between(long validFromMs, long validToMs) =>
        new(MemoryNumber.FormatI64(validFromMs), MemoryNumber.FormatI64(validToMs));
}

public sealed record MemorySlot(
    [property: JsonPropertyName("subject_id")] string SubjectId,
    [property: JsonPropertyName("predicate")] string Predicate,
    [property: JsonPropertyName("slot_key")] string SlotKey);

/// <summary>Caller-declared source of an item (request side, inert and never fetched).</summary>
public sealed record MemorySource(
    [property: JsonPropertyName("source_id")] string SourceId,
    [property: JsonPropertyName("source_kind")] string SourceKind,
    [property: JsonPropertyName("uri")] string? Uri = null,
    [property: JsonPropertyName("transformation_id")] string? TransformationId = null);

/// <summary>Source as stored: the request source plus the server-computed content digest.</summary>
public sealed record MemoryProvenance(
    [property: JsonPropertyName("source_id")] string SourceId,
    [property: JsonPropertyName("source_kind")] string SourceKind,
    [property: JsonPropertyName("content_digest")] string ContentDigest,
    [property: JsonPropertyName("uri")] string? Uri = null,
    [property: JsonPropertyName("transformation_id")] string? TransformationId = null);

/// <summary>Expected revision of a record: absent (create) or an exact revision.</summary>
[JsonConverter(typeof(MemoryExpectedConverter))]
public abstract record MemoryExpected
{
    private MemoryExpected() { }
    public sealed record Absent : MemoryExpected;
    public sealed record Revision(string Value) : MemoryExpected
    {
        public Revision(ulong value) : this(MemoryNumber.FormatU64(value)) { }
    }
}

/// <summary>Typed scalar: a string, a canonical-decimal i64, a finite float or a boolean.</summary>
[JsonConverter(typeof(MemoryScalarConverter))]
public abstract record MemoryScalar
{
    private MemoryScalar() { }
    public sealed record Text(string Value) : MemoryScalar;
    public sealed record Integer(string Value) : MemoryScalar
    {
        public Integer(long value) : this(MemoryNumber.FormatI64(value)) { }
    }
    public sealed record Float(double Value) : MemoryScalar;
    public sealed record Boolean(bool Value) : MemoryScalar;
}

/// <summary>Named typed scalars (at most 64 on the server).</summary>
public sealed class MemoryProperties : Dictionary<string, MemoryScalar> { }

/// <summary>Target of Forget: exactly one variant.</summary>
[JsonConverter(typeof(MemoryTargetConverter))]
public abstract record MemoryTarget
{
    private MemoryTarget() { }

    public sealed record Ids(MemoryRef[] Refs) : MemoryTarget
    {
        public bool Equals(Ids? other) => other is not null && Refs.AsSpan().SequenceEqual(other.Refs);
        public override int GetHashCode() => Refs.Aggregate(17, (hash, item) => hash * 31 + item.GetHashCode());
    }

    public sealed record Series(MemoryRef Ref) : MemoryTarget;
    public sealed record Entity(MemoryRef Ref) : MemoryTarget;
    public sealed record Session : MemoryTarget;
    public sealed record Scope : MemoryTarget;
}

// ---------------------------------------------------------------------------------------------- request records

public sealed record MemoryCondition(
    [property: JsonPropertyName("field")] string Field,
    [property: JsonPropertyName("op")] MemoryConditionOp Op,
    [property: JsonPropertyName("value")] MemoryScalar? Value = null);

/// <summary>Conjunction of <c>All</c> and the disjunction <c>Any</c>; at most 64 conditions in total.</summary>
public sealed record MemoryFilter(
    [property: JsonPropertyName("all")] MemoryCondition[]? All = null,
    [property: JsonPropertyName("any")] MemoryCondition[]? Any = null);

public sealed record MemoryEvidenceRef(
    [property: JsonPropertyName("collection")] string Collection,
    [property: JsonPropertyName("record"), JsonConverter(typeof(MemoryU64Converter))] ulong Record);

/// <summary>Evidence record written with the request. Vectors are caller-produced; nothing is embedded for you.</summary>
public sealed record MemoryEvidence(
    [property: JsonPropertyName("collection")] string Collection,
    [property: JsonPropertyName("record"), JsonConverter(typeof(MemoryU64Converter))] ulong Record,
    [property: JsonPropertyName("vector")] float[]? Vector = null,
    [property: JsonPropertyName("text")] string? Text = null,
    [property: JsonPropertyName("payload_base64")] string? PayloadBase64 = null,
    [property: JsonPropertyName("metadata")] MemoryProperties? Metadata = null);

public sealed record MemoryAssertion(
    [property: JsonPropertyName("subject_id")] string SubjectId,
    [property: JsonPropertyName("predicate")] string Predicate,
    [property: JsonPropertyName("slot_key")] string SlotKey,
    [property: JsonPropertyName("destination_id")] string DestinationId,
    [property: JsonPropertyName("interval")] MemoryInterval Interval,
    [property: JsonPropertyName("slot_expected")] MemoryExpected SlotExpected,
    [property: JsonPropertyName("disagreement")] MemoryDisagreement? Disagreement = null);

public sealed record MemoryItem(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("kind")] MemoryKind Kind,
    [property: JsonPropertyName("content")] string Content,
    [property: JsonPropertyName("observed_at_ms"), JsonConverter(typeof(MemoryI64Converter))] long ObservedAtMs,
    [property: JsonPropertyName("source")] MemorySource Source,
    [property: JsonPropertyName("importance")] double? Importance = null,
    [property: JsonPropertyName("confidence")] double? Confidence = null,
    [property: JsonPropertyName("expires_at_ms"), JsonConverter(typeof(MemoryU64Converter))] ulong? ExpiresAtMs = null,
    [property: JsonPropertyName("provenance")] MemorySource[]? Provenance = null,
    [property: JsonPropertyName("references")] MemoryRef[]? References = null,
    [property: JsonPropertyName("evidence")] MemoryEvidenceRef[]? Evidence = null,
    [property: JsonPropertyName("metadata")] MemoryProperties? Metadata = null,
    [property: JsonPropertyName("assertion")] MemoryAssertion? Assertion = null,
    [property: JsonPropertyName("expected")] MemoryExpected? Expected = null,
    [property: JsonPropertyName("merge")] MemoryMerge? Merge = null);

public sealed record MemoryRememberRequest(
    [property: JsonPropertyName("scope")] MemoryScope Scope,
    [property: JsonPropertyName("idempotency_key")] string IdempotencyKey,
    [property: JsonPropertyName("items")] MemoryItem[] Items,
    [property: JsonPropertyName("evidence")] MemoryEvidence[]? Evidence = null,
    [property: JsonPropertyName("max_references")] uint? MaxReferences = null,
    [property: JsonPropertyName("deadline_ms")] ulong? DeadlineMs = null,
    [property: JsonPropertyName("require_complete_feed")] bool? RequireCompleteFeed = null);

public sealed record MemoryRanking(
    [property: JsonPropertyName("selection")] MemorySelection? Selection = null,
    [property: JsonPropertyName("weights")] double[]? Weights = null,
    [property: JsonPropertyName("half_life_ms")] double? HalfLifeMs = null,
    [property: JsonPropertyName("future_observations")] MemoryFutureObservations? FutureObservations = null,
    [property: JsonPropertyName("lambda")] double? Lambda = null);

/// <summary>Work and output budgets: JSON integers, where <c>DeadlineMs</c> is the engine deadline, not an HTTP timeout.</summary>
public sealed record MemoryLimits(
    [property: JsonPropertyName("candidates")] ulong? Candidates = null,
    [property: JsonPropertyName("vector_components")] ulong? VectorComponents = null,
    [property: JsonPropertyName("evidence_bytes")] ulong? EvidenceBytes = null,
    [property: JsonPropertyName("mmr_components")] ulong? MmrComponents = null,
    [property: JsonPropertyName("deadline_ms")] ulong? DeadlineMs = null,
    [property: JsonPropertyName("max_hops")] ulong? MaxHops = null,
    [property: JsonPropertyName("max_visited_nodes")] ulong? MaxVisitedNodes = null,
    [property: JsonPropertyName("max_edges_examined")] ulong? MaxEdgesExamined = null,
    [property: JsonPropertyName("max_edges_per_node")] ulong? MaxEdgesPerNode = null,
    [property: JsonPropertyName("max_output_nodes")] ulong? MaxOutputNodes = null,
    [property: JsonPropertyName("max_output_edges")] ulong? MaxOutputEdges = null,
    [property: JsonPropertyName("max_output_paths")] ulong? MaxOutputPaths = null);

public sealed record MemoryProjection(
    [property: JsonPropertyName("snippets")] bool? Snippets = null,
    [property: JsonPropertyName("provenance")] bool? Provenance = null,
    [property: JsonPropertyName("paths")] bool? Paths = null,
    [property: JsonPropertyName("metadata")] string[]? Metadata = null);

public sealed record MemoryRecallRequest(
    [property: JsonPropertyName("scope")] MemoryScope Scope,
    [property: JsonPropertyName("scopes")] MemoryScope[]? Scopes = null,
    [property: JsonPropertyName("vector")] float[]? Vector = null,
    [property: JsonPropertyName("text")] string? Text = null,
    [property: JsonPropertyName("embedding_provider")] string? EmbeddingProvider = null,
    [property: JsonPropertyName("entity_seeds")] string[]? EntitySeeds = null,
    [property: JsonPropertyName("kinds")] MemoryKind[]? Kinds = null,
    [property: JsonPropertyName("filter")] MemoryFilter? Filter = null,
    [property: JsonPropertyName("as_of_ms"), JsonConverter(typeof(MemoryI64Converter))] long? AsOfMs = null,
    [property: JsonPropertyName("ranking")] MemoryRanking? Ranking = null,
    [property: JsonPropertyName("limits")] MemoryLimits? Limits = null,
    [property: JsonPropertyName("projection")] MemoryProjection? Projection = null,
    [property: JsonPropertyName("mode")] MemoryRecallMode? Mode = null,
    [property: JsonPropertyName("limit")] uint? Limit = null,
    [property: JsonPropertyName("allow_partial")] bool? AllowPartial = null);

public sealed record MemoryFactsRequest(
    [property: JsonPropertyName("scope")] MemoryScope Scope,
    [property: JsonPropertyName("slot")] MemorySlot Slot,
    [property: JsonPropertyName("as_of_ms"), JsonConverter(typeof(MemoryI64Converter))] long? AsOfMs = null,
    [property: JsonPropertyName("history")] bool? History = null,
    [property: JsonPropertyName("limit")] uint? Limit = null,
    [property: JsonPropertyName("limits")] MemoryLimits? Limits = null,
    [property: JsonPropertyName("projection")] MemoryProjection? Projection = null,
    [property: JsonPropertyName("allow_partial")] bool? AllowPartial = null);

public sealed record MemoryCorrectionRequest(
    [property: JsonPropertyName("scope")] MemoryScope Scope,
    [property: JsonPropertyName("idempotency_key")] string IdempotencyKey,
    [property: JsonPropertyName("slot")] MemorySlot Slot,
    [property: JsonPropertyName("series")] MemoryRef Series,
    [property: JsonPropertyName("slot_revision"), JsonConverter(typeof(MemoryU64Converter))] ulong SlotRevision,
    [property: JsonPropertyName("interval")] MemoryInterval Interval,
    [property: JsonPropertyName("action")] MemoryCorrectionAction Action,
    [property: JsonPropertyName("at_ms"), JsonConverter(typeof(MemoryI64Converter))] long? AtMs = null,
    [property: JsonPropertyName("correction_interval")] MemoryInterval? CorrectionInterval = null,
    [property: JsonPropertyName("replacement")] MemoryItem? Replacement = null,
    [property: JsonPropertyName("evidence")] MemoryEvidence[]? Evidence = null,
    [property: JsonPropertyName("max_references")] uint? MaxReferences = null,
    [property: JsonPropertyName("deadline_ms")] ulong? DeadlineMs = null,
    [property: JsonPropertyName("require_complete_feed")] bool? RequireCompleteFeed = null);

public sealed record MemoryInput(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("revision"), JsonConverter(typeof(MemoryU64Converter))] ulong Revision,
    [property: JsonPropertyName("disposition")] MemoryDisposition? Disposition = null);

public sealed record MemoryConsolidateRequest(
    [property: JsonPropertyName("scope")] MemoryScope Scope,
    [property: JsonPropertyName("idempotency_key")] string IdempotencyKey,
    [property: JsonPropertyName("inputs")] MemoryInput[] Inputs,
    [property: JsonPropertyName("output")] MemoryItem Output,
    [property: JsonPropertyName("evidence")] MemoryEvidence[]? Evidence = null,
    [property: JsonPropertyName("dry_run")] bool? DryRun = null,
    [property: JsonPropertyName("max_references")] uint? MaxReferences = null,
    [property: JsonPropertyName("deadline_ms")] ulong? DeadlineMs = null,
    [property: JsonPropertyName("require_complete_feed")] bool? RequireCompleteFeed = null);

public sealed record MemoryForgetRequest(
    [property: JsonPropertyName("scope")] MemoryScope Scope,
    [property: JsonPropertyName("idempotency_key")] string IdempotencyKey,
    [property: JsonPropertyName("target")] MemoryTarget Target,
    [property: JsonPropertyName("mode")] MemoryForgetMode Mode,
    [property: JsonPropertyName("expires_at_ms"), JsonConverter(typeof(MemoryU64Converter))] ulong? ExpiresAtMs = null,
    [property: JsonPropertyName("allow_job")] bool? AllowJob = null,
    [property: JsonPropertyName("max_references")] uint? MaxReferences = null,
    [property: JsonPropertyName("deadline_ms")] ulong? DeadlineMs = null,
    [property: JsonPropertyName("require_complete_feed")] bool? RequireCompleteFeed = null);

public sealed record MemoryReceiptRequest(
    [property: JsonPropertyName("scope")] MemoryScope Scope,
    [property: JsonPropertyName("operation")] MemoryOperation Operation,
    [property: JsonPropertyName("idempotency_key")] string IdempotencyKey);

public sealed record MemoryErasureRequest(
    [property: JsonPropertyName("scope")] MemoryScope Scope,
    [property: JsonPropertyName("job_id")] string JobId);

public sealed record MemoryResumeRequest(
    [property: JsonPropertyName("scope")] MemoryScope Scope,
    [property: JsonPropertyName("idempotency_key")] string IdempotencyKey,
    [property: JsonPropertyName("job_id")] string JobId,
    [property: JsonPropertyName("revision"), JsonConverter(typeof(MemoryU64Converter))] ulong Revision,
    [property: JsonPropertyName("generation"), JsonConverter(typeof(MemoryU64Converter))] ulong Generation,
    [property: JsonPropertyName("resume_token")] string ResumeToken,
    [property: JsonPropertyName("max_references")] uint? MaxReferences = null,
    [property: JsonPropertyName("deadline_ms")] ulong? DeadlineMs = null,
    [property: JsonPropertyName("require_complete_feed")] bool? RequireCompleteFeed = null);

// ---------------------------------------------------------------------------------------------- response records

public sealed record MemoryReceipt(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("operation")] MemoryOperation Operation,
    [property: JsonPropertyName("scope")] MemoryEffectiveScope Scope,
    [property: JsonPropertyName("sequence")] string Sequence,
    [property: JsonPropertyName("digest_version")] string DigestVersion,
    [property: JsonPropertyName("retry_until_ms")] string? RetryUntilMs = null);

public sealed record MemoryOutcome(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("action")] MemoryOutcomeAction Action,
    [property: JsonPropertyName("revision")] string Revision,
    [property: JsonPropertyName("expires_at_ms")] string? ExpiresAtMs = null);

/// <summary>Change-feed completeness diagnostics, passed through as sent by the server.</summary>
public sealed record MemoryFeed(
    [property: JsonPropertyName("mode")] string Mode,
    [property: JsonPropertyName("temporal_complete")] bool TemporalComplete,
    [property: JsonPropertyName("ttl_complete")] bool TtlComplete,
    [property: JsonPropertyName("consumer_purge_acknowledged")] bool ConsumerPurgeAcknowledged);

public sealed record MemoryErasureNotice(
    [property: JsonPropertyName("live_state_only")] bool LiveStateOnly,
    [property: JsonPropertyName("secure_erasure")] bool SecureErasure,
    [property: JsonPropertyName("external_copies")] string ExternalCopies,
    [property: JsonPropertyName("restore_requires_manifest")] bool RestoreRequiresManifest,
    [property: JsonPropertyName("backup_history_purge")] string BackupHistoryPurge,
    [property: JsonPropertyName("manifest_id")] string? ManifestId = null);

public sealed record MemoryErasure(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("scope")] MemoryEffectiveScope Scope,
    [property: JsonPropertyName("target")] MemoryTarget Target,
    [property: JsonPropertyName("revision")] string Revision,
    [property: JsonPropertyName("generation")] string Generation,
    [property: JsonPropertyName("state")] MemoryErasureState State,
    [property: JsonPropertyName("logical_visibility")] string LogicalVisibility,
    [property: JsonPropertyName("deleted_count")] string DeletedCount,
    [property: JsonPropertyName("remaining_count"), JsonIgnore(Condition = JsonIgnoreCondition.Never)] string? RemainingCount,
    [property: JsonPropertyName("resume_token")] string? ResumeToken = null);

/// <summary>Answer of Consolidate: a committed <see cref="MemoryMutationResponse"/> or a dry-run <see cref="MemoryPlanResponse"/>.</summary>
[JsonConverter(typeof(MemoryConsolidateResponseConverter))]
public abstract record MemoryConsolidateResponse
{
    private protected MemoryConsolidateResponse() { }
}

/// <summary>Receipt and outcomes of a committed mutation (HTTP 200, or 202 with <see cref="MemoryMutationState.Pending"/>).</summary>
public sealed record MemoryMutationResponse(
    [property: JsonPropertyName("receipt")] MemoryReceipt Receipt,
    [property: JsonPropertyName("replayed")] bool Replayed,
    [property: JsonPropertyName("outcomes")] MemoryOutcome[] Outcomes,
    [property: JsonPropertyName("feed")] MemoryFeed Feed,
    [property: JsonPropertyName("remaining_work"), JsonIgnore(Condition = JsonIgnoreCondition.Never)] string? RemainingWork,
    [property: JsonPropertyName("state")] MemoryMutationState? State = null,
    [property: JsonPropertyName("erasure")] MemoryErasure? Erasure = null,
    [property: JsonPropertyName("erasure_notice")] MemoryErasureNotice? ErasureNotice = null) : MemoryConsolidateResponse;

public sealed record MemoryErrorDetail(
    [property: JsonPropertyName("field")] string? Field = null,
    [property: JsonPropertyName("limit")] string? Limit = null,
    [property: JsonPropertyName("requested")] string? Requested = null,
    [property: JsonPropertyName("competing")] MemoryCompeting[]? Competing = null,
    [property: JsonPropertyName("receipt_lookup_required")] bool? ReceiptLookupRequired = null,
    [property: JsonPropertyName("code")] string? Code = null);

public sealed record MemoryCompeting(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("interval")] MemoryInterval Interval,
    [property: JsonPropertyName("revision")] string Revision);

/// <summary>Dry-run result of Consolidate: counts only; nothing was written or reserved.</summary>
public sealed record MemoryPlanResponse(
    [property: JsonPropertyName("dry_run")] bool DryRun,
    [property: JsonPropertyName("user_operations")] string UserOperations,
    [property: JsonPropertyName("encoded_bytes")] string EncodedBytes,
    [property: JsonPropertyName("conflicts")] MemoryErrorDetail[] Conflicts,
    [property: JsonPropertyName("remaining_work"), JsonIgnore(Condition = JsonIgnoreCondition.Never)] string? RemainingWork) : MemoryConsolidateResponse;

public sealed record MemoryReceiptResponse([property: JsonPropertyName("result")] MemoryMutationResponse Result);

public sealed record MemoryErasureResponse(
    [property: JsonPropertyName("erasure")] MemoryErasure Erasure,
    [property: JsonPropertyName("erasure_notice")] MemoryErasureNotice ErasureNotice,
    [property: JsonPropertyName("feed")] MemoryFeed Feed);

public sealed record MemoryEvidenceView(
    [property: JsonPropertyName("collection")] string Collection,
    [property: JsonPropertyName("record")] string Record,
    [property: JsonPropertyName("snippet")] string? Snippet = null);

public sealed record MemoryPath(
    [property: JsonPropertyName("nodes")] string[] Nodes,
    [property: JsonPropertyName("edges")] string[] Edges);

/// <summary>Ranking diagnostics. Scores are version-specific; never compare them across ranking versions.</summary>
public sealed record MemoryScores(
    [property: JsonPropertyName("R")] double R,
    [property: JsonPropertyName("D"), JsonIgnore(Condition = JsonIgnoreCondition.Never)] double? D,
    [property: JsonPropertyName("I")] double I,
    [property: JsonPropertyName("S")] double S,
    [property: JsonPropertyName("dense_rank")] int? DenseRank = null,
    [property: JsonPropertyName("sparse_rank")] int? SparseRank = null,
    [property: JsonPropertyName("text_rank")] int? TextRank = null,
    [property: JsonPropertyName("distance")] double? Distance = null,
    [property: JsonPropertyName("bm25")] double? Bm25 = null,
    [property: JsonPropertyName("sparse_score")] double? SparseScore = null,
    [property: JsonPropertyName("fused_rank")] int? FusedRank = null,
    [property: JsonPropertyName("fused_score")] double? FusedScore = null);

public sealed record MemoryView(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("revision")] string Revision,
    [property: JsonPropertyName("scope")] MemoryEffectiveScope Scope,
    [property: JsonPropertyName("kind")] MemoryKind Kind,
    [property: JsonPropertyName("content")] string Content,
    [property: JsonPropertyName("observed_at_ms")] string ObservedAtMs,
    [property: JsonPropertyName("created_at_ms")] string CreatedAtMs,
    [property: JsonPropertyName("updated_at_ms")] string UpdatedAtMs,
    [property: JsonPropertyName("importance")] double Importance,
    [property: JsonPropertyName("status")] MemoryStatus Status,
    [property: JsonPropertyName("stale")] bool Stale,
    [property: JsonPropertyName("segment"), JsonIgnore(Condition = JsonIgnoreCondition.Never)] MemoryInterval? Segment,
    [property: JsonPropertyName("evidence")] MemoryEvidenceView[] Evidence,
    [property: JsonPropertyName("scores")] MemoryScores Scores,
    [property: JsonPropertyName("confidence")] double? Confidence = null,
    [property: JsonPropertyName("expires_at_ms")] string? ExpiresAtMs = null,
    [property: JsonPropertyName("provenance")] MemoryProvenance[]? Provenance = null,
    [property: JsonPropertyName("references")] MemoryRef[]? References = null,
    [property: JsonPropertyName("paths")] MemoryPath[]? Paths = null,
    [property: JsonPropertyName("metadata")] MemoryProperties? Metadata = null);

/// <summary>Read metadata: <c>Complete</c> means the bounded plan finished, not that the memory is exhaustive.</summary>
public sealed record MemoryReadMeta(
    [property: JsonPropertyName("scopes")] MemoryEffectiveScope[] Scopes,
    [property: JsonPropertyName("as_of_ms")] string AsOfMs,
    [property: JsonPropertyName("ttl_clock_ms")] string TtlClockMs,
    [property: JsonPropertyName("sequence")] string Sequence,
    [property: JsonPropertyName("complete")] bool Complete,
    [property: JsonPropertyName("truncation")] string[] Truncation,
    [property: JsonPropertyName("limitations")] string[] Limitations,
    [property: JsonPropertyName("mode")] string Mode,
    [property: JsonPropertyName("candidates_examined")] string CandidatesExamined,
    [property: JsonPropertyName("edges_charged")] string EdgesCharged,
    [property: JsonPropertyName("vector_components")] string VectorComponents,
    [property: JsonPropertyName("mmr_components")] string MmrComponents,
    [property: JsonPropertyName("evidence_bytes")] string EvidenceBytes);

public sealed record MemoryRecallResponse(
    [property: JsonPropertyName("memories")] MemoryView[] Memories,
    [property: JsonPropertyName("meta")] MemoryReadMeta Meta,
    [property: JsonPropertyName("ranking_version")] string RankingVersion,
    [property: JsonPropertyName("selection")] MemorySelection Selection);

public sealed record MemoryFactView(
    [property: JsonPropertyName("series")] MemoryRef Series,
    [property: JsonPropertyName("slot")] MemorySlot Slot,
    [property: JsonPropertyName("slot_revision")] string SlotRevision,
    [property: JsonPropertyName("destination_id")] string DestinationId,
    [property: JsonPropertyName("interval")] MemoryInterval Interval,
    [property: JsonPropertyName("status")] MemoryStatus Status,
    [property: JsonPropertyName("stale")] bool Stale,
    [property: JsonPropertyName("evidence")] MemoryEvidenceView[] Evidence,
    [property: JsonPropertyName("provenance")] MemoryProvenance[]? Provenance = null);

public sealed record MemoryFactsResponse(
    [property: JsonPropertyName("facts")] MemoryFactView[] Facts,
    [property: JsonPropertyName("meta")] MemoryReadMeta Meta);

/// <summary>
/// Structured error context (<c>error.details</c>) carried by <see cref="VLiteException.Details"/>, for example
/// <c>Feature</c> of a 409 <c>memory_capability_unavailable</c> refusal. <c>Limit</c> and <c>Requested</c> are canonical u64 strings.
/// </summary>
public sealed record MemoryErrorContext(
    [property: JsonPropertyName("field")] string? Field = null,
    [property: JsonPropertyName("limit")] string? Limit = null,
    [property: JsonPropertyName("requested")] string? Requested = null,
    [property: JsonPropertyName("feature")] string? Feature = null,
    [property: JsonPropertyName("category")] string? Category = null);
