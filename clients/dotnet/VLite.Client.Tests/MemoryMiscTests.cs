using System.Text.Json;
using System.Text.Json.Nodes;
using VLite;

static partial class MemoryTests
{
    static async Task NumberHelpers()
    {
        (string Text, ulong Value)[] unsigned =
        [
            ("0", 0), ("1", 1), ("10", 10), ("100", 100), ("9223372036854775808", 9223372036854775808UL),
            ("18446744073709551614", ulong.MaxValue - 1), ("18446744073709551615", ulong.MaxValue)
        ];
        foreach (var (text, value) in unsigned)
        {
            Check(MemoryNumber.TryParseU64(text, out var parsed) && parsed == value, text);
            Same(value, MemoryNumber.ParseU64(text), text);
            Same(text, MemoryNumber.FormatU64(value));
        }
        foreach (var bad in new string?[]
        {
            null, "", "-", "-0", "-1", "+0", "+1", "00", "01", "1_0", " 1", "1 ", "\t1", "1\n", "1.0", "1e1", "0x1", "١", "١٢",
            "18446744073709551616", "18446744073709551620", "99999999999999999999999", "184467440737095516150", "1,0"
        })
        {
            Check(!MemoryNumber.TryParseU64(bad, out var zero) && zero == 0, bad ?? "null");
            await Fails<FormatException>(() => { MemoryNumber.ParseU64(bad!); return Task.CompletedTask; }, bad ?? "null");
        }

        (string Text, long Value)[] signed =
        [
            ("0", 0), ("1", 1), ("-1", -1), ("10", 10), ("-10", -10), ("9223372036854775806", long.MaxValue - 1),
            ("9223372036854775807", long.MaxValue), ("-9223372036854775807", long.MinValue + 1), ("-9223372036854775808", long.MinValue)
        ];
        foreach (var (text, value) in signed)
        {
            Check(MemoryNumber.TryParseI64(text, out var parsed) && parsed == value, text);
            Same(value, MemoryNumber.ParseI64(text), text);
            Same(text, MemoryNumber.FormatI64(value));
        }
        foreach (var bad in new string?[]
        {
            null, "", "-", "--1", "-0", "+0", "+1", "-+1", "00", "-00", "01", "-01", " 1", "-1 ", "1.0", "-1.0", "0x1", "١", "-١",
            "9223372036854775808", "-9223372036854775809", "92233720368547758070", "-92233720368547758080", "1-"
        })
        {
            Check(!MemoryNumber.TryParseI64(bad, out var zero) && zero == 0, bad ?? "null");
            await Fails<FormatException>(() => { MemoryNumber.ParseI64(bad!); return Task.CompletedTask; }, bad ?? "null");
        }
    }

    static async Task MiscApi()
    {
        var server = new Server();
        using var client = server.NewClient();
        Same("notes", client.Memory("notes").Binding);
        Same("a-b_c9", client.Memory("a-b_c9").Binding);

        // The error type keeps its four-argument constructor and gains optional structured details.
        var plain = new VLiteException(500, "internal_error", "boom", "req-1");
        Same(500, plain.Status);
        Same("internal_error", plain.Code);
        Same("boom", plain.Message);
        Same("req-1", plain.RequestId);
        Check(plain.Details is null);
        var detailed = new VLiteException(409, "memory_capability_unavailable", "no", null, new MemoryErrorContext(Feature: "facts"));
        Check(detailed.RequestId is null);
        Same("facts", detailed.Details!.Feature);
        Same("Server does not support KV routes; no fallback performed", new UnsupportedException().Message);

        // Existing methods share the error mapper, so they carry details too.
        var refused = new Server { Status = 422, Reply = Error("invalid_input", "{\"field\":\"key\",\"limit\":\"1024\"}") };
        using var kvClient = refused.NewClient();
        var kvError = await Fails<VLiteException>(() => kvClient.KvGetAsync("cache", "n", "k"));
        Same("invalid_input", kvError.Code);
        Same(new MemoryErrorContext(Field: "key", Limit: "1024"), kvError.Details);
        var health = await Fails<VLiteException>(() => kvClient.HealthAsync());
        Same("key", health.Details!.Field);

        // Union variants with array members compare by content.
        var a = new MemoryTarget.Ids([new MemoryRef("a", 1), new MemoryRef("b", 2)]);
        var b = new MemoryTarget.Ids([new MemoryRef("a", 1), new MemoryRef("b", 2)]);
        var hashOfA = a.GetHashCode();
        Check(a.Equals(b) && a == b && hashOfA == b.GetHashCode());
        Check(!a.Equals(new MemoryTarget.Ids([new MemoryRef("b", 2), new MemoryRef("a", 1)])));
        Check(!a.Equals(new MemoryTarget.Ids([new MemoryRef("a", 1)])));
        Check(!a.Equals(new MemoryTarget.Ids([new MemoryRef("a", 1), new MemoryRef("b", 3)])));
        Check(a != new MemoryTarget.Ids([new MemoryRef("a", 1)]));
        Check(!a.Equals((MemoryTarget)new MemoryTarget.Scope()));
        Check(!a.Equals(null));
        Check(new MemoryTarget.Ids([]).Equals(new MemoryTarget.Ids([])));
        var other = new MemoryTarget.Ids([new MemoryRef("a", 1)]);
        Check(hashOfA != other.GetHashCode());

        // Shared records compare by value and accept numbers or canonical strings.
        Check(new MemoryRef("x", 5) == new MemoryRef("x", "5"));
        Check(new MemoryRef("x", 5) != new MemoryRef("x", "6"));
        Check(new MemoryRef("x", ulong.MaxValue).Revision == "18446744073709551615");
        Check(MemoryInterval.Between(-1, 1) == new MemoryInterval("-1", "1"));
        Check(MemoryInterval.Unbounded == new MemoryInterval(null, null));
        Check(MemoryInterval.From(7) == new MemoryInterval("7", null) && MemoryInterval.Until(7) == new MemoryInterval(null, "7"));
        Check(new MemoryScalar.Integer(long.MinValue).Value == "-9223372036854775808");
        Check(new MemoryExpected.Revision(ulong.MaxValue).Value == "18446744073709551615");
    }

    static async Task OrderPreserved()
    {
        // Results keep the order the server chose: outcomes follow input order, memories rank order, lists as sent.
        var outcomes = new JsonArray();
        foreach (var i in Enumerable.Range(0, 50).Reverse().Where(n => n % 7 != 3))
            outcomes.Add(new JsonObject { ["id"] = "o" + i, ["action"] = i % 2 == 0 ? "created" : "noop", ["revision"] = (i * 11).ToString() });
        var reply = Js.Set(Fx.Value("remember", "response-remember"), "outcomes", outcomes.ToJsonString());
        var parsed = await Parse<MemoryMutationResponse>("remember", reply);
        Same(outcomes.Count, parsed.Outcomes.Length);
        for (var i = 0; i < outcomes.Count; i++)
        {
            Same(outcomes[i]!["id"]!.GetValue<string>(), parsed.Outcomes[i].Id);
            Same(outcomes[i]!["revision"]!.GetValue<string>(), parsed.Outcomes[i].Revision);
            Same(outcomes[i]!["action"]!.GetValue<string>() == "created" ? MemoryOutcomeAction.Created : MemoryOutcomeAction.Noop, parsed.Outcomes[i].Action);
        }

        var view = JsonNode.Parse(Js.Get(MaxRecallJson, "memories/0"))!;
        var views = new JsonArray();
        foreach (var id in new[] { "z", "a", "m", "b" })
        {
            var copy = view.DeepClone();
            copy["id"] = id;
            views.Add(copy);
        }
        var recall = Js.Set(Js.Set(MaxRecallJson, "memories", views.ToJsonString()), "meta/truncation", "[\"deadline\",\"candidates\"]");
        recall = Js.Set(recall, "meta/limitations", "[\"retained_state_only\",\"historical_candidate_discovery_not_exhaustive\"]");
        var read = await Parse<MemoryRecallResponse>("recall", recall);
        Same("z,a,m,b", string.Join(",", read.Memories.Select(m => m.Id)));
        Same("deadline,candidates", string.Join(",", read.Meta.Truncation));
        Same("retained_state_only,historical_candidate_discovery_not_exhaustive", string.Join(",", read.Meta.Limitations));

        // Large answers are returned whole, nothing truncated or capped by the client.
        var big = new JsonArray();
        for (var i = 0; i < 4096; i++) big.Add(new JsonObject { ["id"] = "e" + i, ["action"] = "created", ["revision"] = "1" });
        var large = await Parse<MemoryMutationResponse>("remember", Js.Set(Fx.Value("remember", "response-remember"), "outcomes", big.ToJsonString()));
        Same(4096, large.Outcomes.Length);
        Same("e0", large.Outcomes[0].Id);
        Same("e4095", large.Outcomes[^1].Id);
        var memories = new JsonArray();
        for (var i = 0; i < 100; i++)
        {
            var copy = view.DeepClone();
            copy["id"] = "m" + i;
            memories.Add(copy);
        }
        var many = await Parse<MemoryRecallResponse>("recall", Js.Set(MaxRecallJson, "memories", memories.ToJsonString()));
        Same(100, many.Memories.Length);
        Same("m99", many.Memories[^1].Id);
    }
}
