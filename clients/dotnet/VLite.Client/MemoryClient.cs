using System.Text.RegularExpressions;
using System.Text.Json;

namespace VLite;

/// <summary>
/// Agent-memory operations of one provisioned binding, returned by <see cref="Client.Memory"/>. Every method is one unary
/// POST to <c>/v1/memory/{binding}/...</c> and returns the full typed response. Nothing is embedded, split, paged, retried or
/// polled for you: HTTP 202 (pending erasure) is a normal answer, a 409 refusal such as <c>memory_capability_unavailable</c>
/// is a <see cref="VLiteException"/> with its <see cref="VLiteException.Details"/>, and a timeout or cancellation is a
/// <see cref="TransportException"/> whose mutation outcome is unknown: look up a forget or consolidate with
/// <see cref="GetReceiptAsync"/> and the same idempotency key, or repeat a remember with the same key and body inside the
/// receipt window.
/// <c>deadline_ms</c> in a request is the engine deadline; the HTTP timeout stays the client's own.
/// </summary>
public sealed class MemoryClient
{
    private static readonly Regex BindingName = new(@"\A[a-z][a-z0-9_-]{0,62}\z", RegexOptions.CultureInvariant);
    private readonly Client client;
    private readonly string prefix;

    internal MemoryClient(Client client, string binding)
    {
        ArgumentNullException.ThrowIfNull(binding);
        if (!BindingName.IsMatch(binding)) throw new ArgumentException("Invalid memory binding name", nameof(binding));
        this.client = client;
        Binding = binding;
        prefix = "/v1/memory/" + binding;
    }

    /// <summary>The unqualified binding name every call of this facade is addressed to.</summary>
    public string Binding { get; }

    /// <summary>POST /remember. A first server slice supports the entity and episodic kinds.</summary>
    public Task<MemoryMutationResponse> RememberAsync(MemoryRememberRequest request, CancellationToken cancellation = default) =>
        PostAsync("/remember", request, MemoryWire.ReadMutation, cancellation);

    /// <summary>POST /recall.</summary>
    public Task<MemoryRecallResponse> RecallAsync(MemoryRecallRequest request, CancellationToken cancellation = default) =>
        PostAsync("/recall", request, MemoryWire.ReadRecall, cancellation);

    /// <summary>POST /facts/query.</summary>
    public Task<MemoryFactsResponse> FindFactsAsync(MemoryFactsRequest request, CancellationToken cancellation = default) =>
        PostAsync("/facts/query", request, MemoryWire.ReadFacts, cancellation);

    /// <summary>POST /facts/correct.</summary>
    public Task<MemoryMutationResponse> CorrectFactsAsync(MemoryCorrectionRequest request, CancellationToken cancellation = default) =>
        PostAsync("/facts/correct", request, MemoryWire.ReadMutation, cancellation);

    /// <summary>POST /consolidate: a committed <see cref="MemoryMutationResponse"/> or, for a dry run, a <see cref="MemoryPlanResponse"/>.</summary>
    public Task<MemoryConsolidateResponse> ConsolidateAsync(MemoryConsolidateRequest request, CancellationToken cancellation = default) =>
        PostAsync("/consolidate", request, MemoryWire.ReadConsolidate, cancellation);

    /// <summary>POST /forget. HTTP 202 (<see cref="MemoryMutationState.Pending"/>) is returned like any other answer.</summary>
    public Task<MemoryMutationResponse> ForgetAsync(MemoryForgetRequest request, CancellationToken cancellation = default) =>
        PostAsync("/forget", request, MemoryWire.ReadMutation, cancellation);

    /// <summary>POST /receipts/lookup: how to reconcile a mutation whose outcome is unknown.</summary>
    public Task<MemoryReceiptResponse> GetReceiptAsync(MemoryReceiptRequest request, CancellationToken cancellation = default) =>
        PostAsync("/receipts/lookup", request, MemoryWire.ReadReceiptResponse, cancellation);

    /// <summary>POST /erasures/get.</summary>
    public Task<MemoryErasureResponse> GetErasureAsync(MemoryErasureRequest request, CancellationToken cancellation = default) =>
        PostAsync("/erasures/get", request, MemoryWire.ReadErasureResponse, cancellation);

    /// <summary>POST /erasures/resume: one bounded chunk per call; you decide whether to call again.</summary>
    public Task<MemoryMutationResponse> ResumeErasureAsync(MemoryResumeRequest request, CancellationToken cancellation = default) =>
        PostAsync("/erasures/resume", request, MemoryWire.ReadMutation, cancellation);

    private async Task<T> PostAsync<T>(string route, object request, Func<JsonElement, T> read, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(request);
        var body = MemoryWire.ToElement(request);
        JsonElement response;
        try { response = await client.RequestAsync("POST", prefix + route, body, cancellation).ConfigureAwait(false); }
        catch (VLiteException error) when (error.Status == 404 && error.Code == "not_found")
        { throw new UnsupportedException("Server does not support the agent-memory routes; no fallback performed"); }
        return MemoryWire.Read(response, read);
    }
}
