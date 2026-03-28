# Lakekeeper RisingWave CloudEvent Publisher

## Overview

Lakekeeper can publish catalog change events (table creates, drops, renames, etc.) to a
[RisingWave](https://risingwave.com/) instance using its built-in
[webhook source](https://docs.risingwave.com/integrations/sources/webhook). This enables
downstream consumers such as `iceberg-catalog-sync` to subscribe to a stream of catalog
mutations for event-driven partial sync.

### Data flow

```
Lakekeeper catalog mutation
  -> CloudEventsPublisher (existing internal mechanism)
    -> RisingWaveBackend (HTTP POST, JSON)
      -> RisingWave frontend webhook listener (:4560)
        -> lakekeeper_events_raw table (single JSONB column)
          -> lakekeeper_events materialized view (flat typed columns)
            -> lakekeeper_events_subscription (RisingWave subscription)
              -> Consumer (e.g. iceberg-catalog-sync via risingwave-py)
```

### Why webhook instead of the RisingWave Events API

The [Events API](https://github.com/risingwavelabs/events-api) is a separate Go service
that accepts JSON over HTTP and inserts rows into RisingWave via the PostgreSQL wire
protocol (`INSERT INTO ... VALUES`). It supports typed multi-column tables directly.
However, it requires deploying and operating an additional server.

The webhook source is embedded in the RisingWave frontend node. We chose it for the
following reasons:

| Concern | Webhook (chosen) | Events API |
|---------|-------------------|------------|
| **Deployment** | Nothing extra -- built into RisingWave frontend | Separate Go container to deploy, monitor, upgrade |
| **High availability** | Every frontend node runs a webhook listener; a load balancer gives HA for free | Must run multiple replicas + LB yourself |
| **Failure domains** | 1 (RisingWave) | 2 (Go server + RisingWave) |
| **Durability guarantee** | HTTP 200 = data committed (`wait_for_persistence=true` default) | HTTP 200 = buffered in Go process, not yet in RisingWave |
| **Data loss on crash** | Only if all frontend nodes are down | Go server OOM/crash = buffered events lost silently |
| **Ingestion path** | FastInsert RPC -- bypasses SQL parser/planner, direct to streaming layer | Full SQL INSERT path: parse -> bind -> plan -> batch execute |
| **Upgrade coupling** | None | Must track Events API <-> RisingWave version compatibility |

The trade-off is that the webhook source requires a single JSONB column. We use a
materialized view to flatten it into typed columns. For catalog event volumes (tens to
low hundreds per minute during active operations), this overhead is negligible -- the MV
processes rows incrementally as they arrive.

---

## Implementation

### Files

| Action | File | Description |
|--------|------|-------------|
| **Create** | `crates/lakekeeper/src/service/events/backends/risingwave.rs` | `RisingWaveBackend` implementing `CloudEventBackend` |
| **Modify** | `crates/lakekeeper/src/service/events/backends/mod.rs` | Register module under `#[cfg(feature = "risingwave")]` |
| **Modify** | `crates/lakekeeper/src/service/events/publisher.rs` | Wire backend into `get_default_cloud_event_backends_from_config()` |
| **Modify** | `crates/lakekeeper/src/config.rs` | Add `risingwave_webhook_url` config field |
| **Modify** | `crates/lakekeeper/Cargo.toml` | Add `risingwave` feature flag |

### How it works

1. On startup, if `LAKEKEEPER__RISINGWAVE_WEBHOOK_URL` is set, Lakekeeper creates a
   `RisingWaveBackend` with a `reqwest::Client` and the configured URL.

2. When a catalog mutation occurs (e.g. `createTable`), the existing
   `CloudEventsPublisher` builds a `cloudevents::Event` with standard attributes
   (`id`, `type`, `source`) and Lakekeeper-specific extensions (`namespace`, `name`,
   `tabular-id`, `warehouse-id`, `trace-id`).

3. `RisingWaveBackend::publish()` flattens the CloudEvent into a JSON object and
   HTTP POSTs it to the RisingWave webhook endpoint. The entire JSON object is
   ingested as the single JSONB `data` column in the webhook source table.

4. A materialized view in RisingWave extracts the JSONB fields into flat typed
   columns for downstream consumption.

### Lakekeeper configuration

| Environment variable | Example | Required |
|---------------------|---------|----------|
| `LAKEKEEPER__RISINGWAVE_WEBHOOK_URL` | `http://risingwave:4560/webhook/dev/public/lakekeeper_events_raw` | Yes (to enable) |

When the variable is not set, the backend is silently skipped. Multiple backends can
be active simultaneously (e.g. RisingWave + NATS + console logging).

The URL follows the RisingWave webhook endpoint format:

```
http://<host>:<port>/webhook/<database>/<schema>/<table>
```

- Default webhook port: `4560`
- Default database: `dev`
- Default schema: `public`
- Table name must match the webhook source table you create in RisingWave

### Feature flag

The backend is gated behind the `risingwave` Cargo feature. It is included in the
`all` feature set. No additional crate dependencies are needed -- `reqwest` is already
a non-optional workspace dependency.

```toml
# Cargo.toml
[features]
risingwave = []
all = [..., "risingwave", ...]
```

```bash
# Build with RisingWave support
cargo build --features risingwave

# Run tests
cargo test --features risingwave

# Build without (should still compile)
cargo build
```

---

## RisingWave Setup Guide

### Prerequisites

- RisingWave v2.2+ (webhook source support)
- Webhook listener enabled on the frontend node(s)

### Step 1: Enable the webhook listener

The webhook listener runs on port `4560` of each RisingWave frontend node. It must be
explicitly enabled in some deployment modes.

**Docker Compose** -- the listener is enabled by default. Ensure port 4560 is exposed:

```yaml
services:
  risingwave:
    image: risingwavelabs/risingwave:latest
    ports:
      - "4566:4566"   # PostgreSQL wire protocol
      - "4560:4560"   # Webhook listener
```

**Kubernetes (Operator v0.12.0+)**:

```yaml
apiVersion: risingwave.risingwavelabs.com/v1alpha1
kind: RisingWave
metadata:
  name: risingwave
spec:
  enableWebhookListener: true
```

**Kubernetes (Helm v0.2.38+)**:

```yaml
frontendComponent:
  webhookListener: true
```

```bash
helm install --set tags.bundle=true,frontendComponent.webhookListener=true \
  risingwave risingwavelabs/risingwave --version 0.2.38
```

**Intra-cluster endpoint** (Kubernetes):

```
<service-name>.<namespace>.svc.<cluster-domain>:4560
```

### Step 2: Create the webhook source table

Connect to RisingWave via psql or any PostgreSQL client:

```bash
psql -h <risingwave-host> -p 4566 -d dev -U root
```

Create the webhook source table. The webhook connector in RisingWave requires exactly
one JSONB column:

```sql
CREATE TABLE lakekeeper_events_raw (
    data JSONB
) WITH (
    connector = 'webhook'
);
```

**With request validation (recommended for production):**

```sql
-- Create a secret for signature verification
CREATE SECRET lakekeeper_webhook_secret WITH (backend = 'meta')
    AS 'your-shared-secret-here';

-- Create the table with validation
CREATE TABLE lakekeeper_events_raw (
    data JSONB
) WITH (
    connector = 'webhook'
) VALIDATE SECRET lakekeeper_webhook_secret AS secure_compare(
    headers->>'authorization',
    lakekeeper_webhook_secret
);
```

> Note: Request validation with secrets is a RisingWave Premium feature.
> For non-premium deployments, you can use a raw string literal instead of
> `CREATE SECRET`, or omit the VALIDATE clause entirely.

### Step 3: Create the materialized view (flat columns)

This MV transforms the single JSONB column into typed, queryable columns:

```sql
CREATE MATERIALIZED VIEW lakekeeper_events AS
SELECT
    data ->> 'id'           AS id,
    data ->> 'type'         AS type,
    data ->> 'source'       AS source,
    data ->> 'namespace'    AS namespace,
    data ->> 'name'         AS name,
    data ->> 'tabular_id'   AS tabular_id,
    data ->> 'warehouse_id' AS warehouse_id,
    data ->> 'trace_id'     AS trace_id,
    (data -> 'data')::JSONB AS data
FROM lakekeeper_events_raw;
```

The MV is incrementally maintained -- RisingWave only processes new rows as they
arrive, not full table rescans.

### Step 4: Create the subscription

Subscriptions allow consumers to poll for new rows in order. Create a subscription
on the materialized view:

```sql
CREATE SUBSCRIPTION lakekeeper_events_subscription
    FROM lakekeeper_events
    WITH (retention = '7 days');
```

The `retention` parameter controls how long consumed events are kept. Adjust based
on your consumer's recovery window.

### Step 5: Configure Lakekeeper

Set the webhook URL on the Lakekeeper deployment. The URL must point to the webhook
source table created in Step 2:

```bash
export LAKEKEEPER__RISINGWAVE_WEBHOOK_URL="http://risingwave:4560/webhook/dev/public/lakekeeper_events_raw"
```

### Step 6: Verify

Send a test event by performing a catalog operation (e.g. create a table in Lakekeeper),
then query the materialized view:

```sql
SELECT * FROM lakekeeper_events ORDER BY id DESC LIMIT 5;
```

You can also query the raw table to verify webhook ingestion:

```sql
SELECT * FROM lakekeeper_events_raw ORDER BY data->>'id' DESC LIMIT 5;
```

---

## Payload Schema

The `RisingWaveBackend` flattens each CloudEvent into a JSON object. This object is
ingested as the single JSONB `data` column in `lakekeeper_events_raw`, then unpacked
by the materialized view.

### JSON payload structure

```json
{
    "id": "019735a2-...",
    "type": "createTable",
    "source": "uri:iceberg-catalog-service:hostname",
    "namespace": "my_database",
    "name": "my_table",
    "tabular_id": "Table/550e8400-...",
    "warehouse_id": "6ba7b810-...",
    "data": { ... },
    "trace_id": "019735a2-..."
}
```

### Field mapping

| JSON field | CloudEvent origin | MV column | Type | Description |
|-----------|-------------------|-----------|------|-------------|
| `id` | Standard attribute | `id` | VARCHAR | Unique event ID (UUIDv7) |
| `type` | Standard attribute | `type` | VARCHAR | Event type (e.g. `createTable`) |
| `source` | Standard attribute | `source` | VARCHAR | `uri:iceberg-catalog-service:<hostname>` |
| `namespace` | Extension `namespace` | `namespace` | VARCHAR | Iceberg namespace |
| `name` | Extension `name` | `name` | VARCHAR | Table or view name |
| `tabular_id` | Extension `tabular-id` | `tabular_id` | VARCHAR | `Table/<uuid>` or `View/<uuid>` |
| `warehouse_id` | Extension `warehouse-id` | `warehouse_id` | VARCHAR | Warehouse UUID |
| `data` | Event data payload | `data` | JSONB | Request body (table metadata, rename details, etc.) |
| `trace_id` | Extension `trace-id` | `trace_id` | VARCHAR | Request trace ID for distributed tracing |

> Note: CloudEvent extension keys use hyphens (`tabular-id`) but the JSON payload
> uses underscores (`tabular_id`) since hyphens are not valid in SQL column names.

### Event types

These are the `type` values published by Lakekeeper's `CloudEventsPublisher`:

**Table events:**
- `createTable` -- new table created
- `registerTable` -- existing table registered
- `updateTable` -- table metadata committed (schema change, snapshot, etc.)
- `dropTable` -- table dropped
- `renameTable` -- table renamed

**View events:**
- `createView` -- new view created
- `updateView` -- view metadata committed
- `dropView` -- view dropped
- `renameView` -- view renamed

**Other events:**
- `undropTabulars` -- tables/views restored from soft-delete

---

## Architecture Notes

### Ingestion path inside RisingWave

The webhook source uses RisingWave's **FastInsert RPC**, which is a dedicated
low-overhead path:

```
HTTP POST to frontend:4560
  -> Axum handler validates request (optional signature check)
  -> Packs JSON body as JSONB DataChunk
  -> FastInsert RPC to compute node (bypasses SQL parser, binder, planner)
  -> FastInsertExecutor -> StreamChunk with Op::Insert
  -> DML Manager -> streaming layer
  -> wait_for_persistence (default: true, blocks until Hummock commits)
  -> HTTP 200 returned to Lakekeeper
```

This path is faster than a SQL INSERT because it skips the entire query engine.
The `wait_for_persistence=true` default means that when Lakekeeper receives HTTP 200,
the data is durably committed -- not just buffered.

### High availability

Each RisingWave frontend node runs its own webhook listener. In a multi-frontend
deployment behind a load balancer:

```
Lakekeeper -> Load Balancer -> Frontend-1:4560
                             -> Frontend-2:4560
                             -> Frontend-3:4560
```

If a frontend goes down, the load balancer routes to another. No additional HA
configuration is needed for the event ingestion path.

### Failure behavior

- **Lakekeeper publish failure**: Logged as a warning and skipped. Events are
  fire-and-forget; there is no retry queue. This is the same behavior as NATS
  and Kafka backends.
- **RisingWave down**: HTTP POST fails, Lakekeeper logs a warning, event is lost.
- **Partial RisingWave outage**: Load balancer routes to healthy frontends.

### Delivery semantics

- **At-least-once** to RisingWave (retried HTTP requests could produce duplicates)
- **At-most-once** from Lakekeeper's perspective (no retry on failure)
- No built-in deduplication -- rely on the `id` field (UUIDv7) if you need to
  detect duplicates downstream

---

## Multi-Frontend / Kubernetes Example

A complete Kubernetes deployment with HA:

```yaml
# Lakekeeper deployment
env:
  - name: LAKEKEEPER__RISINGWAVE_WEBHOOK_URL
    value: "http://risingwave-frontend.risingwave.svc.cluster.local:4560/webhook/dev/public/lakekeeper_events_raw"
```

The `risingwave-frontend` service load-balances across all frontend pods
automatically.
