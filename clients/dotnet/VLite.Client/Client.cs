using System.Net;
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.RegularExpressions;

namespace VLite;

public sealed class VLiteException(int status, string code, string message, string? requestId) : Exception(message)
{
    public int Status { get; } = status;
    public string Code { get; } = code;
    public string? RequestId { get; } = requestId;
}

/// <summary>The mutation outcome may be unknown; no retry was performed.</summary>
public sealed class TransportException(Exception cause) : Exception(
    "Request failed or response was invalid; mutation outcome may be unknown; no retry performed", cause)
{
    public string Code => "transport_error";
}

public sealed record MetadataValue(string Type, object Value);
public sealed record PutRequest(float[] Vector, string PayloadBase64 = "", Dictionary<string, MetadataValue>? Metadata = null);
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
                try
                {
                    using var doc = JsonDocument.Parse(await response.Content.ReadAsStringAsync(cancellation).ConfigureAwait(false));
                    var detail = doc.RootElement.GetProperty("error");
                    var parsedCode = detail.GetProperty("code").GetString();
                    var parsedMessage = detail.GetProperty("message").GetString();
                    if (!string.IsNullOrEmpty(parsedCode) && parsedMessage is not null)
                    { code = parsedCode; message = parsedMessage; }
                }
                catch (Exception e) when (e is JsonException or KeyNotFoundException or InvalidOperationException) { }
                var id = response.Headers.TryGetValues("X-Request-ID", out var values) ? values.FirstOrDefault() : null;
                throw new VLiteException((int)response.StatusCode, code, message, id);
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
    public Task<JsonElement> ListCollectionsAsync(CancellationToken cancellation = default) => RequestAsync("GET", "/v1/collections", null, cancellation);
    public Task<JsonElement> CreateCollectionAsync(string name, int dimensions, object? settings = null, CancellationToken cancellation = default)
    {
        ValidateName(name);
        if (dimensions is < 1 or > 65536) throw new ArgumentOutOfRangeException(nameof(dimensions));
        return RequestAsync("POST", "/v1/collections", new { name, dimensions, settings }, cancellation);
    }
    public Records Collection(string name) { ValidateName(name); return new Records("/v1/collections/" + name) { Transport = this }; }
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
    public Task<JsonElement> IndexStatusAsync(CancellationToken cancellation = default) => Transport.RequestAsync("GET", prefix + "/index/hnsw", null, cancellation);
    public async Task BuildIndexAsync(object? options = null, CancellationToken cancellation = default) =>
        _ = await Transport.RequestAsync("POST", prefix + "/index/hnsw", options ?? new { }, cancellation).ConfigureAwait(false);
}
