using System.Diagnostics;
using System.Net;
using System.Text;
using System.Text.Json;
using VLite;

static partial class MemoryTests
{
    static List<(int Status, string Code, string Body)> FixtureErrors()
    {
        var errors = new List<(int, string, string)>();
        foreach (var entry in Fx.Structural("remember"))
            if (entry.GetProperty("schema").GetString() == "exchange" && entry.GetProperty("valid").GetBoolean() &&
                entry.GetProperty("status").GetInt32() >= 400)
                errors.Add((entry.GetProperty("status").GetInt32(), entry.GetProperty("value").GetProperty("error").GetProperty("code").GetString()!,
                    entry.GetProperty("value").GetRawText()));
        return errors;
    }

    static string Error(string code, string? detailsJson = null, string message = "sanitized") =>
        "{\"error\":{\"code\":\"" + code + "\",\"message\":\"" + message + "\"" + (detailsJson is null ? "" : ",\"details\":" + detailsJson) + "}}";

    static async Task<VLiteException> Refused(Probe probe, int status, string reply, string? requestId = "req-1", params (string, string)[] headers)
    {
        var server = new Server { Status = status, Reply = reply, RequestId = requestId };
        foreach (var (name, value) in headers) server.ReplyHeaders[name] = value;
        using var client = server.NewClient();
        var error = await Fails<VLiteException>(() => probe.Call(client.Memory("notes"), default), probe.Operation + " " + status);
        Same(1, server.Calls.Count, probe.Operation + " " + status + " must be sent exactly once");
        return error;
    }

    static async Task ErrorEnvelopes()
    {
        var errors = FixtureErrors();
        Same(34, errors.Count, "34 status/code pairs in the fixtures");
        foreach (var probe in Probes)
            foreach (var (status, code, body) in errors)
            {
                var error = await Refused(probe, status, body, "req-" + status);
                Same(status, error.Status, probe.Operation + " " + code);
                Same(code, error.Code, probe.Operation + " " + status);
                Same("sanitized", error.Message);
                Same("req-" + status, error.RequestId);
                Check(error.Details is null, probe.Operation + " " + code + " carries no details");
            }

        // Refusals of the four operations a first server slice does not implement carry details.feature untouched.
        (string Operation, string Feature)[] unavailable =
            [("facts", "facts"), ("correct_facts", "facts"), ("erasure", "erasure_jobs"), ("resume_erasure", "erasure_jobs")];
        foreach (var (operation, feature) in unavailable)
        {
            var error = await Refused(Op(operation), 409, Error("memory_capability_unavailable", "{\"feature\":\"" + feature + "\"}"), "req-cap");
            Same(409, error.Status);
            Same("memory_capability_unavailable", error.Code);
            Same("req-cap", error.RequestId);
            Same(feature, error.Details!.Feature);
            Check(error.Details.Field is null && error.Details.Limit is null && error.Details.Requested is null && error.Details.Category is null);
        }
        var pending = await Refused(Op("remember"), 409, Error("memory_deletion_pending"));
        Same("memory_deletion_pending", pending.Code);
        Same(409, pending.Status);
        Check(pending.Details is null);

        var precondition = await Refused(Op("remember"), 412, Error("precondition_failed", "{\"field\":\"revision\"}"));
        Same(412, precondition.Status);
        Same("precondition_failed", precondition.Code);
        Same("revision", precondition.Details!.Field);

        // The independent fixtures for details: error body and context schema examples.
        var errorExample = Fx.Value("remember", "schema-valid-Error");
        var invalid = await Refused(Op("consolidate"), 422, errorExample);
        Same(422, invalid.Status);
        Same("invalid_input", invalid.Code);
        Same(new MemoryErrorContext(Field: "inputs", Limit: "64", Requested: "65"), invalid.Details);
        var context = Fx.Value("remember", "schema-valid-ErrorContext");
        var full = await Refused(Op("recall"), 422, Error("invalid_input", context));
        Same(new MemoryErrorContext("inputs", "64", "65", "facts", "budget"), full.Details);

        var deadline = await Refused(Op("recall"), 504, Error("deadline_exceeded"));
        Same(504, deadline.Status);
        Same("deadline_exceeded", deadline.Code);
        var forgetRefusal = await Refused(Op("forget"), 409, Error("memory_capability_unavailable", "{\"feature\":\"forget_target\",\"category\":\"core\"}"));
        Same(new MemoryErrorContext(Feature: "forget_target", Category: "core"), forgetRefusal.Details);

        // Without a request id header there is none; nonstandard bodies keep the generic code.
        var noId = await Refused(Op("remember"), 409, Error("idempotency_conflict"), null);
        Check(noId.RequestId is null);
        var plain = await Refused(Op("recall"), 502, "<html>bad gateway</html>", "req-9");
        Same("http_error", plain.Code);
        Same(502, plain.Status);
        Same("req-9", plain.RequestId);
        Check(plain.Details is null);
        var noMessage = await Refused(Op("recall"), 409, "{\"error\":{\"code\":\"x\",\"details\":{\"feature\":\"f\"}}}");
        Same("http_error", noMessage.Code);
        Check(noMessage.Details is null);
        var empty = await Refused(Op("recall"), 500, "");
        Same("http_error", empty.Code);
        var redirect = await Refused(Op("recall"), 302, Error("moved"));
        Same(302, redirect.Status);
        Same("moved", redirect.Code);
    }

    static async Task ErrorDetails()
    {
        async Task<MemoryErrorContext?> DetailsOf(string detailsJson, string code = "invalid_input")
        {
            var error = await Refused(Op("recall"), 422, Error(code, detailsJson));
            Same(422, error.Status);
            Same(code, error.Code);
            Same("sanitized", error.Message);
            return error.Details;
        }
        Check(await DetailsOf("null") is null);
        Check(await DetailsOf("\"text\"") is null);
        Check(await DetailsOf("[]") is null);
        Check(await DetailsOf("5") is null);
        Check(await DetailsOf("true") is null);
        Same(new MemoryErrorContext(), await DetailsOf("{}"));
        Same(new MemoryErrorContext(Feature: "x"), await DetailsOf("{\"feature\":\"x\",\"future\":{\"a\":[1]},\"also\":null}"));
        Same(new MemoryErrorContext("f", "1", "2", "g", "c"), await DetailsOf("{\"category\":\"c\",\"requested\":\"2\",\"feature\":\"g\",\"limit\":\"1\",\"field\":\"f\"}"));
        // Members of the wrong type are dropped one by one; the code and status never are.
        Same(new MemoryErrorContext(Feature: "ok"), await DetailsOf("{\"field\":1,\"feature\":\"ok\",\"category\":[],\"limit\":true,\"requested\":{}}"));
        Same(new MemoryErrorContext(), await DetailsOf("{\"field\":null,\"feature\":null,\"category\":null,\"limit\":null,\"requested\":null}"));
        // limit and requested are u64: canonical strings or exact JSON integers, else ignored.
        Same("64", (await DetailsOf("{\"limit\":\"64\"}"))!.Limit);
        Same("64", (await DetailsOf("{\"limit\":64}"))!.Limit);
        Same("0", (await DetailsOf("{\"requested\":0}"))!.Requested);
        Same("18446744073709551615", (await DetailsOf("{\"limit\":\"18446744073709551615\"}"))!.Limit);
        Same("18446744073709551615", (await DetailsOf("{\"requested\":18446744073709551615}"))!.Requested);
        foreach (var bad in new[] { "-1", "1.5", "\"01\"", "\"-1\"", "\"1.0\"", "\"abc\"", "\"\"", "\"18446744073709551616\"", "18446744073709551616", "1e3", "null", "[]" })
        {
            var details = await DetailsOf("{\"limit\":" + bad + ",\"requested\":" + bad + ",\"feature\":\"kept\"}");
            Check(details is { Limit: null, Requested: null, Feature: "kept" }, bad);
        }
        // Details never replace the stable code and are carried on any status.
        Same("memory_unsupported_ranking", (await Refused(Op("recall"), 422, Error("memory_unsupported_ranking", "{\"field\":\"ranking\"}"))).Code);
        Same("ranking", (await Refused(Op("recall"), 422, Error("memory_unsupported_ranking", "{\"field\":\"ranking\"}"))).Details!.Field);
        Same("budget", (await Refused(Op("facts"), 504, Error("deadline_exceeded", "{\"category\":\"budget\"}"))).Details!.Category);
    }

    static async Task UnsupportedRoute()
    {
        foreach (var probe in Probes)
        {
            var server = new Server { Status = 404, Reply = Error("not_found", message: "unknown route"), RequestId = "req-404" };
            using var client = server.NewClient();
            var error = await Fails<UnsupportedException>(() => probe.Call(client.Memory("notes"), default), probe.Operation);
            Same(1, server.Calls.Count, probe.Operation + ": no fallback request");
            Check(error.Message.Contains("memory", StringComparison.OrdinalIgnoreCase) && !error.Message.Contains("KV"), error.Message);

            // Only the exact unknown-route answer is "unsupported"; every domain 404 stays an ordinary error.
            foreach (var code in new[] { "memory_not_found", "receipt_not_found", "erasure_not_found", "collection_not_found", "kv_not_found", "Not_Found", "not_found_" })
            {
                var refused = await Refused(probe, 404, Error(code));
                Same(404, refused.Status);
                Same(code, refused.Code);
            }
            foreach (var status in new[] { 400, 403, 409, 410, 422, 500, 503 })
                Same("not_found", (await Refused(probe, status, Error("not_found"))).Code);
            Same("http_error", (await Refused(probe, 404, "<html>nothing here</html>")).Code);
            Same("http_error", (await Refused(probe, 404, "")).Code);
            Same("http_error", (await Refused(probe, 404, "{\"error\":{\"code\":\"not_found\"}}")).Code);
            Same("http_error", (await Refused(probe, 404, "{\"error\":{\"code\":\"\",\"message\":\"m\"}}")).Code);
        }
        // The KV methods keep their own message and behaviour.
        var kv = new Server { Status = 404, Reply = Error("not_found") };
        using var kvClient = kv.NewClient();
        var kvError = await Fails<UnsupportedException>(() => kvClient.KvGetAsync("cache", "n", "k"));
        Same("Server does not support KV routes; no fallback performed", kvError.Message);
    }

    static async Task NoRetry()
    {
        (int Status, string Code)[] failures =
        [
            (429, "rate_limited"), (500, "internal_error"), (503, "busy"), (503, "unavailable"), (503, "commit_indeterminate"),
            (503, "poisoned"), (504, "deadline_exceeded"), (409, "idempotency_conflict"), (412, "precondition_failed"),
            (502, "bad_gateway"), (408, "request_timeout"), (409, "memory_deletion_pending")
        ];
        foreach (var probe in Probes)
        {
            foreach (var (status, code) in failures)
            {
                var error = await Refused(probe, status, Error(code), "req-1", ("Retry-After", "1"));
                Same(code, error.Code);
                Same(status, error.Status);
            }
            // Transport trouble is surfaced as ambiguity and never retried.
            foreach (var failure in new Func<Exception>[]
            {
                () => new HttpRequestException("connection reset"), () => new IOException("broken pipe"),
                () => new TaskCanceledException("timed out"), () => new OperationCanceledException()
            })
            {
                var server = ServerFor(probe);
                server.Custom = (_, _) => throw failure();
                using var client = server.NewClient();
                var error = await Fails<TransportException>(() => probe.Call(client.Memory("notes"), default), probe.Operation);
                Same("transport_error", error.Code);
                Check(error.Message.Contains("no retry", StringComparison.OrdinalIgnoreCase), error.Message);
                Same(1, server.Calls.Count, probe.Operation + " must not be retried");
            }
            // A success that cannot be understood is ambiguous too, and is not repeated.
            var garbled = new Server { Reply = "{\"receipt\":" };
            using var garbledClient = garbled.NewClient();
            await Fails<TransportException>(() => probe.Call(garbledClient.Memory("notes"), default), probe.Operation);
            Same(1, garbled.Calls.Count);
            // A reply that arrives after a transient failure is never awaited.
            var calls = 0;
            var flaky = ServerFor(probe);
            flaky.Custom = (_, _) =>
            {
                calls++;
                if (calls == 1) throw new HttpRequestException("first attempt");
                return Task.FromResult(new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(flaky.Reply, Encoding.UTF8, "application/json") });
            };
            using var flakyClient = flaky.NewClient();
            await Fails<TransportException>(() => probe.Call(flakyClient.Memory("notes"), default), probe.Operation);
            Same(1, calls, probe.Operation + ": the second attempt is the caller's decision");
        }
    }

    static async Task TimeoutAndCancellation()
    {
        foreach (var probe in Probes)
        {
            using var source = new CancellationTokenSource();
            var inFlight = ServerFor(probe);
            inFlight.Custom = async (_, token) =>
            {
                source.Cancel();
                await Task.Delay(10000, token);
                return new HttpResponseMessage(HttpStatusCode.OK);
            };
            using var client = inFlight.NewClient();
            var cancelled = await Fails<TransportException>(() => probe.Call(client.Memory("notes"), source.Token), probe.Operation);
            Check(cancelled.InnerException is OperationCanceledException, probe.Operation);
            Same(1, inFlight.Calls.Count, probe.Operation + " must not be retried after cancellation");

            var slow = ServerFor(probe);
            slow.Custom = async (_, token) =>
            {
                await Task.Delay(10000, token);
                return new HttpResponseMessage(HttpStatusCode.OK);
            };
            using var timed = slow.NewClient(TimeSpan.FromMilliseconds(100));
            var watch = Stopwatch.StartNew();
            var timeout = await Fails<TransportException>(() => probe.Call(timed.Memory("notes"), default), probe.Operation);
            Check(watch.Elapsed < TimeSpan.FromSeconds(8), "timeout took " + watch.Elapsed);
            Check(timeout.InnerException is OperationCanceledException, probe.Operation);
            Same(1, slow.Calls.Count, probe.Operation + " must not be retried after a timeout");

            using var already = new CancellationTokenSource();
            already.Cancel();
            var fresh = ServerFor(probe);
            using var freshClient = fresh.NewClient();
            await Fails<TransportException>(() => probe.Call(freshClient.Memory("notes"), already.Token), probe.Operation);
            Check(fresh.Calls.Count <= 1, probe.Operation);

            // A reply that was fully received before cancellation is still returned: the token only guards waiting.
            using var late = new CancellationTokenSource();
            var done = ServerFor(probe);
            using var doneClient = done.NewClient();
            Check(await probe.Call(doneClient.Memory("notes"), late.Token) is not null);
            Same(1, done.Calls.Count);
        }
    }

    static async Task DeadlineIsNotHttpTimeout()
    {
        // The body's deadline_ms is the engine deadline: a 1 ms engine budget must not become a 1 ms HTTP timeout.
        var patient = ServerFor(Op("remember"));
        patient.Custom = async (_, token) =>
        {
            await Task.Delay(300, token);
            return new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(patient.Reply, Encoding.UTF8, "application/json") };
        };
        using var client = patient.NewClient();
        var result = await client.Memory("notes").RememberAsync(RememberSample with { DeadlineMs = 1 });
        Same("r1", result.Receipt.Id);
        Same("1", Js.Get(patient.Last.Body!, "deadline_ms"));

        var reading = new Server { Reply = Fx.Value("recall", "response-recall") };
        reading.Custom = async (_, token) =>
        {
            await Task.Delay(300, token);
            return new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(reading.Reply, Encoding.UTF8, "application/json") };
        };
        using var readClient = reading.NewClient();
        await readClient.Memory("notes").RecallAsync(RecallSample with { Limits = new MemoryLimits(DeadlineMs: 1) });
        Same("1", Js.Get(reading.Last.Body!, "limits/deadline_ms"));

        var factsServer = new Server { Reply = Fx.Value("facts", "response-facts") };
        factsServer.Custom = async (_, token) =>
        {
            await Task.Delay(300, token);
            return new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(factsServer.Reply, Encoding.UTF8, "application/json") };
        };
        using var factsClient = factsServer.NewClient();
        await factsClient.Memory("notes").FindFactsAsync(FactsSample with { Limits = new MemoryLimits(DeadlineMs: 1) });
        Same("1", Js.Get(factsServer.Last.Body!, "limits/deadline_ms"));

        // Nor does a long engine deadline stretch the HTTP timeout.
        var stuck = ServerFor(Op("remember"));
        stuck.Custom = async (_, token) =>
        {
            await Task.Delay(30000, token);
            return new HttpResponseMessage(HttpStatusCode.OK);
        };
        using var stuckClient = stuck.NewClient(TimeSpan.FromMilliseconds(150));
        var watch = Stopwatch.StartNew();
        await Fails<TransportException>(() => stuckClient.Memory("notes").RememberAsync(RememberSample with { DeadlineMs = 5000 }));
        Check(watch.Elapsed < TimeSpan.FromSeconds(8), "elapsed " + watch.Elapsed);
        Same(1, stuck.Calls.Count);
    }

    static async Task ClientSideRefusals()
    {
        var server = new Server();
        using var client = server.NewClient();
        var memory = client.Memory("notes");
        await Fails<ArgumentNullException>(() => memory.RememberAsync(null!));
        await Fails<ArgumentNullException>(() => memory.RecallAsync(null!));
        await Fails<ArgumentNullException>(() => memory.FindFactsAsync(null!));
        await Fails<ArgumentNullException>(() => memory.CorrectFactsAsync(null!));
        await Fails<ArgumentNullException>(() => memory.ConsolidateAsync(null!));
        await Fails<ArgumentNullException>(() => memory.ForgetAsync(null!));
        await Fails<ArgumentNullException>(() => memory.GetReceiptAsync(null!));
        await Fails<ArgumentNullException>(() => memory.GetErasureAsync(null!));
        await Fails<ArgumentNullException>(() => memory.ResumeErasureAsync(null!));
        Same(0, server.Calls.Count);
    }

    static async Task ConcurrentFacades()
    {
        var server = new Server { Reply = Fx.Value("remember", "response-remember") };
        using var client = server.NewClient();
        var bindings = new[] { client.Memory("alpha"), client.Memory("beta-2") };
        var names = new[] { "alpha", "beta-2" };
        await Task.WhenAll(Enumerable.Range(0, 60).Select(i =>
            bindings[i % 2].RememberAsync(RememberSample with { IdempotencyKey = "k" + i })));
        Same(60, server.Calls.Count);
        foreach (var call in server.Calls)
        {
            var key = call.Json().GetProperty("idempotency_key").GetString()!;
            var index = int.Parse(key[1..]);
            Same("/v1/memory/" + names[index % 2] + "/remember", call.Target, key);
        }
    }
}
