# Configuring the local client

This guide complements the [README getting started guide](../README.md#getting-started).
The `temba.toml` file belongs on the machine running `temba-client`.
It is read when the process starts.

## Basic settings

```toml
catalog_url = "https://catalogue.example.com"
labels = []
refresh_minutes = 60
search_limit = 5
```

| Option | Purpose | Default |
| --- | --- | --- |
| `catalog_url` | REST catalog URL, without an `/mcp` or `/v1/catalog` suffix | Required |
| `labels` | Labels of the MCP servers you choose to include | `[]`: MCP servers without labels |
| `refresh_minutes` | Central catalog refresh interval, between 1 and 1,440 minutes (24 hours) | `60` |
| `search_limit` | Maximum number of tools returned per search, between 1 and 50 | `5` |
| `allow_insecure_http` | Allows unencrypted HTTP for local and remote destinations; HTTPS certificate checks remain enabled | `false` |

If upgrading an existing TOML, replace `allow_loopback_http` or `allow_localhost_http`
with `allow_insecure_http`. This now permits remote HTTP too. OAuth destination
approval remains separate: it does not authorize automatically discovered private destinations.
Replace `refresh_seconds` with `refresh_minutes`, dividing the old value by 60.
For example, `refresh_seconds = 3600` becomes `refresh_minutes = 60`.

For a catalog accessed through port forwarding:

```toml
catalog_url = "http://127.0.0.1:18080"
allow_insecure_http = true
```

Remote MCP servers must be reachable **from the user's machine**.
A localhost URL refers to that machine, not a Kubernetes pod.
See [local HTTP and networking](CONFIGURATION.md).

## Inspecting the catalog

List the MCP definitions available to your user before configuring private headers:

```sh
uvx --from /absolute/path/to/client.whl temba-client --config /absolute/path/to/temba.toml catalog
```

The default terminal view shows each MCP's ID, endpoint, labels, access rule,
OAuth settings, and public or expected private headers. A required private header
is shown with its TOML reference, for example `tool.service.headers.api_token`.
Local labels do not filter this list, so you can discover available categories.

For the complete API response as JSON:

```sh
uvx --from /absolute/path/to/client.whl temba-client --config /absolute/path/to/temba.toml catalog --json
```

If the catalog uses Keycloak, the command opens your browser and waits for login.
It uses its own in-memory session; it does not export or reuse another Temba
process's token. Browser cookies may make login immediate. Login instructions go
to stderr, leaving stdout valid JSON for pipes such as `| jq`.

The command contacts only the catalog and its identity provider. It does not
connect to remote MCPs, send their private headers, or display values from your
local credentials. Group restrictions still apply. With catalog authentication
disabled, only public definitions appear. No tokens are saved when the command exits.

Running the binary without a subcommand still starts the stdio MCP server, so
existing AI client commands remain valid.

## Selecting MCP servers by label

Labels enable categories of MCP servers without changing your permissions.
To include an MCP server, all of its labels must appear in your list.
MCP servers without labels remain included.

| Labels in the TOML file | MCP servers included, among those allowed by the catalog |
| --- | --- |
| `[]` | Those without any labels |
| `["support"]` | Those without labels and those labeled only `support` |
| `["support", "documentation"]` | Those whose labels are a subset of this list |

Group rules are checked separately by the central catalog.
A label cannot bypass a Keycloak rule.

## Providing tokens and private headers

Suppose the catalog contains this definition:

```yaml
id: service
endpoint: https://mcp.example.com/mcp
headers:
  - name: Authorization
    credential: api_token
    mandatory: true
  - name: X-Tenant
    credential: tenant
```

Your TOML file provides values for **this MCP server identifier**:

```toml
[tool.service]
expected_endpoint = "https://mcp.example.com/mcp"

[tool.service.headers]
api_token = { env = "SERVICE_TOKEN" }
tenant = "engineering"
```

`api_token` and `tenant` are the catalog's `credential` references.
They are not HTTP header names.

### Direct value or environment variable?

A string is always a **literal value**:

```toml
[tool.service]
expected_endpoint = "https://mcp.example.com/mcp"

[tool.service.headers]
api_token = "Bearer your-token"
```

To read a variable, explicitly use `{ env = "SERVICE_TOKEN" }`.
Writing `api_token = "SERVICE_TOKEN"` would send those words to the server.

The value is sent as provided: if the MCP server expects `Bearer …`, include
the prefix. If it expects a raw token, provide only the token.

A direct value stays on disk in the TOML file; keep this file private and out of the repository.
An environment variable must be present in the **Temba process**.
An AI server that is already running does not inherit variables exported later in another terminal.

### Why `expected_endpoint`?

`endpoint` is the address advertised by the catalog.
`expected_endpoint` is the address to which **you authorize sending your private values**.
The two must match exactly, including the path and query parameters.

If the destination changes, Temba refuses to send your headers until you
approve the new address in the TOML file. These values are not sent
to the catalog, OAuth providers, or other MCP servers. HTTP redirects are disabled.

This setting is required for private headers configured in the TOML file.
A public header provided by the catalog does not need it.

### What does the assistant see?

A missing optional header is simply omitted. A missing `mandatory: true` header
blocks the connection before any remote call.

In that case, the MCP server remains visible with `credentials_required`, and `missing_headers`
lists only the missing required headers. Once credentials are
configured, header declarations are not exposed by `tool_search`.
Their private values are never returned in its results.

## Understanding OAuth connections

There are two independent connections:

- **To the catalog**, if its administrator has enabled `catalogueAuth`: this determines
  which definitions are visible to your identity.
- **To each OAuth MCP server**: this authorizes access to the remote service with your account.

A public catalog can therefore contain an OAuth MCP server. A local registration can
also use OAuth without authentication to the central catalog.

The catalog connection starts automatically on the first tool call that
requires it. For a remote MCP server, the assistant uses the connection action returned
by the search. The technical actions are `<id>.__temba_connect__`, called through `call_tool`.

Temba waits up to 10 seconds for the result of an authorization it has started. If your
browser session allows quick SSO, the request can continue immediately.
Otherwise, the assistant indicates that browser approval is still pending.
Search may also wait briefly for ongoing tool discovery.

Periodic refresh does not reopen the browser. Renewal using a
refresh token is silent; if new authorization is required,
it waits for a subsequent tool call. Failed service calls are not retried automatically.

OAuth callbacks listen on `127.0.0.1`, expire after ten minutes, and can be used only once.
Temba writes OAuth tokens neither to disk nor to the system keychain.
A new connection may therefore be needed after the process stops.

### Reading catalog states

`tool_search` returns `catalog.status`, among other fields:

| State | Meaning |
| --- | --- |
| `public` | Catalog authentication is disabled |
| `auth_in_progress` | Browser authorization is in progress |
| `loading` | Authentication succeeded; definitions are loading |
| `authenticated` | The authenticated catalog is loaded |
| `auth_required` | A connection is required; a relevant call will trigger it |
| `auth_failed` | The connection failed or expired before approval |
| `catalog_unavailable` | Authentication succeeded, but loading failed |

The response also contains `catalog.authentication_enabled`, `catalog.authenticated`,
and `catalog.automatic_login`. `catalog_connect_tool` offers a retry only
after authentication fails, not while a connection is in progress or already established.

The `<id>.__temba_disconnect__` action closes the MCP connection and removes its OAuth session.
`__catalog__.__temba_disconnect__` ends the catalog session and closes connections,
including local registrations.

## Temporary local additions

This option, disabled by default, adds the `add_mcp` tool:

```toml
[local_catalog]
enabled = true
max_servers = 16
```

| Option | Effect |
| --- | --- |
| `enabled` | Exposes or removes the dynamic addition tool |
| `max_servers` | Maximum simultaneous local registrations per process: 16 by default, between 1 and 64 |

When upgrading, remove the former `idle_timeout_seconds` option from your TOML.
Registrations no longer expire due to inactivity.

### Adding an MCP server with a token

Here are the arguments for `add_mcp`:

```json
{
  "definition": {
    "id": "temporary-service",
    "endpoint": "https://mcp.example.com/mcp",
    "headers": [
      {"name": "Authorization", "credential": "api_token", "mandatory": true}
    ]
  },
  "headers": {"api_token": "Bearer your-token"}
}
```

The values are specific to this registration; it does not read headers from the TOML file.
`add_mcp` returns a secret 256-bit `local_id` and a `connect_tool_id`.

1. Keep the `local_id` in the conversation.
2. Search for tools with `tool_search`, passing `local_ids: [local_id]` and a query.
3. If the status is `auth_required`, call `connect_tool_id` through `call_tool`.
4. Once the status is `ready`, use the `tool_id` values returned by the search.

Without `local_ids`, local registrations are not listed.
The `<local_id>.__temba_disconnect__` action removes the registration.

### Adding an OAuth MCP server

Declare external providers' HTTPS origins in **the local definition**:

```json
{
  "definition": {
    "id": "temporary-service",
    "endpoint": "https://mcp.example.com/mcp",
    "oauth": {}
  }
}
```

An absent or empty `oauth.allowedOrigins` enables automatic discovery of public
OAuth destinations. HTTPS is required unless `allow_insecure_http = true`. A nonempty list restricts discovery to those origins and the
configured MCP origin. Private destinations require explicit approval.
This setting is independent of Temba catalog authentication.

To change an explicit restriction, disconnect the registration and run `add_mcp`
again with the corrected definition. No change to the central catalog is needed.

### Lifetime and scope

Registrations remain available regardless of inactivity. Pausing a conversation
does not remove them. Explicitly disconnecting an entry removes its ID, closes
its connection and clears the private credentials held by the client.
Stopping the process removes all registrations. OAuth token expiration and refresh
remain governed by the identity provider.

Two additions with the same name receive separate IDs and connections.
**Possession of the ID authorizes use** within that process: there is no
universal conversation identity. The AI client may store arguments,
provided tokens, and responses in its own history.

## Updating and troubleshooting

After changing the TOML file or environment, restart the Temba process.
After updating the client, also change the wheel path in your AI client.
Replacing a wheel at the same path may cause `uvx` to use its old cached binary.

With a persistent AI server, restart the server that launches the MCP servers.
Closing only a conversation or reopening the terminal is not always enough.

Logs go to **stderr**, while stdout is reserved for the MCP protocol.
They show MCP server names, their states, and connection progress:

```text
MCP service-a: ready; ready=2/3; pending=[service-b:connecting]
```

| Symptom | Check |
| --- | --- |
| Missing MCP server | Keycloak token groups, local labels, and catalog definition |
| `credentials_required` | `credential` references, nonempty values, and the Temba process environment |
| Destination rejected | Exact match between `expected_endpoint` and `endpoint`; check the change before approving |
| OAuth origin rejected | `oauth.allowedOrigins` in the correct definition, central or local |
| Callback port already in use | Another connection is using this local port; stop the old process or configure another port |
| Call timeout | Check the result of the remote operation before retrying a write |

The client refreshes the catalog every 60 seconds by default. Removing or
changing a definition closes the old connection. An invalid response
does not replace the last valid catalog.

For the memory and token protection model, see [client security](../SECURITY_CLIENT.md).
