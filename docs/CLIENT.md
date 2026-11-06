# Configuring the local client

This guide complements the [README getting started guide](../README.md#getting-started).
The `temba.yaml` file belongs on the machine running `temba`.
It is read when the process starts.

## Create and update the configuration

```sh
temba add catalog
temba add mcp
temba set search_limit 10
temba set refresh_minutes 120
temba set dynamic_catalog true
```

`add catalog` asks for a URL, unique name and optional comma-separated labels.
An empty URL enables the local catalog; use `temba add mcp` to add its MCPs.
Adding an HTTP URL asks permission to enable unencrypted HTTP. These commands configure Temba without connecting to a catalog or starting MCPs.

The default file is **`~/.temba/temba.yaml`** on Linux/macOS, and
**`%USERPROFILE%\.temba\temba.yaml`** on Windows. Both refer to the current user's
home directory. No file is loaded from the working directory automatically.
A missing file is reported with its full location and setup instructions.
`--config FILE` takes priority over `TEMBA_CONFIG`, which takes priority over the default:

```sh
temba --config /absolute/path/to/custom.yaml add catalog
temba --config /absolute/path/to/custom.yaml set search_limit 10
```

`set` also accepts `allow_insecure_http`, `dynamic_catalog.enabled`,
`dynamic_catalog.max_servers` and `local_catalog.enabled`.
See `temba set --help` for boolean values and integer ranges.
Changes are validated before saving; existing settings and credentials are retained.
The commands rewrite YAML formatting and remove comments. New files use private
permissions on Unix. Restart your AI host's Temba process to apply changes.

## Add an MCP interactively

```sh
temba add mcp
```

Choose a catalog by number or name. The menu always offers `local`, including
when it has not been enabled yet:

- **Local catalog:** add an HTTP endpoint or leave it empty to configure a stdio
  program. The assistant asks for a unique name, optional description and timeout.
  HTTP entries support OAuth plus extra headers. Advanced OAuth options are optional.
  For stdio, enter each argument separately, an optional absolute working directory,
  and any environment variables the program needs. The local catalog is enabled on save.
- **Remote catalog:** configure local headers for an MCP already published there.
  Copy its exact name and endpoint from `temba catalog`. Definitions, OAuth and
  access rules remain managed by the shared catalog; this command only writes
  `catalogs[].tools` on your computer.

Headers can read an environment variable or hold a private literal value.
Private values are hidden when typed in a terminal. For local HTTP definitions,
headers can also hold public values, and private headers can be marked mandatory.
Variables referenced from the environment must be available when your AI host starts.

The assistant does not execute commands, connect to MCPs or modify a shared catalog.
Duplicate names or headers cannot overwrite existing settings. An abandoned or
invalid setup leaves the configuration file unchanged.

## Basic settings

```yaml
refresh_minutes: 60
search_limit: 5
catalogs:
  - name: team
    url: https://catalogue.example.com
    labels: []
```

| Option | Purpose | Default |
| --- | --- | --- |
| `catalogs[].name` | Unique local catalog alias; used in tool IDs and credential mappings | Required per catalog |
| `catalogs[].url` | REST catalog URL, without an `/mcp` or `/v1/catalog` suffix | Required per catalog |
| `catalogs[].labels` | Labels selected independently for this remote catalog | `[]`: MCP servers without labels |
| `refresh_minutes` | Refresh interval for each catalog, between 1 and 1,440 minutes (24 hours) | `60` |
| `search_limit` | Maximum results per catalog per search, between 1 and 50 | `5` |
| `allow_insecure_http` | Allows unencrypted HTTP for local and remote destinations; HTTPS certificate checks remain enabled | `false` |

For a catalog accessed through port forwarding:

```yaml
allow_insecure_http: true
catalogs:
  - name: team
    url: http://127.0.0.1:18080
```

Remote MCP servers must be reachable **from the user's machine**.
A localhost URL refers to that machine, not a Kubernetes pod.
See [local HTTP and networking](CONFIGURATION.md).

## Inspecting the catalog

List the MCP definitions available to your user before configuring private headers:

```sh
temba catalog
```

The default terminal view shows each MCP's name, endpoint, labels, access rule,
OAuth settings, and public or expected private headers. A required private header
is shown with its YAML location, for example `catalogs[name=team].tools.service.headers.Authorization`
or `headersFromEnv`.
Per-catalog label selections do not filter this list, so you can discover available categories.

For JSON containing each catalog’s name, URL, status and full definitions, plus a `partial` flag:

```sh
temba catalog --json
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
AI clients launch `temba` without a subcommand. The old executable name was `temba-client`.

## Multiple catalogs and local serving

Configure `catalogs` entries with distinct names. Search returns up to
`search_limit` results from each source, with IDs such as `team.service.search`.
A failed catalog is reported through `partial` and `catalogs` without hiding
results from healthy sources. Sessions, ETags and credentials are isolated.
Catalog login callbacks sharing a port are queued; background refresh never opens a browser.

`dynamic_catalog` enables the LLM's temporary `add_mcp` tool.
`local_catalog` declares your static IaC definitions. Set
`local_catalog.enabled: true` to load them automatically when Temba starts;
no additional process or URL in `catalogs` is needed. Its reserved catalog
name is `local`. HTTP entries are contacted directly; `command` / `args` entries
are launched locally by Temba. `local_catalog.tools` holds private HTTP headers;
program environments belong in `env` / `envFromEnv`.
See [local MCP programs](LOCAL_STDIO.md).
The flag defaults to `false`.
See [multiple catalogs and local configuration](MULTI_CATALOG.md) for the complete setup.

## Selecting MCP servers by label

Each remote entry in `catalogs` has its own optional `labels` list, defaulting
to `[]`. To include an MCP server, all of its labels must appear in that catalog's
list. MCP servers without labels remain included. Labels select categories without
changing your permissions; selecting `support` in one catalog has no effect on
another catalog.

`local_catalog` and `dynamic_catalog` have no label selection: their MCPs are
available without this filter, even if a server definition contains labels.
The former root-level `labels` key is rejected. Move it inside each remote
catalog where you want to keep the same selection.

| Labels in a remote catalog entry | MCP servers included, among those allowed by the catalog |
| --- | --- |
| `[]` | Those without any labels |
| `["support"]` | Those without labels and those labeled only `support` |
| `["support", "documentation"]` | Those whose labels are a subset of this list |

Group rules are checked separately by the central catalog.
A label cannot bypass a Keycloak rule.

## Inventory and tool search

Use `tool_search` to answer which MCPs are available. Its `query` is optional:

```json
{"query": ""}
```

An empty, whitespace-only or omitted query returns `mode: inventory`: visible MCP
names, connection states, per-server `tool_count` and aggregate `loaded_tools`.
`tools` is empty; tool descriptions, parameter schemas and missing-header details
are not included. A server's `tool_count` is `null` until its tools have loaded.
The inventory is not capped by `search_limit` and does not wait for MCP discovery
to finish. Catalog login still starts automatically when required.

For detailed tools and connection actions, supply keywords:

```json
{"query": "search documents"}
```

This returns `mode: search`, with full descriptions and `inputSchema` for at most
`search_limit` matches per catalog. `total_matches`, `returned_tools`, `limit` and
`truncated` explain each catalog's cap; aggregate counts are also returned.
Matches include connection actions for MCPs that are not ready. `partial`
separately signals a catalog awaiting login, loading or unavailable. Neither count
is a token budget: descriptions and schemas can be large even for one tool.

For private dynamic registrations, pass their `local_ids` in either mode. Other
registrations are not enumerated.

## Providing tokens and private headers

Suppose the catalog contains this definition:

```yaml
name: service
endpoint: https://mcp.example.com/mcp
headers:
  - name: Authorization
    credential: api_token
    mandatory: true
  - name: X-Tenant
    credential: tenant
```

Your YAML maps credentials to **a catalog alias and an MCP server identifier**.
The examples use catalog alias `team`:

```yaml
catalogs:
  - name: team
    url: https://catalogue.example.com
    tools:
      service:
        expected_endpoint: https://mcp.example.com/mcp
        headers:
          X-Tenant: engineering
        headersFromEnv:
          - name: Authorization
            valueFrom:
              envVarRef:
                name: SERVICE_TOKEN
```

Use the actual **HTTP header names** in both sections: `Authorization`, `X-Tenant`, etc.
The catalog's `credential` aliases identify private headers in its definitions;
they are not keys in the client configuration. Dynamically registered MCPs still
use credential aliases in their registration arguments.

### Direct value or environment variable?

A string is always a **literal value**:

```yaml
catalogs:
  - name: team
    url: https://catalogue.example.com
    tools:
      service:
        expected_endpoint: https://mcp.example.com/mcp
        headers:
          Authorization: Bearer your-token
```

To read a variable, use `headersFromEnv`:

```yaml
headersFromEnv:
  - name: Authorization
    valueFrom:
      envVarRef:
        name: SERVICE_TOKEN
```

This block belongs inside the MCP's configuration alongside `expected_endpoint`.
`name: Authorization` is the HTTP header; `SERVICE_TOKEN` is the environment
variable to read. This resembles Kubernetes syntax but reads the local process's
environment, not a Kubernetes Secret. No `secretKeyRef` is supported here.

Writing `headers: {Authorization: "SERVICE_TOKEN"}` sends those literal words.
A header must appear only once across both sections; names are case-insensitive.
Missing or empty variables count as absent: a required catalog header blocks the
connection, while optional headers are omitted. Private client headers can also
be supplied without a catalog declaration. `Authorization` is rejected when the
MCP uses OAuth, which supplies that header itself.

The value is sent as provided: if the MCP server expects `Bearer …`, include
the prefix. If it expects a raw token, provide only the token.

A direct value stays on disk in the YAML file; keep this file private and out of the repository.
An environment variable must be present in the **Temba process**.
An AI server that is already running does not inherit variables exported later in another terminal.

### Why `expected_endpoint`?

`endpoint` is the address advertised by the catalog.
`expected_endpoint` is the address to which **you authorize sending your private values**.
The two must match exactly, including the path and query parameters.

If the destination changes, Temba refuses to send your headers until you
approve the new address in the YAML file. These values are not sent
to the catalog, OAuth providers, or other MCP servers. HTTP redirects are disabled.

This setting is required for private headers configured in the YAML file.
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
by the search. The technical actions are `<name>.__temba_connect__`, called through `call_tool`.

Temba waits up to 10 seconds for the result of an authorization it has started. If your
browser session allows quick SSO, the request can continue immediately.
Otherwise, the assistant indicates that browser approval is still pending.
Keyword searches may also wait briefly for ongoing tool discovery.

Periodic refresh does not reopen the browser. Renewal using a
refresh token is silent; if new authorization is required,
it waits for a subsequent tool call. Failed service calls are not retried automatically.

OAuth callbacks listen on `127.0.0.1`, expire after ten minutes, and can be used only once.
Temba writes OAuth tokens neither to disk nor to the system keychain.
A new connection may therefore be needed after the process stops.

### Reading catalog states

`tool_search` reports each source's status in `catalogs[].status`:

| State | Meaning |
| --- | --- |
| `public` | Catalog authentication is disabled |
| `auth_in_progress` | Browser authorization is in progress |
| `loading` | Authentication succeeded; definitions are loading |
| `authenticated` | The authenticated catalog is loaded |
| `auth_required` | A connection is required; a relevant call will trigger it |
| `auth_failed` | The connection failed or expired before approval |
| `catalog_unavailable` | Authentication succeeded, but loading failed |

Detailed searches also include authentication flags per catalog. The compact
inventory keeps the status and relevant hint. `catalogs[].connect_tool` offers a
retry only after authentication fails, not during a connection or after success.

The `<name>.__temba_disconnect__` action closes the MCP connection and removes its OAuth session.
`__catalog__.__temba_disconnect__` ends the catalog session and closes connections,
including local registrations.

## Temporary local additions

This option, disabled by default, adds the `add_mcp` tool:

```yaml
dynamic_catalog:
  enabled: true
  max_servers: 16
```

| Option | Effect |
| --- | --- |
| `enabled` | Exposes or removes the dynamic addition tool |
| `max_servers` | Maximum simultaneous local registrations per process: 16 by default, between 1 and 64 |

When upgrading, remove the former `idle_timeout_seconds` option from your YAML.
Registrations no longer expire due to inactivity.

### Adding an MCP server with a token

Here are the arguments for `add_mcp`:

```json
{
  "definition": {
    "name": "temporary-service",
    "endpoint": "https://mcp.example.com/mcp",
    "headers": [
      {"name": "Authorization", "credential": "api_token", "mandatory": true}
    ]
  },
  "headers": {"api_token": "Bearer your-token"}
}
```

The values are specific to this registration; it does not read headers from the YAML file.
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
    "name": "temporary-service",
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

After changing the YAML file or environment, restart the Temba process.
After updating Temba, replace the installed native executable and restart its process.
If you use the optional uvx method during development, `--no-cache` avoids reusing
an older wheel rebuilt with the same version and path.

With a persistent AI server, restart the server that launches the MCP servers.
Closing only a conversation or reopening the terminal is not always enough.

Logs go to **stderr**, while stdout is reserved for the MCP protocol.
They show MCP server names, their states, and connection progress:

```text
MCP service-a: ready; ready=2/3; pending=[service-b:connecting]
```

Configuration errors identify the YAML file, the nearest line and column, and the
field path (list indices start at zero). For example:

```text
Configuration 'temba.yaml' near line 12, column 5 at local_catalog.servers[0].access:
Unknown field 'access'. Remove access from command/args entries; local stdio MCPs do not use catalog access rules.
```

Type errors show the expected format without echoing the supplied value.
Validation errors identify the catalog or local MCP entry. Missing process
variables name the `envFromEnv` reference; executable errors suggest checking
installation, `PATH`, permissions or `cwd`. HTTP errors distinguish rejected
credentials, denied access, missing endpoints, rate limits and service failures.

| Symptom | Check |
| --- | --- |
| Missing MCP server | Keycloak token groups, per-catalog labels, and catalog definition |
| `credentials_required` | `credential` references, nonempty values, and the Temba process environment |
| Destination rejected | Exact match between `expected_endpoint` and `endpoint`; check the change before approving |
| OAuth origin rejected | `oauth.allowedOrigins` in the correct definition, central or local |
| Callback port already in use | Another connection is using this local port; stop the old process or configure another port |
| Call timeout | Check the result of the remote operation before retrying a write |

The client refreshes the catalog every 60 minutes by default. Removing or
changing a definition closes the old connection. An invalid response
does not replace the last valid catalog.

For the memory and token protection model, see [client security](../SECURITY_CLIENT.md).
