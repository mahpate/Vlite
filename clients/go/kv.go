package vlite

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/url"
	"strconv"
	"unicode/utf8"
)

const (
	kvMaxNamespaceBytes  = 255
	kvMaxKeyBytes        = 1024
	kvMaxValueBytes      = 1047290
	kvMaxEncodedBytes    = (kvMaxValueBytes + 2) / 3 * 4
	kvDefaultListLimit   = 100
	kvMaxListLimit       = 10000
	kvMaxBatchOperations = 1000
	kvMaxBatchBytes      = 4194304
	kvMaxGetManyKeys     = 1000
)

// UnsupportedError means the server answered 404 not_found for a key-value route, so it does
// not serve that route. Nothing falls back to other calls. It matches errors.ErrUnsupported
// and unwraps to the *APIError.
type UnsupportedError struct {
	Cause *APIError
}

func (e *UnsupportedError) Error() string {
	return "vlite: unsupported (the server does not serve this key-value route; no fallback performed)"
}
func (e *UnsupportedError) Unwrap() error        { return e.Cause }
func (e *UnsupportedError) Is(target error) bool { return target == errors.ErrUnsupported }

type kvExpectedKind uint8

const (
	kvExpectAny kvExpectedKind = iota
	kvExpectAbsent
	kvExpectRevision
)

// KvExpected is the condition a put, delete or batch operation needs, checked atomically with
// the write. The zero value is KvAny().
type KvExpected struct {
	kind     kvExpectedKind
	revision uint64
}

// KvAny writes whatever is there.
func KvAny() KvExpected { return KvExpected{} }

// KvAbsent needs no live entry; an expired entry counts as absent.
func KvAbsent() KvExpected { return KvExpected{kind: kvExpectAbsent} }

// KvRevision needs the live entry to have exactly this version.
func KvRevision(version uint64) KvExpected {
	return KvExpected{kind: kvExpectRevision, revision: version}
}

func (e KvExpected) wire() any {
	switch e.kind {
	case kvExpectAbsent:
		return "absent"
	case kvExpectRevision:
		return map[string]string{"revision": strconv.FormatUint(e.revision, 10)}
	}
	return "any"
}

func (e KvExpected) query() string {
	switch e.kind {
	case kvExpectAbsent:
		return "absent"
	case kvExpectRevision:
		return strconv.FormatUint(e.revision, 10)
	}
	return "any"
}

// KvPutOptions sets the write condition and the optional deadline of a put. ExpiresAtMs is an
// absolute Unix-millisecond time; nil clears the entry's deadline, so pass it again whenever a
// value is refreshed.
type KvPutOptions struct {
	Expected    KvExpected
	ExpiresAtMs *uint64
}

// KvListOptions selects a page: keys that start with Prefix, strictly after After, at most Limit
// of them. A Limit of 0 means 100; the largest is 10000.
type KvListOptions struct {
	Prefix string
	After  *string
	Limit  int
}

type KvEntry struct {
	Key         string
	Value       []byte
	Version     uint64
	ExpiresAtMs *uint64
}
type KvPutResult struct {
	Version     uint64
	ExpiresAtMs *uint64
}
type KvPage struct {
	Items     []KvEntry
	NextAfter *string
}

// KvKey names one entry for KvGetMany.
type KvKey struct {
	Namespace string `json:"namespace"`
	Key       string `json:"key"`
}

// KvGetManyResult answers the requested keys in order: Items[i] is nil for a key that is absent
// or expired. A partial result is shorter than the keys and NextIndex is the index of the first
// key not read; it is nil when every key was read.
type KvGetManyResult struct {
	Items     []*KvEntry
	NextIndex *int
}

type KvBatchKind string

const (
	KvBatchKindPut    KvBatchKind = "put"
	KvBatchKindDelete KvBatchKind = "delete"
)

// KvBatchOperation is one put or delete of a batch; build it with KvBatchPut or KvBatchDelete.
// A batch put has no deadline, so it clears the deadline of the entry it replaces.
type KvBatchOperation struct {
	Kind      KvBatchKind
	Namespace string
	Key       string
	Value     []byte
	Expected  KvExpected
}

func KvBatchPut(namespace, key string, value []byte, expected KvExpected) KvBatchOperation {
	return KvBatchOperation{Kind: KvBatchKindPut, Namespace: namespace, Key: key, Value: value, Expected: expected}
}
func KvBatchDelete(namespace, key string, expected KvExpected) KvBatchOperation {
	return KvBatchOperation{Kind: KvBatchKindDelete, Namespace: namespace, Key: key, Expected: expected}
}

// KvBatchResult answers one operation: Version is the version a put was given, Deleted says
// whether a delete removed a live entry. Kind tells which one the result is.
type KvBatchResult struct {
	Kind    KvBatchKind
	Version uint64
	Deleted bool
}

type kvPutWire struct {
	Namespace   string  `json:"namespace"`
	Key         string  `json:"key"`
	ValueBase64 string  `json:"value_base64"`
	Expected    any     `json:"expected"`
	ExpiresAtMs *string `json:"expires_at_ms"`
}
type kvBatchPutWire struct {
	Op          string `json:"op"`
	Namespace   string `json:"namespace"`
	Key         string `json:"key"`
	ValueBase64 string `json:"value_base64"`
	Expected    any    `json:"expected"`
}
type kvBatchDeleteWire struct {
	Op        string `json:"op"`
	Namespace string `json:"namespace"`
	Key       string `json:"key"`
	Expected  any    `json:"expected"`
}

// kvPath is the route of a collection's key-value endpoints. The methods need a collection from
// Client.Collection (which must already exist and have one dimension); on the Client itself they
// return an error.
func (r *Records) kvPath(route string) (string, error) {
	if r.prefix == "/v1" {
		return "", errors.New("select a collection")
	}
	return r.prefix + "/kv/" + route, nil
}

func kvText(name, value string, minimum, maximum int) error {
	if !utf8.ValidString(value) || len(value) < minimum || len(value) > maximum {
		return fmt.Errorf("%s must be %d..%d bytes of valid UTF-8", name, minimum, maximum)
	}
	return nil
}
func kvAddress(namespace, key string) error {
	if err := kvText("namespace", namespace, 1, kvMaxNamespaceBytes); err != nil {
		return err
	}
	return kvText("key", key, 1, kvMaxKeyBytes)
}
func kvValue(value []byte) error {
	if len(value) > kvMaxValueBytes {
		return fmt.Errorf("value must be at most %d bytes", kvMaxValueBytes)
	}
	return nil
}

// kvRequest returns the raw JSON of a success body. A 404 not_found is an unknown route.
func (r *Records) kvRequest(ctx context.Context, method, path string, body any) (json.RawMessage, error) {
	var raw json.RawMessage
	err := r.client.request(ctx, method, path, body, &raw)
	var api *APIError
	if errors.As(err, &api) && api.Status == http.StatusNotFound && api.Code == "not_found" {
		return nil, &UnsupportedError{api}
	}
	if err != nil {
		return nil, err
	}
	if !utf8.Valid(raw) {
		return nil, kvInvalid(errors.New("response is not valid UTF-8"))
	}
	return raw, nil
}

// kvInvalid reports a success response the client cannot trust, like any other transport failure.
func kvInvalid(err error) error {
	return &TransportError{"transport_error", fmt.Errorf("invalid key-value response: %w", err)}
}

// KvGet reads one live entry. A missing or expired key is an *APIError with code kv_not_found.
func (r *Records) KvGet(ctx context.Context, namespace, key string) (KvEntry, error) {
	path, err := r.kvPath("entry")
	if err != nil {
		return KvEntry{}, err
	}
	if err = kvAddress(namespace, key); err != nil {
		return KvEntry{}, err
	}
	query := url.Values{"namespace": {namespace}, "key": {key}}
	raw, err := r.kvRequest(ctx, "GET", path+"?"+query.Encode(), nil)
	if err != nil {
		return KvEntry{}, err
	}
	entry, err := kvReadEntry(raw)
	if err != nil {
		return KvEntry{}, kvInvalid(err)
	}
	return entry, nil
}

// KvPut writes one entry. A TransportError means the write may have committed: read the key
// before deciding anything.
func (r *Records) KvPut(ctx context.Context, namespace, key string, value []byte, opts KvPutOptions) (KvPutResult, error) {
	path, err := r.kvPath("entry")
	if err != nil {
		return KvPutResult{}, err
	}
	if err = kvAddress(namespace, key); err != nil {
		return KvPutResult{}, err
	}
	if err = kvValue(value); err != nil {
		return KvPutResult{}, err
	}
	body := kvPutWire{
		Namespace:   namespace,
		Key:         key,
		ValueBase64: base64.StdEncoding.EncodeToString(value),
		Expected:    opts.Expected.wire(),
	}
	if opts.ExpiresAtMs != nil {
		deadline := strconv.FormatUint(*opts.ExpiresAtMs, 10)
		body.ExpiresAtMs = &deadline
	}
	raw, err := r.kvRequest(ctx, "PUT", path, body)
	if err != nil {
		return KvPutResult{}, err
	}
	result, err := kvReadPut(raw)
	if err != nil {
		return KvPutResult{}, kvInvalid(err)
	}
	return result, nil
}

// KvDelete reports whether a live entry was removed. A TransportError means the delete may have
// committed.
func (r *Records) KvDelete(ctx context.Context, namespace, key string, expected KvExpected) (bool, error) {
	path, err := r.kvPath("entry")
	if err != nil {
		return false, err
	}
	if err = kvAddress(namespace, key); err != nil {
		return false, err
	}
	query := url.Values{"namespace": {namespace}, "key": {key}, "expected": {expected.query()}}
	raw, err := r.kvRequest(ctx, "DELETE", path+"?"+query.Encode(), nil)
	if err != nil {
		return false, err
	}
	deleted, err := kvReadDeleted(raw)
	if err != nil {
		return false, kvInvalid(err)
	}
	return deleted, nil
}

// KvList returns one page of a namespace in key order. There is no snapshot across pages:
// follow NextAfter, passed as After, until it is nil.
func (r *Records) KvList(ctx context.Context, namespace string, opts KvListOptions) (KvPage, error) {
	path, err := r.kvPath("entries")
	if err != nil {
		return KvPage{}, err
	}
	limit := opts.Limit
	if limit == 0 {
		limit = kvDefaultListLimit
	}
	if limit < 1 || limit > kvMaxListLimit {
		return KvPage{}, fmt.Errorf("limit must be in 1..%d", kvMaxListLimit)
	}
	if err = kvText("namespace", namespace, 1, kvMaxNamespaceBytes); err != nil {
		return KvPage{}, err
	}
	if err = kvText("prefix", opts.Prefix, 0, kvMaxKeyBytes); err != nil {
		return KvPage{}, err
	}
	query := url.Values{"namespace": {namespace}, "prefix": {opts.Prefix}, "limit": {strconv.Itoa(limit)}}
	if opts.After != nil {
		if err = kvText("after", *opts.After, 1, kvMaxKeyBytes); err != nil {
			return KvPage{}, err
		}
		query.Set("after", *opts.After)
	}
	raw, err := r.kvRequest(ctx, "GET", path+"?"+query.Encode(), nil)
	if err != nil {
		return KvPage{}, err
	}
	page, err := kvReadPage(raw)
	if err != nil {
		return KvPage{}, kvInvalid(err)
	}
	return page, nil
}

// KvGetMany reads up to 1000 keys in one call; a key may repeat. The server may answer only a
// prefix of the keys, with NextIndex set. The client never continues a partial result: call
// again with keys[*NextIndex:]. Calls share no snapshot.
func (r *Records) KvGetMany(ctx context.Context, keys []KvKey) (KvGetManyResult, error) {
	path, err := r.kvPath("entries/get")
	if err != nil {
		return KvGetManyResult{}, err
	}
	if len(keys) > kvMaxGetManyKeys {
		return KvGetManyResult{}, fmt.Errorf("a get-many call takes at most %d keys", kvMaxGetManyKeys)
	}
	for i, key := range keys {
		if err = kvAddress(key.Namespace, key.Key); err != nil {
			return KvGetManyResult{}, fmt.Errorf("keys[%d]: %w", i, err)
		}
	}
	if keys == nil {
		keys = []KvKey{}
	}
	raw, err := r.kvRequest(ctx, "POST", path, struct {
		Keys []KvKey `json:"keys"`
	}{keys})
	if err != nil {
		return KvGetManyResult{}, err
	}
	result, err := kvReadGetMany(raw, keys)
	if err != nil {
		return KvGetManyResult{}, kvInvalid(err)
	}
	return result, nil
}

// KvBatch commits every operation or none, in one collection: at most 1000 operations, at most
// 4194304 value bytes in all, and no two operations on the same namespace and key. Results
// answers the operations in order. After a TransportError or a 503 commit_indeterminate the batch
// may or may not have committed: read the keys back with KvGetMany before deciding anything.
func (r *Records) KvBatch(ctx context.Context, operations []KvBatchOperation) ([]KvBatchResult, error) {
	path, err := r.kvPath("batch")
	if err != nil {
		return nil, err
	}
	wire, err := kvBatchWire(operations)
	if err != nil {
		return nil, err
	}
	raw, err := r.kvRequest(ctx, "POST", path, struct {
		Operations []any `json:"operations"`
	}{wire})
	if err != nil {
		return nil, err
	}
	results, err := kvReadBatch(raw, operations)
	if err != nil {
		return nil, kvInvalid(err)
	}
	return results, nil
}

func kvBatchWire(operations []KvBatchOperation) ([]any, error) {
	if len(operations) > kvMaxBatchOperations {
		return nil, fmt.Errorf("a batch takes at most %d operations", kvMaxBatchOperations)
	}
	wire := make([]any, 0, len(operations))
	seen := make(map[KvKey]struct{}, len(operations))
	total := 0
	for i, op := range operations {
		if err := kvAddress(op.Namespace, op.Key); err != nil {
			return nil, fmt.Errorf("operations[%d]: %w", i, err)
		}
		address := KvKey{Namespace: op.Namespace, Key: op.Key}
		if _, repeated := seen[address]; repeated {
			return nil, fmt.Errorf("operations[%d]: namespace and key repeat an earlier operation", i)
		}
		seen[address] = struct{}{}
		switch op.Kind {
		case KvBatchKindPut:
			if err := kvValue(op.Value); err != nil {
				return nil, fmt.Errorf("operations[%d]: %w", i, err)
			}
			total += len(op.Value)
			if total > kvMaxBatchBytes {
				return nil, fmt.Errorf("a batch takes at most %d value bytes", kvMaxBatchBytes)
			}
			wire = append(wire, kvBatchPutWire{
				Op:          string(op.Kind),
				Namespace:   op.Namespace,
				Key:         op.Key,
				ValueBase64: base64.StdEncoding.EncodeToString(op.Value),
				Expected:    op.Expected.wire(),
			})
		case KvBatchKindDelete:
			if op.Value != nil {
				return nil, fmt.Errorf("operations[%d]: a delete takes no value", i)
			}
			wire = append(wire, kvBatchDeleteWire{
				Op:        string(op.Kind),
				Namespace: op.Namespace,
				Key:       op.Key,
				Expected:  op.Expected.wire(),
			})
		default:
			return nil, fmt.Errorf("operations[%d]: kind must be %q or %q", i, KvBatchKindPut, KvBatchKindDelete)
		}
	}
	return wire, nil
}

// The readers below hold a success body to the same rules as the other clients: exact JSON
// types, canonical decimal strings, canonical base64 within the value bound, nonzero versions.
type kvFields map[string]json.RawMessage

func kvObject(raw json.RawMessage) (kvFields, error) {
	if len(raw) == 0 || raw[0] != '{' {
		return nil, errors.New("expected a JSON object")
	}
	var fields kvFields
	if err := json.Unmarshal(raw, &fields); err != nil {
		return nil, err
	}
	return fields, nil
}
func kvField(fields kvFields, name string) (json.RawMessage, error) {
	raw, ok := fields[name]
	if !ok {
		return nil, fmt.Errorf("%s is missing", name)
	}
	return raw, nil
}
func kvArray(fields kvFields, name string) ([]json.RawMessage, error) {
	raw, err := kvField(fields, name)
	if err != nil {
		return nil, err
	}
	if raw[0] != '[' {
		return nil, fmt.Errorf("%s must be an array", name)
	}
	var items []json.RawMessage
	if err = json.Unmarshal(raw, &items); err != nil {
		return nil, fmt.Errorf("%s: %w", name, err)
	}
	return items, nil
}
func kvStringField(fields kvFields, name string) (string, error) {
	raw, err := kvField(fields, name)
	if err != nil {
		return "", err
	}
	if raw[0] != '"' {
		return "", fmt.Errorf("%s must be a string", name)
	}
	var text string
	if err = json.Unmarshal(raw, &text); err != nil {
		return "", fmt.Errorf("%s: %w", name, err)
	}
	return text, nil
}
func kvTextField(fields kvFields, name string, minimum, maximum int) (string, error) {
	text, err := kvStringField(fields, name)
	if err != nil {
		return "", err
	}
	return text, kvText(name, text, minimum, maximum)
}
func kvDecimalField(fields kvFields, name string, nonzero bool) (uint64, error) {
	text, err := kvStringField(fields, name)
	if err != nil {
		return 0, err
	}
	value, err := strconv.ParseUint(text, 10, 64)
	if err != nil || !idPattern.MatchString(text) {
		return 0, fmt.Errorf("%s must be a canonical decimal u64 string", name)
	}
	if nonzero && value == 0 {
		return 0, fmt.Errorf("%s must be nonzero", name)
	}
	return value, nil
}
func kvOptionalDecimalField(fields kvFields, name string) (*uint64, error) {
	if string(fields[name]) == "null" {
		return nil, nil
	}
	value, err := kvDecimalField(fields, name, false)
	if err != nil {
		return nil, err
	}
	return &value, nil
}
func kvBoolField(fields kvFields, name string) (bool, error) {
	switch string(fields[name]) {
	case "true":
		return true, nil
	case "false":
		return false, nil
	}
	return false, fmt.Errorf("%s must be a boolean", name)
}
func kvIndexField(fields kvFields, name string) (*int, error) {
	raw, err := kvField(fields, name)
	if err != nil {
		return nil, err
	}
	if string(raw) == "null" {
		return nil, nil
	}
	index, err := strconv.Atoi(string(raw))
	if err != nil {
		return nil, fmt.Errorf("%s must be null or an integer", name)
	}
	return &index, nil
}

func kvDecodeValue(encoded string) ([]byte, error) {
	if len(encoded) > kvMaxEncodedBytes {
		return nil, errors.New("value_base64 is longer than any value")
	}
	value, err := base64.StdEncoding.DecodeString(encoded)
	if err != nil {
		return nil, fmt.Errorf("value_base64: %w", err)
	}
	if len(value) > kvMaxValueBytes || base64.StdEncoding.EncodeToString(value) != encoded {
		return nil, fmt.Errorf("value_base64 must be canonical base64 of at most %d bytes", kvMaxValueBytes)
	}
	return value, nil
}
func kvReadPutFields(fields kvFields) (KvPutResult, error) {
	version, err := kvDecimalField(fields, "version", true)
	if err != nil {
		return KvPutResult{}, err
	}
	expires, err := kvOptionalDecimalField(fields, "expires_at_ms")
	if err != nil {
		return KvPutResult{}, err
	}
	return KvPutResult{Version: version, ExpiresAtMs: expires}, nil
}
func kvReadPut(raw json.RawMessage) (KvPutResult, error) {
	fields, err := kvObject(raw)
	if err != nil {
		return KvPutResult{}, err
	}
	return kvReadPutFields(fields)
}
func kvReadEntry(raw json.RawMessage) (KvEntry, error) {
	fields, err := kvObject(raw)
	if err != nil {
		return KvEntry{}, err
	}
	key, err := kvTextField(fields, "key", 1, kvMaxKeyBytes)
	if err != nil {
		return KvEntry{}, err
	}
	encoded, err := kvStringField(fields, "value_base64")
	if err != nil {
		return KvEntry{}, err
	}
	value, err := kvDecodeValue(encoded)
	if err != nil {
		return KvEntry{}, err
	}
	stamp, err := kvReadPutFields(fields)
	if err != nil {
		return KvEntry{}, err
	}
	return KvEntry{Key: key, Value: value, Version: stamp.Version, ExpiresAtMs: stamp.ExpiresAtMs}, nil
}
func kvReadDeleted(raw json.RawMessage) (bool, error) {
	fields, err := kvObject(raw)
	if err != nil {
		return false, err
	}
	return kvBoolField(fields, "deleted")
}
func kvReadPage(raw json.RawMessage) (KvPage, error) {
	fields, err := kvObject(raw)
	if err != nil {
		return KvPage{}, err
	}
	rawItems, err := kvArray(fields, "items")
	if err != nil {
		return KvPage{}, err
	}
	items := make([]KvEntry, len(rawItems))
	for i, rawItem := range rawItems {
		if items[i], err = kvReadEntry(rawItem); err != nil {
			return KvPage{}, fmt.Errorf("items[%d]: %w", i, err)
		}
	}
	var next *string
	if string(fields["next_after"]) != "null" {
		after, err := kvTextField(fields, "next_after", 1, kvMaxKeyBytes)
		if err != nil {
			return KvPage{}, err
		}
		next = &after
	}
	return KvPage{Items: items, NextAfter: next}, nil
}

// kvReadGetMany needs the requested keys: items answer them by position, and next_index must
// name the first key that was not read.
func kvReadGetMany(raw json.RawMessage, keys []KvKey) (KvGetManyResult, error) {
	fields, err := kvObject(raw)
	if err != nil {
		return KvGetManyResult{}, err
	}
	rawItems, err := kvArray(fields, "items")
	if err != nil {
		return KvGetManyResult{}, err
	}
	if len(rawItems) > len(keys) {
		return KvGetManyResult{}, errors.New("items has more elements than keys were requested")
	}
	items := make([]*KvEntry, len(rawItems))
	for i, rawItem := range rawItems {
		if string(rawItem) == "null" {
			continue
		}
		entry, err := kvReadEntry(rawItem)
		if err != nil {
			return KvGetManyResult{}, fmt.Errorf("items[%d]: %w", i, err)
		}
		if entry.Key != keys[i].Key {
			return KvGetManyResult{}, fmt.Errorf("items[%d] answers a different key than keys[%d]", i, i)
		}
		items[i] = &entry
	}
	next, err := kvIndexField(fields, "next_index")
	if err != nil {
		return KvGetManyResult{}, err
	}
	if next == nil && len(items) != len(keys) {
		return KvGetManyResult{}, errors.New("next_index is null but items has fewer elements than keys")
	}
	if next != nil && (*next < 1 || *next != len(items) || len(items) >= len(keys)) {
		return KvGetManyResult{}, errors.New("next_index must be the index of the first key not read")
	}
	return KvGetManyResult{Items: items, NextIndex: next}, nil
}

// kvReadBatch needs the operations: results answer them by position and kind.
func kvReadBatch(raw json.RawMessage, operations []KvBatchOperation) ([]KvBatchResult, error) {
	fields, err := kvObject(raw)
	if err != nil {
		return nil, err
	}
	rawResults, err := kvArray(fields, "results")
	if err != nil {
		return nil, err
	}
	if len(rawResults) != len(operations) {
		return nil, fmt.Errorf("results has %d elements for %d operations", len(rawResults), len(operations))
	}
	results := make([]KvBatchResult, len(rawResults))
	for i, rawResult := range rawResults {
		if results[i], err = kvReadBatchResult(rawResult, operations[i].Kind); err != nil {
			return nil, fmt.Errorf("results[%d]: %w", i, err)
		}
	}
	return results, nil
}
func kvReadBatchResult(raw json.RawMessage, kind KvBatchKind) (KvBatchResult, error) {
	fields, err := kvObject(raw)
	if err != nil {
		return KvBatchResult{}, err
	}
	_, hasVersion := fields["version"]
	_, hasDeleted := fields["deleted"]
	if kind == KvBatchKindPut {
		if hasDeleted {
			return KvBatchResult{}, errors.New("the result of a put carries deleted")
		}
		version, err := kvDecimalField(fields, "version", true)
		return KvBatchResult{Kind: kind, Version: version}, err
	}
	if hasVersion {
		return KvBatchResult{}, errors.New("the result of a delete carries version")
	}
	deleted, err := kvBoolField(fields, "deleted")
	return KvBatchResult{Kind: kind, Deleted: deleted}, err
}
