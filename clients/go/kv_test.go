package vlite

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"io"
	"math"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

type kvSeen struct {
	method, uri, body, authorization, contentType, accept string
}

// kvFake is a server that records every request and answers through its handler.
type kvFake struct {
	client  *Client
	records *Records
	mu      sync.Mutex
	seen    []kvSeen
}

func (f *kvFake) requests() []kvSeen {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]kvSeen(nil), f.seen...)
}

func newKvServer(t *testing.T, handler http.HandlerFunc) *kvFake {
	t.Helper()
	f := &kvFake{}
	s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		f.mu.Lock()
		f.seen = append(f.seen, kvSeen{r.Method, r.URL.RequestURI(), string(body), r.Header.Get("Authorization"), r.Header.Get("Content-Type"), r.Header.Get("Accept")})
		f.mu.Unlock()
		handler(w, r)
	}))
	t.Cleanup(s.Close)
	c, err := New(s.URL, token, 0)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(c.Close)
	records, err := c.Collection("cache")
	if err != nil {
		t.Fatal(err)
	}
	f.client, f.records = c, records
	return f
}

func newKvFake(t *testing.T, status int, response string) *kvFake {
	t.Helper()
	return newKvServer(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("X-Request-ID", "req-kv")
		w.WriteHeader(status)
		if status != http.StatusNoContent {
			_, _ = w.Write([]byte(response))
		}
	})
}

func kvError(code, message string) string {
	return `{"error":{"code":"` + code + `","message":"` + message + `"}}`
}

const kvEntryHello = `{"key":"hello","value_base64":"d29ybGQ=","version":"7","expires_at_ms":null}`

// kvEntryJSON builds an entry object from kvEntryHello's fields. An override is raw JSON; an
// empty override removes the field.
func kvEntryJSON(overrides map[string]string) string {
	var parts []string
	for _, field := range []struct{ name, value string }{
		{"key", `"hello"`}, {"value_base64", `"d29ybGQ="`}, {"version", `"7"`}, {"expires_at_ms", `null`},
	} {
		value := field.value
		if override, ok := overrides[field.name]; ok {
			if override == "" {
				continue
			}
			value = override
		}
		parts = append(parts, strconv.Quote(field.name)+":"+value)
	}
	return "{" + strings.Join(parts, ",") + "}"
}

type kvCall struct {
	name string
	run  func(*Records) error
}

// kvCalls is one valid call of each key-value method.
func kvCalls(ctx context.Context) []kvCall {
	return []kvCall{
		{"KvGet", func(r *Records) error { _, err := r.KvGet(ctx, "app", "hello"); return err }},
		{"KvPut", func(r *Records) error {
			_, err := r.KvPut(ctx, "app", "hello", []byte("world"), KvPutOptions{})
			return err
		}},
		{"KvDelete", func(r *Records) error { _, err := r.KvDelete(ctx, "app", "hello", KvAny()); return err }},
		{"KvList", func(r *Records) error { _, err := r.KvList(ctx, "app", KvListOptions{}); return err }},
		{"KvGetMany", func(r *Records) error { _, err := r.KvGetMany(ctx, []KvKey{{"app", "hello"}}); return err }},
		{"KvBatch", func(r *Records) error {
			_, err := r.KvBatch(ctx, []KvBatchOperation{KvBatchPut("app", "hello", []byte("world"), KvAny())})
			return err
		}},
	}
}

func TestKvExpected(t *testing.T) {
	if (KvExpected{}) != KvAny() {
		t.Fatal("the zero value is not KvAny()")
	}
	for _, tc := range []struct {
		expected    KvExpected
		wire, query string
	}{
		{KvAny(), `"any"`, "any"},
		{KvAbsent(), `"absent"`, "absent"},
		{KvRevision(0), `{"revision":"0"}`, "0"},
		{KvRevision(math.MaxUint64), `{"revision":"18446744073709551615"}`, "18446744073709551615"},
	} {
		wire, err := json.Marshal(tc.expected.wire())
		if err != nil || string(wire) != tc.wire || tc.expected.query() != tc.query {
			t.Errorf("%v: wire %s query %s (%v)", tc.expected, wire, tc.expected.query(), err)
		}
	}
}

func TestKvWire(t *testing.T) {
	ctx := context.Background()
	deadline, maxU64, zero := uint64(1767225600000), uint64(math.MaxUint64), uint64(0)
	after, nextAfter, two := "job/1", "hello", 2
	cases := []struct {
		name     string
		call     func(*Records) (any, error)
		method   string
		uri      string
		body     string
		response string
		want     any
	}{
		{
			name:     "get",
			call:     func(r *Records) (any, error) { return r.KvGet(ctx, "app", "hello") },
			method:   "GET",
			uri:      "/v1/collections/cache/kv/entry?key=hello&namespace=app",
			response: kvEntryHello,
			want:     KvEntry{Key: "hello", Value: []byte("world"), Version: 7},
		},
		{
			name:     "get with the largest version and a deadline",
			call:     func(r *Records) (any, error) { return r.KvGet(ctx, "app", "hello") },
			method:   "GET",
			uri:      "/v1/collections/cache/kv/entry?key=hello&namespace=app",
			response: `{"key":"hello","value_base64":"AAEC","version":"18446744073709551615","expires_at_ms":"1767225600000"}`,
			want:     KvEntry{Key: "hello", Value: []byte{0, 1, 2}, Version: maxU64, ExpiresAtMs: &deadline},
		},
		{
			name:     "get with an empty value and a due deadline",
			call:     func(r *Records) (any, error) { return r.KvGet(ctx, "app", "hello") },
			method:   "GET",
			uri:      "/v1/collections/cache/kv/entry?key=hello&namespace=app",
			response: `{"key":"hello","value_base64":"","version":"1","expires_at_ms":"0"}`,
			want:     KvEntry{Key: "hello", Value: []byte{}, Version: 1, ExpiresAtMs: &zero},
		},
		{
			name:     "get escapes the namespace and the key",
			call:     func(r *Records) (any, error) { return r.KvGet(ctx, "my app", "a+b&c=d/é%") },
			method:   "GET",
			uri:      "/v1/collections/cache/kv/entry?key=a%2Bb%26c%3Dd%2F%C3%A9%25&namespace=my+app",
			response: kvEntryHello,
			want:     KvEntry{Key: "hello", Value: []byte("world"), Version: 7},
		},
		{
			name:     "put",
			call:     func(r *Records) (any, error) { return r.KvPut(ctx, "app", "hello", []byte("world"), KvPutOptions{}) },
			method:   "PUT",
			uri:      "/v1/collections/cache/kv/entry",
			body:     `{"namespace":"app","key":"hello","value_base64":"d29ybGQ=","expected":"any","expires_at_ms":null}`,
			response: `{"version":"8","expires_at_ms":null}`,
			want:     KvPutResult{Version: 8},
		},
		{
			name: "put absent with a deadline",
			call: func(r *Records) (any, error) {
				return r.KvPut(ctx, "app", "hello", []byte{0, 1, 2}, KvPutOptions{Expected: KvAbsent(), ExpiresAtMs: &deadline})
			},
			method:   "PUT",
			uri:      "/v1/collections/cache/kv/entry",
			body:     `{"namespace":"app","key":"hello","value_base64":"AAEC","expected":"absent","expires_at_ms":"1767225600000"}`,
			response: `{"version":"9","expires_at_ms":"1767225600000"}`,
			want:     KvPutResult{Version: 9, ExpiresAtMs: &deadline},
		},
		{
			name: "put an empty value at the largest revision with the largest deadline",
			call: func(r *Records) (any, error) {
				return r.KvPut(ctx, "app", "hello", nil, KvPutOptions{Expected: KvRevision(math.MaxUint64), ExpiresAtMs: &maxU64})
			},
			method:   "PUT",
			uri:      "/v1/collections/cache/kv/entry",
			body:     `{"namespace":"app","key":"hello","value_base64":"","expected":{"revision":"18446744073709551615"},"expires_at_ms":"18446744073709551615"}`,
			response: `{"version":"18446744073709551615","expires_at_ms":"18446744073709551615"}`,
			want:     KvPutResult{Version: maxU64, ExpiresAtMs: &maxU64},
		},
		{
			name: "put with a due deadline",
			call: func(r *Records) (any, error) {
				return r.KvPut(ctx, "app", "hello", []byte("world"), KvPutOptions{Expected: KvRevision(0), ExpiresAtMs: &zero})
			},
			method:   "PUT",
			uri:      "/v1/collections/cache/kv/entry",
			body:     `{"namespace":"app","key":"hello","value_base64":"d29ybGQ=","expected":{"revision":"0"},"expires_at_ms":"0"}`,
			response: `{"version":"2","expires_at_ms":"0"}`,
			want:     KvPutResult{Version: 2, ExpiresAtMs: &zero},
		},
		{
			name:     "delete",
			call:     func(r *Records) (any, error) { return r.KvDelete(ctx, "app", "hello", KvAny()) },
			method:   "DELETE",
			uri:      "/v1/collections/cache/kv/entry?expected=any&key=hello&namespace=app",
			response: `{"deleted":true}`,
			want:     true,
		},
		{
			name:     "delete with the zero expected",
			call:     func(r *Records) (any, error) { return r.KvDelete(ctx, "app", "hello", KvExpected{}) },
			method:   "DELETE",
			uri:      "/v1/collections/cache/kv/entry?expected=any&key=hello&namespace=app",
			response: `{"deleted":false}`,
			want:     false,
		},
		{
			name:     "delete absent",
			call:     func(r *Records) (any, error) { return r.KvDelete(ctx, "app", "hello", KvAbsent()) },
			method:   "DELETE",
			uri:      "/v1/collections/cache/kv/entry?expected=absent&key=hello&namespace=app",
			response: `{"deleted":false}`,
			want:     false,
		},
		{
			name:     "delete at a revision",
			call:     func(r *Records) (any, error) { return r.KvDelete(ctx, "app", "hello", KvRevision(42)) },
			method:   "DELETE",
			uri:      "/v1/collections/cache/kv/entry?expected=42&key=hello&namespace=app",
			response: `{"deleted":true}`,
			want:     true,
		},
		{
			name:     "delete at the largest revision",
			call:     func(r *Records) (any, error) { return r.KvDelete(ctx, "app", "hello", KvRevision(math.MaxUint64)) },
			method:   "DELETE",
			uri:      "/v1/collections/cache/kv/entry?expected=18446744073709551615&key=hello&namespace=app",
			response: `{"deleted":true}`,
			want:     true,
		},
		{
			name:     "list",
			call:     func(r *Records) (any, error) { return r.KvList(ctx, "app", KvListOptions{}) },
			method:   "GET",
			uri:      "/v1/collections/cache/kv/entries?limit=100&namespace=app&prefix=",
			response: `{"items":[` + kvEntryHello + `],"next_after":null}`,
			want:     KvPage{Items: []KvEntry{{Key: "hello", Value: []byte("world"), Version: 7}}},
		},
		{
			name: "list with every option",
			call: func(r *Records) (any, error) {
				return r.KvList(ctx, "app", KvListOptions{Prefix: "job/", After: &after, Limit: 10000})
			},
			method:   "GET",
			uri:      "/v1/collections/cache/kv/entries?after=job%2F1&limit=10000&namespace=app&prefix=job%2F",
			response: `{"items":[],"next_after":null}`,
			want:     KvPage{Items: []KvEntry{}},
		},
		{
			name:     "list with a next page",
			call:     func(r *Records) (any, error) { return r.KvList(ctx, "app", KvListOptions{Limit: 1}) },
			method:   "GET",
			uri:      "/v1/collections/cache/kv/entries?limit=1&namespace=app&prefix=",
			response: `{"items":[` + kvEntryHello + `],"next_after":"hello"}`,
			want:     KvPage{Items: []KvEntry{{Key: "hello", Value: []byte("world"), Version: 7}}, NextAfter: &nextAfter},
		},
		{
			name: "get-many",
			call: func(r *Records) (any, error) {
				return r.KvGetMany(ctx, []KvKey{{"app", "a"}, {"app", "a"}, {"other", "b"}})
			},
			method:   "POST",
			uri:      "/v1/collections/cache/kv/entries/get",
			body:     `{"keys":[{"namespace":"app","key":"a"},{"namespace":"app","key":"a"},{"namespace":"other","key":"b"}]}`,
			response: `{"items":[{"key":"a","value_base64":"AAEC","version":"9","expires_at_ms":"1767225600000"},null,{"key":"b","value_base64":"","version":"3","expires_at_ms":null}],"next_index":null}`,
			want: KvGetManyResult{Items: []*KvEntry{
				{Key: "a", Value: []byte{0, 1, 2}, Version: 9, ExpiresAtMs: &deadline},
				nil,
				{Key: "b", Value: []byte{}, Version: 3},
			}},
		},
		{
			name: "get-many partial result is handed over unchanged and not continued",
			call: func(r *Records) (any, error) {
				return r.KvGetMany(ctx, []KvKey{{"app", "a"}, {"app", "b"}, {"app", "c"}})
			},
			method:   "POST",
			uri:      "/v1/collections/cache/kv/entries/get",
			body:     `{"keys":[{"namespace":"app","key":"a"},{"namespace":"app","key":"b"},{"namespace":"app","key":"c"}]}`,
			response: `{"items":[{"key":"a","value_base64":"AAEC","version":"9","expires_at_ms":null},null],"next_index":2}`,
			want: KvGetManyResult{Items: []*KvEntry{
				{Key: "a", Value: []byte{0, 1, 2}, Version: 9},
				nil,
			}, NextIndex: &two},
		},
		{
			name:     "get-many without keys",
			call:     func(r *Records) (any, error) { return r.KvGetMany(ctx, nil) },
			method:   "POST",
			uri:      "/v1/collections/cache/kv/entries/get",
			body:     `{"keys":[]}`,
			response: `{"items":[],"next_index":null}`,
			want:     KvGetManyResult{Items: []*KvEntry{}},
		},
		{
			name: "batch",
			call: func(r *Records) (any, error) {
				return r.KvBatch(ctx, []KvBatchOperation{
					KvBatchPut("app", "a", []byte{0, 1, 2}, KvAbsent()),
					KvBatchPut("app", "b", nil, KvRevision(42)),
					KvBatchDelete("app", "c", KvAny()),
					KvBatchDelete("other", "c", KvRevision(math.MaxUint64)),
				})
			},
			method:   "POST",
			uri:      "/v1/collections/cache/kv/batch",
			body:     `{"operations":[{"op":"put","namespace":"app","key":"a","value_base64":"AAEC","expected":"absent"},{"op":"put","namespace":"app","key":"b","value_base64":"","expected":{"revision":"42"}},{"op":"delete","namespace":"app","key":"c","expected":"any"},{"op":"delete","namespace":"other","key":"c","expected":{"revision":"18446744073709551615"}}]}`,
			response: `{"results":[{"version":"43"},{"version":"43"},{"deleted":false},{"deleted":true}]}`,
			want: []KvBatchResult{
				{Kind: KvBatchKindPut, Version: 43},
				{Kind: KvBatchKindPut, Version: 43},
				{Kind: KvBatchKindDelete},
				{Kind: KvBatchKindDelete, Deleted: true},
			},
		},
		{
			name:     "empty batch",
			call:     func(r *Records) (any, error) { return r.KvBatch(ctx, nil) },
			method:   "POST",
			uri:      "/v1/collections/cache/kv/batch",
			body:     `{"operations":[]}`,
			response: `{"results":[]}`,
			want:     []KvBatchResult{},
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			f := newKvFake(t, 200, tc.response)
			got, err := tc.call(f.records)
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(got, tc.want) {
				t.Errorf("result %+v, want %+v", got, tc.want)
			}
			seen := f.requests()
			if len(seen) != 1 {
				t.Fatalf("%d requests, want 1", len(seen))
			}
			request := seen[0]
			if request.method != tc.method || request.uri != tc.uri || request.body != tc.body {
				t.Errorf("request %s %s %s\nwant    %s %s %s", request.method, request.uri, request.body, tc.method, tc.uri, tc.body)
			}
			contentType := ""
			if tc.body != "" {
				contentType = "application/json"
			}
			if request.authorization != "Bearer "+token || request.accept != "application/json" || request.contentType != contentType {
				t.Errorf("headers %+v", request)
			}
		})
	}
}

// TestKvGetManyCallerContinues shows the intended loop: the client never continues a partial
// result, so the caller asks again for keys[*NextIndex:].
func TestKvGetManyCallerContinues(t *testing.T) {
	var f *kvFake
	f = newKvServer(t, func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			Keys []KvKey `json:"keys"`
		}
		seen := f.requests()
		if err := json.Unmarshal([]byte(seen[len(seen)-1].body), &body); err != nil {
			t.Error(err)
		}
		read := min(2, len(body.Keys))
		items := make([]string, read)
		for i := range items {
			items[i] = kvEntryJSON(map[string]string{"key": strconv.Quote(body.Keys[i].Key)})
		}
		next := "null"
		if len(body.Keys) > read {
			next = strconv.Itoa(read)
		}
		_, _ = w.Write([]byte(`{"items":[` + strings.Join(items, ",") + `],"next_index":` + next + `}`))
	})
	keys := []KvKey{{"app", "k0"}, {"app", "k1"}, {"app", "k2"}, {"app", "k3"}, {"app", "k4"}}
	pending := keys
	var entries []*KvEntry
	for len(pending) > 0 {
		page, err := f.records.KvGetMany(context.Background(), pending)
		if err != nil {
			t.Fatal(err)
		}
		entries = append(entries, page.Items...)
		if page.NextIndex == nil {
			break
		}
		pending = pending[*page.NextIndex:]
	}
	if len(entries) != len(keys) {
		t.Fatalf("%d entries for %d keys", len(entries), len(keys))
	}
	for i, entry := range entries {
		if entry == nil || entry.Key != keys[i].Key {
			t.Errorf("entries[%d] = %+v", i, entry)
		}
	}
	bodies := []string{
		`{"keys":[{"namespace":"app","key":"k0"},{"namespace":"app","key":"k1"},{"namespace":"app","key":"k2"},{"namespace":"app","key":"k3"},{"namespace":"app","key":"k4"}]}`,
		`{"keys":[{"namespace":"app","key":"k2"},{"namespace":"app","key":"k3"},{"namespace":"app","key":"k4"}]}`,
		`{"keys":[{"namespace":"app","key":"k4"}]}`,
	}
	seen := f.requests()
	if len(seen) != len(bodies) {
		t.Fatalf("%d requests, want %d", len(seen), len(bodies))
	}
	for i, want := range bodies {
		if seen[i].body != want {
			t.Errorf("request %d: %s, want %s", i, seen[i].body, want)
		}
	}
}

func TestKvEscaping(t *testing.T) {
	ctx := context.Background()
	for _, text := range []string{"plain", "with space", "plus+sign", "a&b=c", "100%", "%2B", "é", "日本語", "tab\tnew\nline", "nul\x00", "a;b", "a#b?c", "slash/and\\back", "emoji😀", " ", "+", "~-._"} {
		for _, call := range []struct {
			name     string
			response string
			run      func(*Records) error
			want     url.Values
		}{
			{"get", kvEntryHello, func(r *Records) error { _, err := r.KvGet(ctx, text, text); return err },
				url.Values{"namespace": {text}, "key": {text}}},
			{"delete", `{"deleted":false}`, func(r *Records) error { _, err := r.KvDelete(ctx, text, text, KvAny()); return err },
				url.Values{"namespace": {text}, "key": {text}, "expected": {"any"}}},
			{"list", `{"items":[],"next_after":null}`, func(r *Records) error {
				_, err := r.KvList(ctx, text, KvListOptions{Prefix: text, After: &text, Limit: 5})
				return err
			}, url.Values{"namespace": {text}, "prefix": {text}, "after": {text}, "limit": {"5"}}},
		} {
			t.Run(call.name+" "+strconv.Quote(text), func(t *testing.T) {
				f := newKvFake(t, 200, call.response)
				if err := call.run(f.records); err != nil {
					t.Fatal(err)
				}
				seen := f.requests()
				if len(seen) != 1 {
					t.Fatalf("%d requests, want 1", len(seen))
				}
				parsed, err := url.ParseRequestURI(seen[0].uri)
				if err != nil {
					t.Fatal(err)
				}
				if got := parsed.Query(); !reflect.DeepEqual(got, call.want) {
					t.Errorf("the server decodes %q, want %q (%s)", got, call.want, seen[0].uri)
				}
			})
		}
	}
}

// TestKvBodiesCarryAnyText decodes the JSON the client sends, as a server does, to check that
// namespaces and keys of any valid UTF-8 survive the request body unchanged.
func TestKvBodiesCarryAnyText(t *testing.T) {
	ctx := context.Background()
	type address struct {
		Namespace string `json:"namespace"`
		Key       string `json:"key"`
	}
	for _, text := range []string{`quote"and\backslash`, "<tag>&amp;", "line\nbreak\ttab\x00nul", "\u2028\u2029", "é日本語😀", "\x7f\u0080"} {
		t.Run(strconv.Quote(text), func(t *testing.T) {
			put := newKvFake(t, 200, `{"version":"1","expires_at_ms":null}`)
			if _, err := put.records.KvPut(ctx, text, text, []byte("v"), KvPutOptions{}); err != nil {
				t.Fatal(err)
			}
			var sentPut address
			if err := json.Unmarshal([]byte(put.requests()[0].body), &sentPut); err != nil || sentPut != (address{text, text}) {
				t.Errorf("put sent %+v (%v)", sentPut, err)
			}
			many := newKvFake(t, 200, `{"items":[null],"next_index":null}`)
			if _, err := many.records.KvGetMany(ctx, []KvKey{{text, text}}); err != nil {
				t.Fatal(err)
			}
			var sentMany struct{ Keys []address }
			if err := json.Unmarshal([]byte(many.requests()[0].body), &sentMany); err != nil || !reflect.DeepEqual(sentMany.Keys, []address{{text, text}}) {
				t.Errorf("get-many sent %+v (%v)", sentMany, err)
			}
			batch := newKvFake(t, 200, `{"results":[{"version":"1"},{"deleted":false}]}`)
			if _, err := batch.records.KvBatch(ctx, []KvBatchOperation{KvBatchPut(text, text, nil, KvAny()), KvBatchDelete(text, text+"2", KvAny())}); err != nil {
				t.Fatal(err)
			}
			var sentBatch struct{ Operations []address }
			if err := json.Unmarshal([]byte(batch.requests()[0].body), &sentBatch); err != nil || !reflect.DeepEqual(sentBatch.Operations, []address{{text, text}, {text, text + "2"}}) {
				t.Errorf("batch sent %+v (%v)", sentBatch, err)
			}
		})
	}
}

func TestKvBoundsAreAccepted(t *testing.T) {
	ctx := context.Background()
	longNamespace, longKey := strings.Repeat("n", 255), strings.Repeat("k", 1024)
	multibyteNamespace, multibyteKey := strings.Repeat("é", 127)+"a", strings.Repeat("é", 512)
	largest, longAfter := make([]byte, 1047290), strings.Repeat("a", 1024)
	manyKeys := make([]KvKey, 1000)
	manyDeletes := make([]KvBatchOperation, 1000)
	for i := range manyKeys {
		manyKeys[i] = KvKey{"app", "k" + strconv.Itoa(i)}
		manyDeletes[i] = KvBatchDelete("app", "k"+strconv.Itoa(i), KvAny())
	}
	fullBatch := make([]KvBatchOperation, 5)
	for i := range fullBatch {
		fullBatch[i] = KvBatchPut("app", "k"+strconv.Itoa(i), make([]byte, 1047290), KvAny())
	}
	fullBatch[4].Value = make([]byte, 5144)
	cases := []struct {
		name     string
		call     func(*Records) error
		response string
		bodyHas  string
	}{
		{"largest namespace and key", func(r *Records) error { _, err := r.KvGet(ctx, longNamespace, longKey); return err },
			kvEntryJSON(map[string]string{"key": strconv.Quote(longKey)}), ""},
		{"multibyte namespace and key at the bounds", func(r *Records) error { _, err := r.KvGet(ctx, multibyteNamespace, multibyteKey); return err },
			kvEntryHello, ""},
		{"largest value", func(r *Records) error { _, err := r.KvPut(ctx, "app", "k", largest, KvPutOptions{}); return err },
			`{"version":"1","expires_at_ms":null}`, `"value_base64":"` + base64.StdEncoding.EncodeToString(largest) + `"`},
		{"largest value in a response", func(r *Records) error { _, err := r.KvGet(ctx, "app", "k"); return err },
			kvEntryJSON(map[string]string{"value_base64": strconv.Quote(base64.StdEncoding.EncodeToString(largest))}), ""},
		{"largest prefix, after and limit", func(r *Records) error {
			_, err := r.KvList(ctx, longNamespace, KvListOptions{Prefix: longKey, After: &longAfter, Limit: 10000})
			return err
		}, `{"items":[` + kvEntryJSON(map[string]string{"key": strconv.Quote(longKey)}) + `],"next_after":` + strconv.Quote(longKey) + `}`, ""},
		{"most keys", func(r *Records) error { _, err := r.KvGetMany(ctx, manyKeys); return err },
			`{"items":[` + strings.TrimSuffix(strings.Repeat("null,", 1000), ",") + `],"next_index":null}`, ""},
		{"most operations", func(r *Records) error { _, err := r.KvBatch(ctx, manyDeletes); return err },
			`{"results":[` + strings.TrimSuffix(strings.Repeat(`{"deleted":false},`, 1000), ",") + `]}`, ""},
		{"most value bytes", func(r *Records) error { _, err := r.KvBatch(ctx, fullBatch); return err },
			`{"results":[` + strings.TrimSuffix(strings.Repeat(`{"version":"1"},`, 5), ",") + `]}`, ""},
		{"the same key in two namespaces and keys that join into the same text", func(r *Records) error {
			_, err := r.KvBatch(ctx, []KvBatchOperation{
				KvBatchDelete("a", "bc", KvAny()), KvBatchDelete("ab", "c", KvAny()),
				KvBatchPut("x", "same", nil, KvAny()), KvBatchPut("y", "same", nil, KvAny()),
			})
			return err
		}, `{"results":[{"deleted":true},{"deleted":true},{"version":"1"},{"version":"1"}]}`, ""},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			f := newKvFake(t, 200, tc.response)
			if err := tc.call(f.records); err != nil {
				t.Fatal(err)
			}
			seen := f.requests()
			if len(seen) != 1 {
				t.Fatalf("%d requests, want 1", len(seen))
			}
			if !strings.Contains(seen[0].body, tc.bodyHas) {
				t.Error("the request body lacks the encoded value")
			}
		})
	}
}

func TestKvLocalValidation(t *testing.T) {
	ctx := context.Background()
	type invalid struct {
		name string
		run  func(*Records) error
		want string
	}
	var cases []invalid
	for _, a := range []struct{ name, namespace, key, want string }{
		{"empty namespace", "", "k", "namespace"},
		{"long namespace", strings.Repeat("n", 256), "k", "namespace"},
		{"multibyte namespace over 255 bytes", strings.Repeat("é", 128), "k", "namespace"},
		{"invalid UTF-8 namespace", "\xff", "k", "namespace"},
		{"surrogate namespace", "\xed\xa0\x80", "k", "namespace"},
		{"empty key", "n", "", "key"},
		{"long key", "n", strings.Repeat("k", 1025), "key"},
		{"multibyte key over 1024 bytes", "n", strings.Repeat("é", 513), "key"},
		{"invalid UTF-8 key", "n", "k\xc3", "key"},
	} {
		cases = append(cases,
			invalid{"get " + a.name, func(r *Records) error { _, err := r.KvGet(ctx, a.namespace, a.key); return err }, a.want},
			invalid{"put " + a.name, func(r *Records) error {
				_, err := r.KvPut(ctx, a.namespace, a.key, []byte("v"), KvPutOptions{})
				return err
			}, a.want},
			invalid{"delete " + a.name, func(r *Records) error { _, err := r.KvDelete(ctx, a.namespace, a.key, KvAny()); return err }, a.want},
			invalid{"get-many " + a.name, func(r *Records) error {
				_, err := r.KvGetMany(ctx, []KvKey{{"ok", "ok"}, {a.namespace, a.key}})
				return err
			}, "keys[1]: " + a.want},
			invalid{"batch put " + a.name, func(r *Records) error {
				_, err := r.KvBatch(ctx, []KvBatchOperation{KvBatchPut("ok", "ok", nil, KvAny()), KvBatchPut(a.namespace, a.key, nil, KvAny())})
				return err
			}, "operations[1]: " + a.want},
			invalid{"batch delete " + a.name, func(r *Records) error {
				_, err := r.KvBatch(ctx, []KvBatchOperation{KvBatchDelete("ok", "ok", KvAny()), KvBatchDelete(a.namespace, a.key, KvAny())})
				return err
			}, "operations[1]: " + a.want},
		)
		if a.want == "namespace" {
			cases = append(cases, invalid{"list " + a.name, func(r *Records) error { _, err := r.KvList(ctx, a.namespace, KvListOptions{}); return err }, a.want})
		}
	}
	tooLong, empty, notUTF8 := strings.Repeat("a", 1025), "", "\xff"
	tooMany := make([]KvKey, 1001)
	tooManyOperations := make([]KvBatchOperation, 1001)
	for i := range tooMany {
		tooMany[i] = KvKey{"app", "k" + strconv.Itoa(i)}
		tooManyOperations[i] = KvBatchDelete("app", "k"+strconv.Itoa(i), KvAny())
	}
	puts := func(sizes ...int) []KvBatchOperation {
		var operations []KvBatchOperation
		for i, size := range sizes {
			operations = append(operations, KvBatchPut("app", "k"+strconv.Itoa(i), make([]byte, size), KvAny()))
		}
		return operations
	}
	batch := func(operations ...KvBatchOperation) func(*Records) error {
		return func(r *Records) error { _, err := r.KvBatch(ctx, operations); return err }
	}
	list := func(opts KvListOptions) func(*Records) error {
		return func(r *Records) error { _, err := r.KvList(ctx, "app", opts); return err }
	}
	cases = append(cases,
		invalid{"put value over the bound", func(r *Records) error {
			_, err := r.KvPut(ctx, "app", "k", make([]byte, 1047291), KvPutOptions{})
			return err
		}, "value"},
		invalid{"list limit below the bound", list(KvListOptions{Limit: -1}), "limit"},
		invalid{"list limit above the bound", list(KvListOptions{Limit: 10001}), "limit"},
		invalid{"list prefix over 1024 bytes", list(KvListOptions{Prefix: tooLong}), "prefix"},
		invalid{"list prefix that is not UTF-8", list(KvListOptions{Prefix: notUTF8}), "prefix"},
		invalid{"list empty after", list(KvListOptions{After: &empty}), "after"},
		invalid{"list after over 1024 bytes", list(KvListOptions{After: &tooLong}), "after"},
		invalid{"list after that is not UTF-8", list(KvListOptions{After: &notUTF8}), "after"},
		invalid{"get-many over 1000 keys", func(r *Records) error { _, err := r.KvGetMany(ctx, tooMany); return err }, "1000 keys"},
		invalid{"batch over 1000 operations", batch(tooManyOperations...), "1000 operations"},
		invalid{"batch value over the bound", batch(puts(1047291)...), "operations[0]: value"},
		invalid{"batch value bytes over the bound", batch(puts(1047290, 1047290, 1047290, 1047290, 5145)...), "4194304 value bytes"},
		invalid{"batch repeats a put", batch(KvBatchPut("app", "k", nil, KvAny()), KvBatchPut("app", "k", nil, KvAny())),
			"operations[1]: namespace and key repeat"},
		invalid{"batch repeats a put as a delete", batch(KvBatchPut("app", "k", nil, KvAny()), KvBatchDelete("app", "k", KvAbsent())),
			"operations[1]: namespace and key repeat"},
		invalid{"batch operation of no kind", batch(KvBatchOperation{Namespace: "app", Key: "k"}), "operations[0]: kind"},
		invalid{"batch operation of another kind", batch(KvBatchOperation{Kind: "check", Namespace: "app", Key: "k"}), "operations[0]: kind"},
		invalid{"batch delete with a value", batch(KvBatchOperation{Kind: KvBatchKindDelete, Namespace: "app", Key: "k", Value: []byte{}}),
			"operations[0]: a delete takes no value"},
	)
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			f := newKvFake(t, 200, `{}`)
			err := tc.run(f.records)
			if err == nil || !strings.Contains(err.Error(), tc.want) {
				t.Fatalf("error %v, want one naming %q", err, tc.want)
			}
			var api *APIError
			var transport *TransportError
			var unsupported *UnsupportedError
			if errors.As(err, &api) || errors.As(err, &transport) || errors.As(err, &unsupported) {
				t.Fatalf("a local validation failure is a server or transport error: %v", err)
			}
			if n := len(f.requests()); n != 0 {
				t.Fatalf("%d requests were sent", n)
			}
		})
	}
}

func TestKvNeedsACollection(t *testing.T) {
	f := newKvFake(t, 200, `{}`)
	for _, call := range kvCalls(context.Background()) {
		t.Run(call.name, func(t *testing.T) {
			err := call.run(f.client.Records)
			if err == nil || !strings.Contains(err.Error(), "select a collection") {
				t.Fatal(err)
			}
		})
	}
	if n := len(f.requests()); n != 0 {
		t.Fatalf("%d requests were sent", n)
	}
}

func TestKvUnsupportedRoute(t *testing.T) {
	for _, call := range kvCalls(context.Background()) {
		t.Run(call.name, func(t *testing.T) {
			f := newKvFake(t, 404, kvError("not_found", "not found"))
			err := call.run(f.records)
			var unsupported *UnsupportedError
			if !errors.As(err, &unsupported) || !errors.Is(err, errors.ErrUnsupported) {
				t.Fatalf("%T %v", err, err)
			}
			var api *APIError
			if !errors.As(err, &api) || api != unsupported.Cause || api.Status != 404 || api.Code != "not_found" || api.RequestID != "req-kv" {
				t.Fatalf("the unsupported error does not carry the API error: %v", err)
			}
			if !strings.Contains(err.Error(), "no fallback performed") {
				t.Error(err)
			}
			if n := len(f.requests()); n != 1 {
				t.Fatalf("%d requests were sent: a retry or fallback ran", n)
			}
		})
	}
}

func TestKvAPIErrors(t *testing.T) {
	for _, tc := range []struct {
		status        int
		code, message string
	}{
		{412, "precondition_failed", "revision precondition failed"},
		{409, "kv_slots_exhausted", "all 16 slots are occupied"},
		{409, "not_kv_collection", "collection does not have one dimension"},
		{409, "upgrade_required", "expiry needs format 5"},
		{409, "graph_attached_record", "record is attached to a graph"},
		{422, "invalid_input", "operations[1]: value too large"},
		{422, "invalid_json", "unknown field"},
		{404, "kv_not_found", "KV entry not found"},
		{404, "collection_not_found", "collection not found"},
		{400, "invalid_query", "invalid query parameters"},
		{400, "not_found", "only a 404 is an unknown route"},
		{403, "read_only", "database is read-only"},
		{429, "quota_exceeded", "record quota exceeded"},
		{503, "commit_indeterminate", "commit outcome is unknown"},
	} {
		for _, call := range kvCalls(context.Background()) {
			t.Run(strconv.Itoa(tc.status)+" "+tc.code+" "+call.name, func(t *testing.T) {
				f := newKvFake(t, tc.status, kvError(tc.code, tc.message))
				err := call.run(f.records)
				var api *APIError
				if !errors.As(err, &api) || api.Status != tc.status || api.Code != tc.code || api.Message != tc.message || api.RequestID != "req-kv" {
					t.Fatalf("%T %v", err, err)
				}
				var unsupported *UnsupportedError
				if errors.As(err, &unsupported) || errors.Is(err, errors.ErrUnsupported) {
					t.Fatalf("an ordinary API error is reported as an unsupported route: %v", err)
				}
				if n := len(f.requests()); n != 1 {
					t.Fatalf("%d requests were sent: a retry or fallback ran", n)
				}
			})
		}
	}
}

func TestKvNonstandardNotFoundIsNotAnUnsupportedRoute(t *testing.T) {
	for _, call := range kvCalls(context.Background()) {
		t.Run(call.name, func(t *testing.T) {
			f := newKvServer(t, func(w http.ResponseWriter, r *http.Request) {
				w.WriteHeader(404)
				_, _ = w.Write([]byte("<html>Not Found</html>"))
			})
			err := call.run(f.records)
			var api *APIError
			if !errors.As(err, &api) || api.Status != 404 || api.Code != "http_error" || errors.Is(err, errors.ErrUnsupported) {
				t.Fatalf("%T %v", err, err)
			}
		})
	}
}

func TestKvIgnoresUnknownResponseFields(t *testing.T) {
	ctx := context.Background()
	extra := `,"extra":{"nested":[1,null]}`
	entry := strings.TrimSuffix(kvEntryHello, "}") + extra + "}"
	for _, tc := range []struct {
		name     string
		run      func(*Records) error
		response string
	}{
		{"KvGet", func(r *Records) error { _, err := r.KvGet(ctx, "app", "hello"); return err }, entry},
		{"KvPut", func(r *Records) error { _, err := r.KvPut(ctx, "app", "hello", nil, KvPutOptions{}); return err },
			`{"version":"8","expires_at_ms":null` + extra + `}`},
		{"KvDelete", func(r *Records) error { _, err := r.KvDelete(ctx, "app", "hello", KvAny()); return err },
			`{"deleted":true` + extra + `}`},
		{"KvList", func(r *Records) error { _, err := r.KvList(ctx, "app", KvListOptions{}); return err },
			`{"items":[` + entry + `],"next_after":null` + extra + `}`},
		{"KvGetMany", func(r *Records) error { _, err := r.KvGetMany(ctx, []KvKey{{"app", "hello"}}); return err },
			`{"items":[` + entry + `],"next_index":null` + extra + `}`},
		{"KvBatch", func(r *Records) error {
			_, err := r.KvBatch(ctx, []KvBatchOperation{KvBatchPut("app", "a", nil, KvAny()), KvBatchDelete("app", "b", KvAny())})
			return err
		}, `{"results":[{"version":"1"` + extra + `},{"deleted":false` + extra + `}]` + extra + `}`},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if err := tc.run(newKvFake(t, 200, tc.response).records); err != nil {
				t.Fatal(err)
			}
		})
	}
}

type kvBody struct{ name, text string }

// kvBadEntries are entry objects that a client must refuse, wrapped for the response they sit in.
func kvBadEntries(wrap func(entry string) string) []kvBody {
	override := func(name, field, value string) kvBody {
		return kvBody{name, wrap(kvEntryJSON(map[string]string{field: value}))}
	}
	bodies := []kvBody{
		{"array", wrap(`[]`)},
		{"string", wrap(`"hello"`)},
		{"number", wrap(`7`)},
		{"empty object", wrap(`{}`)},
		override("missing key", "key", ""),
		override("null key", "key", `null`),
		override("numeric key", "key", `5`),
		override("empty key", "key", `""`),
		override("key over 1024 bytes", "key", `"`+strings.Repeat("k", 1025)+`"`),
		override("missing value", "value_base64", ""),
		override("null value", "value_base64", `null`),
		override("numeric value", "value_base64", `5`),
		override("unpadded base64", "value_base64", `"AAE"`),
		override("nonzero trailing bits", "value_base64", `"AAF="`),
		override("nonzero trailing bits of one byte", "value_base64", `"AB=="`),
		override("line break in base64", "value_base64", `"AAEC\r\n"`),
		override("space in base64", "value_base64", `"AA EC"`),
		override("URL-safe base64", "value_base64", `"-_8="`),
		override("base64 over the encoded bound", "value_base64", `"`+strings.Repeat("A", 1396392)+`"`),
		override("value one byte over the bound", "value_base64", `"`+strings.Repeat("AAAA", 349097)+`"`),
		override("missing version", "version", ""),
		override("null version", "version", `null`),
		override("numeric version", "version", `7`),
		override("zero version", "version", `"0"`),
		override("version with a leading zero", "version", `"07"`),
		override("negative version", "version", `"-1"`),
		override("version with a sign", "version", `"+1"`),
		override("version with a space", "version", `" 7"`),
		override("fractional version", "version", `"7.0"`),
		override("version over u64", "version", `"18446744073709551616"`),
		override("empty version", "version", `""`),
		override("missing expiry", "expires_at_ms", ""),
		override("numeric expiry", "expires_at_ms", `5`),
		override("text expiry", "expires_at_ms", `"soon"`),
		override("expiry with a leading zero", "expires_at_ms", `"05"`),
		override("expiry over u64", "expires_at_ms", `"18446744073709551616"`),
		{"key that is not UTF-8", wrap("{\"key\":\"a\xffb\",\"value_base64\":\"\",\"version\":\"7\",\"expires_at_ms\":null}")},
	}
	return bodies
}

func TestKvInvalidResponses(t *testing.T) {
	ctx := context.Background()
	entryA := `{"key":"a","value_base64":"","version":"7","expires_at_ms":null}`
	entryZ := strings.Replace(entryA, `"a"`, `"z"`, 1)
	bare := func(entry string) string { return entry }
	inPage := func(entry string) string { return `{"items":[` + entry + `],"next_after":null}` }
	inGetMany := func(entry string) string { return `{"items":[` + entry + `],"next_index":null}` }
	envelopes := []kvBody{
		{"empty body", ""},
		{"not JSON", "bad"},
		{"null", "null"},
		{"array", "[]"},
		{"string", `"x"`},
		{"number", "7"},
		{"trailing JSON", "{} {}"},
	}
	groups := []struct {
		name   string
		run    func(*Records) error
		bodies []kvBody
	}{
		{"KvGet", func(r *Records) error { _, err := r.KvGet(ctx, "app", "hello"); return err }, kvBadEntries(bare)},
		{"KvPut", func(r *Records) error {
			_, err := r.KvPut(ctx, "app", "hello", []byte("world"), KvPutOptions{})
			return err
		}, []kvBody{
			{"missing version", `{"expires_at_ms":null}`},
			{"zero version", `{"version":"0","expires_at_ms":null}`},
			{"numeric version", `{"version":7,"expires_at_ms":null}`},
			{"version with a leading zero", `{"version":"07","expires_at_ms":null}`},
			{"missing expiry", `{"version":"7"}`},
			{"numeric expiry", `{"version":"7","expires_at_ms":5}`},
			{"expiry over u64", `{"version":"7","expires_at_ms":"18446744073709551616"}`},
		}},
		{"KvDelete", func(r *Records) error { _, err := r.KvDelete(ctx, "app", "hello", KvAny()); return err }, []kvBody{
			{"missing deleted", `{}`},
			{"null deleted", `{"deleted":null}`},
			{"text deleted", `{"deleted":"true"}`},
			{"numeric deleted", `{"deleted":1}`},
			{"body that is not UTF-8", "{\"deleted\":true,\"note\":\"\xff\"}"},
		}},
		{"KvList", func(r *Records) error { _, err := r.KvList(ctx, "app", KvListOptions{}); return err }, append(kvBadEntries(inPage),
			kvBody{"missing items", `{"next_after":null}`},
			kvBody{"null items", `{"items":null,"next_after":null}`},
			kvBody{"object items", `{"items":{},"next_after":null}`},
			kvBody{"null item", `{"items":[null],"next_after":null}`},
			kvBody{"missing next_after", `{"items":[]}`},
			kvBody{"empty next_after", `{"items":[],"next_after":""}`},
			kvBody{"numeric next_after", `{"items":[],"next_after":5}`},
			kvBody{"next_after over 1024 bytes", `{"items":[],"next_after":"` + strings.Repeat("k", 1025) + `"}`},
		)},
		{"KvGetMany of one key", func(r *Records) error { _, err := r.KvGetMany(ctx, []KvKey{{"app", "hello"}}); return err }, kvBadEntries(inGetMany)},
		{"KvGetMany of three keys", func(r *Records) error {
			_, err := r.KvGetMany(ctx, []KvKey{{"app", "a"}, {"app", "b"}, {"app", "c"}})
			return err
		}, []kvBody{
			{"missing items", `{"next_index":null}`},
			{"null items", `{"items":null,"next_index":null}`},
			{"object items", `{"items":{},"next_index":null}`},
			{"missing next_index", `{"items":[null,null,null]}`},
			{"more items than keys", `{"items":[null,null,null,null],"next_index":null}`},
			{"fewer items than keys without next_index", `{"items":[null,null],"next_index":null}`},
			{"no items without next_index", `{"items":[],"next_index":null}`},
			{"next_index zero", `{"items":[],"next_index":0}`},
			{"next_index below the items", `{"items":[null,null],"next_index":1}`},
			{"next_index beyond the items", `{"items":[null],"next_index":2}`},
			{"next_index with every key read", `{"items":[null,null,null],"next_index":3}`},
			{"negative next_index", `{"items":[null,null],"next_index":-2}`},
			{"text next_index", `{"items":[null,null],"next_index":"2"}`},
			{"fractional next_index", `{"items":[null,null],"next_index":2.0}`},
			{"exponent next_index", `{"items":[null,null],"next_index":2e0}`},
			{"entry of another key", `{"items":[` + entryA + `,` + entryZ + `],"next_index":2}`},
		}},
		{"KvBatch of a put, a delete and a put", func(r *Records) error {
			_, err := r.KvBatch(ctx, []KvBatchOperation{
				KvBatchPut("app", "a", nil, KvAny()),
				KvBatchDelete("app", "b", KvAny()),
				KvBatchPut("app", "c", nil, KvAny()),
			})
			return err
		}, []kvBody{
			{"missing results", `{}`},
			{"null results", `{"results":null}`},
			{"object results", `{"results":{}}`},
			{"too few results", `{"results":[{"version":"5"},{"deleted":true}]}`},
			{"too many results", `{"results":[{"version":"5"},{"deleted":true},{"version":"5"},{"deleted":true}]}`},
			{"no results", `{"results":[]}`},
			{"put answered as a delete", `{"results":[{"deleted":true},{"deleted":true},{"version":"5"}]}`},
			{"delete answered as a put", `{"results":[{"version":"5"},{"version":"5"},{"version":"5"}]}`},
			{"put with both", `{"results":[{"version":"5","deleted":true},{"deleted":true},{"version":"5"}]}`},
			{"delete with both", `{"results":[{"version":"5"},{"version":"5","deleted":true},{"version":"5"}]}`},
			{"empty result", `{"results":[{},{"deleted":true},{"version":"5"}]}`},
			{"null result", `{"results":[null,{"deleted":true},{"version":"5"}]}`},
			{"array result", `{"results":[[],{"deleted":true},{"version":"5"}]}`},
			{"zero version", `{"results":[{"version":"0"},{"deleted":true},{"version":"5"}]}`},
			{"numeric version", `{"results":[{"version":5},{"deleted":true},{"version":"5"}]}`},
			{"version with a leading zero", `{"results":[{"version":"05"},{"deleted":true},{"version":"5"}]}`},
			{"version over u64", `{"results":[{"version":"18446744073709551616"},{"deleted":true},{"version":"5"}]}`},
			{"text deleted", `{"results":[{"version":"5"},{"deleted":"true"},{"version":"5"}]}`},
			{"null deleted", `{"results":[{"version":"5"},{"deleted":null},{"version":"5"}]}`},
		}},
	}
	for _, g := range groups {
		for _, bad := range append(append([]kvBody(nil), envelopes...), g.bodies...) {
			t.Run(g.name+" "+bad.name, func(t *testing.T) {
				f := newKvFake(t, 200, bad.text)
				err := g.run(f.records)
				var transport *TransportError
				if !errors.As(err, &transport) || transport.Code != "transport_error" {
					t.Fatalf("%T %v", err, err)
				}
				var api *APIError
				if errors.As(err, &api) {
					t.Fatalf("an invalid success response is reported as an API error: %v", err)
				}
				if n := len(f.requests()); n != 1 {
					t.Fatalf("%d requests were sent: a retry ran", n)
				}
			})
		}
	}
}

func TestKvNoContentIsAnInvalidResponse(t *testing.T) {
	for _, call := range kvCalls(context.Background()) {
		t.Run(call.name, func(t *testing.T) {
			f := newKvFake(t, 204, "")
			err := call.run(f.records)
			var transport *TransportError
			if !errors.As(err, &transport) {
				t.Fatalf("%T %v", err, err)
			}
		})
	}
}

func TestKvContext(t *testing.T) {
	t.Run("canceled before the call", func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		cancel()
		for _, call := range kvCalls(ctx) {
			t.Run(call.name, func(t *testing.T) {
				f := newKvFake(t, 200, `{}`)
				err := call.run(f.records)
				var transport *TransportError
				if !errors.As(err, &transport) || !errors.Is(err, context.Canceled) {
					t.Fatalf("%T %v", err, err)
				}
				if n := len(f.requests()); n != 0 {
					t.Fatalf("%d requests were sent", n)
				}
			})
		}
	})
	t.Run("canceled while the server is answering", func(t *testing.T) {
		for i, probe := range kvCalls(context.Background()) {
			t.Run(probe.name, func(t *testing.T) {
				ctx, cancel := context.WithCancel(context.Background())
				defer cancel()
				arrived := make(chan struct{})
				var once sync.Once
				f := newKvServer(t, func(w http.ResponseWriter, r *http.Request) {
					once.Do(func() { close(arrived) })
					<-r.Context().Done()
				})
				go func() {
					<-arrived
					cancel()
				}()
				err := kvCalls(ctx)[i].run(f.records)
				var transport *TransportError
				if !errors.As(err, &transport) || !errors.Is(err, context.Canceled) {
					t.Fatalf("%T %v", err, err)
				}
				if n := len(f.requests()); n != 1 {
					t.Fatalf("%d requests were sent", n)
				}
			})
		}
	})
	t.Run("deadline passes while the server is answering", func(t *testing.T) {
		ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
		defer cancel()
		f := newKvServer(t, func(w http.ResponseWriter, r *http.Request) { <-r.Context().Done() })
		_, err := f.records.KvBatch(ctx, []KvBatchOperation{KvBatchPut("app", "k", []byte("v"), KvAny())})
		var transport *TransportError
		if !errors.As(err, &transport) || !errors.Is(err, context.DeadlineExceeded) {
			t.Fatalf("%T %v", err, err)
		}
	})
}

// TestLiveKv is opt-in like TestLive. It needs a server that serves the key-value batch and
// get-many routes, and creates and drops its own one-dimension collection.
func TestLiveKv(t *testing.T) {
	origin := os.Getenv("VLITE_URL")
	if origin == "" {
		t.Skip("VLITE_URL is not set")
	}
	c, e := New(origin, os.Getenv("VLITE_AUTH_TOKEN"), 0)
	if e != nil {
		t.Fatal(e)
	}
	defer c.Close()
	ctx := context.Background()
	name := "go_kv_e2e_" + strconv.FormatInt(time.Now().UnixNano(), 10)
	if _, e = c.CreateCollection(ctx, name, 1, nil); e != nil {
		t.Fatal(e)
	}
	kv, e := c.Collection(name)
	if e != nil {
		t.Fatal(e)
	}
	defer func() {
		if _, err := kv.Drop(ctx); err != nil {
			t.Error(err)
		}
	}()
	expectAPIError := func(err error, status int, code string) {
		t.Helper()
		var api *APIError
		if !errors.As(err, &api) || api.Status != status || api.Code != code {
			t.Fatalf("want HTTP %d %s, got %v", status, code, err)
		}
	}
	put, e := kv.KvPut(ctx, "app", "greeting", []byte("hello"), KvPutOptions{Expected: KvAbsent()})
	if e != nil || put.Version == 0 || put.ExpiresAtMs != nil {
		t.Fatalf("%+v %v", put, e)
	}
	got, e := kv.KvGet(ctx, "app", "greeting")
	if e != nil || got.Key != "greeting" || string(got.Value) != "hello" || got.Version != put.Version {
		t.Fatalf("%+v %v", got, e)
	}
	_, e = kv.KvPut(ctx, "app", "greeting", []byte("again"), KvPutOptions{Expected: KvAbsent()})
	expectAPIError(e, 412, "precondition_failed")
	page, e := kv.KvList(ctx, "app", KvListOptions{Prefix: "gre"})
	if e != nil || len(page.Items) != 1 || page.Items[0].Key != "greeting" || page.NextAfter != nil {
		t.Fatalf("%+v %v", page, e)
	}
	results, e := kv.KvBatch(ctx, []KvBatchOperation{
		KvBatchPut("app", "a", []byte{0, 1, 2}, KvAbsent()),
		KvBatchPut("app", "b", nil, KvAny()),
		KvBatchDelete("app", "greeting", KvRevision(put.Version)),
	})
	if e != nil {
		t.Fatal(e)
	}
	if len(results) != 3 || results[0].Kind != KvBatchKindPut || results[0].Version == 0 ||
		results[1].Kind != KvBatchKindPut || results[1].Version == 0 ||
		results[2].Kind != KvBatchKindDelete || !results[2].Deleted {
		t.Fatalf("%+v", results)
	}
	many, e := kv.KvGetMany(ctx, []KvKey{{"app", "a"}, {"app", "greeting"}, {"app", "b"}})
	if e != nil || many.NextIndex != nil || len(many.Items) != 3 || many.Items[0] == nil || many.Items[1] != nil || many.Items[2] == nil {
		t.Fatalf("%+v %v", many, e)
	}
	if !reflect.DeepEqual(many.Items[0].Value, []byte{0, 1, 2}) || many.Items[0].Version != results[0].Version || len(many.Items[2].Value) != 0 {
		t.Fatalf("%+v %+v", many.Items[0], many.Items[2])
	}
	// A batch that fails on its last operation writes nothing.
	_, e = kv.KvBatch(ctx, []KvBatchOperation{
		KvBatchPut("app", "c", []byte("c"), KvAbsent()),
		KvBatchPut("app", "a", []byte("again"), KvAbsent()),
	})
	expectAPIError(e, 412, "precondition_failed")
	many, e = kv.KvGetMany(ctx, []KvKey{{"app", "c"}})
	if e != nil || len(many.Items) != 1 || many.Items[0] != nil {
		t.Fatalf("%+v %v", many, e)
	}
	deleted, e := kv.KvDelete(ctx, "app", "a", KvRevision(results[0].Version))
	if e != nil || !deleted {
		t.Fatalf("%v %v", deleted, e)
	}
	deleted, e = kv.KvDelete(ctx, "app", "a", KvAny())
	if e != nil || deleted {
		t.Fatalf("%v %v", deleted, e)
	}
	_, e = kv.KvGet(ctx, "app", "a")
	expectAPIError(e, 404, "kv_not_found")
}
