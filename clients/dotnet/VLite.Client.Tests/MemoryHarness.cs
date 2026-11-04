using System.Net;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;

/// <summary>One request seen by the fake server: everything the transport put on the wire.</summary>
sealed record Exchange(string Method, string Target, Dictionary<string, string> Headers, string? Body)
{
    public JsonElement Json() =>
        JsonDocument.Parse(Body ?? throw new InvalidOperationException("request had no body")).RootElement.Clone();
}

/// <summary>In-process HTTP server stand-in that records every call and replies with a canned response.</summary>
sealed class Server
{
    public const string Token = "abcdefghijklmnopqrstuvwxyz012345";
    public List<Exchange> Calls { get; } = [];
    public int Status { get; set; } = 200;
    public string Reply { get; set; } = "{}";
    public string? RequestId { get; set; }
    public Dictionary<string, string> ReplyHeaders { get; } = [];
    public Func<Exchange, CancellationToken, Task<HttpResponseMessage>>? Custom { get; set; }

    public Exchange Last => Calls[^1];

    public VLite.Client NewClient(TimeSpan? timeout = null) =>
        new("http://localhost", Token, timeout, new FakeHandler(Handle));

    async Task<HttpResponseMessage> Handle(HttpRequestMessage request, CancellationToken cancellation)
    {
        var headers = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        foreach (var header in request.Headers) headers[header.Key] = string.Join(",", header.Value);
        if (request.Content is not null)
            foreach (var header in request.Content.Headers) headers[header.Key] = string.Join(",", header.Value);
        var body = request.Content is null ? null : await request.Content.ReadAsStringAsync(cancellation);
        var exchange = new Exchange(request.Method.Method, request.RequestUri!.PathAndQuery, headers, body);
        lock (Calls) Calls.Add(exchange);
        if (Custom is not null) return await Custom(exchange, cancellation);
        var response = new HttpResponseMessage((HttpStatusCode)Status)
        { Content = new StringContent(Reply, Encoding.UTF8, "application/json") };
        if (RequestId is not null) response.Headers.Add("X-Request-ID", RequestId);
        foreach (var (name, value) in ReplyHeaders) response.Headers.Add(name, value);
        return response;
    }
}

/// <summary>Hand-verified wire fixtures (tests/fixtures/agent_memory/wire) and the contract's JSON exchanges.</summary>
static class Fx
{
    public static readonly string Root = FindRoot();
    public static readonly string[] Files =
        ["remember", "recall", "facts", "correct_facts", "consolidate", "forget", "receipt", "erasure", "resume_erasure"];
    static readonly Dictionary<string, JsonElement> loaded = [];

    static string FindRoot()
    {
        for (var directory = new DirectoryInfo(AppContext.BaseDirectory); directory is not null; directory = directory.Parent)
            if (Directory.Exists(Path.Combine(directory.FullName, "tests", "fixtures", "agent_memory", "wire")))
                return directory.FullName;
        throw new InvalidOperationException("tests/fixtures/agent_memory/wire not found above " + AppContext.BaseDirectory);
    }

    public static JsonElement File(string name)
    {
        if (!loaded.TryGetValue(name, out var root))
        {
            var path = Path.Combine(Root, "tests", "fixtures", "agent_memory", "wire", name + ".json");
            using var document = JsonDocument.Parse(System.IO.File.ReadAllText(path));
            loaded[name] = root = document.RootElement.Clone();
        }
        return root;
    }

    public static IEnumerable<JsonElement> Cases(string file) => File(file).GetProperty("cases").EnumerateArray();
    public static IEnumerable<JsonElement> Structural(string file) => File(file).GetProperty("structural").EnumerateArray();

    public static JsonElement Entry(string file, string name)
    {
        var found = Cases(file).Concat(Structural(file)).Where(entry => entry.GetProperty("name").GetString() == name).ToArray();
        return found.Length == 1 ? found[0] : throw new InvalidOperationException($"{file}:{name} matched {found.Length} fixtures");
    }

    /// <summary>The literal body of a structural or exchange fixture.</summary>
    public static string Value(string file, string name) => Entry(file, name).GetProperty("value").GetRawText();

    /// <summary>The expected response body of an execution case.</summary>
    public static string Expected(string file, string name) => Entry(file, name).GetProperty("expected").GetProperty("body").GetRawText();

    /// <summary>The request body of an execution case.</summary>
    public static string RequestBody(string file, string name) => Entry(file, name).GetProperty("request").GetProperty("body").GetRawText();

    /// <summary>The nine independent exchanges printed in section 8 of the serving contract.</summary>
    public static JsonElement[] ContractExchanges()
    {
        var text = System.IO.File.ReadAllText(Path.Combine(Root, "docs", "projects", "roadmap", "serving-memory-contract.md"));
        var start = text.IndexOf("```jsonl", StringComparison.Ordinal);
        var end = text.IndexOf("```", start + 8, StringComparison.Ordinal);
        if (start < 0 || end < 0) throw new InvalidOperationException("section 8 JSON exchanges not found");
        return text[(start + 8)..end].Split('\n', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
            .Select(line => JsonDocument.Parse(line).RootElement.Clone()).ToArray();
    }
}

/// <summary>JSON comparison and editing helpers for the tests; they know nothing about the client.</summary>
static class Js
{
    public static JsonElement Parse(string json)
    {
        using var document = JsonDocument.Parse(json);
        return document.RootElement.Clone();
    }

    public static bool Equivalent(string expected, string actual, out string difference) =>
        Compare(Parse(expected), Parse(actual), "$", out difference);

    public static bool Equivalent(JsonElement expected, JsonElement actual, out string difference) =>
        Compare(expected, actual, "$", out difference);

    static string Brief(JsonElement element)
    {
        var text = element.GetRawText();
        return text.Length > 80 ? text[..80] + "..." : text;
    }

    static bool Compare(JsonElement expected, JsonElement actual, string path, out string difference)
    {
        difference = "";
        if (expected.ValueKind != actual.ValueKind)
        {
            difference = $"{path}: expected {expected.ValueKind} {Brief(expected)} but got {actual.ValueKind} {Brief(actual)}";
            return false;
        }
        switch (expected.ValueKind)
        {
            case JsonValueKind.Object:
                var left = expected.EnumerateObject().ToDictionary(p => p.Name, p => p.Value);
                var right = actual.EnumerateObject().ToDictionary(p => p.Name, p => p.Value);
                foreach (var name in left.Keys.Union(right.Keys).Order(StringComparer.Ordinal))
                {
                    if (!right.TryGetValue(name, out var rightValue)) { difference = $"{path}/{name}: member missing from actual"; return false; }
                    if (!left.TryGetValue(name, out var leftValue)) { difference = $"{path}/{name}: unexpected member in actual"; return false; }
                    if (!Compare(leftValue, rightValue, path + "/" + name, out difference)) return false;
                }
                return true;
            case JsonValueKind.Array:
                var a = expected.EnumerateArray().ToArray();
                var b = actual.EnumerateArray().ToArray();
                if (a.Length != b.Length) { difference = $"{path}: expected {a.Length} elements but got {b.Length}"; return false; }
                for (var i = 0; i < a.Length; i++)
                    if (!Compare(a[i], b[i], $"{path}[{i}]", out difference)) return false;
                return true;
            case JsonValueKind.String:
                if (expected.GetString() == actual.GetString()) return true;
                difference = $"{path}: expected {Brief(expected)} but got {Brief(actual)}";
                return false;
            case JsonValueKind.Number:
                var same = expected.TryGetDecimal(out var x) && actual.TryGetDecimal(out var y) ? x == y : expected.GetDouble() == actual.GetDouble();
                if (same) return true;
                difference = $"{path}: expected {Brief(expected)} but got {Brief(actual)}";
                return false;
            default:
                return true;
        }
    }

    /// <summary>
    /// Section 3 input rule, written independently: a request may carry an exact JSON integer for a
    /// u64/i64 member, and the client must put the canonical decimal string on the wire instead.
    /// </summary>
    public static string WithIntegerInputsAsStrings(string json)
    {
        string[] integerMembers =
            ["observed_at_ms", "expires_at_ms", "as_of_ms", "at_ms", "valid_from_ms", "valid_to_ms", "revision", "generation", "slot_revision", "record"];
        void Walk(JsonNode? node)
        {
            switch (node)
            {
                case JsonObject obj:
                    foreach (var (name, child) in obj.ToArray())
                    {
                        if (child is JsonValue value && value.GetValueKind() == JsonValueKind.Number &&
                            (integerMembers.Contains(name) || (name == "value" && obj["type"]?.GetValue<string>() == "integer")))
                            obj[name] = value.ToJsonString();
                        else Walk(child);
                    }
                    break;
                case JsonArray array:
                    foreach (var child in array) Walk(child);
                    break;
            }
        }
        var root = JsonNode.Parse(json);
        Walk(root);
        return root!.ToJsonString();
    }

    /// <summary>Expands the fixtures' input-only compression markers. Returns null when a marker is not expandable.</summary>
    public static string? Expand(string json)
    {
        var failed = false;
        JsonNode? Walk(JsonNode? node)
        {
            switch (node)
            {
                case JsonObject obj when obj.ContainsKey("$text") && obj.ContainsKey("count"):
                    return new string(obj["$text"]!.GetValue<string>()[0], obj["count"]!.GetValue<int>());
                case JsonObject obj when obj.ContainsKey("$repeat"):
                    var template = obj["$repeat"]!;
                    var count = obj["count"]!.GetValue<int>();
                    var enumerate = obj["enumerate"]?.AsArray().Select(n => n!.GetValue<string>()).ToArray() ?? [];
                    var list = new JsonArray();
                    for (var i = 0; i < count; i++)
                    {
                        var copy = Walk(template.DeepClone())!;
                        foreach (var key in enumerate) copy[key] = copy[key]!.GetValue<string>() + i;
                        list.Add(copy);
                    }
                    return list;
                case JsonObject obj when obj.Any(p => p.Key.StartsWith('$')):
                    failed = true;
                    return node;
                case JsonObject obj:
                    foreach (var (name, child) in obj.ToArray())
                    {
                        var replaced = Walk(child);
                        if (!ReferenceEquals(replaced, child)) obj[name] = replaced;
                    }
                    return obj;
                case JsonArray array:
                    for (var i = 0; i < array.Count; i++)
                    {
                        var child = array[i];
                        var replaced = Walk(child);
                        if (!ReferenceEquals(replaced, child)) array[i] = replaced;
                    }
                    return array;
                default:
                    return node;
            }
        }
        var result = Walk(JsonNode.Parse(json));
        return failed ? null : result!.ToJsonString();
    }

    static JsonNode Navigate(JsonNode root, string[] steps)
    {
        var node = root;
        foreach (var step in steps)
            node = node is JsonArray array ? array[int.Parse(step)]! : node![step]!;
        return node;
    }

    /// <summary>Removes the member at a slash path such as meta/scopes/0/tenant.</summary>
    public static string Remove(string json, string path)
    {
        var root = JsonNode.Parse(json)!;
        var steps = path.Split('/');
        var parent = Navigate(root, steps[..^1]);
        if (parent is JsonObject obj) { if (!obj.Remove(steps[^1])) throw new InvalidOperationException("no member " + path); }
        else throw new InvalidOperationException("not an object parent: " + path);
        return root.ToJsonString();
    }

    /// <summary>Replaces (or adds) the member at a slash path with a JSON value.</summary>
    public static string Set(string json, string path, string valueJson)
    {
        var root = JsonNode.Parse(json)!;
        var steps = path.Split('/');
        var parent = Navigate(root, steps[..^1]);
        var value = JsonNode.Parse(valueJson);
        if (parent is JsonObject obj) obj[steps[^1]] = value;
        else if (parent is JsonArray array) array[int.Parse(steps[^1])] = value;
        return root.ToJsonString();
    }

    public static string Get(string json, string path) => Navigate(JsonNode.Parse(json)!, path.Split('/')).ToJsonString();

    /// <summary>
    /// Adds an unknown member to every object, as a future server might. The values of a "metadata" member
    /// are a map whose keys are data, so those objects only receive additions in their nested scalar objects.
    /// </summary>
    public static string AddUnknownMembers(string json)
    {
        void Walk(JsonNode? node, bool isMap)
        {
            switch (node)
            {
                case JsonObject obj:
                    foreach (var (name, child) in obj.ToArray()) Walk(child, name == "metadata");
                    if (!isMap) obj["x_future"] = new JsonObject { ["nested"] = new JsonArray(1, "two", null) };
                    break;
                case JsonArray array:
                    foreach (var child in array) Walk(child, false);
                    break;
            }
        }
        var root = JsonNode.Parse(json);
        Walk(root, false);
        return root!.ToJsonString();
    }
}
