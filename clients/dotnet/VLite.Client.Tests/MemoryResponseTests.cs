using System.Text.Json;
using VLite;

static partial class MemoryTests
{
    const string MaxMutationJson = """
        {"receipt":{"id":"r9","operation":"resume_erasure","scope":{"tenant":"t","agent":"a","user":"u","session":"s"},
                    "sequence":"18446744073709551615","retry_until_ms":"0","digest_version":"memory-request-v9"},
         "replayed":true,
         "outcomes":[{"id":"o1","action":"retained_shared","revision":"2","expires_at_ms":"18446744073709551615"},
                     {"id":"o2","action":"noop","revision":"0"}],
         "feed":{"mode":"ordinary_changes","temporal_complete":true,"ttl_complete":true,"consumer_purge_acknowledged":true},
         "remaining_work":null,"state":"retention_set",
         "erasure":{"id":"j","scope":{"tenant":"t","agent":"a"},"target":{"ids":[{"id":"x","revision":"1"}]},"revision":"3",
                    "generation":"4","state":"logical","logical_visibility":"hidden","deleted_count":"5","remaining_count":"6","resume_token":"tok"},
         "erasure_notice":{"live_state_only":true,"secure_erasure":false,"external_copies":"operator_managed",
                           "restore_requires_manifest":true,"backup_history_purge":"not_verified","manifest_id":"m"}}
        """;

    const string MaxMetaJson = """
        {"scopes":[{"tenant":"t","agent":"a"}],"as_of_ms":"-5","ttl_clock_ms":"6","sequence":"7","complete":false,
         "truncation":["candidates","deadline","future_reason"],
         "limitations":["historical_candidate_discovery_not_exhaustive","retained_state_only"],"mode":"approximate",
         "candidates_examined":"8","edges_charged":"9","vector_components":"10","mmr_components":"11","evidence_bytes":"12"}
        """;

    const string MaxRecallJson = """
        {"memories":[{"id":"m1","revision":"2","scope":{"tenant":"t","agent":"a","user":"u","session":"s"},"kind":"fact","content":"c",
                      "observed_at_ms":"-9223372036854775808","created_at_ms":"1","updated_at_ms":"2","importance":0.25,"confidence":0.75,
                      "expires_at_ms":"9","status":"disputed","stale":true,"segment":{"valid_from_ms":null,"valid_to_ms":"10"},
                      "evidence":[{"collection":"ev","record":"7","snippet":"s"},{"collection":"ev","record":"8"}],
                      "scores":{"R":0.5,"D":0.25,"I":0.25,"S":0.4,"dense_rank":1,"sparse_rank":2,"text_rank":3,"distance":0.125,
                                "bm25":1.5,"sparse_score":2.5,"fused_rank":4,"fused_score":0.0625},
                      "provenance":[{"source_id":"s","source_kind":"k","content_digest":"v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","uri":"u","transformation_id":"t"}],
                      "references":[{"id":"r","revision":"1"}],"paths":[{"nodes":["1","2"],"edges":["3"]}],
                      "metadata":{"a":{"type":"string","value":"x"},"b":{"type":"integer","value":"-1"},
                                  "c":{"type":"float","value":1.5},"d":{"type":"boolean","value":true}}}],
         "meta":{"scopes":[{"tenant":"t","agent":"a"}],"as_of_ms":"-5","ttl_clock_ms":"6","sequence":"7","complete":false,
                 "truncation":["candidates","deadline","future_reason"],
                 "limitations":["historical_candidate_discovery_not_exhaustive","retained_state_only"],"mode":"approximate",
                 "candidates_examined":"8","edges_charged":"9","vector_components":"10","mmr_components":"11","evidence_bytes":"12"},
         "ranking_version":"memory-ranking-v1","selection":"mmr"}
        """;

    const string MaxFactsJson = """
        {"facts":[{"series":{"id":"f1","revision":"2"},"slot":{"subject_id":"e1","predicate":"role","slot_key":"primary"},
                   "slot_revision":"7","destination_id":"e2","interval":{"valid_from_ms":"-1","valid_to_ms":null},"status":"accepted",
                   "stale":false,"evidence":[{"collection":"ev","record":"1","snippet":"s"}],
                   "provenance":[{"source_id":"s","source_kind":"k","content_digest":"v1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","uri":"u","transformation_id":"t"}]}],
         "meta":{"scopes":[{"tenant":"t","agent":"a"}],"as_of_ms":"-5","ttl_clock_ms":"6","sequence":"7","complete":true,
                 "truncation":[],"limitations":["retained_state_only"],"mode":"exact_enumeration",
                 "candidates_examined":"8","edges_charged":"9","vector_components":"10","mmr_components":"11","evidence_bytes":"12"}}
        """;

    const string MaxPlanJson = """
        {"dry_run":true,"user_operations":"4","encoded_bytes":"512",
         "conflicts":[{"field":"inputs","limit":"64","requested":"65",
                       "competing":[{"id":"f1","interval":{"valid_from_ms":"1","valid_to_ms":null},"revision":"7"}],
                       "receipt_lookup_required":true,"code":"precondition_failed"},{}],
         "remaining_work":"3"}
        """;

    static Probe Op(string operation) => Probes.Single(p => p.Operation == operation);

    static async Task<object> Parse(string operation, string reply, int status = 200)
    {
        var server = new Server { Status = status, Reply = reply };
        using var client = server.NewClient();
        return await Op(operation).Call(client.Memory("notes"), default);
    }

    static async Task<T> Parse<T>(string operation, string reply, int status = 200) where T : class =>
        (T)await Parse(operation, reply, status);

    static Task Invalid(string operation, string reply, string note, int status = 200, [System.Runtime.CompilerServices.CallerLineNumber] int line = 0) =>
        Fails<TransportException>(() => Parse(operation, reply, status), note, line);

    // ------------------------------------------------------------------ fixtures
    static async Task ResponseFixtures()
    {
        var bodies = new List<(string Label, string Operation, int Status, string Body)>();
        foreach (var file in Fx.Files)
        {
            foreach (var entry in Fx.Structural(file))
                if (entry.GetProperty("schema").GetString() == "exchange" && entry.GetProperty("valid").GetBoolean() &&
                    entry.GetProperty("status").GetInt32() < 300)
                    bodies.Add((file + ":" + entry.GetProperty("name").GetString(), entry.GetProperty("operation").GetString()!,
                        entry.GetProperty("status").GetInt32(), entry.GetProperty("value").GetRawText()));
            foreach (var entry in Fx.Cases(file))
                if (entry.GetProperty("expected").GetProperty("status").GetInt32() < 300)
                    bodies.Add((file + ":" + entry.GetProperty("name").GetString(), entry.GetProperty("operation").GetString()!,
                        entry.GetProperty("expected").GetProperty("status").GetInt32(), entry.GetProperty("expected").GetProperty("body").GetRawText()));
        }
        var schemaOperations = new Dictionary<string, string>
        {
            ["MutationResponse"] = "remember", ["ReceiptResponse"] = "receipt", ["ErasureResponse"] = "erasure",
            ["PlanResponse"] = "consolidate", ["RecallResponse"] = "recall", ["FactsResponse"] = "facts"
        };
        foreach (var entry in Fx.Structural("remember"))
            if (entry.GetProperty("valid").GetBoolean() && schemaOperations.TryGetValue(entry.GetProperty("schema").GetString()!, out var op))
                bodies.Add(("schema:" + entry.GetProperty("name").GetString(), op, 200, entry.GetProperty("value").GetRawText()));
        Same(68, bodies.Count, "10 exchanges + 52 successful cases + 6 response schemas");
        foreach (var (label, operation, status, body) in bodies)
        {
            var result = await Parse(operation, body, status);
            Json(body, Serialize(result), label);
        }
        // Operation-specific result types, including the consolidate union.
        Check((await Parse("consolidate", Fx.Value("consolidate", "response-plan"))) is MemoryPlanResponse);
        Check((await Parse("consolidate", Fx.Expected("consolidate", "consolidate-retain-one-source"))) is MemoryMutationResponse);
    }

    static async Task ResponseExplicitValues()
    {
        var remember = await Parse<MemoryMutationResponse>("remember", Fx.Value("remember", "response-remember"));
        Same("r1", remember.Receipt.Id);
        Same(MemoryOperation.Remember, remember.Receipt.Operation);
        Same(new MemoryEffectiveScope("@root", "a"), remember.Receipt.Scope);
        Same("7", remember.Receipt.Sequence);
        Same("200", remember.Receipt.RetryUntilMs);
        Same("memory-request-v1", remember.Receipt.DigestVersion);
        Same(false, remember.Replayed);
        Same(1, remember.Outcomes.Length);
        Same(new MemoryOutcome("e1", MemoryOutcomeAction.Created, "1"), remember.Outcomes[0]);
        Same(new MemoryFeed("ordinary_changes", false, false, false), remember.Feed);
        Same("0", remember.RemainingWork);
        Check(remember.State is null && remember.Erasure is null && remember.ErasureNotice is null);

        // The contract's own first exchange has no retry_until_ms (A1 design).
        var contract = Fx.ContractExchanges()[0].GetProperty("response").GetRawText();
        var first = await Parse<MemoryMutationResponse>("remember", contract);
        Check(first.Receipt.RetryUntilMs is null);
        Same("7", first.Receipt.Sequence);

        var forget = await Parse<MemoryMutationResponse>("forget", Fx.Value("forget", "response-forget"));
        Same(MemoryMutationState.Logical, forget.State);
        Same(MemoryOutcomeAction.Deleted, forget.Outcomes[0].Action);
        Same("7", forget.Outcomes[0].Revision);
        Same(new MemoryErasureNotice(true, false, "operator_managed", true, "not_verified", "manifest1"), forget.ErasureNotice);
        Check(forget.Erasure is null);

        var retention = await Parse<MemoryMutationResponse>("forget", Fx.Value("forget", "response-retention"));
        Same(MemoryMutationState.RetentionSet, retention.State);
        Same(MemoryOutcomeAction.Expired, retention.Outcomes[0].Action);
        Same("150", retention.Outcomes[0].ExpiresAtMs);
        Check(retention.ErasureNotice is not null && retention.ErasureNotice.ManifestId is null);

        var correction = await Parse<MemoryMutationResponse>("correct_facts", Fx.Value("correct_facts", "response-correction"));
        Same(MemoryOperation.CorrectFacts, correction.Receipt.Operation);
        Same(MemoryOutcomeAction.Corrected, correction.Outcomes[0].Action);
        Same("8", correction.Outcomes[0].Revision);

        var receipt = await Parse<MemoryReceiptResponse>("receipt", Fx.Value("receipt", "response-receipt"));
        Same(true, receipt.Result.Replayed);
        Same(MemoryOperation.Forget, receipt.Result.Receipt.Operation);
        Same(MemoryMutationState.Logical, receipt.Result.State);
        Same("manifest1", receipt.Result.ErasureNotice!.ManifestId);

        var erasure = await Parse<MemoryErasureResponse>("erasure", Fx.Value("erasure", "response-erasure"));
        Same("j1", erasure.Erasure.Id);
        Same(new MemoryEffectiveScope("@root", "a"), erasure.Erasure.Scope);
        Same(new MemoryTarget.Scope(), erasure.Erasure.Target);
        Same("8", erasure.Erasure.Revision);
        Same("1", erasure.Erasure.Generation);
        Same(MemoryErasureState.Pending, erasure.Erasure.State);
        Same("hidden", erasure.Erasure.LogicalVisibility);
        Same("0", erasure.Erasure.DeletedCount);
        Check(erasure.Erasure.RemainingCount is null);
        Same("opaque", erasure.Erasure.ResumeToken);
        Same("manifest1", erasure.ErasureNotice.ManifestId);
        Same(new MemoryFeed("ordinary_changes", false, false, false), erasure.Feed);

        var resumed = await Parse<MemoryMutationResponse>("resume_erasure", Fx.Value("resume_erasure", "response-resume-pending"), 202);
        Same(MemoryMutationState.Pending, resumed.State);
        Same(MemoryOperation.ResumeErasure, resumed.Receipt.Operation);
        Same("9", resumed.Receipt.Sequence);
        Same(0, resumed.Outcomes.Length);
        Check(resumed.RemainingWork is null, "remaining_work is nullable and null here");
        Same("j1", resumed.Erasure!.Id);
        Same(new MemoryTarget.Scope(), resumed.Erasure.Target);

        var plan = await Parse<MemoryConsolidateResponse>("consolidate", Fx.Value("consolidate", "response-plan"));
        var planned = plan as MemoryPlanResponse ?? throw new Exception("not a plan: " + plan);
        Same(true, planned.DryRun);
        Same("4", planned.UserOperations);
        Same("512", planned.EncodedBytes);
        Same(0, planned.Conflicts.Length);
        Same("0", planned.RemainingWork);
        var committed = await Parse<MemoryConsolidateResponse>("consolidate", Fx.Expected("consolidate", "consolidate-retain-one-source"));
        Check(committed is MemoryMutationResponse { Replayed: false } mutated && mutated.Receipt.Operation == MemoryOperation.Consolidate);

        var recall = await Parse<MemoryRecallResponse>("recall", Fx.Value("recall", "response-recall"));
        Same(2, recall.Memories.Length);
        var a = recall.Memories[0];
        Same("a", a.Id);
        Same("1", a.Revision);
        Same(new MemoryEffectiveScope("@root", "a"), a.Scope);
        Same(MemoryKind.Entity, a.Kind);
        Same("a", a.Content);
        Same("90", a.ObservedAtMs);
        Same("100", a.CreatedAtMs);
        Same("100", a.UpdatedAtMs);
        Same(0.5, a.Importance);
        Same(MemoryStatus.Accepted, a.Status);
        Same(false, a.Stale);
        Check(a.Segment is null && a.Confidence is null && a.ExpiresAtMs is null && a.Paths is null);
        Same(0, a.Evidence.Length);
        Same(0, a.Provenance!.Length);
        Same(0, a.References!.Length);
        Same(0, a.Metadata!.Count);
        Same(1.0, a.Scores.R);
        Check(a.Scores.D is null);
        Same(0.5, a.Scores.I);
        Same(1.0, a.Scores.S);
        Check(a.Scores.DenseRank is null && a.Scores.Bm25 is null && a.Scores.FusedScore is null);
        Same(0.5, recall.Memories[1].Scores.R);
        Same("6", recall.Meta.Sequence);
        Same("100", recall.Meta.AsOfMs);
        Same("100", recall.Meta.TtlClockMs);
        Same(true, recall.Meta.Complete);
        Same(0, recall.Meta.Truncation.Length);
        Same("retained_state_only", string.Join(",", recall.Meta.Limitations));
        Same("exact_enumeration", recall.Meta.Mode);
        Same("2", recall.Meta.CandidatesExamined);
        Same("0", recall.Meta.EdgesCharged);
        Same("0", recall.Meta.VectorComponents);
        Same("0", recall.Meta.MmrComponents);
        Same("0", recall.Meta.EvidenceBytes);
        Same(new MemoryEffectiveScope("@root", "a"), recall.Meta.Scopes.Single());
        Same("memory-ranking-v1", recall.RankingVersion);
        Same(MemorySelection.Weighted, recall.Selection);

        var facts = await Parse<MemoryFactsResponse>("facts", Fx.Value("facts", "response-facts"));
        var fact = facts.Facts.Single();
        Same(new MemoryRef("e1", "1"), fact.Series);
        Same(new MemorySlot("e1", "role", "primary"), fact.Slot);
        Same("7", fact.SlotRevision);
        Same("e2", fact.DestinationId);
        Same(new MemoryInterval("-9223372036854775808", "9223372036854775807"), fact.Interval);
        Same(MemoryStatus.Disputed, fact.Status);
        Same(true, fact.Stale);
        Same(0, fact.Evidence.Length);
        Check(fact.Provenance is null);
        Same("6", facts.Meta.Sequence);
    }

    static async Task ResponseMaximal()
    {
        var mutation = await Parse<MemoryMutationResponse>("remember", MaxMutationJson);
        Same("r9", mutation.Receipt.Id);
        Same(MemoryOperation.ResumeErasure, mutation.Receipt.Operation);
        Same(new MemoryEffectiveScope("t", "a", "u", "s"), mutation.Receipt.Scope);
        Same("18446744073709551615", mutation.Receipt.Sequence);
        Same("0", mutation.Receipt.RetryUntilMs);
        Same("memory-request-v9", mutation.Receipt.DigestVersion);
        Same(true, mutation.Replayed);
        Same(new MemoryOutcome("o1", MemoryOutcomeAction.RetainedShared, "2", "18446744073709551615"), mutation.Outcomes[0]);
        Same(new MemoryOutcome("o2", MemoryOutcomeAction.Noop, "0"), mutation.Outcomes[1]);
        Same(new MemoryFeed("ordinary_changes", true, true, true), mutation.Feed);
        Check(mutation.RemainingWork is null);
        Same(MemoryMutationState.RetentionSet, mutation.State);
        var erasure = mutation.Erasure!;
        Same("j", erasure.Id);
        Same(new MemoryEffectiveScope("t", "a"), erasure.Scope);
        var ids = (MemoryTarget.Ids)erasure.Target;
        Same(new MemoryRef("x", "1"), ids.Refs.Single());
        Same("3", erasure.Revision);
        Same("4", erasure.Generation);
        Same(MemoryErasureState.Logical, erasure.State);
        Same("5", erasure.DeletedCount);
        Same("6", erasure.RemainingCount);
        Same("tok", erasure.ResumeToken);
        Same("m", mutation.ErasureNotice!.ManifestId);
        Json(MaxMutationJson, Serialize(mutation));

        var recall = await Parse<MemoryRecallResponse>("recall", MaxRecallJson);
        var view = recall.Memories.Single();
        Same("m1", view.Id);
        Same("2", view.Revision);
        Same(new MemoryEffectiveScope("t", "a", "u", "s"), view.Scope);
        Same(MemoryKind.Fact, view.Kind);
        Same("-9223372036854775808", view.ObservedAtMs);
        Same("1", view.CreatedAtMs);
        Same("2", view.UpdatedAtMs);
        Same(0.25, view.Importance);
        Same(0.75, view.Confidence);
        Same("9", view.ExpiresAtMs);
        Same(MemoryStatus.Disputed, view.Status);
        Same(true, view.Stale);
        Same(new MemoryInterval(null, "10"), view.Segment);
        Same(new MemoryEvidenceView("ev", "7", "s"), view.Evidence[0]);
        Same(new MemoryEvidenceView("ev", "8"), view.Evidence[1]);
        var scores = view.Scores;
        Same(0.5, scores.R);
        Same(0.25, scores.D);
        Same(0.25, scores.I);
        Same(0.4, scores.S);
        Same(1, scores.DenseRank);
        Same(2, scores.SparseRank);
        Same(3, scores.TextRank);
        Same(0.125, scores.Distance);
        Same(1.5, scores.Bm25);
        Same(2.5, scores.SparseScore);
        Same(4, scores.FusedRank);
        Same(0.0625, scores.FusedScore);
        var provenance = view.Provenance!.Single();
        Same("s", provenance.SourceId);
        Same("k", provenance.SourceKind);
        Same("v1:" + new string('a', 64), provenance.ContentDigest);
        Same("u", provenance.Uri);
        Same("t", provenance.TransformationId);
        Same(new MemoryRef("r", "1"), view.References!.Single());
        Same("1,2", string.Join(",", view.Paths!.Single().Nodes));
        Same("3", string.Join(",", view.Paths!.Single().Edges));
        Same(new MemoryScalar.Text("x"), view.Metadata!["a"]);
        Same(new MemoryScalar.Integer("-1"), view.Metadata["b"]);
        Same(new MemoryScalar.Float(1.5), view.Metadata["c"]);
        Same(new MemoryScalar.Boolean(true), view.Metadata["d"]);
        Same(4, view.Metadata.Count);
        Same("candidates,deadline,future_reason", string.Join(",", recall.Meta.Truncation));
        Same("historical_candidate_discovery_not_exhaustive,retained_state_only", string.Join(",", recall.Meta.Limitations));
        Same("approximate", recall.Meta.Mode);
        Same(false, recall.Meta.Complete);
        Same("-5", recall.Meta.AsOfMs);
        Same("6", recall.Meta.TtlClockMs);
        Same("7", recall.Meta.Sequence);
        Same("8", recall.Meta.CandidatesExamined);
        Same("9", recall.Meta.EdgesCharged);
        Same("10", recall.Meta.VectorComponents);
        Same("11", recall.Meta.MmrComponents);
        Same("12", recall.Meta.EvidenceBytes);
        Same(MemorySelection.Mmr, recall.Selection);
        Json(MaxRecallJson, Serialize(recall));

        var facts = await Parse<MemoryFactsResponse>("facts", MaxFactsJson);
        var fact = facts.Facts.Single();
        Same(new MemoryInterval("-1", null), fact.Interval);
        Same(MemoryStatus.Accepted, fact.Status);
        Same("s", fact.Evidence.Single().Snippet);
        Same("t", fact.Provenance!.Single().TransformationId);
        Json(MaxFactsJson, Serialize(facts));

        var plan = (MemoryPlanResponse)await Parse<MemoryConsolidateResponse>("consolidate", MaxPlanJson);
        Same("3", plan.RemainingWork);
        Same(2, plan.Conflicts.Length);
        var conflict = plan.Conflicts[0];
        Same("inputs", conflict.Field);
        Same("64", conflict.Limit);
        Same("65", conflict.Requested);
        Same(true, conflict.ReceiptLookupRequired);
        Same("precondition_failed", conflict.Code);
        Same(new MemoryCompeting("f1", new MemoryInterval("1", null), "7"), conflict.Competing!.Single());
        Same(new MemoryErrorDetail(), plan.Conflicts[1]);
        Json(MaxPlanJson, Serialize(plan));
    }

    static async Task ResponseIntegers()
    {
        // (operation, reply, path, unsigned?, nullable-ok?)
        (string Op, Func<string> Reply, int Status, string Path, bool Signed, bool Nullable)[] positions =
        [
            ("remember", () => Fx.Value("remember", "response-remember"), 200, "receipt/sequence", false, false),
            ("remember", () => Fx.Value("remember", "response-remember"), 200, "receipt/retry_until_ms", false, false),
            ("remember", () => Fx.Value("remember", "response-remember"), 200, "outcomes/0/revision", false, false),
            ("remember", () => Fx.Value("remember", "response-remember"), 200, "remaining_work", false, true),
            ("forget", () => Fx.Value("forget", "response-retention"), 200, "outcomes/0/expires_at_ms", false, false),
            ("erasure", () => Fx.Value("erasure", "response-erasure"), 200, "erasure/revision", false, false),
            ("erasure", () => Fx.Value("erasure", "response-erasure"), 200, "erasure/generation", false, false),
            ("erasure", () => Fx.Value("erasure", "response-erasure"), 200, "erasure/deleted_count", false, false),
            ("erasure", () => Fx.Value("erasure", "response-erasure"), 200, "erasure/remaining_count", false, true),
            ("consolidate", () => Fx.Value("consolidate", "response-plan"), 200, "user_operations", false, false),
            ("consolidate", () => Fx.Value("consolidate", "response-plan"), 200, "encoded_bytes", false, false),
            ("consolidate", () => Fx.Value("consolidate", "response-plan"), 200, "remaining_work", false, true),
            ("recall", () => MaxRecallJson, 200, "memories/0/revision", false, false),
            ("recall", () => MaxRecallJson, 200, "memories/0/observed_at_ms", true, false),
            ("recall", () => MaxRecallJson, 200, "memories/0/created_at_ms", false, false),
            ("recall", () => MaxRecallJson, 200, "memories/0/updated_at_ms", false, false),
            ("recall", () => MaxRecallJson, 200, "memories/0/expires_at_ms", false, false),
            ("recall", () => MaxRecallJson, 200, "memories/0/evidence/0/record", false, false),
            ("recall", () => MaxRecallJson, 200, "memories/0/segment/valid_to_ms", true, true),
            ("recall", () => MaxRecallJson, 200, "memories/0/references/0/revision", false, false),
            ("recall", () => MaxRecallJson, 200, "memories/0/paths/0/nodes/1", false, false),
            ("recall", () => MaxRecallJson, 200, "memories/0/paths/0/edges/0", false, false),
            ("recall", () => MaxRecallJson, 200, "meta/as_of_ms", true, false),
            ("recall", () => MaxRecallJson, 200, "meta/ttl_clock_ms", false, false),
            ("recall", () => MaxRecallJson, 200, "meta/sequence", false, false),
            ("recall", () => MaxRecallJson, 200, "meta/candidates_examined", false, false),
            ("recall", () => MaxRecallJson, 200, "meta/edges_charged", false, false),
            ("recall", () => MaxRecallJson, 200, "meta/vector_components", false, false),
            ("recall", () => MaxRecallJson, 200, "meta/mmr_components", false, false),
            ("recall", () => MaxRecallJson, 200, "meta/evidence_bytes", false, false),
            ("facts", () => MaxFactsJson, 200, "facts/0/slot_revision", false, false),
            ("facts", () => MaxFactsJson, 200, "facts/0/series/revision", false, false),
            ("facts", () => MaxFactsJson, 200, "facts/0/interval/valid_from_ms", true, true),
            ("facts", () => MaxFactsJson, 200, "facts/0/interval/valid_to_ms", true, true),
            ("facts", () => MaxFactsJson, 200, "facts/0/evidence/0/record", false, false),
        ];
        string[] badUnsigned = ["", "01", "+1", "-1", "-0", "1.0", "1e3", " 1", "1 ", "0x1", "١", "18446744073709551616", "00", "abc"];
        string[] badSigned = ["", "01", "-01", "+1", "-0", "1.0", " 1", "9223372036854775808", "-9223372036854775809", "abc"];
        string[] goodUnsigned = ["0", "7", "18446744073709551615"];
        string[] goodSigned = ["0", "-1", "7", "-9223372036854775808", "9223372036854775807"];
        foreach (var (operation, reply, status, path, signed, nullable) in positions)
        {
            var good = reply();
            foreach (var value in signed ? goodSigned : goodUnsigned)
                await Parse(operation, Js.Set(good, path, "\"" + value + "\""), status);
            foreach (var value in signed ? badSigned : badUnsigned)
                await Invalid(operation, Js.Set(good, path, JsonSerializer.Serialize(value)), path + " <" + value + ">", status);
            // JSON integers are not canonical strings on output.
            await Invalid(operation, Js.Set(good, path, "7"), path + " as a JSON number", status);
            await Invalid(operation, Js.Set(good, path, "7.0"), path + " as a JSON float", status);
            await Invalid(operation, Js.Set(good, path, "true"), path + " as a boolean", status);
            await Invalid(operation, Js.Set(good, path, "[]"), path + " as an array", status);
            await Invalid(operation, Js.Set(good, path, "{}"), path + " as an object", status);
            if (nullable) await Parse(operation, Js.Set(good, path, "null"), status);
        }
    }

    static async Task ResponseUnions()
    {
        var erasureJson = Fx.Value("erasure", "response-erasure");
        (string Json, MemoryTarget Expected)[] targets =
        [
            ("""{"ids":[{"id":"a","revision":"1"},{"id":"b","revision":"18446744073709551615"}]}""",
                new MemoryTarget.Ids([new MemoryRef("a", "1"), new MemoryRef("b", "18446744073709551615")])),
            ("""{"series":{"id":"f","revision":"3"}}""", new MemoryTarget.Series(new MemoryRef("f", "3"))),
            ("""{"entity":{"id":"e","revision":"4"}}""", new MemoryTarget.Entity(new MemoryRef("e", "4"))),
            ("""{"session":true}""", new MemoryTarget.Session()),
            ("""{"scope":true}""", new MemoryTarget.Scope()),
        ];
        foreach (var (json, expected) in targets)
        {
            var parsed = await Parse<MemoryErasureResponse>("erasure", Js.Set(erasureJson, "erasure/target", json));
            Same<MemoryTarget>(expected, parsed.Erasure.Target, json);
            Same(expected.GetType(), parsed.Erasure.Target.GetType(), json);
        }
        Check(((MemoryTarget.Ids)(await Parse<MemoryErasureResponse>("erasure", Js.Set(erasureJson, "erasure/target", targets[0].Json))).Erasure.Target).Refs.Length == 2);
        foreach (var bad in new[]
        {
            "{}", """{"scope":false}""", """{"scope":"true"}""", """{"session":1}""", """{"session":false}""", """{"scope":true,"session":true}""",
            """{"ids":[],"scope":true}""", """{"ids":{}}""", """{"ids":[{"id":"a"}]}""", """{"ids":[{"revision":"1"}]}""",
            """{"series":"f"}""", """{"series":{"id":"f","revision":1}}""", """{"entity":null}""", """{"ids":null}""", """{"unknown":true}""",
            "[]", "null", "\"scope\"", "true", "1", """{"series":{"id":"f","revision":"3"},"entity":{"id":"e","revision":"4"}}"""
        })
            await Invalid("erasure", Js.Set(erasureJson, "erasure/target", bad), "target " + bad);

        // MemoryConsolidateResponse is chosen by its discriminating member: dry_run (plan) or receipt (committed).
        var plan = Fx.Value("consolidate", "response-plan");
        var committed = Fx.Expected("consolidate", "consolidate-retain-one-source");
        Check(await Parse("consolidate", plan) is MemoryPlanResponse);
        Check(await Parse("consolidate", committed) is MemoryMutationResponse);
        await Invalid("consolidate", "{}", "neither plan nor mutation");
        await Invalid("consolidate", Js.Set(plan, "receipt", Js.Get(committed, "receipt")), "both dry_run and receipt");
        await Invalid("consolidate", Js.Set(plan, "dry_run", "false"), "dry_run must be true");
        await Invalid("consolidate", Js.Set(plan, "dry_run", "\"true\""), "dry_run as a string");
        await Invalid("consolidate", Js.Set(plan, "dry_run", "null"), "dry_run null");
        await Invalid("consolidate", Js.Remove(plan, "user_operations"), "plan without user_operations");
        await Invalid("consolidate", Js.Remove(committed, "outcomes"), "mutation without outcomes");
        // Unknown extra members do not change the variant.
        Check(await Parse("consolidate", Js.Set(plan, "future", "1")) is MemoryPlanResponse);
        Check(await Parse("consolidate", Js.Set(committed, "future", "1")) is MemoryMutationResponse);
        // Other operations never produce the plan variant.
        await Invalid("remember", plan, "a plan is not a mutation response");
        Check(await Parse("remember", committed) is MemoryMutationResponse);

        // Scalars in view metadata.
        foreach (var (scalar, expected) in new (string, MemoryScalar)[]
        {
            ("""{"type":"string","value":"x"}""", new MemoryScalar.Text("x")),
            ("""{"type":"string","value":""}""", new MemoryScalar.Text("")),
            ("""{"type":"integer","value":"-9223372036854775808"}""", new MemoryScalar.Integer("-9223372036854775808")),
            ("""{"type":"integer","value":"0"}""", new MemoryScalar.Integer("0")),
            ("""{"type":"float","value":1.5}""", new MemoryScalar.Float(1.5)),
            ("""{"type":"float","value":1}""", new MemoryScalar.Float(1)),
            ("""{"type":"float","value":-0.0625}""", new MemoryScalar.Float(-0.0625)),
            ("""{"type":"boolean","value":true}""", new MemoryScalar.Boolean(true)),
            ("""{"type":"boolean","value":false}""", new MemoryScalar.Boolean(false)),
        })
        {
            var parsed = await Parse<MemoryRecallResponse>("recall", Js.Set(MaxRecallJson, "memories/0/metadata", "{\"k\":" + scalar + "}"));
            Same<MemoryScalar>(expected, parsed.Memories[0].Metadata!["k"], scalar);
        }
        foreach (var bad in new[]
        {
            """{"type":"string"}""", """{"value":"x"}""", """{"type":"string","value":1}""", """{"type":"string","value":null}""",
            """{"type":"integer","value":1}""", """{"type":"integer","value":"01"}""", """{"type":"integer","value":"9223372036854775808"}""",
            """{"type":"integer","value":"-0"}""", """{"type":"float","value":"1.5"}""", """{"type":"float","value":null}""",
            """{"type":"boolean","value":"true"}""", """{"type":"boolean","value":1}""", """{"type":"boolean","value":null}""",
            """{"type":"decimal","value":"1"}""", """{"type":null,"value":"x"}""", """{"type":1,"value":"x"}""", "\"x\"", "1", "null", "[]"
        })
            await Invalid("recall", Js.Set(MaxRecallJson, "memories/0/metadata", "{\"k\":" + bad + "}"), "scalar " + bad);
        await Invalid("recall", Js.Set(MaxRecallJson, "memories/0/metadata", "[]"), "metadata as an array");
        await Invalid("recall", Js.Set(MaxRecallJson, "memories/0/metadata", "\"x\""), "metadata as a string");
    }

    sealed record Obj(string Path, string[] Required, string[] NullOk, string[] Optional);
    sealed record Doc(string Operation, string Label, Func<string> Json, int Status, Obj[] Objects);

    static readonly string[] MetaMembers =
        ["scopes", "as_of_ms", "ttl_clock_ms", "sequence", "complete", "truncation", "limitations", "mode", "candidates_examined",
         "edges_charged", "vector_components", "mmr_components", "evidence_bytes"];
    static readonly string[] FeedMembers = ["mode", "temporal_complete", "ttl_complete", "consumer_purge_acknowledged"];
    static readonly string[] NoticeMembers =
        ["live_state_only", "secure_erasure", "external_copies", "restore_requires_manifest", "backup_history_purge"];
    static readonly string[] ErasureMembers =
        ["id", "scope", "target", "revision", "generation", "state", "logical_visibility", "deleted_count", "remaining_count"];
    static readonly string[] ReceiptMembers = ["id", "operation", "scope", "sequence", "digest_version"];
    static readonly string[] None = [];

    // Required/optional members hand-copied from the section 8 component list of the serving contract.
    static readonly Doc[] Docs =
    [
        new("remember", "fixture remember", () => Fx.Value("remember", "response-remember"), 200,
        [
            new("", ["receipt", "replayed", "outcomes", "feed", "remaining_work"], ["remaining_work"], ["state", "erasure", "erasure_notice"]),
            new("receipt", ReceiptMembers, None, ["retry_until_ms"]),
            new("receipt/scope", ["tenant", "agent"], None, ["user", "session"]),
            new("outcomes/0", ["id", "action", "revision"], None, ["expires_at_ms"]),
            new("feed", FeedMembers, None, None),
        ]),
        new("forget", "fixture forget", () => Fx.Value("forget", "response-forget"), 200,
        [
            new("", ["receipt", "replayed", "outcomes", "feed", "remaining_work"], ["remaining_work"], ["state", "erasure", "erasure_notice"]),
            new("erasure_notice", NoticeMembers, None, ["manifest_id"]),
        ]),
        new("forget", "fixture retention", () => Fx.Value("forget", "response-retention"), 200,
        [
            new("outcomes/0", ["id", "action", "revision"], None, ["expires_at_ms"]),
        ]),
        new("resume_erasure", "fixture resume", () => Fx.Value("resume_erasure", "response-resume-pending"), 202,
        [
            new("", ["receipt", "replayed", "outcomes", "feed", "remaining_work"], ["remaining_work"], ["state", "erasure", "erasure_notice"]),
            new("erasure", ErasureMembers, ["remaining_count"], ["resume_token"]),
            new("erasure/scope", ["tenant", "agent"], None, ["user", "session"]),
            new("erasure_notice", NoticeMembers, None, ["manifest_id"]),
        ]),
        new("erasure", "fixture erasure", () => Fx.Value("erasure", "response-erasure"), 200,
        [
            new("", ["erasure", "erasure_notice", "feed"], None, None),
            new("erasure", ErasureMembers, ["remaining_count"], ["resume_token"]),
            new("feed", FeedMembers, None, None),
        ]),
        new("receipt", "fixture receipt", () => Fx.Value("receipt", "response-receipt"), 200,
        [
            new("", ["result"], None, None),
            new("result", ["receipt", "replayed", "outcomes", "feed", "remaining_work"], ["remaining_work"], ["state", "erasure", "erasure_notice"]),
            new("result/receipt", ReceiptMembers, None, ["retry_until_ms"]),
            new("result/erasure_notice", NoticeMembers, None, ["manifest_id"]),
        ]),
        new("consolidate", "fixture plan", () => Fx.Value("consolidate", "response-plan"), 200,
        [
            new("", ["dry_run", "user_operations", "encoded_bytes", "conflicts", "remaining_work"], ["remaining_work"], None),
        ]),
        new("recall", "fixture recall", () => Fx.Value("recall", "response-recall"), 200,
        [
            new("", ["memories", "meta", "ranking_version", "selection"], None, None),
            new("memories/0", ["id", "revision", "scope", "kind", "content", "observed_at_ms", "created_at_ms", "updated_at_ms", "importance",
                               "status", "stale", "segment", "evidence", "scores"], ["segment"],
                ["confidence", "expires_at_ms", "provenance", "references", "paths", "metadata"]),
            new("memories/0/scope", ["tenant", "agent"], None, ["user", "session"]),
            new("memories/0/scores", ["R", "D", "I", "S"], ["D"], None),
            new("meta", MetaMembers, None, None),
            new("meta/scopes/0", ["tenant", "agent"], None, ["user", "session"]),
        ]),
        new("facts", "fixture facts", () => Fx.Value("facts", "response-facts"), 200,
        [
            new("", ["facts", "meta"], None, None),
            new("facts/0", ["series", "slot", "slot_revision", "destination_id", "interval", "status", "stale", "evidence"], None, ["provenance"]),
            new("facts/0/series", ["id", "revision"], None, None),
            new("facts/0/slot", ["subject_id", "predicate", "slot_key"], None, None),
            new("facts/0/interval", ["valid_from_ms", "valid_to_ms"], ["valid_from_ms", "valid_to_ms"], None),
        ]),
        new("remember", "maximal mutation", () => MaxMutationJson, 200,
        [
            new("", ["receipt", "replayed", "outcomes", "feed", "remaining_work"], ["remaining_work"], ["state", "erasure", "erasure_notice"]),
            new("receipt", ReceiptMembers, None, ["retry_until_ms"]),
            new("receipt/scope", ["tenant", "agent"], None, ["user", "session"]),
            new("outcomes/0", ["id", "action", "revision"], None, ["expires_at_ms"]),
            new("feed", FeedMembers, None, None),
            new("erasure", ErasureMembers, ["remaining_count"], ["resume_token"]),
            new("erasure/target/ids/0", ["id", "revision"], None, None),
            new("erasure_notice", NoticeMembers, None, ["manifest_id"]),
        ]),
        new("recall", "maximal recall", () => MaxRecallJson, 200,
        [
            new("memories/0", ["id", "revision", "scope", "kind", "content", "observed_at_ms", "created_at_ms", "updated_at_ms", "importance",
                               "status", "stale", "segment", "evidence", "scores"], ["segment"],
                ["confidence", "expires_at_ms", "provenance", "references", "paths", "metadata"]),
            new("memories/0/segment", ["valid_from_ms", "valid_to_ms"], ["valid_from_ms", "valid_to_ms"], None),
            new("memories/0/evidence/0", ["collection", "record"], None, ["snippet"]),
            new("memories/0/scores", ["R", "D", "I", "S"], ["D"],
                ["dense_rank", "sparse_rank", "text_rank", "distance", "bm25", "sparse_score", "fused_rank", "fused_score"]),
            new("memories/0/provenance/0", ["source_id", "source_kind", "content_digest"], None, ["uri", "transformation_id"]),
            new("memories/0/references/0", ["id", "revision"], None, None),
            new("memories/0/paths/0", ["nodes", "edges"], None, None),
            new("memories/0/metadata/a", ["type", "value"], None, None),
        ]),
        new("facts", "maximal facts", () => MaxFactsJson, 200,
        [
            new("facts/0", ["series", "slot", "slot_revision", "destination_id", "interval", "status", "stale", "evidence"], None, ["provenance"]),
            new("facts/0/evidence/0", ["collection", "record"], None, ["snippet"]),
            new("facts/0/provenance/0", ["source_id", "source_kind", "content_digest"], None, ["uri", "transformation_id"]),
            new("meta", MetaMembers, None, None),
        ]),
        new("consolidate", "maximal plan", () => MaxPlanJson, 200,
        [
            new("conflicts/0", None, None, ["field", "limit", "requested", "competing", "receipt_lookup_required", "code"]),
            new("conflicts/0/competing/0", ["id", "interval", "revision"], None, None),
            new("conflicts/0/competing/0/interval", ["valid_from_ms", "valid_to_ms"], ["valid_from_ms", "valid_to_ms"], None),
        ]),
    ];

    static string Member(string path, string name) => path == "" ? name : path + "/" + name;

    static async Task ResponseRequiredMembers()
    {
        var removals = 0;
        foreach (var doc in Docs)
        {
            var good = doc.Json();
            await Parse(doc.Operation, good, doc.Status);
            foreach (var obj in doc.Objects)
                foreach (var member in obj.Required)
                {
                    var path = Member(obj.Path, member);
                    await Invalid(doc.Operation, Js.Remove(good, path), doc.Label + ": missing required " + path, doc.Status);
                    removals++;
                    if (obj.NullOk.Contains(member))
                        await Parse(doc.Operation, Js.Set(good, path, "null"), doc.Status);
                    else
                        await Invalid(doc.Operation, Js.Set(good, path, "null"), doc.Label + ": null for required " + path, doc.Status);
                }
        }
        Check(removals > 150, "removed " + removals + " required members");
    }

    static async Task ResponseOptionalAndNull()
    {
        var removed = 0;
        foreach (var doc in Docs)
        {
            var good = doc.Json();
            foreach (var obj in doc.Objects)
                foreach (var member in obj.Optional)
                {
                    var path = Member(obj.Path, member);
                    string? present;
                    try { present = Js.Get(good, path); } catch (Exception) { present = null; }
                    if (present is null or "null") continue;
                    // Absent and explicit null both mean "not set" for optional members.
                    await Parse(doc.Operation, Js.Remove(good, path), doc.Status);
                    await Parse(doc.Operation, Js.Set(good, path, "null"), doc.Status);
                    removed++;
                }
        }
        Check(removed > 40, "exercised " + removed + " optional members");

        // Absence is visible as null in the typed result.
        var json = MaxRecallJson;
        foreach (var member in new[] { "confidence", "expires_at_ms", "provenance", "references", "paths", "metadata" })
            json = Js.Remove(json, "memories/0/" + member);
        var bare = (await Parse<MemoryRecallResponse>("recall", json)).Memories[0];
        Check(bare.Confidence is null && bare.ExpiresAtMs is null && bare.Provenance is null && bare.References is null &&
              bare.Paths is null && bare.Metadata is null);
        var scores = Js.Set(Js.Remove(Js.Remove(MaxRecallJson, "memories/0/scores/bm25"), "memories/0/scores/dense_rank"), "memories/0/scores/D", "null");
        var parsedScores = (await Parse<MemoryRecallResponse>("recall", scores)).Memories[0].Scores;
        Check(parsedScores.Bm25 is null && parsedScores.DenseRank is null && parsedScores.D is null);
        Same(0.125, parsedScores.Distance);
        var minimal = await Parse<MemoryMutationResponse>("remember", Js.Remove(Js.Remove(MaxMutationJson, "erasure"), "state"));
        Check(minimal.State is null && minimal.Erasure is null && minimal.ErasureNotice is not null);
        Check((await Parse<MemoryMutationResponse>("remember", Js.Set(MaxMutationJson, "remaining_work", "\"12\""))).RemainingWork == "12");
        Check((await Parse<MemoryMutationResponse>("remember", Js.Set(MaxMutationJson, "remaining_work", "null"))).RemainingWork is null);
    }

    static async Task ResponseUnknownMembers()
    {
        // A future server may add members anywhere; parsing ignores them and loses nothing else.
        foreach (var probe in Probes)
        {
            var good = Fx.Value(probe.Operation, probe.ResponseName);
            var result = await Parse(probe.Operation, Js.AddUnknownMembers(good), probe.Status);
            Json(good, Serialize(result), probe.Operation);
        }
        foreach (var (operation, json) in new[]
        {
            ("remember", MaxMutationJson), ("recall", MaxRecallJson), ("facts", MaxFactsJson), ("consolidate", MaxPlanJson)
        })
        {
            var result = await Parse(operation, Js.AddUnknownMembers(json));
            Json(json, Serialize(result), operation);
        }
        // Unknown members inside union objects keep the discriminating member in charge.
        var erasure = Js.Set(Fx.Value("erasure", "response-erasure"), "erasure/target", """{"session":true,"x_future":1}""");
        Same(new MemoryTarget.Session(), (await Parse<MemoryErasureResponse>("erasure", erasure)).Erasure.Target);
        var scalar = Js.Set(MaxRecallJson, "memories/0/metadata", """{"k":{"type":"integer","value":"5","x_future":[1]}}""");
        Same<MemoryScalar>(new MemoryScalar.Integer("5"), (await Parse<MemoryRecallResponse>("recall", scalar)).Memories[0].Metadata!["k"]);
    }

    static async Task ResponseInvalidShapes()
    {
        var good = Fx.Value("remember", "response-remember");
        foreach (var (reply, note) in new (string, string)[]
        {
            ("", "empty body"), ("   ", "blank body"), ("not json", "text"), ("{", "truncated"), (good[..^2], "truncated mutation"),
            ("null", "null"), ("[]", "array"), ("\"x\"", "string"), ("1", "number"), ("true", "boolean"), ("{}", "empty object"),
            (good + good, "two documents"), ("{\"error\":{\"code\":\"x\",\"message\":\"y\"}}", "error envelope on a 200"),
        })
            await Invalid("remember", reply, note);
        foreach (var (path, value) in new (string, string)[]
        {
            ("replayed", "\"true\""), ("replayed", "1"), ("replayed", "null"), ("outcomes", "{}"), ("outcomes", "null"), ("outcomes", "\"x\""),
            ("outcomes/0", "\"x\""), ("outcomes/0/id", "1"), ("outcomes/0/id", "null"), ("outcomes/0/action", "\"nope\""),
            ("outcomes/0/action", "\"Created\""), ("outcomes/0/action", "0"), ("outcomes/0/action", "null"),
            ("receipt", "[]"), ("receipt", "null"), ("receipt/id", "7"), ("receipt/operation", "\"nope\""), ("receipt/operation", "\"Remember\""),
            ("receipt/operation", "1"), ("receipt/scope", "\"@root\""), ("receipt/scope/agent", "null"), ("receipt/scope/tenant", "1"),
            ("receipt/digest_version", "1"), ("feed", "[]"), ("feed/mode", "1"), ("feed/temporal_complete", "\"false\""),
            ("feed/ttl_complete", "0"), ("feed/consumer_purge_acknowledged", "null"), ("remaining_work", "0"), ("remaining_work", "true"),
        })
            await Invalid("remember", Js.Set(good, path, value), path + " = " + value);
        // Optional members of the wrong type are invalid too, not silently dropped.
        var forget = Fx.Value("forget", "response-forget");
        foreach (var (path, value) in new (string, string)[]
        {
            ("state", "\"nope\""), ("state", "1"), ("state", "\"Logical\""), ("erasure_notice", "[]"), ("erasure_notice/manifest_id", "1"),
            ("erasure_notice/live_state_only", "\"true\""), ("erasure_notice/external_copies", "null"), ("erasure", "\"x\""),
        })
            await Invalid("forget", Js.Set(forget, path, value), path + " = " + value);
        var recall = MaxRecallJson;
        foreach (var (path, value) in new (string, string)[]
        {
            ("memories/0/kind", "\"nope\""), ("memories/0/status", "\"nope\""), ("memories/0/stale", "\"false\""), ("memories/0/importance", "\"0.5\""),
            ("memories/0/importance", "null"), ("memories/0/confidence", "\"1\""), ("memories/0/scores/R", "\"1\""), ("memories/0/scores/D", "\"1\""),
            ("memories/0/scores/dense_rank", "1.5"), ("memories/0/scores/dense_rank", "\"1\""), ("memories/0/scores/bm25", "\"1\""),
            ("memories/0/evidence/0/snippet", "1"), ("memories/0/provenance/0/uri", "1"), ("memories/0/paths/0/nodes", "{}"),
            ("memories/0/paths/0/nodes/0", "1"), ("memories", "{}"), ("memories/0/scope", "[]"), ("meta/complete", "\"true\""),
            ("meta/truncation", "\"deadline\""), ("meta/truncation/0", "1"), ("meta/limitations/0", "null"), ("meta/mode", "1"),
            ("meta/scopes", "{}"), ("ranking_version", "1"), ("selection", "\"nope\""), ("selection", "1"), ("meta", "[]"),
        })
            await Invalid("recall", Js.Set(recall, path, value), path + " = " + value);
        var facts = MaxFactsJson;
        foreach (var (path, value) in new (string, string)[]
        {
            ("facts/0/status", "\"nope\""), ("facts/0/stale", "null"), ("facts/0/slot", "[]"), ("facts/0/series", "\"x\""), ("facts/0/interval", "[]"),
            ("facts/0/destination_id", "1"), ("facts", "null"),
        })
            await Invalid("facts", Js.Set(facts, path, value), path + " = " + value);
        // A 204 carries no document at all.
        var empty = new Server { Status = 204, Reply = "" };
        using var client = empty.NewClient();
        await Fails<TransportException>(() => client.Memory("notes").RememberAsync(RememberSample));
        // Whatever the transport raised, the cause is preserved for diagnosis.
        var failure = await Fails<TransportException>(() => Parse("remember", "not json"));
        Check(failure.InnerException is not null);
        Same("transport_error", failure.Code);
    }

    static async Task Http202()
    {
        var pendingForget = Js.Set(Js.Set(Fx.Value("forget", "response-forget"), "state", "\"pending\""), "outcomes", "[]");
        foreach (var status in new[] { 200, 202 })
        {
            var forgot = await Parse<MemoryMutationResponse>("forget", pendingForget, status);
            Same(MemoryMutationState.Pending, forgot.State);
            Same(0, forgot.Outcomes.Length);
            var resumed = await Parse<MemoryMutationResponse>("resume_erasure", Fx.Value("resume_erasure", "response-resume-pending"), status);
            Same(MemoryMutationState.Pending, resumed.State);
            Same("opaque", resumed.Erasure!.ResumeToken);
        }
        // 202 is a normal response with the full typed body, on the facade and nowhere else treated as failure.
        var server = new Server { Status = 202, Reply = pendingForget, RequestId = "req-202" };
        using var client = server.NewClient();
        var accepted = await client.Memory("notes").ForgetAsync(ForgetSample);
        Same(MemoryMutationState.Pending, accepted.State);
        Same(1, server.Calls.Count);
    }
}
