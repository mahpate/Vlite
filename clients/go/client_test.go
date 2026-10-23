package vlite

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"reflect"
	"strconv"
	"strings"
	"testing"
	"time"
)

const token = "abcdefghijklmnopqrstuvwxyz012345"

func TestRoutes(t *testing.T) {
	cases := []struct {
		method, path, response string
		call                   func(*Client) error
	}{
		{"GET", "/health", `{"status":"ok"}`, func(c *Client) error { _, e := c.Health(context.Background()); return e }},
		{"GET", "/ready", `{"status":"ready"}`, func(c *Client) error { _, e := c.Ready(context.Background()); return e }},
		{"GET", "/v1/database", `{"dimensions":3}`, func(c *Client) error { _, e := c.Database(context.Background()); return e }},
		{"GET", "/v1/collections", `{"items":[]}`, func(c *Client) error { _, e := c.ListCollections(context.Background()); return e }},
		{"POST", "/v1/collections", `{"name":"docs"}`, func(c *Client) error { _, e := c.CreateCollection(context.Background(), "docs", 3, nil); return e }},
		{"PUT", "/v1/vectors/18446744073709551615", "", func(c *Client) error {
			return c.Put(context.Background(), "18446744073709551615", Put{Vector: []float32{1, 0, 0}})
		}},
		{"PATCH", "/v1/vectors/1/metadata", "", func(c *Client) error { return c.UpdateMetadata(context.Background(), "1", Metadata{}) }},
		{"GET", "/v1/changes?limit=50", `{"changes":[]}`, func(c *Client) error { _, e := c.Changes(context.Background(), "", 50); return e }},
		{"GET", "/v1/admin/webhooks", `{"items":[]}`, func(c *Client) error { _, e := c.ListWebhooks(context.Background()); return e }},
		{"GET", "/v1/vectors/1", `{"id":"1","vector":[1,0,0],"payload_base64":"","metadata":{}}`, func(c *Client) error {
			r, e := c.Get(context.Background(), "1")
			if r.ID != "1" {
				t.Fatal(r)
			}
			return e
		}},
		{"DELETE", "/v1/vectors/1", `{"deleted":true}`, func(c *Client) error {
			v, e := c.Delete(context.Background(), "1")
			if !v {
				t.Fatal(v)
			}
			return e
		}},
		{"POST", "/v1/batches", "", func(c *Client) error { return c.Batch(context.Background(), []Object{{"op": "delete", "id": "1"}}) }},
		{"GET", "/v1/vectors?after=0&limit=25", `{"items":[],"next_after":null}`, func(c *Client) error { a := "0"; _, e := c.Browse(context.Background(), 25, &a); return e }},
		{"POST", "/v1/index/hnsw", "", func(c *Client) error { return c.BuildIndex(context.Background(), nil) }},
		{"GET", "/v1/collections/docs", `{"name":"docs"}`, func(c *Client) error { r, _ := c.Collection("docs"); _, e := r.Info(context.Background()); return e }},
		{"PUT", "/v1/collections/docs/settings", `{}`, func(c *Client) error {
			r, _ := c.Collection("docs")
			_, e := r.SetSettings(context.Background(), Object{"metric": "cosine"})
			return e
		}},
		{"DELETE", "/v1/collections/docs", `{"deleted":true}`, func(c *Client) error { r, _ := c.Collection("docs"); _, e := r.Drop(context.Background()); return e }},
	}
	for _, tc := range cases {
		t.Run(tc.method+tc.path, func(t *testing.T) {
			s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				if r.Method != tc.method || r.URL.RequestURI() != tc.path || r.Header.Get("Authorization") != "Bearer "+token {
					t.Errorf("unexpected request: %s %s", r.Method, r.URL)
				}
				if tc.response == "" {
					w.WriteHeader(204)
				} else {
					w.Header().Set("Content-Type", "application/json")
					_, _ = w.Write([]byte(tc.response))
				}
			}))
			defer s.Close()
			c, e := New(s.URL, token, 0)
			if e != nil {
				t.Fatal(e)
			}
			defer c.Close()
			if e = tc.call(c); e != nil {
				t.Fatal(e)
			}
		})
	}
}
func TestSearchFilters(t *testing.T) {
	for _, mode := range []string{"exact", "approximate", "approximate_ready"} {
		body := Object{"vector": []any{1.0, 0.0}, "k": 1.0, "mode": mode, "filter": []any{Object{"op": "eq", "field": "tag", "value": Object{"type": "integer", "value": "9223372036854775807"}}}}
		s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			var got Object
			_ = json.NewDecoder(r.Body).Decode(&got)
			if !reflect.DeepEqual(got, body) {
				t.Errorf("body: %v", got)
			}
			if r.URL.Path != "/v1/collections/docs/search" {
				t.Error(r.URL)
			}
			_, _ = w.Write([]byte(`{"hits":[{"id":"18446744073709551615","distance":0.5}]}`))
		}))
		c, _ := New(s.URL, token, 0)
		r, _ := c.Collection("docs")
		hits, e := r.Search(context.Background(), body)
		if e != nil || len(hits) != 1 || hits[0].ID != "18446744073709551615" {
			t.Fatalf("%v %v", hits, e)
		}
		c.Close()
		s.Close()
	}
}
func TestErrorsAndRedirect(t *testing.T) {
	for _, status := range []int{401, 404, 409, 422, 503, 302} {
		s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			w.Header().Set("X-Request-ID", "req-1")
			w.Header().Set("Location", "/health")
			w.WriteHeader(status)
			_, _ = w.Write([]byte(`{"error":{"code":"stable_code","message":"detail"}}`))
		}))
		c, _ := New(s.URL, token, 0)
		_, e := c.Health(context.Background())
		var api *APIError
		if !errors.As(e, &api) || api.Code != "stable_code" || api.Status != status || api.RequestID != "req-1" {
			t.Fatal(e)
		}
		c.Close()
		s.Close()
	}
}
func TestNonstandardError(t *testing.T) {
	s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(502)
		_, _ = w.Write([]byte("upstream unavailable"))
	}))
	defer s.Close()
	c, _ := New(s.URL, token, 0)
	defer c.Close()
	_, err := c.Health(context.Background())
	var api *APIError
	if !errors.As(err, &api) || api.Code != "http_error" || api.Status != 502 {
		t.Fatal(err)
	}
}
func TestTimeoutAndInvalidJSON(t *testing.T) {
	for _, slow := range []bool{false, true} {
		s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if slow {
				time.Sleep(50 * time.Millisecond)
			}
			_, _ = w.Write([]byte("bad"))
		}))
		c, _ := New(s.URL, token, time.Millisecond*10)
		_, e := c.Health(context.Background())
		var transport *TransportError
		if !errors.As(e, &transport) || transport.Code != "transport_error" {
			t.Fatal(e)
		}
		c.Close()
		s.Close()
	}
}
func TestValidation(t *testing.T) {
	for _, origin := range []string{"ftp://localhost", "http://user@localhost", "http://localhost/path", "http://localhost?x=1"} {
		if _, e := New(origin, token, 0); e == nil {
			t.Fatal(origin)
		}
	}
	c, _ := New("http://localhost", token, 0)
	defer c.Close()
	for _, id := range []string{"01", "-1", "18446744073709551616", "../x"} {
		if e := c.Put(context.Background(), id, Put{}); e == nil {
			t.Fatal(id)
		}
	}
	if _, e := New("http://localhost", strings.Repeat("x", 31), 0); e == nil {
		t.Fatal("short token accepted")
	}
}

// Live tests are opt-in and use a high ID. Run against a disposable 3-dimensional database.
func TestLive(t *testing.T) {
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
	name := "go_e2e_" + strconv.FormatInt(time.Now().UnixNano(), 10)
	if _, e = c.CreateCollection(ctx, name, 3, nil); e != nil {
		t.Fatal(e)
	}
	scoped, _ := c.Collection(name)
	defer func() {
		if _, err := scoped.Delete(ctx, "1"); err != nil {
			t.Error(err)
		}
		if _, err := scoped.Drop(ctx); err != nil {
			t.Error(err)
		}
	}()
	metadata := Metadata{"tag": MetadataValue{Type: "string", Value: "e2e"}}
	if e = scoped.Put(ctx, "1", Put{Vector: []float32{1, 0, 0}, Metadata: metadata}); e != nil {
		t.Fatal(e)
	}
	filter := []Object{{"op": "eq", "field": "tag", "value": Object{"type": "string", "value": "e2e"}}}
	for _, mode := range []string{"exact", "approximate"} {
		hits, err := scoped.Search(ctx, Object{"vector": []float32{1, 0, 0}, "k": 1, "mode": mode, "filter": filter})
		if err != nil || len(hits) != 1 || hits[0].ID != "1" {
			t.Fatalf("collection filtered %s: %v %v", mode, hits, err)
		}
	}
	if _, e = c.Health(ctx); e != nil {
		t.Fatal(e)
	}
	if _, e = c.Database(ctx); e != nil {
		t.Fatal(e)
	}
	id := "18446744073709551019"
	if e = c.Put(ctx, id, Put{Vector: []float32{1, 0, 0}}); e != nil {
		t.Fatal(e)
	}
	defer c.Delete(ctx, id)
	if _, e = c.Get(ctx, id); e != nil {
		t.Fatal(e)
	}
	if e = c.Batch(ctx, []Object{{"op": "put", "id": id, "vector": []float32{1, 0, 0}}}); e != nil {
		t.Fatal(e)
	}
	if _, e = c.Search(ctx, Object{"vector": []float32{1, 0, 0}, "k": 1, "mode": "exact"}); e != nil {
		t.Fatal(e)
	}
	if _, e = c.Browse(ctx, 25, nil); e != nil {
		t.Fatal(e)
	}
	if e = c.BuildIndex(ctx, nil); e != nil {
		t.Fatal(e)
	}
	if _, e = c.Search(ctx, Object{"vector": []float32{1, 0, 0}, "k": 1, "mode": "approximate_ready"}); e != nil {
		t.Fatal(e)
	}
}
