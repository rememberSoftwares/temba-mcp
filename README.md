# dynmcp

An MCP gateway that exposes only 2–3 tools (`tool_search`, `call_tool`,
optional `add_mcp`) and aggregates a catalog of remote MCP servers (streamable
HTTP) on demand. Agents no longer load 70 tools at boot: they search by
keywords, receive the matching tools along with their JSON Schema, and execute
them through the gateway.

```
opencode / agents ──HTTP──> dynmcp (2-3 tools) ──HTTP──> remote MCP servers
                            │  tool_search → index (BM25 or embeddings)
                            │  call_tool   → tools/call routed to the source MCP
                            │  add_mcp     → private ephemeral server (24h)
                            │  /servers    → admin API (bearer)
                            │  /auth       → human OAuth login (single-use ticket)
                            └── Redis: shared state across replicas
```

## Layout

```
chart/dynmcp/
├── Chart.yaml
├── values.yaml                  # all configuration (catalog, redis, ingress, secrets…)
├── files/server.py              # gateway code — single source of truth (mounted as ConfigMap, no image)
└── templates/                   # deployment, service, redis, configmaps, secret, ingress, networkpolicy…
extra-mcp/                        # companion MCP servers, applied manually if needed
├── grafana-mcp.yaml             #   mcp-grafana (pod + service, internal bearer)
└── atlassian-mcp.yaml           #   mcp-atlassian (pod + service, creds via secret)
test_client.py                    # MCP dev client (tool_search/call_tool/add_mcp)
```

## Deploying

```sh
helm upgrade --install dynmcp ./chart/dynmcp
kubectl port-forward svc/dynmcp 18080:8080
# → MCP endpoint for agents: http://localhost:18080/mcp
# → OAuth login page       : http://localhost:18080/auth
```

After any code change: re-run `helm upgrade` (the code is embedded in the chart
and mounted as a ConfigMap).

opencode client workflow (in `~/.config/opencode/opencode.json`):

```json
"dynmcp": {
  "type": "remote",
  "url": "http://localhost:18080/mcp",
  "headers": { "X-Dynmcp-Labels": "grafana,jira" }
}
```

## Catalog (infrastructure as code)

The MCP catalog is declared in `values.yaml` (`catalog.servers`). Supported
per-server options:

```yaml
catalog:
  servers:
    - id: deepwiki
      url: https://mcp.deepwiki.com/mcp
      timeout: 45                  # seconds, optional
      labels: [docs]               # optional: restricted visibility (see below)
      headers: {}                  # optional, ${VAR} interpolated from pod env
      oauth: {}                    # optional: dynamic OAuth (DCR + PKCE)
    # oauth: {client_id: ..., client_secret: ${SECRET}, scope: "..."}  # pre-registered client
```

### Search

Scoring runs over the tool `name` (weight 3) + `description` (2) +
`input_schema` (1), with fr/en stemming. **Embeddings** (semantic search,
recommended) if `embeddings.baseUrl` is set in values.yaml
(OpenAI-compatible `/v1/embeddings` endpoint + `embeddings.apiKey` or
`embeddings.secretRef` + `embeddings.model`);
otherwise local **BM25** fallback. The active mode is returned in the payload
(`"index": ...`). Zero results → `catalog` of servers as a hint.

### Labels

A server with `labels` is only visible (tool_search **and** call_tool) to
clients sending at least one matching label in the `X-Dynmcp-Labels` header
(opencode config). Servers without labels are visible to everyone.

### Dynamic OAuth (MCP auth spec)

`oauth: {}` on a catalog entry → first request gets 401 → metadata discovery
(RFC 9728/8414) → **Dynamic Client Registration** (RFC 7591) → the LLM receives
a personal `/auth?t=<ticket>` link (single-use, 15 min) → the user logs in
(redirect_uri = `publicBaseUrl`) → tokens stored in Redis with automatic
refresh afterwards. Once per server.

## Admin API (`/servers`, bearer `ADMIN_API_TOKEN`)

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"id": "my-team", "url": "https://…/mcp", "labels": ["team-x"], "headers": {"Authorization": "Bearer ${…}"}}' \
  http://…/servers
curl -H "Authorization: Bearer $TOKEN" http://…/servers
curl -X DELETE -H "Authorization: Bearer $TOKEN" http://…/servers/my-team
```

These servers join the catalog shared by **all** LLM clients, with persistence
in Redis (survive restarts, replicated across pods).

## Secrets

The admin API token comes from a k8s Secret, configured under `adminApi` in
values.yaml:

- `adminApi.secretName` set (default `dynmcp-secrets`): the chart references a
  pre-existing Secret. Create it manually:
  ```sh
  kubectl create secret generic dynmcp-secrets \
    --from-literal=ADMIN_API_TOKEN=$(openssl rand -hex 24) \
    --from-literal=GRAFANA_MCP_TOKEN=$(openssl rand -hex 24)
  ```
- `adminApi.secretName: ""`: the chart creates `<release>-admin` with a random
  `ADMIN_API_TOKEN`, kept across upgrades (that Secret holds the admin token
  only — point extra `env` secretKeyRefs at your own Secrets).

Other pod env vars come from the `env` key in values.yaml, using the standard
k8s syntax; values pulled from Secrets can then be interpolated into catalog
headers via `${VAR}`:

```yaml
env:
  - name: GRAFANA_MCP_TOKEN
    valueFrom:
      secretKeyRef:
        name: dynmcp-secrets
        key: GRAFANA_MCP_TOKEN

catalog:
  servers:
    - id: grafana
      headers:
        Authorization: Bearer ${GRAFANA_MCP_TOKEN}
```

| Secret key | Purpose |
|---|---|
| `ADMIN_API_TOKEN` (key `adminApi.tokenKey`) | bearer for the `/servers` admin API (never sent to the LLM) |
| `GRAFANA_MCP_TOKEN` | internal bearer gateway ↔ mcp-grafana (via `env` secretKeyRef, if extra-mcp applied) |
| `GRAFANA_SERVICE_ACCOUNT_TOKEN` | credentials used by the mcp-grafana pod against your Grafana (extra-mcp) |

## Network security

- `add_mcp` = public internet only, enforced strictly (SSRF guard resolves
  DNS and rejects private/loopback/link-local/metadata addresses; internal MCPs
  can only be registered by an admin through the API).
- `automountServiceAccountToken: false`: no k8s API token in the pod.
- **Egress NetworkPolicy** (opt-in, `networkPolicy.enabled`): the only strong
  guarantee against DNS rebinding — public internet only, plus explicitly
  declared internal MCPs and CIDRs. Requires a CNI that enforces NetworkPolicy
  (Calico/Cilium); on minikube: `minikube start --cni=calico`.
- The `/auth` page is only reachable through the one-time ticket sent in the
  LLM hint; no secret ever flows through the LLM context.

## Multi-replica

`replicaCount > 1` works: OAuth tokens and the dynamic registry live in Redis
(`dynmcp:tokens:*`, hash `dynmcp:dynamic_servers`), replicated via pub/sub
(`dynmcp:events`). Redis persistence uses emptyDir by default — enable
`redis.persistence` (PVC) to survive pod deletion.

## Main environment variables

| Variable | Default | Purpose |
|---|---|---|
| `PUBLIC_BASE_URL` | derived from Ingress | public URL (OAuth redirect_uri, /auth links) |
| `ADD_MCP_ENABLED` | `false` | expose `add_mcp` to LLMs |
| `REDIS_URL` | `redis://…-redis:6379/0` | shared state (tokens + dynamic servers) |
| `ADMIN_API_TOKEN` | empty = API disabled | bearer for `/servers` |
| `TOOL_SEARCH_LIMIT` | `5` | top N tool_search results |
| `PRIVATE_SERVER_TTL` | `86400` | lifetime of add_mcp servers (seconds) |
| `AUTH_LOGIN_TIMEOUT` | `600` | time granted to the human for the OAuth login |
| `embeddings.*` (values.yaml) | empty | enables embeddings mode (`baseUrl`/`apiKey`/`secretRef`/`model`) |
