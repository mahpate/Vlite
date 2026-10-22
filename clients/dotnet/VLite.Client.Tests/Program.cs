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
