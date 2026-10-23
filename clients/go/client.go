// Package vlite provides a dependency-free REST client. Requests are never retried.
package vlite

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"regexp"
	"strconv"
	"strings"
	"time"
)

// Object preserves the REST contract, including decimal-string IDs and typed metadata.
type Object = map[string]any

type APIError struct {
	Status                   int
	Code, Message, RequestID string
}

func (e *APIError) Error() string {
	return fmt.Sprintf("vlite: HTTP %d %s: %s", e.Status, e.Code, e.Message)
}

// TransportError means a mutation may have committed; callers must not blindly retry.
type TransportError struct {
	Code  string
	Cause error
}

func (e *TransportError) Error() string {
	return "vlite: transport_error (mutation outcome may be unknown; no retry performed)"
}
func (e *TransportError) Unwrap() error { return e.Cause }

type MetadataValue struct {
	Type  string `json:"type"`
	Value any    `json:"value"`
}
type Metadata map[string]MetadataValue
type Put struct {
	Vector        []float32 `json:"vector"`
	PayloadBase64 string    `json:"payload_base64,omitempty"`
	Metadata      Metadata  `json:"metadata,omitempty"`
	ExpiresAtMs   *uint64   `json:"expires_at_ms,omitempty"`
	TTLms         *uint64   `json:"ttl_ms,omitempty"`
}
type Record struct {
	ID            string    `json:"id"`
	Revision      string    `json:"revision,omitempty"`
	Vector        []float32 `json:"vector"`
	PayloadBase64 string    `json:"payload_base64"`
	Metadata      Metadata  `json:"metadata"`
}
type Hit struct {
	ID       string   `json:"id"`
	Distance float64  `json:"distance"`
	Score    *float64 `json:"score,omitempty"`
}
type SearchResponse struct {
	Hits  []Hit  `json:"hits"`
	Stats Object `json:"stats,omitempty"`
	Plan  Object `json:"plan,omitempty"`
}
type Summary struct {
	ID       string   `json:"id"`
	Metadata Metadata `json:"metadata"`
}
type Page struct {
	Items     []Summary `json:"items"`
	NextAfter *string   `json:"next_after"`
}

type Client struct {
	*Records
	base, token string
	http        *http.Client
}
type Records struct {
	client *Client
	prefix string
}

var tokenPattern = regexp.MustCompile(`^[A-Za-z0-9_-]{32,256}$`)
var namePattern = regexp.MustCompile(`^[a-z][a-z0-9_-]{0,62}$`)
var idPattern = regexp.MustCompile(`^(0|[1-9][0-9]{0,19})$`)

func validID(id string) error {
	if !idPattern.MatchString(id) {
		return errors.New("id must be a canonical decimal u64 string")
	}
	_, err := strconv.ParseUint(id, 10, 64)
	return err
}

// New requires an HTTP(S) origin and bearer token. A zero timeout means 30 seconds.
// Proxies and redirects are disabled so credentials stay on the supplied origin.
func New(origin, token string, timeout time.Duration) (*Client, error) {
	u, err := url.Parse(origin)
	if err != nil || u == nil || (u.Scheme != "http" && u.Scheme != "https") || u.Hostname() == "" || u.User != nil || (u.Path != "" && u.Path != "/") || u.RawQuery != "" || u.ForceQuery || u.Fragment != "" {
		return nil, errors.New("origin must be an HTTP(S) origin without credentials, path, query or fragment")
	}
	if !tokenPattern.MatchString(token) {
		return nil, errors.New("token must contain 32..256 base64url characters")
	}
	if timeout < 0 {
		return nil, errors.New("timeout must be positive")
	}
	if timeout == 0 {
		timeout = 30 * time.Second
	}
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.Proxy = nil
	c := &Client{base: strings.TrimRight(origin, "/"), token: token, http: &http.Client{Timeout: timeout, Transport: transport, CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}}
	c.Records = &Records{client: c, prefix: "/v1"}
	return c, nil
}
func (c *Client) Close() { c.http.CloseIdleConnections() }

func (c *Client) request(ctx context.Context, method, path string, body, out any) error {
	var data []byte
	var err error
	if body != nil {
		data, err = json.Marshal(body)
		if err != nil {
			return err
		}
	}
	req, err := http.NewRequestWithContext(ctx, method, c.base+path, bytes.NewReader(data))
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+c.token)
	req.Header.Set("Accept", "application/json")
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	res, err := c.http.Do(req)
	if err != nil {
		return &TransportError{"transport_error", err}
	}
	defer res.Body.Close()
	if res.StatusCode < 200 || res.StatusCode >= 300 {
		detail := struct {
			Error struct {
				Code    string `json:"code"`
				Message string `json:"message"`
			} `json:"error"`
		}{}
		if json.NewDecoder(res.Body).Decode(&detail) != nil || detail.Error.Code == "" {
			detail.Error.Code = "http_error"
			detail.Error.Message = "Server returned a nonstandard error response"
		}
		return &APIError{res.StatusCode, detail.Error.Code, detail.Error.Message, res.Header.Get("X-Request-ID")}
	}
	if res.StatusCode == http.StatusNoContent {
		return nil
	}
	decoder := json.NewDecoder(res.Body)
	decoder.UseNumber()
	if out == nil {
		var discard any
		out = &discard
	}
	if err = decoder.Decode(out); err != nil {
		return &TransportError{"transport_error", err}
	}
	var extra any
	if err = decoder.Decode(&extra); err != io.EOF {
		return &TransportError{"transport_error", errors.New("invalid trailing JSON")}
	}
	return nil
}
func (c *Client) object(ctx context.Context, method, path string, body any) (Object, error) {
	var out Object
	err := c.request(ctx, method, path, body, &out)
	return out, err
}
func (c *Client) Health(ctx context.Context) (Object, error) {
	return c.object(ctx, "GET", "/health", nil)
}
func (c *Client) Ready(ctx context.Context) (Object, error) {
	return c.object(ctx, "GET", "/ready", nil)
}
func (c *Client) Database(ctx context.Context) (Object, error) {
	return c.object(ctx, "GET", "/v1/database", nil)
}
func (c *Client) ListCollections(ctx context.Context) (Object, error) {
	return c.object(ctx, "GET", "/v1/collections", nil)
}
func (c *Client) CreateCollection(ctx context.Context, name string, dimensions int, settings Object) (Object, error) {
	if !namePattern.MatchString(name) || dimensions < 1 || dimensions > 65536 {
		return nil, errors.New("invalid collection name or dimensions")
	}
	body := Object{"name": name, "dimensions": dimensions}
	if settings != nil {
		body["settings"] = settings
	}
	return c.object(ctx, "POST", "/v1/collections", body)
}
func (c *Client) Collection(name string) (*Records, error) {
	if !namePattern.MatchString(name) {
		return nil, errors.New("invalid collection name")
	}
	return &Records{c, "/v1/collections/" + name}, nil
}
func (r *Records) Info(ctx context.Context) (Object, error) {
	if r.prefix == "/v1" {
		return r.client.Database(ctx)
	}
	return r.client.object(ctx, "GET", r.prefix, nil)
}
func (r *Records) Drop(ctx context.Context) (bool, error) {
	if r.prefix == "/v1" {
		return false, errors.New("cannot drop legacy route; select a collection")
	}
	var v struct {
		Deleted bool `json:"deleted"`
	}
	err := r.client.request(ctx, "DELETE", r.prefix, nil, &v)
	return v.Deleted, err
}
func (r *Records) SetSettings(ctx context.Context, settings Object) (Object, error) {
	if r.prefix == "/v1" {
		return nil, errors.New("select a collection")
	}
	return r.client.object(ctx, "PUT", r.prefix+"/settings", settings)
}
func (r *Records) Put(ctx context.Context, id string, record Put) error {
	if err := validID(id); err != nil {
		return err
	}
	return r.client.request(ctx, "PUT", r.prefix+"/vectors/"+id, record, nil)
}
func (r *Records) Get(ctx context.Context, id string) (Record, error) {
	var out Record
	if err := validID(id); err != nil {
		return out, err
	}
	err := r.client.request(ctx, "GET", r.prefix+"/vectors/"+id, nil, &out)
	return out, err
}
func (r *Records) Delete(ctx context.Context, id string) (bool, error) {
	if err := validID(id); err != nil {
		return false, err
	}
	var out struct {
		Deleted bool `json:"deleted"`
	}
	err := r.client.request(ctx, "DELETE", r.prefix+"/vectors/"+id, nil, &out)
	return out.Deleted, err
}
func (r *Records) Batch(ctx context.Context, operations []Object) error {
	for _, op := range operations {
		id, ok := op["id"].(string)
		if !ok {
			return errors.New("batch id must be a string")
		}
		if err := validID(id); err != nil {
			return err
		}
	}
	if operations == nil {
		operations = []Object{}
	}
	return r.client.request(ctx, "POST", r.prefix+"/batches", Object{"operations": operations}, nil)
}

// SearchDetailed passes mode, metric, filters and ANN options unchanged; no fallback/build is implicit.
func (r *Records) SearchDetailed(ctx context.Context, request Object) (SearchResponse, error) {
	var out SearchResponse
	err := r.client.request(ctx, "POST", r.prefix+"/search", request, &out)
	return out, err
}
func (r *Records) Search(ctx context.Context, request Object) ([]Hit, error) {
	out, err := r.SearchDetailed(ctx, request)
	return out.Hits, err
}
func (r *Records) Browse(ctx context.Context, limit int, after *string) (Page, error) {
	var out Page
	if limit < 1 || limit > 100 {
		return out, errors.New("limit must be in 1..100")
	}
	q := url.Values{"limit": {strconv.Itoa(limit)}}
	if after != nil {
		if err := validID(*after); err != nil {
			return out, err
		}
		q.Set("after", *after)
	}
	err := r.client.request(ctx, "GET", r.prefix+"/vectors?"+q.Encode(), nil, &out)
	return out, err
}
func (r *Records) IndexStatus(ctx context.Context) (Object, error) {
	return r.client.object(ctx, "GET", r.prefix+"/index/hnsw", nil)
}
func (r *Records) BuildIndex(ctx context.Context, options Object) error {
	if options == nil {
		options = Object{}
	}
	return r.client.request(ctx, "POST", r.prefix+"/index/hnsw", options, nil)
}

// UpdateMetadata replaces only metadata on a V5 record.
func (r *Records) UpdateMetadata(ctx context.Context, id string, metadata Metadata) error {
 if err := validID(id); err != nil { return err }; return r.client.request(ctx, "PATCH", r.prefix+"/vectors/"+id+"/metadata", Object{"metadata": metadata}, nil)
}
// GroupedSearch and Recommend expose collection-scoped retrieval APIs.
func (r *Records) GroupedSearch(ctx context.Context, request Object) (Object, error) { prefix:=r.prefix;if prefix=="/v1"{prefix="/v1/collections/default"};return r.client.object(ctx, "POST", prefix+"/search/grouped", request) }
func (r *Records) Recommend(ctx context.Context, request Object) (Object, error) { prefix:=r.prefix;if prefix=="/v1"{prefix="/v1/collections/default"};return r.client.object(ctx, "POST", prefix+"/recommend", request) }
// Changes returns a durable cursor page from the default collection's feed.
func (c *Client) Changes(ctx context.Context, cursor string, limit int) (Object, error) {
 if limit < 1 || limit > 10000 { return nil, errors.New("limit must be in 1..10000") }; q:=url.Values{"limit":{strconv.Itoa(limit)}}; if cursor!="" {q.Set("cursor",cursor)}; return c.object(ctx,"GET","/v1/changes?"+q.Encode(),nil)
}
func (c *Client) ListWebhooks(ctx context.Context) (Object,error) { return c.object(ctx,"GET","/v1/admin/webhooks",nil) }
func (c *Client) CreateWebhook(ctx context.Context, request Object) (Object,error) { return c.object(ctx,"POST","/v1/admin/webhooks",request) }
func (c *Client) WebhookStatus(ctx context.Context, id string) (Object,error) { if !regexp.MustCompile(`^[a-f0-9]{24}$`).MatchString(id){return nil,errors.New("invalid webhook ID")};return c.object(ctx,"GET","/v1/admin/webhooks/"+id,nil) }
