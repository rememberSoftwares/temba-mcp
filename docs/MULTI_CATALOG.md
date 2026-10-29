# Multiple catalogs and a local IaC server

One local `temba` process can query several catalogs. The AI client still exposes
only `tool_search`, `call_tool`, and optionally `add_mcp`.

## Connect to several catalogs

Define the shared options and catalog entries:

```yaml
allow_insecure_http: true
search_limit: 5
refresh_minutes: 60
labels: []
catalogs:
  - id: team
    url: https://catalogue.example.com
  - id: partner
    url: https://partner-catalog.example.com
```

Each catalog contributes up to five ranked results. Two catalogs can therefore
return up to ten tools. A source with fewer matches contributes fewer results.
Search scores are evaluated within each catalog; raw scores are not compared across sources.
An unavailable catalog is reported in `catalogs` with `partial: true`; others remain usable.
An unavailable source retries in the background at most once per minute; healthy
sources use `refresh_minutes`. Failed login retries remain explicit tool actions.

## Names and credentials stay separate

`team.service.search` and `local.service.search` are distinct tools. Temba removes
only the catalog and MCP prefixes before calling the upstream tool; dots inside
an upstream tool name are preserved. Use returned IDs exactly.

Catalog IDs must be unique in YAML, and MCP IDs must be unique within a catalog.
An MCP returning duplicate tool names or names reserved for connection actions is rejected.
The same URL may appear under distinct catalog IDs; those sessions are not merged.

```yaml
catalogs:
  - id: team
    url: https://catalogue.example.com
    tools:
      service:
        expected_endpoint: https://mcp.example.com/mcp
        headersFromEnv:
          - name: Authorization
            valueFrom:
              envVarRef:
                name: SERVICE_TOKEN
```

These credentials cannot be borrowed by `local.service`. OAuth sessions, token
refresh, permissions and HTTP ETags are also independent. Keycloak browser logins
sharing a callback port are queued briefly to prevent port conflicts. If the port
is still occupied after ten seconds, the queued source waits for another tool
request instead of opening a browser much later during inactivity. Each identity
provider still controls its own browser session and group rules.

## Serve static definitions locally

Add this section to the same YAML:

```yaml
local_catalog:
  enabled: true
  # listen: "127.0.0.1:8787" # Optional: this is the default
  servers:
    - id: documentation
      endpoint: https://mcp.example.com/mcp
      description: Technical documentation
      access:
        public: true
      headers:
        - name: X-Api-Key
          credential: token
          mandatory: true
  tools:
    documentation:
      expected_endpoint: https://mcp.example.com/mcp
      headersFromEnv:
        - name: X-Api-Key
          valueFrom:
            envVarRef:
              name: DOCUMENTATION_TOKEN
```

Start a separate process:

```sh
uvx --from /absolute/path/to/client.whl temba --config /absolute/path/to/temba.yaml serve
```

The server reads its definitions once at startup. Restart it after edits. Clients
pick up changes on their next `refresh_minutes` tick; restart the client for an
immediate reload.
It exposes `GET /v1/bootstrap`, `GET /v1/catalog` (with ETag), and `/healthz`.
There is no management API, volume, identity provider, or upstream MCP connection.
Definitions must be public; the server never returns private values from `catalogs[].tools`.
It binds to loopback by default. Choosing a network-wide listen address exposes
these public definitions to that network.

When `local_catalog.enabled: true` and at least one server is declared, the
client automatically includes it as catalog `local`. Do not add its URL to
`catalogs`; that ID is reserved. Disabled or empty local catalogs are not included.
`enabled` defaults to `false`. The `serve` process is independent and must stay
running; `serve` refuses to start if the catalog is disabled.

`listen` is optional and defaults to `127.0.0.1:8787`. The client derives its URL
from this address. Wildcard listeners (`0.0.0.0` / `::`) are contacted through
loopback. Choose a fixed nonzero port; if it is occupied, `serve` reports the
address and exits. Using the automatically configured local HTTP listener does
not require `allow_insecure_http` and does not allow HTTP MCP endpoints.

Put private headers under `local_catalog.tools`, with the same shape as
`catalogs[].tools`. Only `servers` is published by `serve`; private tools settings
are discarded before listening. If the server is stopped, other catalogs remain
usable and `local` is reported as unavailable.

## Keep dynamic additions independent

The LLM's third tool uses a different section:

```yaml
dynamic_catalog:
  enabled: true
  max_servers: 16
```

Its random IDs and credentials remain process-local. Only searches supplying the
secret `local_ids` can find those registrations; such scoped searches do not
trigger remote catalog login. Entries remain until disconnect or process exit.

## Inspect what your user can access

```sh
uvx --from /absolute/path/to/client.whl temba --config /absolute/path/to/temba.yaml catalog
uvx --from /absolute/path/to/client.whl temba --config /absolute/path/to/temba.yaml catalog --json
```

The terminal view uses qualified IDs and YAML credential paths. JSON contains a
`catalogs` array with source IDs, URLs, statuses and full definitions; `partial`
signals failures. No local label filter is applied. Group restrictions still apply.
If every source fails, the command exits nonzero without emitting partial JSON.

## Upgrade from the single-catalog client

The executable is now `temba`; the wheel package is still named `temba-client`.
Replace `catalog_url` with `catalogs`, move tool mappings under their catalog
alias, and rename the old dynamic `local_catalog` section to `dynamic_catalog`.
`local_catalog` is now exclusively for static server definitions. Set its
`enabled` flag, remove the former explicit local source from `catalogs`, and move
that source's `tools` block to `local_catalog.tools`.
Restart the OpenCode server or other AI host to replace existing MCP processes.
