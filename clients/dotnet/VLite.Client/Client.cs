using System.Net;
using System.Globalization;
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.RegularExpressions;

namespace VLite;

public sealed class VLiteException(int status, string code, string message, string? requestId, MemoryErrorContext? details) : Exception(message)
{
    public VLiteException(int status, string code, string message, string? requestId) : this(status, code, message, requestId, null) { }
    public int Status { get; } = status;
    public string Code { get; } = code;
    public string? RequestId { get; } = requestId;
    /// <summary>Structured <c>error.details</c> when the server sent any, e.g. <c>Feature</c> of a capability refusal.</summary>
    public MemoryErrorContext? Details { get; } = details;
}

/// <summary>The mutation outcome may be unknown; no retry was performed.</summary>
public sealed class TransportException(Exception cause) : Exception(
    "Request failed or response was invalid; mutation outcome may be unknown; no retry performed", cause)
{
    public string Code => "transport_error";
}

public sealed class UnsupportedException : Exception
{
    public UnsupportedException() : base("Server does not support KV routes; no fallback performed") { }
    internal UnsupportedException(string message) : base(message) { }
}

public abstract record Expected
{
    private Expected() { }
    public sealed record Any : Expected;
    public sealed record Absent : Expected;
    public sealed record Revision([property: JsonPropertyName("revision")] ulong Value) : Expected;
}

public sealed record KvEntry(
    [property: JsonPropertyName("key")] string Key,
    [property: JsonPropertyName("value")] byte[] Value,
    [property: JsonPropertyName("version"), JsonNumberHandling(JsonNumberHandling.AllowReadingFromString | JsonNumberHandling.WriteAsString)] ulong Version,
    [property: JsonPropertyName("expires_at_ms"), JsonNumberHandling(JsonNumberHandling.AllowReadingFromString | JsonNumberHandling.WriteAsString)] ulong? ExpiresAtMs);
public sealed record KvPutResult(
    [property: JsonPropertyName("version"), JsonNumberHandling(JsonNumberHandling.AllowReadingFromString | JsonNumberHandling.WriteAsString)] ulong Version,
    [property: JsonPropertyName("expires_at_ms"), JsonNumberHandling(JsonNumberHandling.AllowReadingFromString | JsonNumberHandling.WriteAsString)] ulong? ExpiresAtMs);
public sealed record KvPage(
    [property: JsonPropertyName("items")] KvEntry[] Items,
    [property: JsonPropertyName("next_after")] string? NextAfter);

public sealed record MetadataValue(string Type, object Value);
public sealed record PutRequest(float[] Vector, string PayloadBase64 = "", Dictionary<string, MetadataValue>? Metadata = null, ulong? ExpiresAtMs = null, ulong? TtlMs = null);
public sealed record VectorRecord(string Id, float[] Vector, string PayloadBase64, Dictionary<string, MetadataValue> Metadata, string? Revision = null);
public sealed record Hit(string Id, double Distance, double? Score = null);
public sealed record Summary(string Id, Dictionary<string, MetadataValue> Metadata);
public sealed record Page(Summary[] Items, string? NextAfter);

/// <summary>HTTP(S) origin client. No automatic redirects, proxies, retries or search fallbacks.</summary>
public sealed class Client : Records, IDisposable
{
    private readonly HttpClient http;
    internal static readonly JsonSerializerOptions Json = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull
    };

    public Client(string origin, string token, TimeSpan? timeout = null, HttpMessageHandler? handler = null) : base("/v1")
    {
        if (!Uri.TryCreate(origin, UriKind.Absolute, out var uri) ||
            (uri.Scheme != "http" && uri.Scheme != "https") || string.IsNullOrEmpty(uri.Host) ||
            uri.UserInfo.Length != 0 || uri.AbsolutePath != "/" || uri.Query.Length != 0 || uri.Fragment.Length != 0)
            throw new ArgumentException("origin must be an HTTP(S) origin without credentials, path, query or fragment", nameof(origin));
        if (!Regex.IsMatch(token, @"\A[A-Za-z0-9_-]{32,256}\z"))
            throw new ArgumentException("token must contain 32..256 base64url characters", nameof(token));
        var duration = timeout ?? TimeSpan.FromSeconds(30);
        if (duration <= TimeSpan.Zero || duration.TotalMilliseconds > int.MaxValue)
            throw new ArgumentOutOfRangeException(nameof(timeout));
        http = new HttpClient(handler ?? new HttpClientHandler { AllowAutoRedirect = false, UseProxy = false })
        { BaseAddress = uri, Timeout = duration };
        http.DefaultRequestHeaders.Authorization = new AuthenticationHeaderValue("Bearer", token);
        http.DefaultRequestHeaders.Accept.Add(new MediaTypeWithQualityHeaderValue("application/json"));
        Transport = this;
    }

    internal async Task<JsonElement> RequestAsync(string method, string path, object? body, CancellationToken cancellation)
    {
        using var request = new HttpRequestMessage(new HttpMethod(method), path);
        if (body is not null)
            request.Content = new StringContent(JsonSerializer.Serialize(body, Json), Encoding.UTF8, "application/json");
        try
        {
            using var response = await http.SendAsync(request, cancellation).ConfigureAwait(false);
            if (!response.IsSuccessStatusCode)
            {
                var code = "http_error";
                var message = "Server returned a nonstandard error response";
                MemoryErrorContext? details = null;
                try
                {
                    using var doc = JsonDocument.Parse(await response.Content.ReadAsStringAsync(cancellation).ConfigureAwait(false));
                    var detail = doc.RootElement.GetProperty("error");
                    var parsedCode = detail.GetProperty("code").GetString();
                    var parsedMessage = detail.GetProperty("message").GetString();
                    if (!string.IsNullOrEmpty(parsedCode) && parsedMessage is not null)
                    { code = parsedCode; message = parsedMessage; details = MemoryWire.ReadErrorContext(detail); }
                }
                catch (Exception e) when (e is JsonException or KeyNotFoundException or InvalidOperationException) { }
                var id = response.Headers.TryGetValues("X-Request-ID", out var values) ? values.FirstOrDefault() : null;
                throw new VLiteException((int)response.StatusCode, code, message, id, details);
            }
            if (response.StatusCode == HttpStatusCode.NoContent) return default;
            using var document = JsonDocument.Parse(await response.Content.ReadAsStringAsync(cancellation).ConfigureAwait(false));
            return document.RootElement.Clone();
        }
        catch (Exception e) when (e is HttpRequestException or OperationCanceledException or JsonException or IOException)
        { throw new TransportException(e); }
    }

    internal static T Decode<T>(JsonElement element)
    {
        try { return element.Deserialize<T>(Json) ?? throw new JsonException("Missing response"); }
        catch (Exception e) when (e is JsonException or InvalidOperationException) { throw new TransportException(e); }
    }

    public Task<JsonElement> HealthAsync(CancellationToken cancellation = default) => RequestAsync("GET", "/health", null, cancellation);
    public Task<JsonElement> ReadyAsync(CancellationToken cancellation = default) => RequestAsync("GET", "/ready", null, cancellation);
    public Task<JsonElement> DatabaseAsync(CancellationToken cancellation = default) => RequestAsync("GET", "/v1/database", null, cancellation);
    private static string KvText(string value, string name, int maximum, int minimum = 1)
    {
        ArgumentNullException.ThrowIfNull(value, name);
        var size = new UTF8Encoding(false, true).GetByteCount(value);
        if (size < minimum || size > maximum)
            throw new ArgumentException($"{name} must be {minimum}..{maximum} UTF-8 bytes", name);
        return value;
    }
    private static string KvPath(string collection)
    {
        ValidateName(collection);
        return "/v1/collections/" + collection + "/kv";
    }
    private static string KvDecimal(ulong value) => value.ToString(CultureInfo.InvariantCulture);
    private static object KvExpected(Expected? expected) => expected switch
    {
        null or Expected.Any => "any",
        Expected.Absent => "absent",
        Expected.Revision revision => new Dictionary<string, object> { ["revision"] = KvDecimal(revision.Value) },
        _ => throw new ArgumentException("Invalid expected variant", nameof(expected))
    };
    private static string KvQuery(string name, string value) => name + "=" + Uri.EscapeDataString(value);
    private async Task<JsonElement> KvRequestAsync(string method, string path, object? body, CancellationToken cancellation)
    {
        try { return await RequestAsync(method, path, body, cancellation).ConfigureAwait(false); }
        catch (VLiteException error) when (error.Status == 404 && error.Code == "not_found")
        { throw new UnsupportedException(); }
    }
    private static T KvDecode<T>(JsonElement value, Func<JsonElement, T> decode)
    {
        try { return decode(value); }
        catch (Exception error) when (error is JsonException or InvalidOperationException or KeyNotFoundException or FormatException or ArgumentException)
        { throw new TransportException(error); }
    }
    private static ulong KvReadDecimal(JsonElement value, bool nonzero = false)
    {
        var text = value.GetString();
        if (text is null || !Regex.IsMatch(text, @"\A(0|[1-9][0-9]{0,19})\z") ||
            !ulong.TryParse(text, NumberStyles.None, CultureInfo.InvariantCulture, out var result) || (nonzero && result == 0))
            throw new JsonException("Invalid canonical decimal u64");
        return result;
    }
    private static KvPutResult KvReadPut(JsonElement value)
    {
        var expiry = value.GetProperty("expires_at_ms");
        return new(KvReadDecimal(value.GetProperty("version"), true),
            expiry.ValueKind == JsonValueKind.Null ? null : KvReadDecimal(expiry));
    }
    private static KvEntry KvReadEntry(JsonElement value)
    {
        var encoded = value.GetProperty("value_base64").GetString();
        if (encoded is null || encoded.Length > 1396388) throw new JsonException("Invalid KV base64");
        var decoded = Convert.FromBase64String(encoded);
        if (decoded.Length > 1047290 || Convert.ToBase64String(decoded) != encoded)
            throw new JsonException("Noncanonical KV base64");
        var result = KvReadPut(value);
        return new(KvText(value.GetProperty("key").GetString()!, "key", 1024), decoded, result.Version, result.ExpiresAtMs);
    }
    public async Task<KvEntry> KvGetAsync(string collection, string @namespace, string key, CancellationToken cancellation = default)
    {
        var path = KvPath(collection) + "/entry?" + KvQuery("namespace", KvText(@namespace, "namespace", 255)) +
            "&" + KvQuery("key", KvText(key, "key", 1024));
        return KvDecode(await KvRequestAsync("GET", path, null, cancellation).ConfigureAwait(false), KvReadEntry);
    }
    public async Task<KvPutResult> KvPutAsync(string collection, string @namespace, string key, byte[] value,
        Expected? expected = null, ulong? expiresAtMs = null, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(value);
        if (value.Length > 1047290) throw new ArgumentException("value must be at most 1047290 bytes", nameof(value));
        var path = KvPath(collection) + "/entry";
        var body = new Dictionary<string, object?>
        {
            ["namespace"] = KvText(@namespace, "namespace", 255), ["key"] = KvText(key, "key", 1024),
            ["value_base64"] = Convert.ToBase64String(value), ["expected"] = KvExpected(expected),
            ["expires_at_ms"] = expiresAtMs is null ? null : KvDecimal(expiresAtMs.Value)
        };
        return KvDecode(await KvRequestAsync("PUT", path, body, cancellation).ConfigureAwait(false), KvReadPut);
    }
    public async Task<bool> KvDeleteAsync(string collection, string @namespace, string key,
        Expected? expected = null, CancellationToken cancellation = default)
    {
        var condition = expected is Expected.Revision revision ? KvDecimal(revision.Value) : (string)KvExpected(expected);
        var path = KvPath(collection) + "/entry?" + KvQuery("namespace", KvText(@namespace, "namespace", 255)) +
            "&" + KvQuery("key", KvText(key, "key", 1024)) + "&" + KvQuery("expected", condition);
        return KvDecode(await KvRequestAsync("DELETE", path, null, cancellation).ConfigureAwait(false),
            value => value.GetProperty("deleted").GetBoolean());
    }
    public async Task<KvPage> KvListAsync(string collection, string @namespace, string prefix = "", string? after = null,
        int limit = 100, CancellationToken cancellation = default)
    {
        if (limit is < 1 or > 10000) throw new ArgumentOutOfRangeException(nameof(limit));
        var path = KvPath(collection) + "/entries?" + KvQuery("namespace", KvText(@namespace, "namespace", 255)) +
            "&" + KvQuery("prefix", KvText(prefix, "prefix", 1024, 0)) +
            (after is null ? "" : "&" + KvQuery("after", KvText(after, "after", 1024))) +
            "&limit=" + limit.ToString(CultureInfo.InvariantCulture);
        return KvDecode(await KvRequestAsync("GET", path, null, cancellation).ConfigureAwait(false), value =>
        {
            var next = value.GetProperty("next_after");
            return new KvPage(value.GetProperty("items").EnumerateArray().Select(KvReadEntry).ToArray(),
                next.ValueKind == JsonValueKind.Null ? null : KvText(next.GetString()!, "next_after", 1024));
        });
    }

    public Task<JsonElement> ChangesAsync(string? cursor = null, int limit = 1000, CancellationToken cancellation = default)
    {
        if (limit is < 1 or > 10000) throw new ArgumentOutOfRangeException(nameof(limit));
        var query = "?limit=" + limit + (cursor is null ? "" : "&cursor=" + Uri.EscapeDataString(cursor));
        return RequestAsync("GET", "/v1/changes" + query, null, cancellation);
    }
    public Task<JsonElement> ListWebhooksAsync(CancellationToken cancellation = default) => RequestAsync("GET", "/v1/admin/webhooks", null, cancellation);
    public Task<JsonElement> CreateWebhookAsync(object request, CancellationToken cancellation = default) => RequestAsync("POST", "/v1/admin/webhooks", request, cancellation);
    public Task<JsonElement> DeleteWebhookAsync(string id, CancellationToken cancellation = default)
    {
        if (!Regex.IsMatch(id, @"\A[a-f0-9]{24}\z")) throw new ArgumentException("Invalid webhook ID", nameof(id));
        return RequestAsync("DELETE", "/v1/admin/webhooks/" + id, null, cancellation);
    }
    public Task<JsonElement> WebhookStatusAsync(string id, CancellationToken cancellation = default)
    {
        if (!Regex.IsMatch(id, @"\A[a-f0-9]{24}\z")) throw new ArgumentException("Invalid webhook ID", nameof(id));
        return RequestAsync("GET", "/v1/admin/webhooks/" + id, null, cancellation);
    }
    public Task<JsonElement> ListCollectionsAsync(CancellationToken cancellation = default) => RequestAsync("GET", "/v1/collections", null, cancellation);
    public Task<JsonElement> CreateCollectionAsync(string name, int dimensions, object? settings = null, CancellationToken cancellation = default)
    {
        ValidateName(name);
        if (dimensions is < 1 or > 65536) throw new ArgumentOutOfRangeException(nameof(dimensions));
        return RequestAsync("POST", "/v1/collections", new { name, dimensions, settings }, cancellation);
    }
    public Records Collection(string name) { ValidateName(name); return new Records("/v1/collections/" + name) { Transport = this }; }
    /// <summary>Agent-memory operations of one provisioned binding; see <see cref="MemoryClient"/>.</summary>
    public MemoryClient Memory(string binding) => new(this, binding);
    private static void ValidateName(string name)
    {
        if (!Regex.IsMatch(name, @"\A[a-z][a-z0-9_-]{0,62}\z")) throw new ArgumentException("Invalid collection name", nameof(name));
    }
    public void Dispose() => http.Dispose();
}

/// <summary>Record APIs scoped to the legacy default collection or a named collection.</summary>
public class Records
{
    private readonly string prefix;
    internal Client Transport { get; set; } = null!;
    internal Records(string prefix) { this.prefix = prefix; }
    private static string Id(string id)
    {
        if (!Regex.IsMatch(id, @"\A(0|[1-9][0-9]{0,19})\z") || !ulong.TryParse(id, out _))
            throw new ArgumentException("ID must be a canonical decimal u64 string", nameof(id));
        return id;
    }
    public async Task PutAsync(string id, PutRequest record, CancellationToken cancellation = default) =>
        _ = await Transport.RequestAsync("PUT", prefix + "/vectors/" + Id(id), record, cancellation).ConfigureAwait(false);
    public async Task<VectorRecord> GetAsync(string id, CancellationToken cancellation = default) =>
        Client.Decode<VectorRecord>(await Transport.RequestAsync("GET", prefix + "/vectors/" + Id(id), null, cancellation).ConfigureAwait(false));
    public async Task<bool> DeleteAsync(string id, CancellationToken cancellation = default) =>
        (await Transport.RequestAsync("DELETE", prefix + "/vectors/" + Id(id), null, cancellation).ConfigureAwait(false)).GetProperty("deleted").GetBoolean();
    public async Task BatchAsync(IEnumerable<object> operations, CancellationToken cancellation = default)
    {
        var rows = operations.ToArray();
        foreach (var row in rows) Id(JsonSerializer.SerializeToElement(row, Client.Json).GetProperty("id").GetString()!);
        _ = await Transport.RequestAsync("POST", prefix + "/batches", new { operations = rows }, cancellation).ConfigureAwait(false);
    }
    /// <summary>Pass mode, metric, filter and ANN options using REST field names. Never falls back to exact.</summary>
    public Task<JsonElement> SearchDetailedAsync(object request, CancellationToken cancellation = default) =>
        Transport.RequestAsync("POST", prefix + "/search", request, cancellation);
    public async Task<Hit[]> SearchAsync(object request, CancellationToken cancellation = default) =>
        Client.Decode<Hit[]>((await SearchDetailedAsync(request, cancellation).ConfigureAwait(false)).GetProperty("hits"));
    public async Task<Page> BrowseAsync(int limit = 25, string? after = null, CancellationToken cancellation = default)
    {
        if (limit is < 1 or > 100) throw new ArgumentOutOfRangeException(nameof(limit));
        var query = "?limit=" + limit + (after is null ? "" : "&after=" + Id(after));
        return Client.Decode<Page>(await Transport.RequestAsync("GET", prefix + "/vectors" + query, null, cancellation).ConfigureAwait(false));
    }
    public Task<JsonElement> InfoAsync(CancellationToken cancellation = default) =>
        Transport.RequestAsync("GET", prefix == "/v1" ? "/v1/database" : prefix, null, cancellation);
    public async Task<bool> DropAsync(CancellationToken cancellation = default)
    {
        if (prefix == "/v1") throw new InvalidOperationException("Select a collection before dropping it");
        return (await Transport.RequestAsync("DELETE", prefix, null, cancellation).ConfigureAwait(false)).GetProperty("deleted").GetBoolean();
    }
    public Task<JsonElement> SetSettingsAsync(object settings, CancellationToken cancellation = default)
    {
        if (prefix == "/v1") throw new InvalidOperationException("Select a collection before updating settings");
        return Transport.RequestAsync("PUT", prefix + "/settings", settings, cancellation);
    }
    public Task<JsonElement> UpdateMetadataAsync(string id, Dictionary<string, MetadataValue> metadata, CancellationToken cancellation = default) =>
        Transport.RequestAsync("PATCH", prefix + "/vectors/" + Id(id) + "/metadata", new { metadata }, cancellation);
    public Task<JsonElement> GroupedSearchAsync(object request, CancellationToken cancellation = default) =>
        Transport.RequestAsync("POST", (prefix == "/v1" ? "/v1/collections/default" : prefix) + "/search/grouped", request, cancellation);
    public Task<JsonElement> RecommendAsync(object request, CancellationToken cancellation = default) =>
        Transport.RequestAsync("POST", (prefix == "/v1" ? "/v1/collections/default" : prefix) + "/recommend", request, cancellation);
    public Task<JsonElement> IndexStatusAsync(CancellationToken cancellation = default) => Transport.RequestAsync("GET", prefix + "/index/hnsw", null, cancellation);
    public async Task BuildIndexAsync(object? options = null, CancellationToken cancellation = default) =>
        _ = await Transport.RequestAsync("POST", prefix + "/index/hnsw", options ?? new { }, cancellation).ConfigureAwait(false);
}
