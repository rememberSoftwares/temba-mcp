# Temba-mcp

Temba is an MCP gateway that exposes a catalog of remote MCP servers for the LLM to discover.

**Classic MCP pain points:**
* Adding a single MCP server can easily cost more than 20.000 tokens each => Induces lag, expense, degraded answers
* Adding/removing MCP servers is a manual process that requires upgrading config file each time => Painfull as an admin
* Near futur will probably force us to deal with 40+ MCP servers on a daily basis hence making above pain points untolerable.

**How temba saves the day ?** 🎉

Temba provides a search tools capability so that the LLM loads only the tools it needs.  
Agents no longer load 70 tools at boot: they search by keywords, receive the matching tools along with their JSON Schema, and execute them through the gateway.  
Your context window will stay light and fast to process. Admins can administer the catalogue for the whole organisation with ease.  

**Features list:**
* 🔍 LLM can search tools in a tool catalog
* 🤖 LLM can add new MCPs to it's own catalog on the go (optionnal feature flag)
* 🔌 Admins can dynamically add tools using the API
* 🏷️ Admins can select what tools can be discovered to clients through labels (not a security feature though)

## ⚙️ How it works

Temba exposes 3 tools (`tool_search`, `call_tool`, optional `add_mcp`) and aggregates a catalog of remote MCP servers (streamable HTTP) on demand.  

```
opencode / agents ──HTTP──> temba-mcp (2-3 tools) ──HTTP──> remote MCP servers
                            │__tool_search → index (BM25 or embeddings)
                            │__call_tool   → tools/call routed to the source MCP
                            │__add_mcp     → private ephemeral server (24h)
                            
```

## 🚀 Deploying on k8s

Public MCPs can run without Keycloak. By default, `auth.enabled: false` starts
Temba in public-only mode. Only servers without configured headers, OAuth,
embedded URL credentials or a private owner are accessible.

All clients configure **one URL: `/mcp`**. They do not choose an endpoint based on
the upstream server's authentication mode. Temba routes each request internally.
Anonymous initialization, discovery and calls to public servers do not contact
Keycloak, even when the catalog also contains protected servers.

## Catalog authentication modes

| MCP type | Catalog configuration | What the user does | Account used upstream |
| --- | --- | --- | --- |
| No authentication | URL only; no OAuth or credential headers | Calls tools immediately, without SSO | No credentials |
| Static token | Administrator sets that server's `headers`, using Secret references | Connects to Temba's SSO when this protected server is requested | Organization account associated with that server's token |
| OAuth/OIDC provider | Administrator sets `oauth: {}` | Connects to Temba's SSO, then authorizes their own provider account through the login link | Personal provider account, isolated by Temba identity |

The upstream authentication type and the user's identity at Temba are separate.
A static catalog token authenticates Temba to Jira/GitLab; it does not identify the
human using Temba. That is why credentialed servers require Temba SSO before their
credentials can be used. **Personal static tokens passed from OpenCode through
Temba are not supported yet.** A credentialless internal server may also be
restricted explicitly with `access: authenticated`.

Anonymous `tool_search` returns public tools and, subject to catalog labels, names
of protected servers with a connection hint. When the agent requests one of those
servers, the HTTP endpoint returns the standard MCP OAuth **401 challenge**. An
OAuth-capable client handles the Temba login and retries with its access token.
The user configures no extra URL, GUID or per-service token in the Temba connection.
The reserved `call_tool` name `<server_id>.__temba_connect__` lets the agent start
this transition before the provider's real tool names are available; it returns
provider login instructions or the available tool schemas, without executing an
upstream action. Actual client-side retry behavior is part of the pending
OpenCode/Keycloak integration validation.

If Keycloak is not configured, public servers keep working; attempts to use
protected entries are refused. Supplying an invalid/expired bearer token never
silently downgrades the request to anonymous access.

## Enabling protected catalog access

To enable credentialed MCPs, configure Keycloak and HTTPS first:

```yaml
publicBaseUrl: https://temba.example.com
replicaCount: 1
auth:
  enabled: true
  issuer: https://keycloak.example.com/realms/company
  audience: temba-mcp
  scopes: [temba:use]
  browserClientId: temba-login
```

```sh
helm upgrade --install temba-mcp ./chart/temba-mcp -f production-values.yaml
```

With `auth.enabled: true`, missing issuer/HTTPS configuration prevents startup
(except for the explicit local HTTP development option below);
protected calls never fall back to anonymous access. With authentication
disabled, token-authenticated and OAuth servers stay inaccessible. You can also
restrict a credentialless server with `access: authenticated`. Setting
`access: public` does not override the protections on a credentialed server.

For local development without an HTTPS reverse proxy, use:

```yaml
ingress:
  enabled: false
publicBaseUrl: http://localhost:8080
security:
  allowInsecureHttp: true
auth:
  enabled: true
  issuer: https://keycloak.example.com/realms/company
  audience: temba-mcp
  scopes: [temba:use]
  browserClientId: temba-login
```

Expose the gateway with `kubectl port-forward service/temba-mcp 8080:8080`
(replace the service name with your Helm release name). Register exactly
`http://localhost:8080/oidc/callback` as the `temba-login` client's valid redirect
URI in Keycloak. Root URL, Home URL, post-logout redirect URIs and Web origins
can remain empty. OpenCode's callback belongs to its separate client.

Enabling `security.allowInsecureHttp` only permits Temba HTTP
on `localhost`, `127.0.0.1` or `::1`. Use the same host and port in the client URL,
`publicBaseUrl` and Keycloak redirect URI. In HTTP mode, login cookies retain
HttpOnly and SameSite=Lax but omit Secure; bearer tokens and cookies travel without
TLS. Use this only for local development. HTTPS URLs keep Secure cookies even
when the option is enabled. Keycloak issuer/discovery endpoints, upstream OAuth
requirements remain enforced.

Only **one gateway replica/process** is supported in this version. The chart rejects
other replica counts and uses `Recreate` to avoid concurrent OAuth owners during
upgrades. All runtime state lives in memory: a restart requires restarting login
flows, reconnecting personal OAuth providers and recreating dynamic catalog entries.
The static catalog configured through Helm remains available.


Example opencode client MCP config (in `~/.config/opencode/opencode.json`):

```json
"temba-mcp": {
  "type": "remote",
  "url": "http(s)://<temba-url>/mcp",
  "headers": { "X-Temba-Labels": "see-labels-section-bellow" }
}
```

## 📚 Adding MCPs to the catalogue

The catalog is where you'll add remote MCP URLs that will be proxified by Temba.  
Each tool added to the catalog will be made available to any LLMs that look for it.  

### Catalog using infrastructure as code

The IAC catalog is declared in `values.yaml` (`catalog.servers`).

ie for DeepWiki:
```yaml
catalog:
  servers:
    - id: deepwiki
      url: https://mcp.deepwiki.com/mcp
      timeout: 45                  # seconds, optional
      labels: [docs]               # optional: restricted visibility (see below)
      headers: {}                  # optional, ${VAR} interpolated from pod env
      oauth: {}                    # optional: dynamic OAuth (DCR + PKCE); {} = dynamic
```

### Catalog using REST API

The catalog can be managed by API so admins can programmatically list, add or remove tools on the fly.

First you'll need the temba's API token (auto-generated by the chart as `<release>-admin` when `adminApi.secretName` is empty):

```sh
export TOKEN=$(kubectl get secret temba-mcp-admin -o jsonpath='{.data.ADMIN_API_TOKEN}' | base64 -d)
```


#### Add a new MCP to the catalog

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"id": "mcp-name-or-whatever", "url": "https://…/mcp", "labels": ["team-x"], "headers": {"Authorization": "Bearer ${…}"}}' \
  http://<temba-url>/servers
```
:information_source: Added servers join the catalog shared by **all** authorized LLM clients


#### Delete an MCP server

```sh
curl -X DELETE -H "Authorization: Bearer $TOKEN" http://<temba-url>/servers/<mcp-name-used-during-registration>
```

#### List current MCP servers

```sh
curl -H "Authorization: Bearer $TOKEN" http://<temba-url>/servers
```

```json
{
  "servers": [
    {
      "id": "mcp-name-or-whatever",
      "status": "ready",
      "labels": ["team-x"],
      "private": false,
      "tools": 8,
      "expires_at": null,
      "error": null
    }
  ]
}
```

status is one of `starting` | `ready` | `error` | `auth_required`.


## 🔐 Adding Oauth MCPs to the catalog

MCPs protected by OAuth 2.1 (MCP auth spec) are supported out of the box:  
The gateway acts as the provider's OAuth client and delegates login to the
corresponding authenticated user through a one-time link. Provider client
registration uses **dynamic client registration** (RFC 7591) where supported.
Temba's own Keycloak authentication is configured separately below.

### Example 1 - IaC (values.yaml)

```yaml
catalog:
  servers:
    - id: jira
      # Atlassian-hosted Jira MCP (?tools=all exposes the full flat tool list)
      url: https://mcp.atlassian.com/v2/mcp?tools=all
      oauth: {}       # dynamic client registration (DCR + PKCE) — no creds needed
      # only optional key: oauth: {scope: "read:jira-work read:jira-user"}
```

Then `helm upgrade --install temba-mcp ./chart/temba-mcp`.

### Example 2 - REST API

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"id": "jira", "url": "https://mcp.atlassian.com/v2/mcp?tools=all", "oauth": {}}' \
  http://<temba-url>/servers
```

### Token-authenticated MCPs and header routing

Incoming OpenCode headers are **not forwarded** to upstream MCPs. Each outbound
connection receives only the headers declared in that server's own catalog entry:

```yaml
catalog:
  servers:
    - id: jira-token
      url: https://jira-mcp.example.com/mcp
      headers:
        Authorization: Bearer ${JIRA_MCP_TOKEN}
    - id: gitlab-token
      url: https://gitlab-mcp.example.com/mcp
      headers:
        Authorization: Bearer ${GITLAB_MCP_TOKEN}
```

`JIRA_MCP_TOKEN` goes only to `jira-token`; `GITLAB_MCP_TOKEN` goes only to
`gitlab-token`. Supply these environment variables via Kubernetes Secret references.
Anonymous clients cannot use these servers. After authentication to Temba, such
catalog credentials represent an **organization account shared by authorized users**;
they are not personal credentials.

An `Authorization` header in OpenCode's **Temba** connection authenticates that
connection to Temba. A Jira token placed there will not authenticate Jira; on the
authenticated request it fails Keycloak validation. Personal static-token routing from
OpenCode through Temba is not implemented. Do not put several unrelated service
tokens in global headers or tool arguments. Direct connections to individual MCPs
should have their own per-server client configuration.

### Temba authentication and personal provider connections

Temba is an OAuth resource server: it advertises Keycloak through MCP Protected
Resource Metadata and validates the access token on **every bearer-authenticated**
`/mcp` request. Anonymous calls can execute only public tools; a protected call
requires the bearer token and configured scopes.
Validation requires an RS256 signature from the realm JWKS, exact issuer,
configured audience, expiration, subject, authorized client (`azp`), required
scopes, and Keycloak's access-token `typ=Bearer`. ID tokens are not accepted as
MCP access tokens. Identity is `(issuer, sub)`, never the token value or a
client-supplied user ID. Provider tokens are never forwarded from incoming headers.

Configure Keycloak with:

- A public client for OpenCode, authorization code flow and **S256 PKCE**, restricted
  to the client's actual loopback callback URI. Disable implicit and password grants.
- An audience mapper adding `temba-mcp` to its access tokens and the `temba:use`
  client scope, granted only to clients/users permitted to access Temba.
- A browser client `temba-login`, authorization code flow, **S256 PKCE**, and one exact
  redirect URI: `https://temba.example.com/oidc/callback`. Disable implicit/password
  grants. A public client needs no client secret. A confidential client may use
  `auth.browserClientSecretName`/`auth.browserClientSecretKey` for a Kubernetes Secret.

OpenCode discovers the issuer from Temba's authentication challenge. URL-only
configuration depends on Keycloak allowing appropriate dynamic client registration.
If company policy disables DCR, supply a centrally managed, pre-registered **public
client ID** via OpenCode's `oauth.clientId`. This is a shared application identifier,
not a personal GUID or a secret. Actual OpenCode/Keycloak interoperability must be
verified against the company's realm and installed OpenCode version.

Personal Jira/provider login:

1. An authenticated user's first search/call creates that user's own MCP connections.
   An OAuth-protected provider returns a login hint. The admin catalog does not log in.
2. The agent provides a one-time `/auth?t=...` link, valid for at most the pending flow
   lifetime (default 10 minutes). Do not share this link.
3. Temba first verifies the **same user's Keycloak identity in the browser**, using
   state, nonce, S256 PKCE and an HttpOnly/Secure/SameSite cookie. A different identity
   is refused, so a forwarded link cannot attach someone else's Jira account.
4. Temba redirects to the provider. Its callback also requires the matching browser
   cookie and a live, single-use state; the provider issuer is passed to the MCP SDK
   for response validation.
5. Provider tokens and client credentials are held only in process memory, per user/server/
   endpoint. Another user gets a separate connection and cannot use these tokens.

Private `add_mcp` entries also belong to the authenticated user. Knowing their
`server_key` does not authorize another user. Provider calls and background tool
refreshes are serialized per connection to avoid concurrent token refreshes.

OAuth discovery/token/registration requests also require HTTPS. Destinations other
than the configured MCP origin must resolve to public addresses. An administrator
can explicitly allow a private identity-provider origin with
`oauth: {allowedOrigins: ["https://sso.internal.example.com"]}`; this exception is
never available to LLM-added servers. Network isolation remains necessary against
DNS rebinding between validation and connection.

Token retention defaults to 30 days, renewed on **writes**, including token refresh.
It is not an inactivity timer refreshed on every tool call. Removing a server purges
its stored tokens. Provider-side revocation remains a separate operation.

### In-memory state

OAuth provider tokens, client registration information, pending login flows and
all dynamic catalog entries live only in the gateway process. There is no external
state service, storage volume, token-encryption Secret or persistent token store.
Static catalog entries remain in the Helm-managed registry ConfigMap; configured
credentials should still come from Kubernetes Secrets.

A gateway restart or deployment loses personal provider connections and entries
created through the admin API or `add_mcp`. Users must reconnect their OAuth MCPs;
admins must recreate dynamic entries or move durable definitions to the Helm
catalog. These are expected lifecycle semantics, not a recoverable storage failure.

Token records expire after `TOKEN_RETENTION` (30 days by default, renewed on
writes). Expired records are swept periodically. When a user gateway is evicted
for inactivity (`security.userIdleTtl`, default one hour), its provider token records
are purged as well. Removing a server purges its tokens for every user; shutdown
clears all runtime records. This drops Python references; it does not guarantee
physical memory zeroization. A compromised gateway can still access active tokens.

The application disables HTTP access logging to avoid recording login tickets and
OAuth codes. Configure the ingress/proxy to omit or redact query strings for `/auth`,
`/oidc/callback` and `/oauth/callback` as well.

### Upgrading an existing deployment

Stop the old gateway before starting this version. Existing shared provider tokens
are not reused; revoke old grants and have users reconnect. Recreate required
dynamic catalog entries from trusted definitions, or add them to the Helm catalog.

The chart no longer creates Redis resources. Upgrading removes previously managed
Redis resources, potentially including their PVC; preserve needed catalog definitions
before upgrading. Old external Redis instances, retained volumes, credentials and
backups are not erased by this code change and must be retired separately. The old
migration script is no longer included.

Security regression tests (mocked Keycloak/JWKS and MCP provider, plus Helm rendering):

```sh
.venv/bin/python -m unittest discover -s tests -v
helm lint ./chart/temba-mcp
```


## 🏷️ Partitioning the catalog with labels

As an admin, all your users may not need the same tools in their catalog. Some will want powerpoint MCPs and others dev related MCPs...  
To scope tools to different usecase, you can set labels on each one.  
For a client to get a labeled tool, it has to provide the same label inside its client configuration.  

For instance, if the catalog is configured like so:  

```yaml
catalog:
  servers:

    - id: deepwiki
      url: https://mcp.deepwiki.com/mcp
      timeout: 45
      labels: [meant-for-devs]

    - id: jira
      url: https://mcp.atlassian.com/v2/mcp?tools=all
      oauth: {}
      labels: [meant-for-pm]
```

Now, in your opencode config (or any MCP compatible client):

* If you're a dev:
```jsonc
"temba-mcp": {
  "type": "remote",
  "url": "http(s)://<temba-url>/mcp",
  "headers": { "X-Temba-Labels": "meant-for-devs" } // Set catalog label here
}
```

=> Tools list: deepwiki ✅ jira ❌


* If you're a project manager:
```jsonc
"temba-mcp": {
  "type": "remote",
  "url": "http(s)://<temba-url>/mcp",
  "headers": { "X-Temba-Labels": "meant-for-pm" } // Set catalog label here
}
```

=> Tools list: deepwiki ❌ jira ✅


:information_source: Servers without labels are discoverable by all clients; credentialed servers still require Temba authentication to execute their tools.
:warning: This is not a security feature. Merely a way to organize things a bit.  


## 🔎 How does tool search work ?

Scoring runs over the tool `name` (weight 3) + `description` (2) + `input_schema` (1), with fr/en stemming.  

**Embeddings** (semantic search, recommended) if `embeddings.baseUrl` is set in values.yaml (OpenAI-compatible `/v1/embeddings` endpoint + `embeddings.apiKey` or`embeddings.secretRef` + `embeddings.model`);  

otherwise local **BM25** fallback. The active mode is returned in the payload (`"index": ...`). Zero results → `catalog` of servers as a hint.


## 🔒 Network security

Network settings are grouped under `security`:

```yaml
security:
  networkPolicy:
    enabled: true
    internalMcpPods: [mcp-atlassian, mcp-grafana]
    internalCidrs: []
```

`internalMcpPods` allows TCP port 8000 to pods with the matching `app` label in
the gateway namespace. Without this allowance, the default egress policy blocks
internal MCP services. Use `internalCidrs` for explicitly trusted internal networks.
The former top-level `networkPolicy` field is rejected with a migration message;
move it under `security` when upgrading.

When using the `allowNewMcpRegisteringByLlm` option that let's an LLM add tools to its catalog, ALWAYS make sure that `security.networkPolicy.enabled` is true (default).
A prompt injection could force the LLM to register a component from your internal k8s network. Securities have been added at the application level.  
But nothing beats an old school NetworkPolicy with default deny.

## Connection logs

Temba logs each MCP when initialization and tool discovery succeed, with a
catalog progress counter and the names still pending, failed or waiting for login:

```text
MCP 'jira' prêt (63 outils) — catalogue user-a1b2c3d4 : 2/4 MCP prêts ; en attente: slow ; en erreur: aucun ; login requis: gitlab
```

Counters are per gateway catalog: the public gateway counts only public MCPs;
each authenticated user gateway counts its own connections. The temporary
`user-...` identifier distinguishes concurrent catalogs without logging identity
or tokens. A successful HTTP response alone does not mark an MCP as ready.
While connections are incomplete, a summary appears every 10 seconds, before
their connection timeout. Override `CONNECTION_PROGRESS_INTERVAL` via `env` to
adjust this interval. Successful periodic health checks do not repeat readiness
logs; reconnects do. Connection failures include safe exception type names,
without response bodies or credentials. The SDK's repetitive
`Terminating session: None` message is filtered; other session messages remain.

## 📄 License

This project is licensed under the MIT License. See the LICENSE file for more details.

HF  
**Temba, his arms wide.**
