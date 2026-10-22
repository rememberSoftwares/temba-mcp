# Defining and managing the catalog

The shared catalog contains **MCP server definitions**: their identifiers,
addresses, connection options, and visibility rules.
MCP calls and their OAuth sessions are managed by the local client.

To install the service, follow the [deployment guide](DEPLOYMENT.md).

## Declaring your first MCP server

In your Helm values file:

```yaml
catalog:
  servers:
    - id: documentation
      description: Search and browse the team's documentation
      endpoint: https://docs-mcp.example.com/mcp
      access:
        public: true
```

Replace the example domain with a real MCP server. The address must be reachable
**from users' machines**, usually through an HTTPS Ingress.
A Kubernetes Service address is not an address these machines can use.

| Field | Purpose |
| --- | --- |
| `id` | Unique MCP identifier, used in the TOML file, display and `tool_id` values |
| `description` | Helps the assistant identify what the service can be used for |
| `endpoint` | MCP Streamable HTTP transport URL |
| `labels` | Categories that users can enable locally |
| `timeoutSeconds` | Maximum time for MCP connections and calls, between 1 and 300 seconds, 60 by default |
| `oauth` | Enables and configures OAuth connections to the MCP server |
| `headers` | Declares expected private or public headers |
| `access` | Defines who can see the MCP server in the catalog |

Without `oauth` or `headers`, no remote authentication is configured.
Without `access`, the entry is public.

There is no separate display `name`. IDs are case-sensitive and must contain
1–64 ASCII letters, digits, hyphens or underscores, without the reserved `__` prefix.

Helm rejects duplicate IDs in `catalog.servers` at render time, before installation
or upgrade. The API rejects IDs already owned by either Helm or an API addition
with HTTP `409 Conflict`. Collisions with persisted API definitions are also
rejected when the catalog starts.

Outside Helm, the file loaded by the catalog binary uses the following schema:

```yaml
schemaVersion: 2
servers:
  - id: documentation
    endpoint: https://docs-mcp.example.com/mcp
```

## OAuth and headers: two options you can combine

### An OAuth connection

```yaml
catalog:
  servers:
    - id: service
      endpoint: https://mcp.example.com/mcp
      oauth: {}
```

The client discovers the provider and uses the browser with PKCE S256.
With `oauth: {}`, the client automatically discovers the provider announced by the MCP.
Additional destinations must resolve only to public IP addresses. HTTPS is required
unless the local client explicitly sets `allow_insecure_http = true`.
Private, loopback, link-local, and reserved destinations are blocked, including during
token requests and refreshes. The configured MCP origin remains implicitly allowed.

For explicit control, provide a nonempty `allowedOrigins` list. Only those origins
and the MCP origin are then allowed. An absent or empty list enables automatic discovery.
Use an explicit list for an identity provider on a private network, or on another
port of the MCP host. Adding an MCP in automatic mode also trusts the public OAuth
provider it announces.

For a provider without dynamic client registration (DCR), add:

```yaml
oauth:
  clientId: temba-service
  callbackPort: 19878
  scopes: [read]
  allowedOrigins: [https://identity.example.com]
```

The `clientId` and scopes depend on the provider. Register the exact
redirect URI `http://127.0.0.1:19878/oauth/callback` with it.
The callback runs on the user's machine. Without a fixed port for an MCP server,
the client chooses an available local port.

This connection is independent of Temba catalog authentication.
Tokens and their renewal stay within the local process.

### A token or multiple headers

```yaml
catalog:
  servers:
    - id: service
      endpoint: https://mcp.example.com/mcp
      headers:
        - name: Authorization
          credential: api_token
          mandatory: true
        - name: X-Tenant
          credential: tenant
        - name: X-Client
          value: temba
```

Each header has **exactly one source**:

- `credential` references a private value that the user provides locally;
- `value` contains a public value, identical for all clients.

`mandatory` defaults to `false`. A missing optional private value is omitted.
If a required value is missing, the client reports `credentials_required` before
contacting the MCP server. See [credentials in TOML](CLIENT.md#providing-tokens-and-private-headers).

Public values are visible in the user API and may be persisted.
**Do not put tokens or API keys there.** Private headers are not sent to the catalog.

Allowed names are `Authorization` and `X-*` names.
A public `value` header must be an `X-*`; duplicate names are rejected,
including names that differ only in case.

### OAuth with additional headers

The same MCP server can declare `oauth` and multiple `X-*` headers.
OAuth already provides `Authorization`, so it cannot be combined with a
second static `Authorization` header.

The client sends complete values without adding a prefix.
It does not forward private headers to OAuth discovery or token exchange endpoints.

## Making an MCP server visible to the right users

Choose one `access` rule per entry:

| Rule | Visibility |
| --- | --- |
| `public: true` | Everyone, even without a catalog connection |
| `authenticated: true` | All users authenticated with the catalog |
| `groupsAnyOf: ["team-a", "team-b"]` | Members of at least one group |

The initial `/` is optional in `groupsAnyOf`: `team-a` and `/team-a` are equivalent.
Nested paths remain exact: `teams/team-a` means `/teams/team-a`, not `/team-a`.
Matching is case-sensitive. Keep the Keycloak group mapper’s **Full group path** enabled.

Rules apply to **all tools of an MCP server**, not to the service operations
it subsequently authorizes. Knowing an MCP server's address directly does not
grant additional rights to that service.

Protected entries require `catalogueAuth.enabled: true`.
If this option is disabled, only public entries are visible.
The [Keycloak tutorial](KEYCLOAK.md) covers groups, mappers, and audience.

Labels are independent of groups: even a public MCP server with a label
must be enabled in the [local selection](CLIENT.md#selecting-mcp-servers-by-label).

If Keycloak discovery advertises incorrect endpoints, the Helm values
`catalogueAuth.jwksUri` and `catalogueAuth.tokenEndpoint` let you specify them.
These URLs must use HTTPS and share the same origin as `catalogueAuth.issuer`.
`jwksUri` is used for signature verification by the catalog; `tokenEndpoint`
is sent to the native client for code exchange and token renewal.


## Updating the shared catalog

There are two sources of definitions:

| Source | How to change it | Storage |
| --- | --- | --- |
| `catalog.servers` in Helm values | Helm update | Values file / deployed configuration |
| Administrative API | Addition or deletion using an administrative bearer token | File on a persistent volume |

Helm definitions remain managed by Helm: the API cannot replace or delete them.
Identifiers must be unique across both sources.

### Enabling the administrative API

By default, `adminApi.secretName` is empty: the administrative API allows no access.
Create a Kubernetes Secret containing a random administrative token, then reference it:

```yaml
adminApi:
  secretName: temba-admin
  tokenKey: token
  readOnly: false
persistence:
  enabled: true
  size: 100Mi
```

The Secret must exist in the catalog's namespace. The chart does not generate it.
`readOnly: true` only allows viewing definitions; `false` allows
additions and deletions, with a single replica and a persistent volume.

For storage, NetworkPolicies, and network protection for this API, see
[catalog options](CONFIGURATION.md).

### Adding a definition through the API

Create a `server.json` file:

```json
{
  "id": "service",
  "endpoint": "https://mcp.example.com/mcp",
  "access": {"public": true}
}
```

Then send it with the administrative bearer token supplied by your operator:

```sh
curl --fail-with-body \
  -H "Authorization: Bearer ${TEMBA_ADMIN_TOKEN}" \
  -H 'Content-Type: application/json' \
  --data @server.json \
  https://catalogue.example.com/v1/admin/servers
```

These additions are shared according to their `access` rule. For a personal,
temporary need, use the [local `add_mcp` tool](CLIENT.md#temporary-local-additions).

### What happens on restart?

API additions are saved in `/var/lib/temba/additions.json`, then loaded at startup.
The API confirms a change after saving and syncing the file.
A write failure does not publish the change in RAM.

An invalid file or a conflict with a Helm definition blocks startup
to prevent silent data loss. Back up the PVC: it is retained when
Helm uninstalls the release, and permanently deleting it remains an administrative action.

The volume contains neither OAuth tokens nor users' private headers.

## Exposed routes

| Route | Access and purpose |
| --- | --- |
| `GET /healthz`, `GET /readyz` | Liveness and readiness probes |
| `GET /v1/bootstrap` | Public, nonsecret catalog OAuth settings |
| `GET /v1/catalog` | User API: filtered definitions, ETag, and private cache |
| `GET /v1/admin/servers` | Administrative bearer token: all definitions |
| `POST /v1/admin/servers` | Persistent addition if writes are enabled |
| `DELETE /v1/admin/servers/{id}` | Persistent deletion of an API addition |

Users can inspect these definitions with `temba-client catalog` or `temba-client catalog --json`;
the command handles catalog login and lists all authorized entries before local label filtering.
See [the client guide](CLIENT.md#inspecting-the-catalog).

`GET /v1/catalog` exposes `headers` declarations, including `mandatory`,
`credential`, or `value`, to authorized clients. The local client uses them to determine
configuration requirements without unnecessarily exposing these declarations to the LLM.

The server validates catalog access tokens: RS256 signature, issuer,
audience, expiration, `nbf`, subject, and `Bearer` type. No custom scope is required.
