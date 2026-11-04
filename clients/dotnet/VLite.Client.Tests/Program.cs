using System.Net;
using System.Text;
using System.Text.Json;
using VLite;

const string Token = "abcdefghijklmnopqrstuvwxyz012345";
var count = 0;
void Check(bool condition) { if (!condition) throw new Exception("Assertion failed"); count++; }
async Task Fails<T>(Func<Task> action, Action<T>? inspect = null) where T : Exception
{
    try { await action(); } catch (T error) { inspect?.Invoke(error); count++; return; }
    throw new Exception("Expected " + typeof(T).Name);
}
var calls = new List<string>();
using (var client = new Client("http://localhost", Token, handler: new FakeHandler(async (request, cancellation) =>
{
    Check(request.Headers.Authorization?.ToString() == "Bearer " + Token);
    var path = request.RequestUri!.PathAndQuery;
    calls.Add(request.Method + " " + path);
    var response = path switch
    {
        "/v1/vectors/1" when request.Method == HttpMethod.Get => """{"id":"1","vector":[1,0,0],"payload_base64":"AP8=","metadata":{}}""",
        _ when request.Method == HttpMethod.Delete => """{"deleted":true}""",
        _ when path.Contains("/search") => """{"hits":[{"id":"18446744073709551615","distance":0.25}]}""",
        _ when path.Contains("/vectors?") => """{"items":[],"next_after":null}""",
        _ when request.Method == HttpMethod.Put || path.EndsWith("/batches") || path.EndsWith("/index/hnsw") => "",
        _ => "{}"
    };
    if (request.Content is not null)
    {
        Check(request.Content.Headers.ContentType?.MediaType == "application/json");
        using var body = JsonDocument.Parse(await request.Content.ReadAsStringAsync(cancellation));
        if (path.EndsWith("/search"))
        {
            Check(body.RootElement.GetProperty("filter")[0].GetProperty("value").GetProperty("value").GetString() == "9223372036854775807");
            Check(body.RootElement.GetProperty("mode").GetString() is "exact" or "approximate" or "approximate_ready");
        }
        if (request.Method == HttpMethod.Put && path.Contains("/vectors/"))
            Check(body.RootElement.GetProperty("metadata").GetProperty("rank").GetProperty("value").GetString() == "9223372036854775807");
    }
    return new HttpResponseMessage(response == "" ? HttpStatusCode.NoContent : HttpStatusCode.OK)
    { Content = new StringContent(response, Encoding.UTF8, "application/json") };
})))
{
    await client.HealthAsync(); await client.ReadyAsync(); await client.DatabaseAsync(); await client.ListCollectionsAsync();
    await client.CreateCollectionAsync("docs", 3);
    await client.PutAsync("18446744073709551615", new PutRequest([1, 0, 0], "AP8=", new() { ["rank"] = new("integer", "9223372036854775807") }));
    Check((await client.GetAsync("1")).PayloadBase64 == "AP8=");
    Check(await client.DeleteAsync("1"));
    await client.BatchAsync([new { op = "delete", id = "1" }]);
    Check((await client.BrowseAsync(after: "0")).NextAfter is null);
    await client.BuildIndexAsync(); await client.IndexStatusAsync();
    var collection = client.Collection("docs");
    await collection.InfoAsync(); await collection.SetSettingsAsync(new { metric = "cosine" }); Check(await collection.DropAsync());
    foreach (var mode in new[] { "exact", "approximate", "approximate_ready" })
    {
        var hits = await collection.SearchAsync(new { vector = new[] { 1, 0, 0 }, k = 1, mode,
            filter = new[] { new { op = "eq", field = "rank", value = new { type = "integer", value = "9223372036854775807" } } } });
        Check(hits[0].Id == "18446744073709551615");
    }
    Check(calls.Contains("GET /v1/vectors?limit=25&after=0"));
    Check(calls.Contains("POST /v1/collections/docs/search"));
    foreach (var id in new[] { "01", "-1", "18446744073709551616", "../x" })
        await Fails<ArgumentException>(() => client.GetAsync(id));
}
foreach (var status in new[] { 401, 404, 409, 422, 503, 302 })
{
    using var client = new Client("http://localhost", Token, handler: new FakeHandler((_, _) =>
    {
        var result = new HttpResponseMessage((HttpStatusCode)status)
        { Content = new StringContent("""{"error":{"code":"stable_code","message":"detail"}}""") };
        result.Headers.Add("X-Request-ID", "req-1");
        return Task.FromResult(result);
    }));
    await Fails<VLiteException>(() => client.HealthAsync(), e => Check(e.Status == status && e.Code == "stable_code" && e.RequestId == "req-1"));
}
using (var client = new Client("http://localhost", Token, handler: new FakeHandler((_, _) => Task.FromResult(
    new HttpResponseMessage(HttpStatusCode.BadGateway) { Content = new StringContent("not json") }))))
    await Fails<VLiteException>(() => client.HealthAsync(), e => Check(e.Code == "http_error"));
using (var client = new Client("http://localhost", Token, handler: new FakeHandler((_, _) => Task.FromResult(
    new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent("not json") }))))
    await Fails<TransportException>(() => client.HealthAsync(), e => Check(e.Code == "transport_error"));
using (var client = new Client("http://localhost", Token, TimeSpan.FromMilliseconds(10), new FakeHandler(async (_, cancellation) =>
    { await Task.Delay(10000, cancellation); return new HttpResponseMessage(HttpStatusCode.OK); })))
    await Fails<TransportException>(() => client.HealthAsync());
foreach (var origin in new[] { "ftp://localhost", "http://user@localhost", "http://localhost/path", "http://localhost?x=1" })
    await Fails<ArgumentException>(() => { using var c = new Client(origin, Token); return Task.CompletedTask; });

async Task KvWireContractTests()
{
    // KV wire fixtures come from the serving contract, not the client codec.
    var kvCalls = new List<(string Method, string Path, string? Body)>();
    var kvReply = """{"key":"雪 /+?%","value_base64":"AP8=","version":"18446744073709551615","expires_at_ms":"0","future":true}""";
    var kvStatus = 200;
    using (var client = new Client("http://localhost", Token, handler: new FakeHandler(async (request, cancellation) =>
    {
        Check(request.Headers.Authorization?.ToString() == "Bearer " + Token);
        Check(request.Headers.Accept.Single().MediaType == "application/json");
        if (request.Content is not null) Check(request.Content.Headers.ContentType?.MediaType == "application/json");
        kvCalls.Add((request.Method.Method, request.RequestUri!.PathAndQuery,
            request.Content is null ? null : await request.Content.ReadAsStringAsync(cancellation)));
        var response = new HttpResponseMessage((HttpStatusCode)kvStatus) { Content = new StringContent(kvReply) };
        response.Headers.Add("X-Request-ID", "kv-error");
        return response;
    })))
    {
        var entry = await client.KvGetAsync("cache", "名 +", "雪 /+?%");
        Check(entry.Key == "雪 /+?%" && entry.Value.SequenceEqual(new byte[] { 0, 255 }) &&
            entry.Version == ulong.MaxValue && entry.ExpiresAtMs == 0);
        Check(kvCalls[^1] == ("GET", "/v1/collections/cache/kv/entry?namespace=%E5%90%8D%20%2B&key=%E9%9B%AA%20%2F%2B%3F%25", null));
        var expectations = new (Expected Value, string Json, string Query)[]
        {
            (new Expected.Any(), "\"any\"", "any"), (new Expected.Absent(), "\"absent\"", "absent"),
            (new Expected.Revision(0), """{"revision":"0"}""", "0"),
            (new Expected.Revision(9007199254740993), """{"revision":"9007199254740993"}""", "9007199254740993"),
            (new Expected.Revision(ulong.MaxValue), """{"revision":"18446744073709551615"}""", "18446744073709551615")
        };
        foreach (var (expected, json, query) in expectations)
        {
            kvReply = """{"version":"9007199254740993","expires_at_ms":"18446744073709551615"}""";
            var result = await client.KvPutAsync("cache", "n", "k", [255], expected, ulong.MaxValue);
            Check(result.Version == 9007199254740993 && result.ExpiresAtMs == ulong.MaxValue);
            Check(kvCalls[^1].Method == "PUT" && kvCalls[^1].Path == "/v1/collections/cache/kv/entry");
            using var body = JsonDocument.Parse(kvCalls[^1].Body!);
            Check(body.RootElement.EnumerateObject().Count() == 5);
            Check(body.RootElement.GetProperty("namespace").GetString() == "n" && body.RootElement.GetProperty("key").GetString() == "k");
            Check(body.RootElement.GetProperty("value_base64").GetString() == "/w==");
            Check(body.RootElement.GetProperty("expected").GetRawText() == json);
            Check(body.RootElement.GetProperty("expires_at_ms").GetString() == "18446744073709551615");
            foreach (var deleted in new[] { true, false })
            {
                kvReply = deleted ? """{"deleted":true}""" : """{"deleted":false}""";
                Check(await client.KvDeleteAsync("cache", "名 +", "雪 /+?%", expected) == deleted);
                Check(kvCalls[^1] == ("DELETE", "/v1/collections/cache/kv/entry?namespace=%E5%90%8D%20%2B&key=%E9%9B%AA%20%2F%2B%3F%25&expected=" + query, null));
            }
        }
        kvReply = """{"items":[{"key":"雪 /+?%","value_base64":"","version":"1","expires_at_ms":null}],"next_after":"雪 /+?%"}""";
        var page = await client.KvListAsync("cache", "名 +", "雪 ", "a/+", 10000);
        Check(page.Items.Length == 1 && page.Items[0].Value.Length == 0 && page.Items[0].Version == 1 &&
            page.Items[0].ExpiresAtMs is null && page.NextAfter == "雪 /+?%");
        Check(kvCalls[^1] == ("GET", "/v1/collections/cache/kv/entries?namespace=%E5%90%8D%20%2B&prefix=%E9%9B%AA%20&after=a%2F%2B&limit=10000", null));
        kvReply = """{"items":[],"next_after":null}""";
        Check((await client.KvListAsync("cache", "n")).NextAfter is null);
        Check(kvCalls[^1].Path == "/v1/collections/cache/kv/entries?namespace=n&prefix=&limit=100");
        kvReply = """{"key":"k","value_base64":"","version":"1","expires_at_ms":null}""";
        Check((await client.KvGetAsync("cache", new string('n', 255), new string('é', 512))).Value.Length == 0);
        kvReply = """{"items":[],"next_after":null}""";
        await client.KvListAsync("cache", "n", new string('é', 512), new string('é', 512), 1);
        kvReply = """{"version":"1","expires_at_ms":"0"}""";
        Check((await client.KvPutAsync("cache", "n", "k", [], expiresAtMs: 0)).ExpiresAtMs == 0);
        using (var body = JsonDocument.Parse(kvCalls[^1].Body!))
            Check(body.RootElement.GetProperty("expires_at_ms").GetString() == "0");

        foreach (var (size, encoded) in new[] { (0, ""), (1047290, new string('A', 1396387) + "=") })
        {
            kvReply = """{"version":"1","expires_at_ms":null}""";
            var result = await client.KvPutAsync("cache", "n", "k", new byte[size]);
            Check(result.Version == 1 && result.ExpiresAtMs is null);
            using var body = JsonDocument.Parse(kvCalls[^1].Body!);
            Check(body.RootElement.GetProperty("value_base64").GetString() == encoded);
            Check(body.RootElement.GetProperty("expected").GetString() == "any");
            Check(body.RootElement.GetProperty("expires_at_ms").ValueKind == JsonValueKind.Null);
        }
        var before = kvCalls.Count;
        await Fails<ArgumentException>(() => client.KvPutAsync("cache", "n", "k", new byte[1047291]));
        foreach (var (ns, key) in new[] { ("", "k"), (new string('é', 128), "k"), ("n", ""),
            ("n", new string('é', 513)), ("n", "\ud800") })
            await Fails<ArgumentException>(() => client.KvGetAsync("cache", ns, key));
        await Fails<ArgumentException>(() => client.KvGetAsync("", "n", "k"));
        await Fails<ArgumentException>(() => client.KvListAsync("cache", "n", after: ""));
        await Fails<ArgumentException>(() => client.KvListAsync("cache", "n", prefix: new string('é', 513)));
        foreach (var limit in new[] { 0, 10001 })
            await Fails<ArgumentException>(() => client.KvListAsync("cache", "n", limit: limit));
        Check(kvCalls.Count == before);

        foreach (var (status, code) in new[] { (404, "kv_not_found"), (412, "precondition_failed"),
            (409, "not_kv_collection"), (409, "kv_slots_exhausted"), (422, "invalid_input"),
            (404, "collection_not_found"), (503, "commit_indeterminate"), (404, "not_found"), (409, "not_found") })
        {
            kvStatus = status;
            kvReply = "{\"error\":{\"code\":\"" + code + "\",\"message\":\"detail\"}}";
            foreach (var action in new Func<Task>[] {
                () => client.KvGetAsync("cache", "n", "k"), () => client.KvPutAsync("cache", "n", "k", []),
                () => client.KvDeleteAsync("cache", "n", "k"), () => client.KvListAsync("cache", "n") })
            {
                before = kvCalls.Count;
                if (status == 404 && code == "not_found")
                    await Fails<UnsupportedException>(action);
                else
                    await Fails<VLiteException>(action, e => Check(e.Status == status && e.Code == code && e.RequestId == "kv-error"));
                Check(kvCalls.Count == before + 1);
            }
        }
        kvStatus = 200;
        foreach (var invalid in new[] {
            """{"key":"k","value_base64":"/x==","version":"1","expires_at_ms":null}""",
            """{"key":"k","value_base64":"/w","version":"1","expires_at_ms":null}""",
            """{"key":"k","value_base64":"_w==","version":"1","expires_at_ms":null}""",
            """{"key":"k","value_base64":" /w==","version":"1","expires_at_ms":null}""",
            """{"key":"k","value_base64":"","version":"0","expires_at_ms":null}""",
            """{"key":"k","value_base64":"","version":1,"expires_at_ms":null}""",
            """{"key":"k","value_base64":"","version":"01","expires_at_ms":null}""",
            """{"key":"k","value_base64":"","version":"18446744073709551616","expires_at_ms":null}""",
            """{"key":"k","value_base64":"","version":"1","expires_at_ms":0}""" })
        {
            kvReply = invalid;
            await Fails<TransportException>(() => client.KvGetAsync("cache", "n", "k"));
        }
        kvReply = "{\"key\":\"k\",\"value_base64\":\"" + new string('A', 1396387) + "=\",\"version\":\"1\",\"expires_at_ms\":null}";
        Check((await client.KvGetAsync("cache", "n", "k")).Value.SequenceEqual(new byte[1047290]));
        foreach (var (response, action) in new (string, Func<Task>)[] {
            ("""{"version":"1"}""", () => client.KvPutAsync("cache", "n", "k", [])),
            ("""{"deleted":1}""", () => client.KvDeleteAsync("cache", "n", "k")),
            ("""{"items":[],"next_after":0}""", () => client.KvListAsync("cache", "n")),
            ("""{"items":{}}""", () => client.KvListAsync("cache", "n")),
            ("null", () => client.KvGetAsync("cache", "n", "k")) })
        {
            kvReply = response;
            await Fails<TransportException>(action);
        }
    }
}
await KvWireContractTests();
async Task KvCancellationTests()
{
    foreach (var timeout in new[] { false, true })
    {
        var admitted = 0;
        using var cancellation = new CancellationTokenSource();
        using var client = new Client("http://localhost", Token, timeout ? TimeSpan.FromMilliseconds(100) : null,
            new FakeHandler(async (_, token) =>
            {
                admitted++;
                if (!timeout) cancellation.Cancel();
                await Task.Delay(10000, token);
                return new HttpResponseMessage(HttpStatusCode.OK);
            }));
        await Fails<TransportException>(() => client.KvPutAsync("cache", "n", "k", [], cancellation: cancellation.Token));
        Check(admitted == 1);
    }
}
await KvCancellationTests();

if (Environment.GetEnvironmentVariable("VLITE_URL") is { Length: > 0 } url)
{
    using var client = new Client(url, Environment.GetEnvironmentVariable("VLITE_AUTH_TOKEN") ?? "");
    await client.HealthAsync(); await client.DatabaseAsync();
    const string id = "18446744073709551020";
    try
    {
        await client.PutAsync(id, new PutRequest([1, 0, 0]));
        Check((await client.GetAsync(id)).Id == id);
        await client.BatchAsync([new { op = "put", id, vector = new[] { 1, 0, 0 } }]);
        await client.SearchAsync(new { vector = new[] { 1, 0, 0 }, k = 1, mode = "exact" });
        await client.BrowseAsync(); await client.BuildIndexAsync();
        await client.SearchAsync(new { vector = new[] { 1, 0, 0 }, k = 1, mode = "approximate_ready" });
    }
    finally { await client.DeleteAsync(id); }
    var name = "dotnet_e2e_" + Guid.NewGuid().ToString("N");
    await client.CreateCollectionAsync(name, 3);
    var scoped = client.Collection(name);
    try
    {
        await scoped.PutAsync("1", new PutRequest([1, 0, 0], Metadata: new() { ["tag"] = new("string", "e2e") }));
        foreach (var mode in new[] { "exact", "approximate" })
        {
            var hits = await scoped.SearchAsync(new { vector = new[] { 1, 0, 0 }, k = 1, mode,
                filter = new[] { new { op = "eq", field = "tag", value = new { type = "string", value = "e2e" } } } });
            Check(hits.Length == 1 && hits[0].Id == "1");
        }
    }
    finally { await scoped.DeleteAsync("1"); await scoped.DropAsync(); }
    Console.WriteLine("Live e2e passed");
}
else Console.WriteLine("Live e2e skipped (VLITE_URL is unset)");
Console.WriteLine($"All {count} assertions passed");

sealed class FakeHandler(Func<HttpRequestMessage, CancellationToken, Task<HttpResponseMessage>> handle) : HttpMessageHandler
{
    protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken) => handle(request, cancellationToken);
}
