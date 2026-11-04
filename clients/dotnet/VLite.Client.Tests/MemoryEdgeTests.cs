using System.Globalization;
using System.Text.Json;
using VLite;

// Edge scenarios: members that carry the same type and often the same value (feed and notice flags, request flags),
// culture independence of the numeric wire format, the union converters used directly through System.Text.Json, and
// numbers a response reader or request writer must refuse.
static partial class MemoryTests
{
    static async Task ResponseBooleanPositions()
    {
        // Each of the three feed flags is true alone, so a mix-up between them cannot go unnoticed.
        var remember = Fx.Value("remember", "response-remember");
        (string Mode, bool Temporal, bool Ttl, bool Purge)[] feeds =
            [("m1", true, false, false), ("m2", false, true, false), ("m3", false, false, true), ("m4", true, true, true), ("m5", false, false, false)];
        foreach (var (mode, temporal, ttl, purge) in feeds)
        {
            var feed = "{\"mode\":\"" + mode + "\",\"temporal_complete\":" + (temporal ? "true" : "false") +
                ",\"ttl_complete\":" + (ttl ? "true" : "false") + ",\"consumer_purge_acknowledged\":" + (purge ? "true" : "false") + "}";
            var parsed = (await Parse<MemoryMutationResponse>("remember", Js.Set(remember, "feed", feed))).Feed;
            Same(mode, parsed.Mode, feed);
            Same((temporal, ttl, purge), (parsed.TemporalComplete, parsed.TtlComplete, parsed.ConsumerPurgeAcknowledged), feed);
            var erasure = Js.Set(Fx.Value("erasure", "response-erasure"), "feed", feed);
            var viaErasure = (await Parse<MemoryErasureResponse>("erasure", erasure)).Feed;
            Same((temporal, ttl, purge), (viaErasure.TemporalComplete, viaErasure.TtlComplete, viaErasure.ConsumerPurgeAcknowledged), feed);
        }

        var forget = Fx.Value("forget", "response-forget");
        (bool Live, bool Secure, bool Restore, string Copies, string Purge, string? Manifest)[] notices =
        [
            (true, false, false, "c1", "p1", "m1"), (false, true, false, "c2", "p2", null), (false, false, true, "c3", "p3", "m3"),
            (true, true, true, "c4", "p4", null), (false, false, false, "c5", "p5", "m5")
        ];
        foreach (var (live, secure, restore, copies, purge, manifest) in notices)
        {
            var notice = "{\"live_state_only\":" + (live ? "true" : "false") + ",\"secure_erasure\":" + (secure ? "true" : "false") +
                ",\"external_copies\":\"" + copies + "\",\"restore_requires_manifest\":" + (restore ? "true" : "false") +
                ",\"backup_history_purge\":\"" + purge + "\"" + (manifest is null ? "" : ",\"manifest_id\":\"" + manifest + "\"") + "}";
            var parsed = (await Parse<MemoryMutationResponse>("forget", Js.Set(forget, "erasure_notice", notice))).ErasureNotice!;
            Same((live, secure, restore), (parsed.LiveStateOnly, parsed.SecureErasure, parsed.RestoreRequiresManifest), notice);
            Same(copies, parsed.ExternalCopies, notice);
            Same(purge, parsed.BackupHistoryPurge, notice);
            Same(manifest, parsed.ManifestId, notice);
            var viaErasure = (await Parse<MemoryErasureResponse>("erasure", Js.Set(Fx.Value("erasure", "response-erasure"), "erasure_notice", notice))).ErasureNotice;
            Same((live, secure, restore), (viaErasure.LiveStateOnly, viaErasure.SecureErasure, viaErasure.RestoreRequiresManifest), notice);
        }

        // replayed, complete and stale are single flags: both values are read.
        foreach (var replayed in new[] { true, false })
            Same(replayed, (await Parse<MemoryMutationResponse>("remember", Js.Set(remember, "replayed", replayed ? "true" : "false"))).Replayed);
        foreach (var complete in new[] { true, false })
            Same(complete, (await Parse<MemoryRecallResponse>("recall", Js.Set(MaxRecallJson, "meta/complete", complete ? "true" : "false"))).Meta.Complete);
        foreach (var stale in new[] { true, false })
        {
            Same(stale, (await Parse<MemoryRecallResponse>("recall", Js.Set(MaxRecallJson, "memories/0/stale", stale ? "true" : "false"))).Memories[0].Stale);
            Same(stale, (await Parse<MemoryFactsResponse>("facts", Js.Set(MaxFactsJson, "facts/0/stale", stale ? "true" : "false"))).Facts[0].Stale);
        }
        foreach (var lookup in new[] { true, false })
        {
            var plan = Js.Set(MaxPlanJson, "conflicts/0/receipt_lookup_required", lookup ? "true" : "false");
            Same(lookup, ((MemoryPlanResponse)await Parse<MemoryConsolidateResponse>("consolidate", plan)).Conflicts[0].ReceiptLookupRequired);
        }
    }

    static async Task RequestBooleanPositions()
    {
        // Every boolean request member alone: the body carries exactly that member, set to true.
        (string Path, string File, string Reply, int Status, Func<MemoryClient, Task> Call)[] toggles =
        [
            ("require_complete_feed", "remember", "response-remember", 200, m => m.RememberAsync(RememberSample with { RequireCompleteFeed = true })),
            ("require_complete_feed", "correct_facts", "response-correction", 200, m => m.CorrectFactsAsync(CorrectSample with { RequireCompleteFeed = true })),
            ("require_complete_feed", "consolidate", "response-plan", 200, m => m.ConsolidateAsync(ConsolidateSample with { RequireCompleteFeed = true })),
            ("require_complete_feed", "forget", "response-forget", 200, m => m.ForgetAsync(ForgetSample with { RequireCompleteFeed = true })),
            ("require_complete_feed", "resume_erasure", "response-resume-pending", 202, m => m.ResumeErasureAsync(ResumeSample with { RequireCompleteFeed = true })),
            ("allow_partial", "recall", "response-recall", 200, m => m.RecallAsync(RecallSample with { AllowPartial = true })),
            ("allow_partial", "facts", "response-facts", 200, m => m.FindFactsAsync(FactsSample with { AllowPartial = true })),
            ("history", "facts", "response-facts", 200, m => m.FindFactsAsync(FactsSample with { History = true })),
            ("dry_run", "consolidate", "response-plan", 200, m => m.ConsolidateAsync(ConsolidateSample with { DryRun = true })),
            ("allow_job", "forget", "response-forget", 200, m => m.ForgetAsync(ForgetSample with { AllowJob = true })),
            ("projection/snippets", "recall", "response-recall", 200, m => m.RecallAsync(RecallSample with { Projection = new MemoryProjection(Snippets: true) })),
            ("projection/provenance", "recall", "response-recall", 200, m => m.RecallAsync(RecallSample with { Projection = new MemoryProjection(Provenance: true) })),
            ("projection/paths", "recall", "response-recall", 200, m => m.RecallAsync(RecallSample with { Projection = new MemoryProjection(Paths: true) })),
            ("projection/snippets", "facts", "response-facts", 200, m => m.FindFactsAsync(FactsSample with { Projection = new MemoryProjection(Snippets: true) })),
        ];
        foreach (var (path, file, reply, status, call) in toggles)
        {
            var body = await SentBody(file, reply, call, status);
            Same("true", SentMember(body, path), file + " " + path + " in " + body);
            var trues = CountTrue(Js.Parse(body));
            Same(1, trues, file + " " + path + " must be the only flag set: " + body);
        }
        // And false is sent as false, never dropped, for each of them.
        foreach (var (path, file, reply, status, call) in new (string, string, string, int, Func<MemoryClient, Task>)[]
        {
            ("allow_partial", "recall", "response-recall", 200, m => m.RecallAsync(RecallSample with { AllowPartial = false })),
            ("history", "facts", "response-facts", 200, m => m.FindFactsAsync(FactsSample with { History = false })),
            ("dry_run", "consolidate", "response-plan", 200, m => m.ConsolidateAsync(ConsolidateSample with { DryRun = false })),
            ("allow_job", "forget", "response-forget", 200, m => m.ForgetAsync(ForgetSample with { AllowJob = false })),
            ("projection/provenance", "recall", "response-recall", 200, m => m.RecallAsync(RecallSample with { Projection = new MemoryProjection(Provenance: false) })),
        })
        {
            var body = await SentBody(file, reply, call, status);
            Same("false", SentMember(body, path), file + " " + path + " in " + body);
        }
    }

    /// <summary>The raw JSON of the member at a slash path, or a marker naming what is missing.</summary>
    static string SentMember(string body, string path)
    {
        var element = Js.Parse(body);
        foreach (var step in path.Split('/'))
        {
            if (element.ValueKind != JsonValueKind.Object || !element.TryGetProperty(step, out var next)) return "<missing " + path + ">";
            element = next;
        }
        return element.GetRawText();
    }

    static int CountTrue(JsonElement element) => element.ValueKind switch
    {
        JsonValueKind.True => 1,
        JsonValueKind.Object => element.EnumerateObject().Sum(p => CountTrue(p.Value)),
        JsonValueKind.Array => element.EnumerateArray().Sum(CountTrue),
        _ => 0
    };

    static async Task CultureInvariance()
    {
        var original = CultureInfo.CurrentCulture;
        try
        {
            // Swedish uses U+2212 as its minus sign and a decimal comma; the wire never does. Hosts running with
            // invariant globalization have no such culture, and the assertions then hold trivially.
            try { CultureInfo.CurrentCulture = new CultureInfo("sv-SE"); }
            catch (CultureNotFoundException) { CultureInfo.CurrentCulture = CultureInfo.InvariantCulture; }
            Same("-1", MemoryNumber.FormatI64(-1));
            Same("-9223372036854775808", MemoryNumber.FormatI64(long.MinValue));
            Same("9223372036854775807", MemoryNumber.FormatI64(long.MaxValue));
            Same("18446744073709551615", MemoryNumber.FormatU64(ulong.MaxValue));
            Check(MemoryNumber.TryParseI64("-1", out var minusOne) && minusOne == -1);
            Check(MemoryNumber.TryParseI64("-9223372036854775808", out var minimum) && minimum == long.MinValue);
            Check(MemoryNumber.TryParseU64("18446744073709551615", out var maximum) && maximum == ulong.MaxValue);
            Same(-7L, MemoryNumber.ParseI64("-7"));
            Check(new MemoryScalar.Integer(-5).Value == "-5" && MemoryInterval.Between(-3, -2) == new MemoryInterval("-3", "-2"));

            var body = Js.Parse(await SentBody("remember", "response-remember", m => m.RememberAsync(RememberSample with
            {
                Items = [RememberSample.Items[0] with { ObservedAtMs = long.MinValue, Importance = 0.5, Metadata = new MemoryProperties { ["f"] = new MemoryScalar.Float(1.25) } }]
            })));
            var item = body.GetProperty("items")[0];
            Same("-9223372036854775808", item.GetProperty("observed_at_ms").GetString());
            Same("0.5", item.GetProperty("importance").GetRawText());
            Same("1.25", item.GetProperty("metadata").GetProperty("f").GetProperty("value").GetRawText());

            // Responses parse the same way under any culture.
            var recall = await Parse<MemoryRecallResponse>("recall", Js.Set(Js.Set(MaxRecallJson, "memories/0/observed_at_ms", "\"-9223372036854775808\""), "meta/as_of_ms", "\"-5\""));
            Same("-9223372036854775808", recall.Memories[0].ObservedAtMs);
            Same("-5", recall.Meta.AsOfMs);
            Same(0.25, recall.Memories[0].Importance);
            Same(1.5, ((MemoryScalar.Float)recall.Memories[0].Metadata!["c"]).Value);
            Same("-5", (await Parse<MemoryRecallResponse>("recall", MaxRecallJson)).Meta.AsOfMs);
        }
        finally { CultureInfo.CurrentCulture = original; }
    }

    static async Task UnionConverters()
    {
        // The union converters also work through System.Text.Json directly on the declared base type.
        var plan = Fx.Value("consolidate", "response-plan");
        var committed = Fx.Expected("consolidate", "consolidate-retain-one-source");
        Check(JsonSerializer.Deserialize<MemoryConsolidateResponse>(plan) is MemoryPlanResponse { DryRun: true });
        Check(JsonSerializer.Deserialize<MemoryConsolidateResponse>(committed) is MemoryMutationResponse { Replayed: false });
        foreach (var json in new[] { plan, committed })
            Json(json, JsonSerializer.Serialize(JsonSerializer.Deserialize<MemoryConsolidateResponse>(json)!, Plain));
        foreach (var bad in new[] { "{}", "[]", "null", "\"plan\"", Js.Set(plan, "receipt", Js.Get(committed, "receipt")), Js.Set(plan, "dry_run", "false") })
        {
            Exception? error = null;
            try { JsonSerializer.Deserialize<MemoryConsolidateResponse>(bad); } catch (Exception e) { error = e; }
            Check(error is JsonException || bad == "null", bad);
        }

        foreach (var json in new[]
        {
            """{"ids":[{"id":"a","revision":"1"}]}""", """{"series":{"id":"f","revision":"3"}}""", """{"entity":{"id":"e","revision":"4"}}""",
            """{"session":true}""", """{"scope":true}"""
        })
            Json(json, JsonSerializer.Serialize(JsonSerializer.Deserialize<MemoryTarget>(json)!, Plain));
        foreach (var json in new[] { """{"absent":true}""", """{"revision":"7"}""", """{"revision":"18446744073709551615"}""" })
            Json(json, JsonSerializer.Serialize(JsonSerializer.Deserialize<MemoryExpected>(json)!, Plain));
        foreach (var json in new[]
        {
            """{"type":"string","value":"x"}""", """{"type":"integer","value":"-1"}""", """{"type":"float","value":1.5}""", """{"type":"boolean","value":false}"""
        })
            Json(json, JsonSerializer.Serialize(JsonSerializer.Deserialize<MemoryScalar>(json)!, Plain));
        Same<MemoryScalar>(new MemoryScalar.Integer("5"), JsonSerializer.Deserialize<MemoryScalar>("""{"type":"integer","value":5}""")!);
        Same<MemoryExpected>(new MemoryExpected.Revision("5"), JsonSerializer.Deserialize<MemoryExpected>("""{"revision":5}""")!);
        foreach (var bad in new[] { "{}", """{"ids":[],"scope":true}""", """{"scope":false}""", """{"unknown":true}""" })
            Check(Throws<JsonException>(() => JsonSerializer.Deserialize<MemoryTarget>(bad)), bad);
        foreach (var bad in new[] { "{}", """{"absent":false}""", """{"absent":true,"revision":"1"}""", """{"revision":"01"}""", """{"revision":1.5}""", """{"revision":-1}""" })
            Check(Throws<JsonException>(() => JsonSerializer.Deserialize<MemoryExpected>(bad)), bad);
        foreach (var bad in new[] { "{}", """{"type":"string"}""", """{"type":"integer","value":"01"}""", """{"type":"integer","value":1.5}""", """{"type":"float","value":"1"}""", """{"type":"nope","value":1}""" })
            Check(Throws<JsonException>(() => JsonSerializer.Deserialize<MemoryScalar>(bad)), bad);
        await Task.CompletedTask;
    }

    static bool Throws<T>(Action action) where T : Exception
    {
        try { action(); return false; }
        catch (T) { return true; }
    }

    static async Task NumberEdges()
    {
        // Numbers a JSON reader cannot hold exactly are refused, not rounded or clipped.
        foreach (var (path, value) in new (string, string)[]
        {
            ("memories/0/importance", "1e999"), ("memories/0/importance", "-1e999"), ("memories/0/scores/R", "1e999"),
            ("memories/0/scores/dense_rank", "3000000000"), ("memories/0/scores/dense_rank", "-3000000000"), ("memories/0/scores/text_rank", "1e3"),
            ("memories/0/scores/fused_rank", "1.0")
        })
            await Invalid("recall", Js.Set(MaxRecallJson, path, value), path + " = " + value);
        var scores = (await Parse<MemoryRecallResponse>("recall", Js.Set(Js.Set(MaxRecallJson, "memories/0/scores/dense_rank", "2147483647"), "memories/0/scores/R", "-1.5e-3"))).Memories[0].Scores;
        Same(int.MaxValue, scores.DenseRank);
        Same(-0.0015, scores.R);
        // Values the client cannot write are refused before any request is made.
        var server = Reply("remember", "response-remember");
        using var client = server.NewClient();
        await Fails<ArgumentException>(() => client.Memory("notes").RememberAsync(RememberSample with
        { Items = [RememberSample.Items[0] with { Kind = (MemoryKind)99 }] }));
        await Fails<ArgumentException>(() => client.Memory("notes").ForgetAsync(ForgetSample with { Mode = (MemoryForgetMode)42 }));
        await Fails<ArgumentException>(() => client.Memory("notes").GetReceiptAsync(ReceiptSample with { Operation = (MemoryOperation)(-1) }));
        await Fails<ArgumentException>(() => client.Memory("notes").RecallAsync(RecallSample with { Kinds = [MemoryKind.Entity, (MemoryKind)7] }));
        Same(0, server.Calls.Count);
    }
}
