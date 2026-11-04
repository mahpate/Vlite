using System.Globalization;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace VLite;

/// <summary>
/// Checked conversions between integers and the canonical decimal strings of MemoryU64 (0..18446744073709551615)
/// and MemoryI64 (-9223372036854775808..9223372036854775807): no sign for u64, no leading zero, no "-0".
/// </summary>
public static class MemoryNumber
{
    public static string FormatU64(ulong value) => value.ToString(CultureInfo.InvariantCulture);
    public static string FormatI64(long value) => value.ToString(CultureInfo.InvariantCulture);

    public static bool TryParseU64(string? text, out ulong value)
    {
        value = 0;
        return IsCanonical(text, signed: false) && ulong.TryParse(text, NumberStyles.None, CultureInfo.InvariantCulture, out value);
    }

    public static bool TryParseI64(string? text, out long value)
    {
        value = 0;
        return IsCanonical(text, signed: true) && long.TryParse(text, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out value);
    }

    /// <exception cref="FormatException">Not a canonical decimal in range.</exception>
    public static ulong ParseU64(string text) =>
        TryParseU64(text, out var value) ? value : throw new FormatException("Not a canonical decimal u64: \"" + text + "\"");

    /// <exception cref="FormatException">Not a canonical decimal in range.</exception>
    public static long ParseI64(string text) =>
        TryParseI64(text, out var value) ? value : throw new FormatException("Not a canonical decimal i64: \"" + text + "\"");

    private static bool IsCanonical(string? text, bool signed)
    {
        if (string.IsNullOrEmpty(text)) return false;
        var start = signed && text[0] == '-' ? 1 : 0;
        if (start == text.Length) return false;
        if (text[start] == '0') return text.Length == 1;
        for (var i = start; i < text.Length; i++)
            if (text[i] is < '0' or > '9') return false;
        return true;
    }
}

/// <summary>Serialization options, strict response readers and the request/response converters of the memory client.</summary>
internal static class MemoryWire
{
    internal static readonly JsonSerializerOptions Options = new() { DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull };

    /// <summary>Serializes a request. Anything that cannot be written exactly is refused before a request is made.</summary>
    internal static JsonElement ToElement(object request)
    {
        try { return JsonSerializer.SerializeToElement(request, request.GetType(), Options); }
        catch (Exception e) when (e is JsonException or ArgumentException or InvalidOperationException or NotSupportedException
                                      or FormatException or OverflowException)
        { throw new ArgumentException("Invalid memory request: " + e.Message, "request", e); }
    }

    /// <summary>Reads a response; a missing or malformed member makes the whole response untrustworthy.</summary>
    internal static T Read<T>(JsonElement element, Func<JsonElement, T> read)
    {
        try { return read(element); }
        catch (Exception e) when (e is JsonException or InvalidOperationException or KeyNotFoundException or FormatException
                                      or ArgumentException or OverflowException)
        { throw new TransportException(e); }
    }

    internal static string CheckU64(string? text, string what) =>
        MemoryNumber.TryParseU64(text, out _) ? text! : throw new JsonException(what + " must be a canonical decimal u64 string, not " + Quote(text));

    internal static string CheckI64(string? text, string what) =>
        MemoryNumber.TryParseI64(text, out _) ? text! : throw new JsonException(what + " must be a canonical decimal i64 string, not " + Quote(text));

    private static string Quote(string? text) => text is null ? "null" : "\"" + text + "\"";

    // ------------------------------------------------------------------------------------ element helpers
    private static JsonException Bad(string message) => new(message);

    private static JsonElement Obj(JsonElement element) =>
        element.ValueKind == JsonValueKind.Object ? element : throw Bad("expected a JSON object but found " + element.ValueKind);

    private static JsonElement Req(JsonElement obj, string name) =>
        obj.TryGetProperty(name, out var value) ? value : throw Bad("missing required member \"" + name + "\"");

    // An optional member that is absent or null is simply not set.
    private static bool TryOpt(JsonElement obj, string name, out JsonElement value) =>
        obj.TryGetProperty(name, out value) && value.ValueKind != JsonValueKind.Null;

    private static string Str(JsonElement e) =>
        e.ValueKind == JsonValueKind.String ? e.GetString()! : throw Bad("expected a string but found " + e.ValueKind);

    private static bool Bool(JsonElement e) => e.ValueKind switch
    {
        JsonValueKind.True => true,
        JsonValueKind.False => false,
        _ => throw Bad("expected a boolean but found " + e.ValueKind)
    };

    private static double Num(JsonElement e) =>
        e.ValueKind == JsonValueKind.Number && e.TryGetDouble(out var value) && double.IsFinite(value)
            ? value : throw Bad("expected a finite number but found " + e.ValueKind);

    private static int Int(JsonElement e) =>
        e.ValueKind == JsonValueKind.Number && e.TryGetInt32(out var value) ? value : throw Bad("expected an integer but found " + e.ValueKind);

    // Responses carry canonical decimal strings only. Request-side reading (lenient) also takes an exact JSON integer.
    private static string U64(JsonElement e, bool lenient = false)
    {
        if (e.ValueKind == JsonValueKind.String) return CheckU64(e.GetString(), "u64");
        if (lenient && e.ValueKind == JsonValueKind.Number && e.TryGetUInt64(out var number)) return MemoryNumber.FormatU64(number);
        throw Bad("expected a canonical decimal u64 string but found " + e.ValueKind);
    }

    private static string I64(JsonElement e, bool lenient = false)
    {
        if (e.ValueKind == JsonValueKind.String) return CheckI64(e.GetString(), "i64");
        if (lenient && e.ValueKind == JsonValueKind.Number && e.TryGetInt64(out var number)) return MemoryNumber.FormatI64(number);
        throw Bad("expected a canonical decimal i64 string but found " + e.ValueKind);
    }

    private static string? NullableU64(JsonElement e) => e.ValueKind == JsonValueKind.Null ? null : U64(e);

    private static string? NullableI64(JsonElement e, bool lenient) => e.ValueKind == JsonValueKind.Null ? null : I64(e, lenient);

    private static T[] Arr<T>(JsonElement e, Func<JsonElement, T> read)
    {
        if (e.ValueKind != JsonValueKind.Array) throw Bad("expected an array but found " + e.ValueKind);
        var result = new T[e.GetArrayLength()];
        var i = 0;
        foreach (var item in e.EnumerateArray()) result[i++] = read(item);
        return result;
    }

    private static string? OptStr(JsonElement obj, string name) => TryOpt(obj, name, out var value) ? Str(value) : null;

    private static T? OptRef<T>(JsonElement obj, string name, Func<JsonElement, T> read) where T : class =>
        TryOpt(obj, name, out var value) ? read(value) : null;

    private static T? OptVal<T>(JsonElement obj, string name, Func<JsonElement, T> read) where T : struct =>
        TryOpt(obj, name, out var value) ? read(value) : null;

    private static T Pick<T>(JsonElement e) where T : struct, Enum => MemoryEnumConverter<T>.Parse(Str(e));

    // ------------------------------------------------------------------------------------ shared readers
    internal static MemoryEffectiveScope ReadEffectiveScope(JsonElement e)
    {
        var o = Obj(e);
        return new(Str(Req(o, "tenant")), Str(Req(o, "agent")), OptStr(o, "user"), OptStr(o, "session"));
    }

    internal static MemoryRef ReadRef(JsonElement e, bool lenient = false)
    {
        var o = Obj(e);
        return new(Str(Req(o, "id")), U64(Req(o, "revision"), lenient));
    }

    internal static MemoryInterval ReadInterval(JsonElement e, bool lenient = false)
    {
        var o = Obj(e);
        return new(NullableI64(Req(o, "valid_from_ms"), lenient), NullableI64(Req(o, "valid_to_ms"), lenient));
    }

    private static MemorySlot ReadSlot(JsonElement e)
    {
        var o = Obj(e);
        return new(Str(Req(o, "subject_id")), Str(Req(o, "predicate")), Str(Req(o, "slot_key")));
    }

    internal static MemoryScalar ReadScalar(JsonElement e, bool lenient = false)
    {
        var o = Obj(e);
        var value = Req(o, "value");
        return Str(Req(o, "type")) switch
        {
            "string" => new MemoryScalar.Text(Str(value)),
            "integer" => new MemoryScalar.Integer(I64(value, lenient)),
            "float" => new MemoryScalar.Float(Num(value)),
            "boolean" => new MemoryScalar.Boolean(Bool(value)),
            var other => throw Bad("unknown scalar type \"" + other + "\"")
        };
    }

    private static MemoryProperties ReadProperties(JsonElement e)
    {
        var result = new MemoryProperties();
        foreach (var property in Obj(e).EnumerateObject()) result[property.Name] = ReadScalar(property.Value);
        return result;
    }

    internal static MemoryExpected ReadExpected(JsonElement e, bool lenient = false)
    {
        var o = Obj(e);
        var absent = o.TryGetProperty("absent", out var absentValue);
        var revision = o.TryGetProperty("revision", out var revisionValue);
        if (absent == revision) throw Bad("expected has exactly one of absent or revision");
        if (!absent) return new MemoryExpected.Revision(U64(revisionValue, lenient));
        return Bool(absentValue) ? new MemoryExpected.Absent() : throw Bad("absent must be true");
    }

    internal static MemoryTarget ReadTarget(JsonElement e, bool lenient = false)
    {
        var o = Obj(e);
        string[] discriminators = ["ids", "series", "entity", "session", "scope"];
        var present = discriminators.Where(name => o.TryGetProperty(name, out _)).ToArray();
        if (present.Length != 1) throw Bad("a target has exactly one of ids, series, entity, session or scope");
        var value = o.GetProperty(present[0]);
        return present[0] switch
        {
            "ids" => new MemoryTarget.Ids(Arr(value, item => ReadRef(item, lenient))),
            "series" => new MemoryTarget.Series(ReadRef(value, lenient)),
            "entity" => new MemoryTarget.Entity(ReadRef(value, lenient)),
            "session" => Bool(value) ? new MemoryTarget.Session() : throw Bad("session must be true"),
            _ => Bool(value) ? new MemoryTarget.Scope() : throw Bad("scope must be true")
        };
    }

    private static MemoryProvenance ReadProvenance(JsonElement e)
    {
        var o = Obj(e);
        return new(Str(Req(o, "source_id")), Str(Req(o, "source_kind")), Str(Req(o, "content_digest")),
            OptStr(o, "uri"), OptStr(o, "transformation_id"));
    }

    // ------------------------------------------------------------------------------------ mutation family
    private static MemoryReceipt ReadReceipt(JsonElement e)
    {
        var o = Obj(e);
        return new(Str(Req(o, "id")), Pick<MemoryOperation>(Req(o, "operation")), ReadEffectiveScope(Req(o, "scope")),
            U64(Req(o, "sequence")), Str(Req(o, "digest_version")),
            TryOpt(o, "retry_until_ms", out var retry) ? U64(retry) : null);
    }

    private static MemoryOutcome ReadOutcome(JsonElement e)
    {
        var o = Obj(e);
        return new(Str(Req(o, "id")), Pick<MemoryOutcomeAction>(Req(o, "action")), U64(Req(o, "revision")),
            TryOpt(o, "expires_at_ms", out var expires) ? U64(expires) : null);
    }

    private static MemoryFeed ReadFeed(JsonElement e)
    {
        var o = Obj(e);
        return new(Str(Req(o, "mode")), Bool(Req(o, "temporal_complete")), Bool(Req(o, "ttl_complete")),
            Bool(Req(o, "consumer_purge_acknowledged")));
    }

    private static MemoryErasureNotice ReadNotice(JsonElement e)
    {
        var o = Obj(e);
        return new(Bool(Req(o, "live_state_only")), Bool(Req(o, "secure_erasure")), Str(Req(o, "external_copies")),
            Bool(Req(o, "restore_requires_manifest")), Str(Req(o, "backup_history_purge")), OptStr(o, "manifest_id"));
    }

    private static MemoryErasure ReadErasure(JsonElement e)
    {
        var o = Obj(e);
        return new(Str(Req(o, "id")), ReadEffectiveScope(Req(o, "scope")), ReadTarget(Req(o, "target")), U64(Req(o, "revision")),
            U64(Req(o, "generation")), Pick<MemoryErasureState>(Req(o, "state")), Str(Req(o, "logical_visibility")),
            U64(Req(o, "deleted_count")), NullableU64(Req(o, "remaining_count")), OptStr(o, "resume_token"));
    }

    internal static MemoryMutationResponse ReadMutation(JsonElement e)
    {
        var o = Obj(e);
        return new(ReadReceipt(Req(o, "receipt")), Bool(Req(o, "replayed")), Arr(Req(o, "outcomes"), ReadOutcome),
            ReadFeed(Req(o, "feed")), NullableU64(Req(o, "remaining_work")), OptVal(o, "state", Pick<MemoryMutationState>),
            OptRef(o, "erasure", ReadErasure), OptRef(o, "erasure_notice", ReadNotice));
    }

    private static MemoryCompeting ReadCompeting(JsonElement e)
    {
        var o = Obj(e);
        return new(Str(Req(o, "id")), ReadInterval(Req(o, "interval")), U64(Req(o, "revision")));
    }

    private static MemoryErrorDetail ReadErrorDetail(JsonElement e)
    {
        var o = Obj(e);
        return new(OptStr(o, "field"), TryOpt(o, "limit", out var limit) ? U64(limit) : null,
            TryOpt(o, "requested", out var requested) ? U64(requested) : null,
            OptRef(o, "competing", value => Arr(value, ReadCompeting)), OptVal(o, "receipt_lookup_required", Bool), OptStr(o, "code"));
    }

    private static MemoryPlanResponse ReadPlan(JsonElement e)
    {
        var o = Obj(e);
        if (!Bool(Req(o, "dry_run"))) throw Bad("dry_run must be true");
        return new(true, U64(Req(o, "user_operations")), U64(Req(o, "encoded_bytes")), Arr(Req(o, "conflicts"), ReadErrorDetail),
            NullableU64(Req(o, "remaining_work")));
    }

    /// <summary>The variant is chosen by its discriminating member: <c>dry_run</c> (plan) or <c>receipt</c> (committed).</summary>
    internal static MemoryConsolidateResponse ReadConsolidate(JsonElement e)
    {
        var o = Obj(e);
        var plan = o.TryGetProperty("dry_run", out _);
        var committed = o.TryGetProperty("receipt", out _);
        if (plan == committed) throw Bad(plan ? "a consolidate answer has either dry_run or receipt, not both" : "a consolidate answer has dry_run or receipt");
        return plan ? ReadPlan(o) : ReadMutation(o);
    }

    internal static MemoryReceiptResponse ReadReceiptResponse(JsonElement e) => new(ReadMutation(Req(Obj(e), "result")));

    internal static MemoryErasureResponse ReadErasureResponse(JsonElement e)
    {
        var o = Obj(e);
        return new(ReadErasure(Req(o, "erasure")), ReadNotice(Req(o, "erasure_notice")), ReadFeed(Req(o, "feed")));
    }

    // ------------------------------------------------------------------------------------ read family
    private static MemoryEvidenceView ReadEvidenceView(JsonElement e)
    {
        var o = Obj(e);
        return new(Str(Req(o, "collection")), U64(Req(o, "record")), OptStr(o, "snippet"));
    }

    private static MemoryPath ReadPath(JsonElement e)
    {
        var o = Obj(e);
        return new(Arr(Req(o, "nodes"), value => U64(value)), Arr(Req(o, "edges"), value => U64(value)));
    }

    private static MemoryScores ReadScores(JsonElement e)
    {
        var o = Obj(e);
        var d = Req(o, "D");
        return new(Num(Req(o, "R")), d.ValueKind == JsonValueKind.Null ? null : Num(d), Num(Req(o, "I")), Num(Req(o, "S")),
            OptVal(o, "dense_rank", Int), OptVal(o, "sparse_rank", Int), OptVal(o, "text_rank", Int),
            OptVal(o, "distance", Num), OptVal(o, "bm25", Num), OptVal(o, "sparse_score", Num),
            OptVal(o, "fused_rank", Int), OptVal(o, "fused_score", Num));
    }

    private static MemoryView ReadView(JsonElement e)
    {
        var o = Obj(e);
        var segment = Req(o, "segment");
        return new(Str(Req(o, "id")), U64(Req(o, "revision")), ReadEffectiveScope(Req(o, "scope")), Pick<MemoryKind>(Req(o, "kind")),
            Str(Req(o, "content")), I64(Req(o, "observed_at_ms")), U64(Req(o, "created_at_ms")), U64(Req(o, "updated_at_ms")),
            Num(Req(o, "importance")), Pick<MemoryStatus>(Req(o, "status")), Bool(Req(o, "stale")),
            segment.ValueKind == JsonValueKind.Null ? null : ReadInterval(segment), Arr(Req(o, "evidence"), ReadEvidenceView),
            ReadScores(Req(o, "scores")), OptVal(o, "confidence", Num), TryOpt(o, "expires_at_ms", out var expires) ? U64(expires) : null,
            OptRef(o, "provenance", value => Arr(value, ReadProvenance)), OptRef(o, "references", value => Arr(value, item => ReadRef(item))),
            OptRef(o, "paths", value => Arr(value, ReadPath)), OptRef(o, "metadata", ReadProperties));
    }

    private static MemoryReadMeta ReadMeta(JsonElement e)
    {
        var o = Obj(e);
        return new(Arr(Req(o, "scopes"), ReadEffectiveScope), I64(Req(o, "as_of_ms")), U64(Req(o, "ttl_clock_ms")),
            U64(Req(o, "sequence")), Bool(Req(o, "complete")), Arr(Req(o, "truncation"), Str), Arr(Req(o, "limitations"), Str),
            Str(Req(o, "mode")), U64(Req(o, "candidates_examined")), U64(Req(o, "edges_charged")), U64(Req(o, "vector_components")),
            U64(Req(o, "mmr_components")), U64(Req(o, "evidence_bytes")));
    }

    internal static MemoryRecallResponse ReadRecall(JsonElement e)
    {
        var o = Obj(e);
        return new(Arr(Req(o, "memories"), ReadView), ReadMeta(Req(o, "meta")), Str(Req(o, "ranking_version")),
            Pick<MemorySelection>(Req(o, "selection")));
    }

    private static MemoryFactView ReadFactView(JsonElement e)
    {
        var o = Obj(e);
        return new(ReadRef(Req(o, "series")), ReadSlot(Req(o, "slot")), U64(Req(o, "slot_revision")), Str(Req(o, "destination_id")),
            ReadInterval(Req(o, "interval")), Pick<MemoryStatus>(Req(o, "status")), Bool(Req(o, "stale")),
            Arr(Req(o, "evidence"), ReadEvidenceView), OptRef(o, "provenance", value => Arr(value, ReadProvenance)));
    }

    internal static MemoryFactsResponse ReadFacts(JsonElement e)
    {
        var o = Obj(e);
        return new(Arr(Req(o, "facts"), ReadFactView), ReadMeta(Req(o, "meta")));
    }

    // ------------------------------------------------------------------------------------ error context
    /// <summary>Best-effort <c>error.details</c>: a malformed member is dropped, never allowed to hide the error itself.</summary>
    internal static MemoryErrorContext? ReadErrorContext(JsonElement error)
    {
        try
        {
            if (!error.TryGetProperty("details", out var details) || details.ValueKind != JsonValueKind.Object) return null;
            return new(LooseString(details, "field"), LooseU64(details, "limit"), LooseU64(details, "requested"),
                LooseString(details, "feature"), LooseString(details, "category"));
        }
        catch (Exception e) when (e is JsonException or InvalidOperationException or KeyNotFoundException) { return null; }
    }

    private static string? LooseString(JsonElement obj, string name) =>
        obj.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;

    private static string? LooseU64(JsonElement obj, string name)
    {
        if (!obj.TryGetProperty(name, out var value)) return null;
        return value.ValueKind switch
        {
            JsonValueKind.String when MemoryNumber.TryParseU64(value.GetString(), out _) => value.GetString(),
            JsonValueKind.Number when value.TryGetUInt64(out var number) => MemoryNumber.FormatU64(number),
            _ => null
        };
    }
}

// ---------------------------------------------------------------------------------------------- converters

/// <summary>Enum values travel as the snake_case of their member names; numbers and unknown names are rejected.</summary>
internal sealed class MemoryEnumConverter<T> : JsonConverter<T> where T : struct, Enum
{
    private static readonly Dictionary<string, T> ByName =
        Enum.GetValues<T>().ToDictionary(value => JsonNamingPolicy.SnakeCaseLower.ConvertName(value.ToString()), StringComparer.Ordinal);
    private static readonly Dictionary<T, string> ByValue = ByName.ToDictionary(pair => pair.Value, pair => pair.Key);

    internal static T Parse(string text) =>
        ByName.TryGetValue(text, out var value) ? value : throw new JsonException("unknown " + typeof(T).Name + " \"" + text + "\"");

    public override T Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options) =>
        reader.TokenType == JsonTokenType.String ? Parse(reader.GetString()!) : throw new JsonException(typeof(T).Name + " must be a string");

    public override void Write(Utf8JsonWriter writer, T value, JsonSerializerOptions options) =>
        writer.WriteStringValue(ByValue.TryGetValue(value, out var name) ? name : throw new JsonException("undefined " + typeof(T).Name + " value"));
}

/// <summary>u64 request numbers: an exact integer or canonical string in, the canonical string out.</summary>
internal sealed class MemoryU64Converter : JsonConverter<ulong>
{
    public override ulong Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options) => reader.TokenType switch
    {
        JsonTokenType.String when MemoryNumber.TryParseU64(reader.GetString(), out var text) => text,
        JsonTokenType.Number when reader.TryGetUInt64(out var number) => number,
        _ => throw new JsonException("expected a canonical decimal u64 string or an exact integer")
    };

    public override void Write(Utf8JsonWriter writer, ulong value, JsonSerializerOptions options) =>
        writer.WriteStringValue(MemoryNumber.FormatU64(value));
}

/// <summary>i64 request numbers: an exact integer or canonical string in, the canonical string out.</summary>
internal sealed class MemoryI64Converter : JsonConverter<long>
{
    public override long Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options) => reader.TokenType switch
    {
        JsonTokenType.String when MemoryNumber.TryParseI64(reader.GetString(), out var text) => text,
        JsonTokenType.Number when reader.TryGetInt64(out var number) => number,
        _ => throw new JsonException("expected a canonical decimal i64 string or an exact integer")
    };

    public override void Write(Utf8JsonWriter writer, long value, JsonSerializerOptions options) =>
        writer.WriteStringValue(MemoryNumber.FormatI64(value));
}

internal sealed class MemoryRefConverter : JsonConverter<MemoryRef>
{
    public override MemoryRef Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        using var document = JsonDocument.ParseValue(ref reader);
        return MemoryWire.ReadRef(document.RootElement, lenient: true);
    }

    public override void Write(Utf8JsonWriter writer, MemoryRef value, JsonSerializerOptions options)
    {
        writer.WriteStartObject();
        writer.WriteString("id", value.Id);
        writer.WriteString("revision", MemoryWire.CheckU64(value.Revision, "revision"));
        writer.WriteEndObject();
    }
}

/// <summary>Both endpoints are required members that may be null (unbounded), so null is written, never omitted.</summary>
internal sealed class MemoryIntervalConverter : JsonConverter<MemoryInterval>
{
    public override MemoryInterval Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        using var document = JsonDocument.ParseValue(ref reader);
        return MemoryWire.ReadInterval(document.RootElement, lenient: true);
    }

    public override void Write(Utf8JsonWriter writer, MemoryInterval value, JsonSerializerOptions options)
    {
        writer.WriteStartObject();
        Endpoint(writer, "valid_from_ms", value.ValidFromMs);
        Endpoint(writer, "valid_to_ms", value.ValidToMs);
        writer.WriteEndObject();
    }

    private static void Endpoint(Utf8JsonWriter writer, string name, string? text)
    {
        if (text is null) writer.WriteNull(name);
        else writer.WriteString(name, MemoryWire.CheckI64(text, name));
    }
}

internal sealed class MemoryExpectedConverter : JsonConverter<MemoryExpected>
{
    public override MemoryExpected Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        using var document = JsonDocument.ParseValue(ref reader);
        return MemoryWire.ReadExpected(document.RootElement, lenient: true);
    }

    public override void Write(Utf8JsonWriter writer, MemoryExpected value, JsonSerializerOptions options)
    {
        writer.WriteStartObject();
        switch (value)
        {
            case MemoryExpected.Absent: writer.WriteBoolean("absent", true); break;
            case MemoryExpected.Revision revision: writer.WriteString("revision", MemoryWire.CheckU64(revision.Value, "revision")); break;
            default: throw new JsonException("unknown MemoryExpected variant");
        }
        writer.WriteEndObject();
    }
}

internal sealed class MemoryScalarConverter : JsonConverter<MemoryScalar>
{
    public override MemoryScalar Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        using var document = JsonDocument.ParseValue(ref reader);
        return MemoryWire.ReadScalar(document.RootElement, lenient: true);
    }

    public override void Write(Utf8JsonWriter writer, MemoryScalar value, JsonSerializerOptions options)
    {
        writer.WriteStartObject();
        switch (value)
        {
            case MemoryScalar.Text text:
                writer.WriteString("type", "string");
                writer.WriteString("value", text.Value ?? throw new JsonException("a string scalar must not be null"));
                break;
            case MemoryScalar.Integer integer:
                writer.WriteString("type", "integer");
                writer.WriteString("value", MemoryWire.CheckI64(integer.Value, "integer scalar"));
                break;
            case MemoryScalar.Float number:
                writer.WriteString("type", "float");
                writer.WriteNumber("value", number.Value);
                break;
            case MemoryScalar.Boolean flag:
                writer.WriteString("type", "boolean");
                writer.WriteBoolean("value", flag.Value);
                break;
            default: throw new JsonException("unknown MemoryScalar variant");
        }
        writer.WriteEndObject();
    }
}

internal sealed class MemoryTargetConverter : JsonConverter<MemoryTarget>
{
    public override MemoryTarget Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        using var document = JsonDocument.ParseValue(ref reader);
        return MemoryWire.ReadTarget(document.RootElement, lenient: true);
    }

    public override void Write(Utf8JsonWriter writer, MemoryTarget value, JsonSerializerOptions options)
    {
        writer.WriteStartObject();
        switch (value)
        {
            case MemoryTarget.Ids ids:
                writer.WritePropertyName("ids");
                JsonSerializer.Serialize(writer, ids.Refs, options);
                break;
            case MemoryTarget.Series series:
                writer.WritePropertyName("series");
                JsonSerializer.Serialize(writer, series.Ref, options);
                break;
            case MemoryTarget.Entity entity:
                writer.WritePropertyName("entity");
                JsonSerializer.Serialize(writer, entity.Ref, options);
                break;
            case MemoryTarget.Session: writer.WriteBoolean("session", true); break;
            case MemoryTarget.Scope: writer.WriteBoolean("scope", true); break;
            default: throw new JsonException("unknown MemoryTarget variant");
        }
        writer.WriteEndObject();
    }
}

internal sealed class MemoryConsolidateResponseConverter : JsonConverter<MemoryConsolidateResponse>
{
    public override MemoryConsolidateResponse Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        using var document = JsonDocument.ParseValue(ref reader);
        return MemoryWire.ReadConsolidate(document.RootElement);
    }

    public override void Write(Utf8JsonWriter writer, MemoryConsolidateResponse value, JsonSerializerOptions options) =>
        JsonSerializer.Serialize(writer, value, value.GetType(), options);
}
