# Dashboard guide

The dashboard is VLite's browser console for one database. Use it to sign in,
browse records, write data, search, build indexes and run the server. The
server serves it at `/dashboard`, and Docker Compose also serves it on port 8081.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="../site/assets/dashboard/current-overview-dark.png?v=connected">
  <img src="../site/assets/dashboard/current-overview-light.png?v=connected" data-light="../site/assets/dashboard/current-overview-light.png?v=connected" data-dark="../site/assets/dashboard/current-overview-dark.png?v=connected" alt="Connected dashboard with sample records, navigation and database snapshot cards." width="1440" height="1100">
</picture>

The [dashboard tour](https://mahpate.github.io/Vlite/dashboard.html#screenshots)
walks through every workspace with sample data.

## Sign in

The dashboard opens on a sign-in screen and shows the rest after you sign in.
Use your username and password; a session cookie keeps you signed in.

![Sign-in screen with username and password fields and the access token option](../site/assets/dashboard/current-sign-in.png?v=connected)

- **First account:** a new server has no accounts and no default password, so
  the sign-in screen asks for the admin token. Sign in with it, open
  **Administration → Users & access** and create your own super admin account
  first.
- **Temporary password:** a password set by an admin must be changed at first
  sign-in. Passwords are 15–128 characters.
- **Forgot your password:** ask a super admin to reset it.
- **Sessions** end when you sign out, when they expire and when the server
  restarts. Sign in again to continue.
- **HTTPS or a loopback-only server:** sign-in is accepted only over HTTPS or
  when the server listens on loopback only. When a TLS-terminating or local
  reverse proxy fronts the API, the operator sets `VLITE_TRUSTED_PROXY=1`.
  Docker Compose sets it.
- **Open local mode** (`--insecure-local`) has no sign-in.

See [dashboard sign-in and accounts](SECURITY.md#dashboard-sign-in-and-accounts)
for the full model.

What you can do depends on your role:

| Role | Can |
|---|---|
| Super admin | Everything, including the server, tenants, users and API keys. |
| Editor | Read, search, write, import, index and manage collections. |
| Viewer | Read and search. |

Editors and viewers see either one tenant's collections or all collections.
In source builds with the experimental `graph` feature, viewers also read
every graph and editors read and write every graph, whatever their tenant.
Graphs are shared, not per tenant; vectors attached through a graph still
need a collection your tenant owns.

### Use an access token

The admin token is for automation and recovery. Choose **Use an access token
instead** and paste the value only, without the `Bearer` prefix. **Where do I
get a token?** below the field explains where to find it:

- **Docker Compose:** `.secrets/api-token` in the checkout that started the
  server. PowerShell: `Get-Content .secrets\api-token -Raw`.
- **Windows devbox:** `.\scripts\devbox.ps1 -Token` copies it to the clipboard.
- **Direct server:** the file passed with `--token-file`.
- **Shared server:** ask its administrator for an account.

Treat tokens as secrets. Keep them out of screenshots and logs.

## Find your way around

The sidebar groups six workspaces. Tabs inside each workspace hold its views:

| Group | Workspace | Inside |
|---|---|---|
| | **Overview** | Database size, health and shortcuts |
| Data | **Collections** | Records, Add & edit, Import & export, Index, Manage |
| Data | **Search** | Vector search, Advanced retrieval, Analyze & compare |
| Server | **Observability** | Live monitor, Diagnostics, Change feed |
| Server | **Administration** | Health, Users & access, Backup & recovery, Server settings |
| Tools | **Developer Console** | Requests and responses this dashboard sends |

Some tabs split into tools, such as **Users & access → API keys**. Everyone
sees every workspace; a control your role cannot use is disabled and says why.
On a narrow screen, open the sidebar from the **Menu** button.

The top bar shows the active collection and the account menu. Open the account
menu to see who you are signed in as, change your password, pick a Light, Dark
or System theme and a density, reopen the **Quick tour** or **Sign out**.
**Remember theme and density on this device** saves just those choices in the
browser.

![Account menu with the signed-in identity, appearance controls and sign out](../site/assets/dashboard/current-preferences.png?v=connected)

## Create a collection and add records

1. **Create.** In **Collections → Manage**, enter a name, dimensions, a
   metric and optional metadata indexes, such as
   `[{"field":"category","kind":"eq"}]` (kinds: `eq`, `range`, `eq_range`).
   **Review collection** shows the request before you confirm.
2. **Select.** Choose the collection in the sidebar picker. Every browse,
   write, search and index action uses the active collection. Switching lists
   any unsaved work it will discard and asks first.
3. **Write.** In **Add & edit**, use **One record**, or **Bulk JSON** for up
   to 10,000 documents (4 MiB):

   ```json
   [{"id":"1","vector":[1,0,0],"metadata":{"category":{"type":"string","value":"demo"}},"payload":""}]
   ```

   **Validate / preview** checks every document before anything is written.
   Writes go in atomic batches of 20, but the whole upload is not atomic: if a
   batch fails, earlier batches stay. An upsert replaces the vector, payload,
   metadata and expiry.
4. **Import.** In **Import & export**, load a JSON or CSV file of up to 100
   records, or use **Large file import** for big JSON, NDJSON or CSV files.
   Each chunk commits atomically, and **Resume** continues from the last
   committed chunk.

Creating collections needs a format v4 file. The dashboard never upgrades a
file; that is an offline operator step. You can drop only an empty collection.

![Add & edit view with validated records awaiting confirmation](../site/assets/dashboard/current-write.png?v=connected)

## Browse and edit records

**Collections → Records** pages through records with selectable metadata
columns. Sorting applies to the loaded page only. Select a row to open the
inspector with **Payload**, **Metadata**, **Vector** and **Raw JSON** tabs.

- **Only if unchanged** protects edits. If the record changed meanwhile, nothing
  is written; reload it and reapply your edit.
- **Save metadata only** replaces the metadata and keeps the vector and payload.
- **Export** saves the loaded page as JSON or CSV.
- **Delete selected** and **Replace selected metadata** act on IDs from the
  loaded page.

## Search

Open **Search** and enter a query vector, `k`, a metric and an optional filter.
The filter builder joins equality and numeric range conditions with AND.

| Mode | What it does |
|---|---|
| Exact | Scans every record. The reference answer. |
| Approximate | Uses the HNSW index and builds it if missing. |
| Approximate (ready) | Uses an existing index and never builds one. |
| Auto | Lets the server pick a plan. Never builds an index. |
| Hybrid | Fuses vector results with text matches. Higher scores are better. |

Distances are lower-is-better. **Explain** shows the chosen plan. **Compare**
runs exact and ready approximate search side by side. **Analyze & compare**
samples up to 32 records for duplicates and metadata types, plots them in 2-D
and compares two records. **Advanced retrieval**
adds reranking, grouping, geo radius, named and sparse vectors, recommend,
facets and the [query language](VECTORS.md). Save a query to a file and load it
later. **Copy as code** turns the form into curl, Python or JavaScript with
placeholders for the server address and token; it never includes your
credentials.

![Search workspace with a vector query and ranked neighbors](../site/assets/dashboard/current-search.png?v=connected)

## Build an index

**Collections → Index** shows HNSW readiness, memory and a sampled recall
estimate. Build an index, or start a background rebuild that keeps the current
index serving until the new one is published. Writes stay searchable while
they wait for the next rebuild.

## Monitor the server

Super admins open **Observability → Live monitor** for request rates, errors,
latency, a per-route table and a live log tail with level and text filters and
JSONL export. The server keeps the log in memory only, so it clears on
restart. **Diagnostics** lists slow queries. **Change feed** lists committed
events across the database and needs read access to all collections. Server
health is under **Administration → Health**.

The **Developer Console** holds the request inspector, a REST playground and
read-only test scenarios.

## Administer users and tenants

Super admins use **Administration → Users & access**, with tabs for
**Users**, **API keys**, **Tenants**, **Admin tokens** and the **Audit log**.

- **Add a user:** in **Users**, choose a role, bind an editor or viewer to a
  tenant or to all collections, set a temporary password and select
  **Add user**.
- **Reset a password:** set a new temporary password. The user must change it
  at next sign-in.
- **Create a tenant:** in **Tenants**, enter a name and an optional vector
  limit. Then add its users or API keys.
- **Rename or change limits:** use **Save name and limit**.
- **Suspend a tenant:** this blocks its sign-ins and API keys and ends its
  sessions. Its data stays. **Resume** restores access.
- **Delete a tenant:** drop its collections first. Deleting also removes its
  user accounts.
- **Issue an API key:** in **API keys**, pick collections, operations and an
  expiry of 1–365 days. The secret appears once.

The [administration walkthrough](CONSOLE-ADMIN.md#dashboard-administration-walkthrough)
covers every view, including backups, webhooks, TLS and replication.

![Tenants view with vector limits, state and usage](../site/assets/dashboard/current-tenants.png?v=connected)

## Safety

- Every write asks for confirmation and names the collection. Nothing retries
  automatically.
- **Stop waiting** stops the browser from waiting, but the server may still
  finish the work. Check the result before you retry.
- Requests time out after 60 seconds; index operations after one hour.
- Choosing a file sends nothing until you confirm.
- Besides the HttpOnly session cookie, the browser stores only your appearance
  choice, if you opt in, and whether you closed the quick tour. Records,
  queries and tokens are never stored.
