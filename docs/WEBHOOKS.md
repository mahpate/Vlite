# Change-feed webhooks

Webhooks are configured by bootstrap admin credentials and require a private
`VLITE_ADMIN_DIR`. `POST /v1/admin/webhooks` accepts:

```json
{
  "url": "https://consumer.example/hooks/vlite",
  "collections": ["documents"],
  "events": ["put", "delete", "metadata_update"],
  "secret": "use-a-random-secret-with-at-least-16-bytes"
}
```

`collections` is optional (omitted means every collection); event names are
`put`, `delete`, `metadata_update`, `collection_created`, `collection_dropped`,
`settings_updated`, and `vectors_updated`. URLs must be absolute HTTP(S) URLs
without embedded credentials or fragments. Redirects are disabled so a
signature secret is not forwarded to a redirect target. Subscription secrets
are persisted in the private admin directory and are never returned by list,
status, or create responses. Restrict directory permissions and backup access.

The background dispatcher polls the durable log change feed on a separate
thread, so a slow or unavailable receiver does not block database writes. A
POST body is JSON with `subscription_id` and a `changes` array. The receiver
must verify `X-VLite-Signature: sha256=<lowercase-hex>` as HMAC-SHA256 over the
exact raw request body using the configured secret. It should durably accept a
request before returning any 2xx status.

Delivery is **at least once**. Each attempted request has a ten-second timeout;
non-2xx/network failures are retried with exponential waits of 1, 2, 4 and 8
seconds. If a feed batch is still undelivered, its durable cursor is not
advanced; a process restart resumes from the last saved cursor. Successful
subscriptions in a partially delivered batch can receive duplicate events on
retry. Consumers should deduplicate by `(sequence, ordinal, collection)`.
Status `GET /v1/admin/webhooks` and `GET /v1/admin/webhooks/{id}` reports
attempts, last success and last error. Remove subscriptions with
`DELETE /v1/admin/webhooks/{id}`.

The cursor is shared by all subscriptions. New subscriptions receive changes
from the current dispatcher position onward, not a replay of all historical
records. The engine change feed scans the log from its beginning per poll and
returns commit-boundary batches; this simple implementation is intended for
moderate volumes, not a high-throughput event-broker replacement. Events do not
include expiry actions (expiry is represented on the original put), and
`settings_updated`/`vectors_updated` are change notifications rather than full
snapshot payloads. There is no event-level dead-letter queue or per-subscription
cursor; monitor delivery status and provision consumer-side idempotency.
