# VLite 0.6.1 upgrade and rollback

This patch release fixes dashboard sign-in for tenants that hold graphs but no
vector collections. It follows [0.6.0](RELEASE-0.6.0.md). Only the dashboard
changes: the server, its API, file formats and storage are the same as 0.6.0,
and the 0.6.0 benchmark baseline still applies.

## Fixed: sign-in for graph-only tenants

Before 0.6.1, the dashboard refused to finish signing in when the credential
could see no vector collection. It showed "This credential covers no existing
collection" and stayed on the sign-in screen, even when the account or access
token could read graphs. This affected, for example, a user whose tenant holds
only property graphs.

Now an empty collection list is a valid state:

- You stay signed in, and the Graph workspace opens first when the credential
  can read graphs.
- Records, imports, indexes and vector search show "No vector collections in
  this tenant yet" and their controls are disabled. The dashboard sends no
  collection-specific requests in this state.
- Overview, Graph, Observability and Administration work as your role allows.
- After a collection is created, select Refresh to use the vector views.

Sign-in still fails closed on real errors: an unsupported API version or role,
a rejected credential, or an unreachable server.

## Upgrade

Replace the binary or image with 0.6.1 and restart. No file upgrade, setting
or data migration is needed. Follow the usual single-writer rule: stop the old
process (or scale the old revision to zero replicas) before the new one opens
the file.

## Rollback

Restart with the 0.6.0 binary or image. 0.6.1 writes nothing that 0.6.0 cannot
read. Graph-only tenants will again be unable to sign in to the dashboard;
their data and API access are not affected.
