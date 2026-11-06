# Multiple catalogs and a local IaC catalog

One local `temba` process can query several catalogs. The AI client still exposes
only `tool_search`, `call_tool`, and optionally `add_mcp`.

## Connect to several catalogs

Define the shared options and catalog entries:

```yaml
allow_insecure_http: true
search_limit: 5
refresh_minutes: 60
catalogs:
  - name: team
    url: https://catalogue.example.com
    labels: [support]
  - name: partner
    url: https://partner-catalog.example.com
    labels: [documentation]
```

Each remote catalog selects its own `labels` (default `[]`); the global `labels`
key is no longer accepted. Local and dynamically added MCPs have no label filter.

Keyword queries contribute up to five ranked results per catalog. Two catalogs can therefore
return up to ten tools. A source with fewer matches contributes fewer results.
Search scores are evaluated within each catalog; raw scores are not compared across sources.
An empty or omitted query instead lists visible MCP servers and loaded tool counts,
without tool descriptions or schemas. Keyword searches expose `total_matches`,
`returned_tools` and `truncated` globally and per catalog; `partial` describes
catalog availability, independently of the result cap.
An unavailable catalog is reported in `catalogs` with `partial: true`; others remain usable.
An unavailable source retries in the background at most once per minute; healthy
sources use `refresh_minutes`. Failed login retries remain explicit tool actions.

## Names and credentials stay separate

`team.service.search` and `local.service.search` are distinct tools. Temba removes
only the catalog and MCP prefixes before calling the upstream tool; dots inside
an upstream tool name are preserved. Use returned IDs exactly.

Catalog names must be unique in YAML, and MCP names must be unique within a catalog.
An MCP returning duplicate tool names or names reserved for connection actions is rejected.
The same URL may appear under distinct catalog names; those sessions are not merged.

```yaml
catalogs:
  - name: team
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

## Define a local static catalog

Add this section to the same YAML:

```yaml
local_catalog:
  enabled: true
  servers:
    - name: documentation
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

Set `enabled: true` and start your AI client as usual. Temba loads the local
catalog automatically as `local`: no separate process and no URL in `catalogs`
are needed. Restart the AI client's Temba process after editing the file.
Disabled or empty local catalogs are not included; `enabled` defaults to `false`.

HTTP entries are contacted directly from Temba. Local programs (`command` / `args`)
are managed by the same Temba process through an internal gateway on a free
loopback port. Each instance has its own catalog and programs. See
[local MCP programs](LOCAL_STDIO.md). HTTP definitions must be public; the local
catalog has no identity provider. Private headers belong in `local_catalog.tools`;
programs use `env` / `envFromEnv`.

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
temba catalog
temba catalog --json
```

The terminal view uses qualified IDs and YAML credential paths. JSON contains a
`catalogs` array with source names, URLs, statuses and full definitions; `partial`
signals failures. No local label filter is applied. Group restrictions still apply.
If every source fails, the command exits nonzero without emitting partial JSON.

## Upgrade from the single-catalog client

The native executable is `temba`; configure your AI client to run it directly.
Replace `catalog_url` with `catalogs`, move tool mappings under their catalog
alias, and rename the old dynamic `local_catalog` section to `dynamic_catalog`.
`local_catalog` is now exclusively for static server definitions. Set its
`enabled` flag, remove the former explicit local source from `catalogs`, and move
that source's `tools` block to `local_catalog.tools`.
Restart the OpenCode server or other AI host to replace existing MCP processes.
