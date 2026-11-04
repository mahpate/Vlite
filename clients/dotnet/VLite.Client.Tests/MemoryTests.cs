using System.Reflection;
using System.Runtime.CompilerServices;
using System.Text.Json;
using System.Text.Json.Serialization;
using VLite;

/// <summary>
/// Agent-memory HTTP operations of the .NET client. Wire expectations come from the serving contract and
/// tests/fixtures/agent_memory/wire; no expected value is derived from the code under test.
/// </summary>
static partial class MemoryTests
{
    static int count;
    static string? only;
    static readonly List<string> failures = [];
    static readonly JsonSerializerOptions Plain = new() { DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull };

    // Equal to the fixtures' http-inventory bodies for binding "notes", scope {agent:"a"}.
    static readonly MemoryScope A = new("a");
    static readonly MemorySlot Slot = new("e1", "role", "primary");
    static readonly MemoryRememberRequest RememberSample = new(A, "k1",
        [new MemoryItem("e1", MemoryKind.Entity, "Ada", 90, new MemorySource("s1", "caller"))]);
    static readonly MemoryRecallRequest RecallSample = new(A, Text: "Ada");
    static readonly MemoryFactsRequest FactsSample = new(A, Slot);
    static readonly MemoryCorrectionRequest CorrectSample = new(A, "correct", Slot, new MemoryRef("f1", 7), 7,
        MemoryInterval.From(0), MemoryCorrectionAction.Close, AtMs: 100);
    static readonly MemoryConsolidateRequest ConsolidateSample = new(A, "summary-key", [new MemoryInput("m", 1)],
        new MemoryItem("summary1", MemoryKind.Episodic, "Caller summary", 90,
            new MemorySource("summary-source", "caller", TransformationId: "summary-v1")));
    static readonly MemoryForgetRequest ForgetSample = new(A, "erase",
        new MemoryTarget.Ids([new MemoryRef("e1", 1)]), MemoryForgetMode.LogicalDelete);
    static readonly MemoryReceiptRequest ReceiptSample = new(A, MemoryOperation.Remember, "k1");
    static readonly MemoryErasureRequest ErasureSample = new(A, "j1");
    static readonly MemoryResumeRequest ResumeSample = new(A, "chunk1", "j1", 8, 1, "opaque1");

    sealed record Probe(string Operation, string Route, bool Mutation, string ResponseName, int Status,
        Func<MemoryClient, CancellationToken, Task<object>> Call);

    static async Task<object> Run<T>(Task<T> task) where T : notnull => await task;

    static readonly Probe[] Probes =
    [
        new("remember", "/remember", true, "response-remember", 200, (m, c) => Run(m.RememberAsync(RememberSample, c))),
        new("recall", "/recall", false, "response-recall", 200, (m, c) => Run(m.RecallAsync(RecallSample, c))),
        new("facts", "/facts/query", false, "response-facts", 200, (m, c) => Run(m.FindFactsAsync(FactsSample, c))),
        new("correct_facts", "/facts/correct", true, "response-correction", 200, (m, c) => Run(m.CorrectFactsAsync(CorrectSample, c))),
        new("consolidate", "/consolidate", true, "response-plan", 200, (m, c) => Run(m.ConsolidateAsync(ConsolidateSample, c))),
        new("forget", "/forget", true, "response-forget", 200, (m, c) => Run(m.ForgetAsync(ForgetSample, c))),
        new("receipt", "/receipts/lookup", false, "response-receipt", 200, (m, c) => Run(m.GetReceiptAsync(ReceiptSample, c))),
        new("erasure", "/erasures/get", false, "response-erasure", 200, (m, c) => Run(m.GetErasureAsync(ErasureSample, c))),
        new("resume_erasure", "/erasures/resume", true, "response-resume-pending", 202, (m, c) => Run(m.ResumeErasureAsync(ResumeSample, c)))
    ];

    static Server ServerFor(Probe probe) =>
        new() { Status = probe.Status, Reply = Fx.Value(probe.Operation, probe.ResponseName) };

    // ------------------------------------------------------------------ assertions
    static void Check(bool condition, string? note = null,
        [CallerArgumentExpression(nameof(condition))] string? expression = null, [CallerLineNumber] int line = 0)
    {
        if (!condition) throw new Exception($"line {line}: {expression}{(note is null ? "" : " :: " + note)}");
        count++;
    }

    static void Same<T>(T expected, T actual, string? note = null, [CallerLineNumber] int line = 0)
    {
        if (!EqualityComparer<T>.Default.Equals(expected, actual))
            throw new Exception($"line {line}: expected <{expected}> but was <{actual}>{(note is null ? "" : " :: " + note)}");
        count++;
    }

    static void Json(string expected, string actual, string? note = null, [CallerLineNumber] int line = 0)
    {
        if (!Js.Equivalent(expected, actual, out var difference))
            throw new Exception($"line {line}: JSON differs: {difference}{(note is null ? "" : " :: " + note)}");
        count++;
    }

    static async Task<T> Fails<T>(Func<Task> action, string? note = null, [CallerLineNumber] int line = 0) where T : Exception
    {
        try { await action(); }
        catch (T error) { count++; return error; }
        catch (Exception other)
        {
            throw new Exception($"line {line}: expected {typeof(T).Name} but got {other.GetType().Name}: {other.Message}{(note is null ? "" : " :: " + note)}");
        }
        throw new Exception($"line {line}: expected {typeof(T).Name}{(note is null ? "" : " :: " + note)}");
    }

    static async Task Scenario(string name, Func<Task> body)
    {
        if (only is not null && !name.Contains(only, StringComparison.OrdinalIgnoreCase)) return;
        try { await body(); }
        catch (Exception error)
        {
            var cause = error.InnerException is { } inner ? " <- " + inner.Message : "";
            failures.Add($"{name}: {error.Message}{cause}");
        }
    }

    public static async Task<int> RunAsync(string[] args)
    {
        only = args.SkipWhile(a => a != "--only").Skip(1).FirstOrDefault();
        count = 0;
        failures.Clear();
        await Scenario("memory-api-shape", ApiShape);
        await Scenario("memory-binding-validation", BindingValidation);
        await Scenario("memory-routes-and-headers", RoutesAndHeaders);
        await Scenario("memory-bodies-match-fixture-inventory", BodiesMatchFixtureInventory);
        await Scenario("memory-fixture-cases-end-to-end", FixtureCasesEndToEnd);
        await Scenario("memory-contract-exchanges", ContractExchanges);
        await Scenario("memory-request-components-round-trip", RequestComponentsRoundTrip);
        await Scenario("memory-maximal-requests", MaximalRequests);
        await Scenario("memory-optional-and-null-members-request", OptionalAndNullRequestMembers);
        await Scenario("memory-integers-request", IntegersRequest);
        await Scenario("memory-integer-input-rejection", IntegerInputRejection);
        await Scenario("memory-unions-request", UnionsRequest);
        await Scenario("memory-enum-wire-names", EnumWireNames);
        await Scenario("memory-vectors-and-floats", VectorsAndFloats);
        await Scenario("memory-keys-are-sent-verbatim", KeysVerbatim);
        await Scenario("memory-no-splitting-or-paging", NoSplittingOrPaging);
        await Scenario("memory-response-fixtures", ResponseFixtures);
        await Scenario("memory-response-explicit-values", ResponseExplicitValues);
        await Scenario("memory-response-maximal", ResponseMaximal);
        await Scenario("memory-response-integers", ResponseIntegers);
        await Scenario("memory-response-unions", ResponseUnions);
        await Scenario("memory-response-required-members", ResponseRequiredMembers);
        await Scenario("memory-response-optional-and-null", ResponseOptionalAndNull);
        await Scenario("memory-response-unknown-members", ResponseUnknownMembers);
        await Scenario("memory-response-invalid-shapes", ResponseInvalidShapes);
        await Scenario("memory-http-202", Http202);
        await Scenario("memory-error-envelopes", ErrorEnvelopes);
        await Scenario("memory-error-details", ErrorDetails);
        await Scenario("memory-unsupported-route", UnsupportedRoute);
        await Scenario("memory-no-retry", NoRetry);
        await Scenario("memory-timeout-and-cancellation", TimeoutAndCancellation);
        await Scenario("memory-deadline-is-not-http-timeout", DeadlineIsNotHttpTimeout);
        await Scenario("memory-client-side-refusals", ClientSideRefusals);
        await Scenario("memory-concurrent-facades", ConcurrentFacades);
        await Scenario("memory-number-helpers", NumberHelpers);
        await Scenario("memory-misc-api", MiscApi);
        await Scenario("memory-order-preserved", OrderPreserved);
        await Scenario("memory-response-boolean-positions", ResponseBooleanPositions);
        await Scenario("memory-request-boolean-positions", RequestBooleanPositions);
        await Scenario("memory-culture-invariance", CultureInvariance);
        await Scenario("memory-union-converters", UnionConverters);
        await Scenario("memory-number-edges", NumberEdges);
        if (failures.Count > 0)
        {
            Console.Error.WriteLine($"{failures.Count} memory scenario(s) FAILED:");
            foreach (var failure in failures) Console.Error.WriteLine("  - " + failure);
            Environment.Exit(1);
        }
        Console.WriteLine($"Memory scenarios passed ({count} assertions)");
        return count;
    }

    // ------------------------------------------------------------------ facade and routes
    static Task ApiShape()
    {
        var methods = typeof(MemoryClient).GetMethods(BindingFlags.Public | BindingFlags.Instance | BindingFlags.DeclaredOnly)
            .Where(m => !m.IsSpecialName).OrderBy(m => m.Name, StringComparer.Ordinal).ToArray();
        (string Name, Type Request, Type Result)[] expected =
        [
            ("ConsolidateAsync", typeof(MemoryConsolidateRequest), typeof(MemoryConsolidateResponse)),
            ("CorrectFactsAsync", typeof(MemoryCorrectionRequest), typeof(MemoryMutationResponse)),
            ("FindFactsAsync", typeof(MemoryFactsRequest), typeof(MemoryFactsResponse)),
            ("ForgetAsync", typeof(MemoryForgetRequest), typeof(MemoryMutationResponse)),
            ("GetErasureAsync", typeof(MemoryErasureRequest), typeof(MemoryErasureResponse)),
            ("GetReceiptAsync", typeof(MemoryReceiptRequest), typeof(MemoryReceiptResponse)),
            ("RecallAsync", typeof(MemoryRecallRequest), typeof(MemoryRecallResponse)),
            ("RememberAsync", typeof(MemoryRememberRequest), typeof(MemoryMutationResponse)),
            ("ResumeErasureAsync", typeof(MemoryResumeRequest), typeof(MemoryMutationResponse))
        ];
        Same(9, methods.Length, string.Join(",", methods.Select(m => m.Name)));
        for (var i = 0; i < expected.Length; i++)
        {
            var method = methods[i];
            Same(expected[i].Name, method.Name);
            Same(typeof(Task<>).MakeGenericType(expected[i].Result), method.ReturnType, method.Name);
            var parameters = method.GetParameters();
            Same(2, parameters.Length, method.Name);
            Same(expected[i].Request, parameters[0].ParameterType, method.Name);
            Same(typeof(CancellationToken), parameters[1].ParameterType, method.Name);
            Same("cancellation", parameters[1].Name, method.Name);
            Check(parameters[1].HasDefaultValue && parameters[1].DefaultValue is null or CancellationToken, method.Name);
            Check(parameters[1].IsOptional, method.Name);
        }
        var factory = typeof(Client).GetMethod("Memory", [typeof(string)]);
        Check(factory is not null && factory.ReturnType == typeof(MemoryClient) && factory.IsPublic);

        // Section 8 component names exist as public types.
        string[] names =
        [
            "MemoryScope", "MemoryEffectiveScope", "MemoryExpected", "MemoryInterval", "MemoryRef", "MemorySource",
            "MemoryProvenance", "MemoryScalar", "MemoryProperties", "MemoryCondition", "MemoryFilter", "MemoryEvidenceRef",
            "MemoryEvidence", "MemoryAssertion", "MemoryItem", "MemoryRememberRequest", "MemoryRanking", "MemoryLimits",
            "MemoryProjection", "MemoryRecallRequest", "MemorySlot", "MemoryFactsRequest", "MemoryCorrectionRequest",
            "MemoryInput", "MemoryConsolidateRequest", "MemoryTarget", "MemoryForgetRequest", "MemoryReceiptRequest",
            "MemoryErasureRequest", "MemoryResumeRequest", "MemoryReceipt", "MemoryOutcome", "MemoryFeed",
            "MemoryErasureNotice", "MemoryErasure", "MemoryMutationResponse", "MemoryReceiptResponse",
            "MemoryErasureResponse", "MemoryPlanResponse", "MemoryEvidenceView", "MemoryPath", "MemoryScores", "MemoryView",
            "MemoryReadMeta", "MemoryRecallResponse", "MemoryFactView", "MemoryFactsResponse", "MemoryCompeting",
            "MemoryErrorDetail", "MemoryErrorContext", "MemoryKind", "MemoryOperation", "MemoryConsolidateResponse"
        ];
        foreach (var name in names)
        {
            var type = typeof(Client).Assembly.GetType("VLite." + name);
            Check(type is { IsPublic: true }, name);
        }
        // Unions are sealed hierarchies: abstract base, sealed variants, no foreign subclasses.
        foreach (var union in new[] { typeof(MemoryTarget), typeof(MemoryExpected), typeof(MemoryScalar), typeof(MemoryConsolidateResponse) })
        {
            Check(union.IsAbstract, union.Name);
            var variants = typeof(Client).Assembly.GetTypes().Where(t => t.IsSubclassOf(union)).ToArray();
            Check(variants.Length >= 2 && variants.All(t => t.IsSealed), union.Name);
            Check(union.GetConstructors(BindingFlags.Public | BindingFlags.Instance).Length == 0, union.Name + " must not be extensible");
        }
        Same(5, typeof(Client).Assembly.GetTypes().Count(t => t.IsSubclassOf(typeof(MemoryTarget))));
        Same(2, typeof(Client).Assembly.GetTypes().Count(t => t.IsSubclassOf(typeof(MemoryExpected))));
        Same(4, typeof(Client).Assembly.GetTypes().Count(t => t.IsSubclassOf(typeof(MemoryScalar))));
        Same(2, typeof(Client).Assembly.GetTypes().Count(t => t.IsSubclassOf(typeof(MemoryConsolidateResponse))));
        return Task.CompletedTask;
    }

    static async Task BindingValidation()
    {
        var server = new Server();
        using var client = server.NewClient();
        foreach (var good in new[] { "a", "notes", "a-b_c9", "z" + new string('0', 62) })
            Check(client.Memory(good) is not null, good);
        foreach (var bad in new[] { "", "Notes", "1abc", "-a", "_a", "a b", "a/b", "a.b", "../x", "a\n", "z" + new string('0', 63), "é", "a?x=1", "a%2fb" })
            await Fails<ArgumentException>(() => { client.Memory(bad); return Task.CompletedTask; }, bad);
        await Fails<ArgumentException>(() => { client.Memory(null!); return Task.CompletedTask; });
        Same(0, server.Calls.Count);
    }

    static async Task RoutesAndHeaders()
    {
        foreach (var probe in Probes)
        {
            var server = ServerFor(probe);
            using var client = server.NewClient();
            await probe.Call(client.Memory("notes"), default);
            Same(1, server.Calls.Count, probe.Operation);
            var call = server.Last;
            Same("POST", call.Method, probe.Operation);
            Same("/v1/memory/notes" + probe.Route, call.Target, probe.Operation);
            // The same path appears in the independent fixture inventory.
            var inventory = Js.Parse(Fx.Value(probe.Operation, "http-inventory-" + probe.Operation));
            Same(inventory.GetProperty("path").GetString(), call.Target, probe.Operation);
            Same(inventory.GetProperty("method").GetString(), call.Method, probe.Operation);
            Same("Bearer " + Server.Token, call.Headers["Authorization"], probe.Operation);
            Check(call.Headers["Accept"].Contains("application/json"), probe.Operation);
            Check(call.Headers["Content-Type"].StartsWith("application/json"), probe.Operation);
            Check(call.Body is { Length: > 0 }, probe.Operation);
            Check(!call.Headers.Keys.Any(h => h.Equals("Idempotency-Key", StringComparison.OrdinalIgnoreCase)), probe.Operation);
            Check(!call.Headers.Keys.Any(h => h.Contains("tenant", StringComparison.OrdinalIgnoreCase)), probe.Operation);
        }
        // Another binding selects another path; facades from one client never share a binding.
        var probeServer = new Server { Reply = Fx.Value("recall", "response-recall") };
        using var shared = probeServer.NewClient();
        var first = shared.Memory("first");
        var second = shared.Memory("second-2_b");
        await second.RecallAsync(RecallSample);
        await first.RecallAsync(RecallSample);
        await second.RecallAsync(RecallSample);
        Same("/v1/memory/second-2_b/recall", probeServer.Calls[0].Target);
        Same("/v1/memory/first/recall", probeServer.Calls[1].Target);
        Same("/v1/memory/second-2_b/recall", probeServer.Calls[2].Target);
    }

    static async Task BodiesMatchFixtureInventory()
    {
        foreach (var probe in Probes)
        {
            var server = ServerFor(probe);
            using var client = server.NewClient();
            await probe.Call(client.Memory("notes"), default);
            var expected = Js.Parse(Fx.Value(probe.Operation, "http-inventory-" + probe.Operation)).GetProperty("body").GetRawText();
            Json(expected, server.Last.Body!, probe.Operation);
        }
    }

    // Typed deserialization of fixture request bodies (default options: attributes and converters only).
    static T Des<T>(string json) => JsonSerializer.Deserialize<T>(json) ?? throw new Exception("null request");

    static readonly Dictionary<string, Func<MemoryClient, string, Task<object>>> Calls = new()
    {
        ["remember"] = (m, json) => Run(m.RememberAsync(Des<MemoryRememberRequest>(json))),
        ["recall"] = (m, json) => Run(m.RecallAsync(Des<MemoryRecallRequest>(json))),
        ["facts"] = (m, json) => Run(m.FindFactsAsync(Des<MemoryFactsRequest>(json))),
        ["correct_facts"] = (m, json) => Run(m.CorrectFactsAsync(Des<MemoryCorrectionRequest>(json))),
        ["consolidate"] = (m, json) => Run(m.ConsolidateAsync(Des<MemoryConsolidateRequest>(json))),
        ["forget"] = (m, json) => Run(m.ForgetAsync(Des<MemoryForgetRequest>(json))),
        ["receipt"] = (m, json) => Run(m.GetReceiptAsync(Des<MemoryReceiptRequest>(json))),
        ["erasure"] = (m, json) => Run(m.GetErasureAsync(Des<MemoryErasureRequest>(json))),
        ["resume_erasure"] = (m, json) => Run(m.ResumeErasureAsync(Des<MemoryResumeRequest>(json)))
    };

    static string Serialize(object response) => JsonSerializer.Serialize(response, response.GetType(), Plain);

    static async Task FixtureCasesEndToEnd()
    {
        var sent = 0;
        foreach (var file in Fx.Files)
            foreach (var entry in Fx.Cases(file))
            {
                var expected = entry.GetProperty("expected");
                var status = expected.GetProperty("status").GetInt32();
                var name = entry.GetProperty("name").GetString()!;
                if (status >= 300) continue;
                var body = Js.Expand(entry.GetProperty("request").GetProperty("body").GetRawText());
                if (body is null) continue;
                var server = new Server { Status = status, Reply = expected.GetProperty("body").GetRawText() };
                using var client = server.NewClient();
                var operation = entry.GetProperty("operation").GetString()!;
                var result = await Calls[operation](client.Memory("notes"), body);
                Same(1, server.Calls.Count, name);
                Json(Js.WithIntegerInputsAsStrings(body), server.Last.Body!, name);
                Json(server.Reply, Serialize(result), name);
                sent++;
            }
        Same(51, sent, "51 executable 2xx fixture cases (the 8 MiB padding case is skipped)");
    }

    static async Task ContractExchanges()
    {
        var exchanges = Fx.ContractExchanges();
        Same(9, exchanges.Length);
        foreach (var exchange in exchanges)
        {
            var operation = exchange.GetProperty("operation").GetString()!;
            var server = new Server
            {
                Status = exchange.GetProperty("status").GetInt32(),
                Reply = exchange.GetProperty("response").GetRawText()
            };
            using var client = server.NewClient();
            var request = exchange.GetProperty("request").GetRawText();
            var result = await Calls[operation](client.Memory("notes"), request);
            Json(Js.WithIntegerInputsAsStrings(request), server.Last.Body!, operation);
            Json(server.Reply, Serialize(result), operation);
        }
        Check(exchanges.Count(e => e.GetProperty("status").GetInt32() == 202) == 1);
    }
}
