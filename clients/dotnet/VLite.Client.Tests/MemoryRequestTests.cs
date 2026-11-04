using System.Text.Json;
using VLite;

static partial class MemoryTests
{
    static Server Reply(string file, string name, int status = 200) => new() { Status = status, Reply = Fx.Value(file, name) };

    static async Task<string> SentBody(string file, string name, Func<MemoryClient, Task> call, int status = 200)
    {
        var server = Reply(file, name, status);
        using var client = server.NewClient();
        await call(client.Memory("notes"));
        Same(1, server.Calls.Count);
        return server.Last.Body!;
    }

    static readonly Dictionary<string, Type> ComponentTypes = new()
    {
        ["Scope"] = typeof(MemoryScope), ["Expected"] = typeof(MemoryExpected), ["Interval"] = typeof(MemoryInterval),
        ["Ref"] = typeof(MemoryRef), ["Source"] = typeof(MemorySource), ["Scalar"] = typeof(MemoryScalar),
        ["Properties"] = typeof(MemoryProperties), ["Condition"] = typeof(MemoryCondition), ["Filter"] = typeof(MemoryFilter),
        ["EvidenceRef"] = typeof(MemoryEvidenceRef), ["Evidence"] = typeof(MemoryEvidence), ["Assertion"] = typeof(MemoryAssertion),
        ["Item"] = typeof(MemoryItem), ["RememberRequest"] = typeof(MemoryRememberRequest), ["Ranking"] = typeof(MemoryRanking),
        ["Limits"] = typeof(MemoryLimits), ["Projection"] = typeof(MemoryProjection), ["RecallRequest"] = typeof(MemoryRecallRequest),
        ["Slot"] = typeof(MemorySlot), ["FactsRequest"] = typeof(MemoryFactsRequest),
        ["CorrectionRequest"] = typeof(MemoryCorrectionRequest), ["Input"] = typeof(MemoryInput),
        ["ConsolidateRequest"] = typeof(MemoryConsolidateRequest), ["Target"] = typeof(MemoryTarget),
        ["ForgetRequest"] = typeof(MemoryForgetRequest), ["ReceiptRequest"] = typeof(MemoryReceiptRequest),
        ["ErasureRequest"] = typeof(MemoryErasureRequest), ["ResumeRequest"] = typeof(MemoryResumeRequest)
    };

    /// <summary>Every valid request-side schema example of the fixtures survives a typed round trip.</summary>
    static Task RequestComponentsRoundTrip()
    {
        var checkedExamples = 0;
        foreach (var file in Fx.Files)
            foreach (var entry in Fx.Structural(file))
            {
                if (!entry.GetProperty("valid").GetBoolean()) continue;
                var schema = entry.GetProperty("schema").GetString()!;
                var name = entry.GetProperty("name").GetString()!;
                var raw = Js.Expand(entry.GetProperty("value").GetRawText());
                if (raw is null) continue;
                if (ComponentTypes.TryGetValue(schema, out var type))
                {
                    var typed = JsonSerializer.Deserialize(raw, type) ?? throw new Exception(name + ": null");
                    Json(Js.WithIntegerInputsAsStrings(raw), JsonSerializer.Serialize(typed, type, Plain), file + ":" + name);
                    checkedExamples++;
                }
                else if (schema == "U64" && entry.GetProperty("request").GetBoolean())
                {
                    // Exact JSON integer or canonical string in, canonical string out.
                    var viaRef = JsonSerializer.Deserialize<MemoryRef>("{\"id\":\"m\",\"revision\":" + raw + "}")!;
                    var viaInput = JsonSerializer.Deserialize<MemoryInput>("{\"id\":\"m\",\"revision\":" + raw + "}")!;
                    var canonical = raw.Trim('"');
                    Same(canonical, viaRef.Revision, name);
                    Same(canonical, viaInput.Revision.ToString(), name);
                    Json("{\"id\":\"m\",\"revision\":\"" + canonical + "\"}", JsonSerializer.Serialize(viaRef, Plain), name);
                    Json("{\"id\":\"m\",\"revision\":\"" + canonical + "\"}", JsonSerializer.Serialize(viaInput, Plain), name);
                    checkedExamples++;
                }
                else if (schema == "I64" && entry.GetProperty("request").GetBoolean())
                {
                    var viaInterval = JsonSerializer.Deserialize<MemoryInterval>("{\"valid_from_ms\":" + raw + ",\"valid_to_ms\":null}")!;
                    var canonical = raw.Trim('"');
                    Same(canonical, viaInterval.ValidFromMs, name);
                    Json("{\"valid_from_ms\":\"" + canonical + "\",\"valid_to_ms\":null}", JsonSerializer.Serialize(viaInterval, Plain), name);
                    checkedExamples++;
                }
            }
        Same(81, checkedExamples, "81 expandable valid request-side fixtures (the 64-field $properties example is skipped)");
        return Task.CompletedTask;
    }

    static async Task MaximalRequests()
    {
        var recall = new MemoryRecallRequest(
            new MemoryScope("a", "u", "s"),
            Scopes: [new MemoryScope("a"), new MemoryScope("a", "u")],
            Vector: [1f, 0.5f, -0.25f],
            Text: "Ada",
            EmbeddingProvider: "prov",
            EntitySeeds: ["e1", "e2"],
            Kinds: [MemoryKind.Entity, MemoryKind.Episodic, MemoryKind.Fact, MemoryKind.Relation],
            Filter: new MemoryFilter(
                All: [new MemoryCondition("rank", MemoryConditionOp.Gt, new MemoryScalar.Integer(5)),
                      new MemoryCondition("tag", MemoryConditionOp.Exists)],
                Any: [new MemoryCondition("name", MemoryConditionOp.Eq, new MemoryScalar.Text("x"))]),
            AsOfMs: -5,
            Ranking: new MemoryRanking(MemorySelection.Mmr, [0.5, 0.25, 0.25], 1000, MemoryFutureObservations.Clamp, 0.75),
            Limits: new MemoryLimits(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12),
            Projection: new MemoryProjection(false, true, true, ["p", "q"]),
            Mode: MemoryRecallMode.Approximate,
            Limit: 7,
            AllowPartial: true);
        Json("""
            {"scope":{"agent":"a","user":"u","session":"s"},"scopes":[{"agent":"a"},{"agent":"a","user":"u"}],
             "vector":[1,0.5,-0.25],"text":"Ada","embedding_provider":"prov","entity_seeds":["e1","e2"],
             "kinds":["entity","episodic","fact","relation"],
             "filter":{"all":[{"field":"rank","op":"gt","value":{"type":"integer","value":"5"}},{"field":"tag","op":"exists"}],
                       "any":[{"field":"name","op":"eq","value":{"type":"string","value":"x"}}]},
             "as_of_ms":"-5",
             "ranking":{"selection":"mmr","weights":[0.5,0.25,0.25],"half_life_ms":1000,"future_observations":"clamp","lambda":0.75},
             "limits":{"candidates":1,"vector_components":2,"evidence_bytes":3,"mmr_components":4,"deadline_ms":5,"max_hops":6,
                       "max_visited_nodes":7,"max_edges_examined":8,"max_edges_per_node":9,"max_output_nodes":10,
                       "max_output_edges":11,"max_output_paths":12},
             "projection":{"snippets":false,"provenance":true,"paths":true,"metadata":["p","q"]},
             "mode":"approximate","limit":7,"allow_partial":true}
            """, await SentBody("recall", "response-recall", m => m.RecallAsync(recall)));

        var remember = new MemoryRememberRequest(
            new MemoryScope("a", "u"), "k-max",
            [new MemoryItem("m1", MemoryKind.Fact, "Ada is a pioneer", 90, new MemorySource("s1", "caller", "urn:x", "t1"),
                Importance: 0.75, Confidence: 0.5, ExpiresAtMs: 5000,
                Provenance: [new MemorySource("p1", "doc"), new MemorySource("p2", "doc", Uri: "u", TransformationId: "tr")],
                References: [new MemoryRef("r1", 3), new MemoryRef("r2", 0)],
                Evidence: [new MemoryEvidenceRef("evidence", 18446744073709551615UL)],
                Metadata: new MemoryProperties
                {
                    ["rank"] = new MemoryScalar.Integer(-1), ["tag"] = new MemoryScalar.Text("t"),
                    ["ok"] = new MemoryScalar.Boolean(true), ["score"] = new MemoryScalar.Float(1.25)
                },
                Assertion: new MemoryAssertion("e1", "role", "primary", "e2", MemoryInterval.Between(0, 100),
                    new MemoryExpected.Revision(7), MemoryDisagreement.Disputed),
                Expected: new MemoryExpected.Absent(), Merge: MemoryMerge.MergeProvenance)],
            Evidence: [new MemoryEvidence("evidence", 1, Vector: [1f, 0f], Text: "Ada", PayloadBase64: "AA==",
                Metadata: new MemoryProperties { ["p"] = new MemoryScalar.Boolean(false) })],
            MaxReferences: 256, DeadlineMs: 5000, RequireCompleteFeed: false);
        Json("""
            {"scope":{"agent":"a","user":"u"},"idempotency_key":"k-max",
             "items":[{"id":"m1","kind":"fact","content":"Ada is a pioneer","observed_at_ms":"90",
                       "source":{"source_id":"s1","source_kind":"caller","uri":"urn:x","transformation_id":"t1"},
                       "importance":0.75,"confidence":0.5,"expires_at_ms":"5000",
                       "provenance":[{"source_id":"p1","source_kind":"doc"},{"source_id":"p2","source_kind":"doc","uri":"u","transformation_id":"tr"}],
                       "references":[{"id":"r1","revision":"3"},{"id":"r2","revision":"0"}],
                       "evidence":[{"collection":"evidence","record":"18446744073709551615"}],
                       "metadata":{"rank":{"type":"integer","value":"-1"},"tag":{"type":"string","value":"t"},
                                   "ok":{"type":"boolean","value":true},"score":{"type":"float","value":1.25}},
                       "assertion":{"subject_id":"e1","predicate":"role","slot_key":"primary","destination_id":"e2",
                                    "interval":{"valid_from_ms":"0","valid_to_ms":"100"},"slot_expected":{"revision":"7"},
                                    "disagreement":"disputed"},
                       "expected":{"absent":true},"merge":"merge_provenance"}],
             "evidence":[{"collection":"evidence","record":"1","vector":[1,0],"text":"Ada","payload_base64":"AA==",
                          "metadata":{"p":{"type":"boolean","value":false}}}],
             "max_references":256,"deadline_ms":5000,"require_complete_feed":false}
            """, await SentBody("remember", "response-remember", m => m.RememberAsync(remember)));

        var correction = new MemoryCorrectionRequest(A, "k-fix", Slot, new MemoryRef("f1", 7), 7, new MemoryInterval("0", null),
            MemoryCorrectionAction.Replace, AtMs: -100, CorrectionInterval: MemoryInterval.Between(-5, 5),
            Replacement: new MemoryItem("f2", MemoryKind.Relation, "x", 1, new MemorySource("s", "k"),
                Assertion: new MemoryAssertion("e1", "role", "primary", "e3", MemoryInterval.Unbounded, new MemoryExpected.Revision(7))),
            Evidence: [new MemoryEvidence("ev", 9)], MaxReferences: 1, DeadlineMs: 1, RequireCompleteFeed: true);
        Json("""
            {"scope":{"agent":"a"},"idempotency_key":"k-fix","slot":{"subject_id":"e1","predicate":"role","slot_key":"primary"},
             "series":{"id":"f1","revision":"7"},"slot_revision":"7","interval":{"valid_from_ms":"0","valid_to_ms":null},
             "action":"replace","at_ms":"-100","correction_interval":{"valid_from_ms":"-5","valid_to_ms":"5"},
             "replacement":{"id":"f2","kind":"relation","content":"x","observed_at_ms":"1","source":{"source_id":"s","source_kind":"k"},
                            "assertion":{"subject_id":"e1","predicate":"role","slot_key":"primary","destination_id":"e3",
                                         "interval":{"valid_from_ms":null,"valid_to_ms":null},"slot_expected":{"revision":"7"}}},
             "evidence":[{"collection":"ev","record":"9"}],"max_references":1,"deadline_ms":1,"require_complete_feed":true}
            """, await SentBody("correct_facts", "response-correction", m => m.CorrectFactsAsync(correction)));

        var consolidate = new MemoryConsolidateRequest(A, "k-c",
            [new MemoryInput("m1", 1, MemoryDisposition.Retain), new MemoryInput("m2", 2, MemoryDisposition.LogicalDelete),
             new MemoryInput("m3", 18446744073709551615UL)],
            new MemoryItem("s1", MemoryKind.Episodic, "sum", 3, new MemorySource("src", "caller", TransformationId: "t")),
            Evidence: [new MemoryEvidence("ev", 2, Text: "t")], DryRun: true, MaxReferences: 64, DeadlineMs: 250, RequireCompleteFeed: false);
        Json("""
            {"scope":{"agent":"a"},"idempotency_key":"k-c",
             "inputs":[{"id":"m1","revision":"1","disposition":"retain"},{"id":"m2","revision":"2","disposition":"logical_delete"},
                       {"id":"m3","revision":"18446744073709551615"}],
             "output":{"id":"s1","kind":"episodic","content":"sum","observed_at_ms":"3","source":{"source_id":"src","source_kind":"caller","transformation_id":"t"}},
             "evidence":[{"collection":"ev","record":"2","text":"t"}],"dry_run":true,"max_references":64,"deadline_ms":250,"require_complete_feed":false}
            """, await SentBody("consolidate", "response-plan", m => m.ConsolidateAsync(consolidate)));

        var forget = new MemoryForgetRequest(A, "k-f", new MemoryTarget.Series(new MemoryRef("f1", 2)), MemoryForgetMode.Expire,
            ExpiresAtMs: 123, AllowJob: false, MaxReferences: 2, DeadlineMs: 3, RequireCompleteFeed: true);
        Json("""
            {"scope":{"agent":"a"},"idempotency_key":"k-f","target":{"series":{"id":"f1","revision":"2"}},"mode":"expire",
             "expires_at_ms":"123","allow_job":false,"max_references":2,"deadline_ms":3,"require_complete_feed":true}
            """, await SentBody("forget", "response-retention", m => m.ForgetAsync(forget)));

        var facts = new MemoryFactsRequest(A, Slot, AsOfMs: 0, History: false, Limit: 100, Limits: new MemoryLimits(Candidates: 1000),
            Projection: new MemoryProjection(Metadata: []), AllowPartial: false);
        Json("""
            {"scope":{"agent":"a"},"slot":{"subject_id":"e1","predicate":"role","slot_key":"primary"},"as_of_ms":"0","history":false,
             "limit":100,"limits":{"candidates":1000},"projection":{"metadata":[]},"allow_partial":false}
            """, await SentBody("facts", "response-facts", m => m.FindFactsAsync(facts)));

        var resume = new MemoryResumeRequest(new MemoryScope("a", "u", "s"), "k", "j", ulong.MaxValue, 0, "tok",
            MaxReferences: 5, DeadlineMs: 6, RequireCompleteFeed: false);
        Json("""
            {"scope":{"agent":"a","user":"u","session":"s"},"idempotency_key":"k","job_id":"j","revision":"18446744073709551615",
             "generation":"0","resume_token":"tok","max_references":5,"deadline_ms":6,"require_complete_feed":false}
            """, await SentBody("resume_erasure", "response-resume-pending", m => m.ResumeErasureAsync(resume), 202));
    }

    static async Task OptionalAndNullRequestMembers()
    {
        // Unset optional members are omitted; nothing is defaulted or sent as null.
        Json("""{"scope":{"agent":"a"},"text":"x"}""",
            await SentBody("recall", "response-recall", m => m.RecallAsync(new MemoryRecallRequest(A, Text: "x"))));
        Json("""{"scope":{"agent":"a"},"text":"x","filter":{},"ranking":{},"limits":{},"projection":{}}""",
            await SentBody("recall", "response-recall", m => m.RecallAsync(new MemoryRecallRequest(A, Text: "x",
                Filter: new MemoryFilter(), Ranking: new MemoryRanking(), Limits: new MemoryLimits(), Projection: new MemoryProjection()))));
        Json("""{"scope":{"agent":"u"},"text":"x"}""",
            await SentBody("recall", "response-recall", m => m.RecallAsync(new MemoryRecallRequest(new MemoryScope("u"), Text: "x"))));
        Json("""{"scope":{"agent":"a","session":"s"},"text":"x"}""",
            await SentBody("recall", "response-recall", m => m.RecallAsync(new MemoryRecallRequest(new MemoryScope("a", Session: "s"), Text: "x"))));
        // Interval endpoints are required but nullable: explicit null, never omitted.
        foreach (var (interval, json) in new (MemoryInterval, string)[]
        {
            (MemoryInterval.Unbounded, """{"valid_from_ms":null,"valid_to_ms":null}"""),
            (MemoryInterval.From(5), """{"valid_from_ms":"5","valid_to_ms":null}"""),
            (MemoryInterval.Until(-5), """{"valid_from_ms":null,"valid_to_ms":"-5"}"""),
            (MemoryInterval.Between(long.MinValue, long.MaxValue), """{"valid_from_ms":"-9223372036854775808","valid_to_ms":"9223372036854775807"}"""),
            (new MemoryInterval(null, "0"), """{"valid_from_ms":null,"valid_to_ms":"0"}"""),
        })
        {
            var request = CorrectSample with { Interval = interval };
            var body = Js.Parse(await SentBody("correct_facts", "response-correction", m => m.CorrectFactsAsync(request)));
            Json(json, body.GetProperty("interval").GetRawText());
        }
        // Zero, false and empty values are values, not absence.
        var falsy = new MemoryRememberRequest(A, "k", [new MemoryItem("m", MemoryKind.Entity, "", 0, new MemorySource("s", "k"),
            Importance: 0, Confidence: 0, ExpiresAtMs: 0, Provenance: [], References: [], Evidence: [], Metadata: new MemoryProperties())],
            Evidence: [], MaxReferences: 0, DeadlineMs: 0, RequireCompleteFeed: false);
        Json("""
            {"scope":{"agent":"a"},"idempotency_key":"k",
             "items":[{"id":"m","kind":"entity","content":"","observed_at_ms":"0","source":{"source_id":"s","source_kind":"k"},
                       "importance":0,"confidence":0,"expires_at_ms":"0","provenance":[],"references":[],"evidence":[],"metadata":{}}],
             "evidence":[],"max_references":0,"deadline_ms":0,"require_complete_feed":false}
            """, await SentBody("remember", "response-remember", m => m.RememberAsync(falsy)));
        Json("""{"scope":{"agent":"a"},"slot":{"subject_id":"e1","predicate":"role","slot_key":"primary"},"limit":0,"history":false,"allow_partial":false}""",
            await SentBody("facts", "response-facts", m => m.FindFactsAsync(new MemoryFactsRequest(A, Slot, History: false, Limit: 0, AllowPartial: false))));
        // A bare required record keeps exactly its required members.
        var bare = Js.Parse(await SentBody("consolidate", "response-plan", m => m.ConsolidateAsync(ConsolidateSample)));
        Same(4, bare.EnumerateObject().Count());
        Same(2, bare.GetProperty("inputs")[0].EnumerateObject().Count());
        var bareItem = bare.GetProperty("output");
        Same(5, bareItem.EnumerateObject().Count());
        Same(3, bareItem.GetProperty("source").EnumerateObject().Count());
        var bareSource = Js.Parse(await SentBody("remember", "response-remember", m => m.RememberAsync(RememberSample)));
        Same(2, bareSource.GetProperty("items")[0].GetProperty("source").EnumerateObject().Count());
        Same(3, bareSource.EnumerateObject().Count());
    }

    static async Task IntegersRequest()
    {
        string[] unsigned = ["0", "1", "18446744073709551615", "9223372036854775808"];
        ulong[] numbers = [0, 1, ulong.MaxValue, 9223372036854775808UL];
        for (var i = 0; i < unsigned.Length; i++)
        {
            var text = unsigned[i];
            var number = numbers[i];
            // Shared records accept the number or its canonical string; both produce the same wire string.
            foreach (var reference in new[] { new MemoryRef("x", number), new MemoryRef("x", text) })
            {
                var body = Js.Parse(await SentBody("forget", "response-forget", m => m.ForgetAsync(ForgetSample with
                { Target = new MemoryTarget.Entity(reference) })));
                var revision = body.GetProperty("target").GetProperty("entity").GetProperty("revision");
                Same(JsonValueKind.String, revision.ValueKind);
                Same(text, revision.GetString());
            }
            var typed = new (string File, string Reply, Func<MemoryClient, Task> Call, string Path)[]
            {
                ("forget", "response-forget", m => m.ForgetAsync(ForgetSample with { Mode = MemoryForgetMode.Expire, ExpiresAtMs = number }), "expires_at_ms"),
                ("consolidate", "response-plan", m => m.ConsolidateAsync(ConsolidateSample with { Inputs = [new MemoryInput("m", number)] }), "inputs/0/revision"),
                ("resume_erasure", "response-resume-pending", m => m.ResumeErasureAsync(ResumeSample with { Revision = number }), "revision"),
                ("resume_erasure", "response-resume-pending", m => m.ResumeErasureAsync(ResumeSample with { Generation = number }), "generation"),
                ("correct_facts", "response-correction", m => m.CorrectFactsAsync(CorrectSample with { SlotRevision = number }), "slot_revision"),
                ("remember", "response-remember", m => m.RememberAsync(RememberSample with { Items = [RememberSample.Items[0] with { ExpiresAtMs = number }] }), "items/0/expires_at_ms"),
                ("remember", "response-remember", m => m.RememberAsync(RememberSample with { Items = [RememberSample.Items[0] with { Expected = new MemoryExpected.Revision(number) }] }), "items/0/expected/revision"),
                ("remember", "response-remember", m => m.RememberAsync(RememberSample with { Items = [RememberSample.Items[0] with { Expected = new MemoryExpected.Revision(text) }] }), "items/0/expected/revision"),
                ("remember", "response-remember", m => m.RememberAsync(RememberSample with { Items = [RememberSample.Items[0] with { Evidence = [new MemoryEvidenceRef("ev", number)] }] }), "items/0/evidence/0/record"),
                ("remember", "response-remember", m => m.RememberAsync(RememberSample with { Evidence = [new MemoryEvidence("ev", number)] }), "evidence/0/record"),
            };
            foreach (var (file, reply, call, path) in typed)
            {
                var status = file == "resume_erasure" ? 202 : 200;
                var body = await SentBody(file, reply, call, status);
                var value = Js.Parse(Js.Get(body, path));
                Same(JsonValueKind.String, value.ValueKind, path);
                Same(text, value.GetString(), path);
            }
        }

        string[] signed = ["0", "-1", "1", "-9223372036854775808", "9223372036854775807"];
        long[] longs = [0, -1, 1, long.MinValue, long.MaxValue];
        for (var i = 0; i < signed.Length; i++)
        {
            var text = signed[i];
            var number = longs[i];
            var typed = new (string File, string Reply, Func<MemoryClient, Task> Call, string Path)[]
            {
                ("remember", "response-remember", m => m.RememberAsync(RememberSample with { Items = [RememberSample.Items[0] with { ObservedAtMs = number }] }), "items/0/observed_at_ms"),
                ("recall", "response-recall", m => m.RecallAsync(RecallSample with { AsOfMs = number }), "as_of_ms"),
                ("facts", "response-facts", m => m.FindFactsAsync(FactsSample with { AsOfMs = number }), "as_of_ms"),
                ("correct_facts", "response-correction", m => m.CorrectFactsAsync(CorrectSample with { AtMs = number }), "at_ms"),
                ("correct_facts", "response-correction", m => m.CorrectFactsAsync(CorrectSample with { Interval = MemoryInterval.Between(number, number) }), "interval/valid_to_ms"),
                ("correct_facts", "response-correction", m => m.CorrectFactsAsync(CorrectSample with { Interval = MemoryInterval.Between(number, number) }), "interval/valid_from_ms"),
                ("correct_facts", "response-correction", m => m.CorrectFactsAsync(CorrectSample with { Interval = new MemoryInterval(text, text) }), "interval/valid_from_ms"),
                ("correct_facts", "response-correction", m => m.CorrectFactsAsync(CorrectSample with { CorrectionInterval = MemoryInterval.From(number) }), "correction_interval/valid_from_ms"),
                ("recall", "response-recall", m => m.RecallAsync(RecallSample with { Filter = new MemoryFilter(All: [new MemoryCondition("f", MemoryConditionOp.Eq, new MemoryScalar.Integer(number))]) }), "filter/all/0/value/value"),
                ("recall", "response-recall", m => m.RecallAsync(RecallSample with { Filter = new MemoryFilter(All: [new MemoryCondition("f", MemoryConditionOp.Eq, new MemoryScalar.Integer(text))]) }), "filter/all/0/value/value"),
            };
            foreach (var (file, reply, call, path) in typed)
            {
                var body = await SentBody(file, reply, call);
                var value = Js.Parse(Js.Get(body, path));
                Same(JsonValueKind.String, value.ValueKind, path);
                Same(text, value.GetString(), path);
            }
        }

        // Budgets and counts are JSON integers, never strings.
        var budgets = Js.Parse(await SentBody("recall", "response-recall", m => m.RecallAsync(RecallSample with
        {
            Limit = 5, Limits = new MemoryLimits(Candidates: ulong.MaxValue, DeadlineMs: 1, MaxHops: 0), Ranking = new MemoryRanking(HalfLifeMs: 10)
        })));
        Same(JsonValueKind.Number, budgets.GetProperty("limit").ValueKind);
        Same(JsonValueKind.Number, budgets.GetProperty("limits").GetProperty("candidates").ValueKind);
        Same("18446744073709551615", budgets.GetProperty("limits").GetProperty("candidates").GetRawText());
        Same(JsonValueKind.Number, budgets.GetProperty("limits").GetProperty("deadline_ms").ValueKind);
        Same(JsonValueKind.Number, budgets.GetProperty("limits").GetProperty("max_hops").ValueKind);
        Same(JsonValueKind.Number, budgets.GetProperty("ranking").GetProperty("half_life_ms").ValueKind);
        var mutation = Js.Parse(await SentBody("remember", "response-remember", m => m.RememberAsync(RememberSample with { DeadlineMs = 5000, MaxReferences = 256 })));
        Same(JsonValueKind.Number, mutation.GetProperty("deadline_ms").ValueKind);
        Same("5000", mutation.GetProperty("deadline_ms").GetRawText());
        Same(JsonValueKind.Number, mutation.GetProperty("max_references").ValueKind);
        Same(JsonValueKind.True, Js.Parse(await SentBody("remember", "response-remember", m => m.RememberAsync(RememberSample with { RequireCompleteFeed = true }))).GetProperty("require_complete_feed").ValueKind);
    }

    static async Task IntegerInputRejection()
    {
        string[] badUnsigned = ["", "01", "+1", "-1", "-0", "1.0", "1e3", " 1", "1 ", "0x10", "١", "18446744073709551616",
            "99999999999999999999", "00", "1_0", "NaN", "\n1"];
        string[] badSigned = ["", "01", "-01", "+1", "-0", "1.0", "1e3", " 1", "-1 ", "0x10", "٠", "9223372036854775808",
            "-9223372036854775809", "--1", "1-"];
        string[] goodUnsigned = ["0", "7", "18446744073709551615"];
        string[] goodSigned = ["0", "-1", "9223372036854775807", "-9223372036854775808"];

        // Every string-typed integer position: forget target ref, interval endpoints, scalar integer, expected revision.
        (string Position, Func<MemoryClient, string, Task> Call, bool Signed)[] positions =
        [
            ("ref.revision", (m, v) => m.ForgetAsync(ForgetSample with { Target = new MemoryTarget.Ids([new MemoryRef("x", v)]) }), false),
            ("series.revision", (m, v) => m.ForgetAsync(ForgetSample with { Target = new MemoryTarget.Series(new MemoryRef("x", v)) }), false),
            ("entity.revision", (m, v) => m.ForgetAsync(ForgetSample with { Target = new MemoryTarget.Entity(new MemoryRef("x", v)) }), false),
            ("correction.series", (m, v) => m.CorrectFactsAsync(CorrectSample with { Series = new MemoryRef("f", v) }), false),
            ("item.references", (m, v) => m.RememberAsync(RememberSample with { Items = [RememberSample.Items[0] with { References = [new MemoryRef("r", v)] }] }), false),
            ("expected.revision", (m, v) => m.RememberAsync(RememberSample with { Items = [RememberSample.Items[0] with { Expected = new MemoryExpected.Revision(v) }] }), false),
            ("interval.from", (m, v) => m.CorrectFactsAsync(CorrectSample with { Interval = new MemoryInterval(v, null) }), true),
            ("interval.to", (m, v) => m.CorrectFactsAsync(CorrectSample with { Interval = new MemoryInterval(null, v) }), true),
            ("correction_interval", (m, v) => m.CorrectFactsAsync(CorrectSample with { CorrectionInterval = new MemoryInterval(v, v) }), true),
            ("scalar.integer", (m, v) => m.RecallAsync(RecallSample with { Filter = new MemoryFilter(Any: [new MemoryCondition("f", MemoryConditionOp.Lt, new MemoryScalar.Integer(v))]) }), true),
            ("metadata.integer", (m, v) => m.RememberAsync(RememberSample with { Items = [RememberSample.Items[0] with { Metadata = new MemoryProperties { ["k"] = new MemoryScalar.Integer(v) } }] }), true),
        ];
        foreach (var (position, call, isSigned) in positions)
        {
            foreach (var bad in isSigned ? badSigned : badUnsigned)
            {
                var server = Reply("remember", "response-remember");
                using var client = server.NewClient();
                await Fails<ArgumentException>(() => call(client.Memory("notes"), bad), position + " <" + bad + ">");
                Same(0, server.Calls.Count, position + " <" + bad + "> must not reach the network");
            }
            foreach (var good in isSigned ? goodSigned : goodUnsigned)
            {
                var server = Reply("remember", "response-remember");
                using var client = server.NewClient();
                try { await call(client.Memory("notes"), good); } catch (TransportException) { /* the canned reply may not fit the operation */ }
                Same(1, server.Calls.Count, position + " <" + good + ">");
            }
        }
    }

    static async Task UnionsRequest()
    {
        var targets = new (MemoryTarget Target, string Json)[]
        {
            (new MemoryTarget.Ids([new MemoryRef("a", 1), new MemoryRef("b", 2)]), """{"ids":[{"id":"a","revision":"1"},{"id":"b","revision":"2"}]}"""),
            (new MemoryTarget.Series(new MemoryRef("f", 3)), """{"series":{"id":"f","revision":"3"}}"""),
            (new MemoryTarget.Entity(new MemoryRef("e", 4)), """{"entity":{"id":"e","revision":"4"}}"""),
            (new MemoryTarget.Session(), """{"session":true}"""),
            (new MemoryTarget.Scope(), """{"scope":true}"""),
        };
        foreach (var (target, json) in targets)
        {
            var body = Js.Parse(await SentBody("forget", "response-forget", m => m.ForgetAsync(ForgetSample with { Target = target })));
            Json(json, body.GetProperty("target").GetRawText(), target.GetType().Name);
            Same(4, body.EnumerateObject().Count());
        }

        var expecteds = new (MemoryExpected Expected, string Json)[]
        {
            (new MemoryExpected.Absent(), """{"absent":true}"""),
            (new MemoryExpected.Revision(7), """{"revision":"7"}"""),
            (new MemoryExpected.Revision("18446744073709551615"), """{"revision":"18446744073709551615"}"""),
        };
        foreach (var (expected, json) in expecteds)
        {
            var item = RememberSample.Items[0] with
            {
                Expected = expected,
                Assertion = new MemoryAssertion("e1", "role", "primary", "e2", MemoryInterval.Unbounded, expected)
            };
            var body = Js.Parse(await SentBody("remember", "response-remember", m => m.RememberAsync(RememberSample with { Items = [item] })));
            Json(json, body.GetProperty("items")[0].GetProperty("expected").GetRawText(), expected.GetType().Name);
            Json(json, body.GetProperty("items")[0].GetProperty("assertion").GetProperty("slot_expected").GetRawText(), expected.GetType().Name);
        }

        (MemoryScalar Scalar, string Json)[] scalars =
        [
            (new MemoryScalar.Text("x"), """{"type":"string","value":"x"}"""),
            (new MemoryScalar.Text(""), """{"type":"string","value":""}"""),
            (new MemoryScalar.Text("雪 \"q\" \\ \n"), "{\"type\":\"string\",\"value\":\"雪 \\\"q\\\" \\\\ \\n\"}"),
            (new MemoryScalar.Integer(-9223372036854775808), """{"type":"integer","value":"-9223372036854775808"}"""),
            (new MemoryScalar.Integer("42"), """{"type":"integer","value":"42"}"""),
            (new MemoryScalar.Float(1.25), """{"type":"float","value":1.25}"""),
            (new MemoryScalar.Float(0), """{"type":"float","value":0}"""),
            (new MemoryScalar.Float(-0.5), """{"type":"float","value":-0.5}"""),
            (new MemoryScalar.Float(1e21), """{"type":"float","value":1e21}"""),
            (new MemoryScalar.Float(double.MaxValue), """{"type":"float","value":1.7976931348623157e308}"""),
            (new MemoryScalar.Boolean(true), """{"type":"boolean","value":true}"""),
            (new MemoryScalar.Boolean(false), """{"type":"boolean","value":false}"""),
        ];
        foreach (var (scalar, json) in scalars)
        {
            var viaFilter = Js.Parse(await SentBody("recall", "response-recall", m => m.RecallAsync(RecallSample with
            { Filter = new MemoryFilter(All: [new MemoryCondition("f", MemoryConditionOp.Eq, scalar)]) })));
            Json(json, viaFilter.GetProperty("filter").GetProperty("all")[0].GetProperty("value").GetRawText(), json);
            var viaMetadata = Js.Parse(await SentBody("remember", "response-remember", m => m.RememberAsync(RememberSample with
            { Items = [RememberSample.Items[0] with { Metadata = new MemoryProperties { ["k"] = scalar } }] })));
            Json(json, viaMetadata.GetProperty("items")[0].GetProperty("metadata").GetProperty("k").GetRawText(), json);
        }
        // Kind of the float value is a JSON number, even for integral values.
        var floatBody = Js.Parse(await SentBody("recall", "response-recall", m => m.RecallAsync(RecallSample with
        { Filter = new MemoryFilter(All: [new MemoryCondition("f", MemoryConditionOp.Gte, new MemoryScalar.Float(3))]) })));
        Same(JsonValueKind.Number, floatBody.GetProperty("filter").GetProperty("all")[0].GetProperty("value").GetProperty("value").ValueKind);
        // A condition without a value (exists) omits it.
        var exists = Js.Parse(await SentBody("recall", "response-recall", m => m.RecallAsync(RecallSample with
        { Filter = new MemoryFilter(All: [new MemoryCondition("f", MemoryConditionOp.Exists)]) })));
        Same(2, exists.GetProperty("filter").GetProperty("all")[0].EnumerateObject().Count());

        // Variants compare by value and differ from each other.
        Check(new MemoryTarget.Scope() == new MemoryTarget.Scope());
        Check(new MemoryTarget.Scope() != (MemoryTarget)new MemoryTarget.Session());
        Check(new MemoryExpected.Absent() != (MemoryExpected)new MemoryExpected.Revision(0));
        Check(new MemoryScalar.Integer(1) == new MemoryScalar.Integer("1"));
        Check(new MemoryScalar.Integer(1) != (MemoryScalar)new MemoryScalar.Text("1"));
    }

    static async Task EnumWireNames()
    {
        var recall = Js.Parse(await SentBody("recall", "response-recall", m => m.RecallAsync(new MemoryRecallRequest(A, Text: "x",
            Kinds: [MemoryKind.Entity, MemoryKind.Episodic, MemoryKind.Fact, MemoryKind.Relation],
            Filter: new MemoryFilter(All:
            [
                new MemoryCondition("f", MemoryConditionOp.Eq, new MemoryScalar.Boolean(true)),
                new MemoryCondition("f", MemoryConditionOp.Lt, new MemoryScalar.Boolean(true)),
                new MemoryCondition("f", MemoryConditionOp.Lte, new MemoryScalar.Boolean(true)),
                new MemoryCondition("f", MemoryConditionOp.Gt, new MemoryScalar.Boolean(true)),
                new MemoryCondition("f", MemoryConditionOp.Gte, new MemoryScalar.Boolean(true)),
                new MemoryCondition("f", MemoryConditionOp.Exists),
            ]), Mode: MemoryRecallMode.Exact))));
        Json("""["entity","episodic","fact","relation"]""", recall.GetProperty("kinds").GetRawText());
        Json("""["eq","lt","lte","gt","gte","exists"]""",
            "[" + string.Join(",", recall.GetProperty("filter").GetProperty("all").EnumerateArray().Select(c => c.GetProperty("op").GetRawText())) + "]");
        Same("exact", recall.GetProperty("mode").GetString());
        foreach (var (mode, wire) in new[] { (MemoryRecallMode.Exact, "exact"), (MemoryRecallMode.Approximate, "approximate") })
            Same(wire, Js.Parse(await SentBody("recall", "response-recall", m => m.RecallAsync(RecallSample with { Mode = mode }))).GetProperty("mode").GetString());
        foreach (var (selection, wire) in new[] { (MemorySelection.Weighted, "weighted"), (MemorySelection.Exact, "exact"), (MemorySelection.Mmr, "mmr") })
            Same(wire, Js.Parse(await SentBody("recall", "response-recall", m => m.RecallAsync(RecallSample with { Ranking = new MemoryRanking(selection) }))).GetProperty("ranking").GetProperty("selection").GetString());
        foreach (var (policy, wire) in new[] { (MemoryFutureObservations.Reject, "reject"), (MemoryFutureObservations.Clamp, "clamp") })
            Same(wire, Js.Parse(await SentBody("recall", "response-recall", m => m.RecallAsync(RecallSample with { Ranking = new MemoryRanking(FutureObservations: policy) }))).GetProperty("ranking").GetProperty("future_observations").GetString());

        foreach (var (kind, wire) in new[] { (MemoryKind.Entity, "entity"), (MemoryKind.Episodic, "episodic"), (MemoryKind.Fact, "fact"), (MemoryKind.Relation, "relation") })
            Same(wire, Js.Parse(await SentBody("remember", "response-remember", m => m.RememberAsync(RememberSample with { Items = [RememberSample.Items[0] with { Kind = kind }] }))).GetProperty("items")[0].GetProperty("kind").GetString());
        foreach (var (merge, wire) in new[] { (MemoryMerge.None, "none"), (MemoryMerge.MergeProvenance, "merge_provenance") })
            Same(wire, Js.Parse(await SentBody("remember", "response-remember", m => m.RememberAsync(RememberSample with { Items = [RememberSample.Items[0] with { Merge = merge }] }))).GetProperty("items")[0].GetProperty("merge").GetString());
        foreach (var (policy, wire) in new[] { (MemoryDisagreement.Reject, "reject"), (MemoryDisagreement.Disputed, "disputed") })
        {
            var item = RememberSample.Items[0] with { Assertion = new MemoryAssertion("e1", "p", "k", "e2", MemoryInterval.Unbounded, new MemoryExpected.Absent(), policy) };
            Same(wire, Js.Parse(await SentBody("remember", "response-remember", m => m.RememberAsync(RememberSample with { Items = [item] }))).GetProperty("items")[0].GetProperty("assertion").GetProperty("disagreement").GetString());
        }
        foreach (var (action, wire) in new[] { (MemoryCorrectionAction.Close, "close"), (MemoryCorrectionAction.Replace, "replace"), (MemoryCorrectionAction.Remove, "remove") })
            Same(wire, Js.Parse(await SentBody("correct_facts", "response-correction", m => m.CorrectFactsAsync(CorrectSample with { Action = action }))).GetProperty("action").GetString());
        foreach (var (disposition, wire) in new[] { (MemoryDisposition.Retain, "retain"), (MemoryDisposition.LogicalDelete, "logical_delete") })
            Same(wire, Js.Parse(await SentBody("consolidate", "response-plan", m => m.ConsolidateAsync(ConsolidateSample with { Inputs = [new MemoryInput("m", 1, disposition)] }))).GetProperty("inputs")[0].GetProperty("disposition").GetString());
        foreach (var (mode, wire) in new[] { (MemoryForgetMode.LogicalDelete, "logical_delete"), (MemoryForgetMode.Expire, "expire") })
            Same(wire, Js.Parse(await SentBody("forget", "response-forget", m => m.ForgetAsync(ForgetSample with { Mode = mode }))).GetProperty("mode").GetString());
        foreach (var (operation, wire) in new[]
        {
            (MemoryOperation.Remember, "remember"), (MemoryOperation.CorrectFacts, "correct_facts"), (MemoryOperation.Consolidate, "consolidate"),
            (MemoryOperation.Forget, "forget"), (MemoryOperation.ResumeErasure, "resume_erasure")
        })
            Same(wire, Js.Parse(await SentBody("receipt", "response-receipt", m => m.GetReceiptAsync(ReceiptSample with { Operation = operation }))).GetProperty("operation").GetString());
    }

    static async Task VectorsAndFloats()
    {
        float[] vector = [1f, 0.1f, -2.5f, 1e-7f, float.MaxValue, float.Epsilon, -0f, 16777217f];
        var body = Js.Parse(await SentBody("recall", "response-recall", m => m.RecallAsync(RecallSample with { Vector = vector })));
        var sent = body.GetProperty("vector").EnumerateArray().Select(e => e.GetDouble()).ToArray();
        Same(vector.Length, sent.Length);
        for (var i = 0; i < vector.Length; i++)
        {
            Same(JsonValueKind.Number, body.GetProperty("vector")[i].ValueKind);
            // The server rounds each finite JSON number once to binary32: it must land on the caller's float.
            Same(vector[i], (float)sent[i], "vector[" + i + "]");
        }
        var evidence = Js.Parse(await SentBody("remember", "response-remember", m => m.RememberAsync(RememberSample with { Evidence = [new MemoryEvidence("ev", 1, Vector: [0.1f, 3f])] })));
        Same(0.1f, (float)evidence.GetProperty("evidence")[0].GetProperty("vector")[0].GetDouble());

        foreach (var broken in new Func<MemoryClient, Task>[]
        {
            m => m.RecallAsync(RecallSample with { Vector = [1f, float.NaN] }),
            m => m.RecallAsync(RecallSample with { Vector = [float.PositiveInfinity] }),
            m => m.RecallAsync(RecallSample with { Vector = [float.NegativeInfinity] }),
            m => m.RecallAsync(RecallSample with { Ranking = new MemoryRanking(Weights: [1, double.NaN, 0]) }),
            m => m.RecallAsync(RecallSample with { Ranking = new MemoryRanking(HalfLifeMs: double.PositiveInfinity) }),
            m => m.RecallAsync(RecallSample with { Ranking = new MemoryRanking(Lambda: double.NaN) }),
            m => m.RememberAsync(RememberSample with { Items = [RememberSample.Items[0] with { Importance = double.NaN }] }),
            m => m.RememberAsync(RememberSample with { Items = [RememberSample.Items[0] with { Confidence = double.PositiveInfinity }] }),
            m => m.RememberAsync(RememberSample with { Evidence = [new MemoryEvidence("ev", 1, Vector: [float.NaN])] }),
            m => m.RecallAsync(RecallSample with { Filter = new MemoryFilter(All: [new MemoryCondition("f", MemoryConditionOp.Eq, new MemoryScalar.Float(double.NaN))]) }),
            m => m.RecallAsync(RecallSample with { Filter = new MemoryFilter(All: [new MemoryCondition("f", MemoryConditionOp.Eq, new MemoryScalar.Float(double.NegativeInfinity))]) }),
        })
        {
            var server = Reply("recall", "response-recall");
            using var client = server.NewClient();
            await Fails<ArgumentException>(() => broken(client.Memory("notes")));
            Same(0, server.Calls.Count, "a non-finite number must never be sent");
        }
    }

    static async Task KeysVerbatim()
    {
        foreach (var key in new[] { "雪 /+?%", new string('x', 256), " ", "a\u0000b", "with \"quotes\" and \\ backslash", "emoji \ud83d\ude00" })
        {
            var body = Js.Parse(await SentBody("remember", "response-remember", m => m.RememberAsync(RememberSample with { IdempotencyKey = key })));
            Same(key, body.GetProperty("idempotency_key").GetString());
            var receipt = Js.Parse(await SentBody("receipt", "response-receipt", m => m.GetReceiptAsync(ReceiptSample with { IdempotencyKey = key })));
            Same(key, receipt.GetProperty("idempotency_key").GetString());
        }
        foreach (var id in new[] { "e 1", "é", new string('é', 128) })
        {
            var body = Js.Parse(await SentBody("erasure", "response-erasure", m => m.GetErasureAsync(ErasureSample with { JobId = id })));
            Same(id, body.GetProperty("job_id").GetString());
        }
    }

    static async Task NoSplittingOrPaging()
    {
        var items = Enumerable.Range(0, 5000)
            .Select(i => new MemoryItem("e" + i, MemoryKind.Entity, "c", i, new MemorySource("s", "k"))).ToArray();
        var evidence = Enumerable.Range(0, 300).Select(i => new MemoryEvidence("ev", (ulong)i)).ToArray();
        var server = Reply("remember", "response-remember");
        using var client = server.NewClient();
        await client.Memory("notes").RememberAsync(new MemoryRememberRequest(A, "big", items, evidence));
        Same(1, server.Calls.Count, "never auto-split");
        var body = server.Last.Json();
        Same(5000, body.GetProperty("items").GetArrayLength());
        Same(300, body.GetProperty("evidence").GetArrayLength());
        Same("e4999", body.GetProperty("items")[4999].GetProperty("id").GetString());

        // A partial read is a normal 200: no follow-up call, no cursor, nothing hidden.
        var partial = Js.Set(Js.Set(Fx.Value("recall", "response-recall"), "meta/complete", "false"), "meta/truncation", "[\"candidates\",\"deadline\"]");
        var reads = new Server { Reply = partial };
        using var readClient = reads.NewClient();
        var recalled = await readClient.Memory("notes").RecallAsync(RecallSample with { AllowPartial = true });
        Same(1, reads.Calls.Count, "never auto-paginate");
        Check(!recalled.Meta.Complete);
        Same("candidates,deadline", string.Join(",", recalled.Meta.Truncation));
        Check(!reads.Last.Json().EnumerateObject().Any(p => p.Name is "cursor" or "after" or "next_cursor" or "page"));

        // Pending erasure is returned as is; the client neither polls nor resumes.
        var pending = new Server { Status = 202, Reply = Fx.Value("resume_erasure", "response-resume-pending") };
        using var pendingClient = pending.NewClient();
        var resumed = await pendingClient.Memory("notes").ResumeErasureAsync(ResumeSample);
        Same(1, pending.Calls.Count, "no job loop");
        Same(MemoryMutationState.Pending, resumed.State);
        Check(resumed.Erasure is not null);
    }
}
