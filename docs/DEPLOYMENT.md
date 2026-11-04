# Deploying the catalog

This guide installs **the central REST catalog**. Users then run
[the local client](../README.md#getting-started) on their machines.
Remote MCP servers must already exist and be accessible from those machines.

The catalog uses a container image and the [temba-catalog](../chart/temba-catalog) chart.
Catalog and client versions evolve independently: currently,
the catalog is at **1.3.3** and the client at **2.3.0**.

## Prerequisites

For the Minikube walkthrough:

- a running Minikube instance and a `kubectl` context named `minikube`;
- Helm and `nerdctl`;
- the repository source code and access to the base images used by the Dockerfile.

For another cluster, build and publish the image to your usual registry,
then set `image.repository`, `image.tag`, or `image.digest` in the Helm values.
Also adjust the context and namespace in the commands.

## 1. Build the image

From the repository root:

```sh
nerdctl build -t temba-catalog:1.3.3 .
```

In Minikube, explicitly load an image archive:

```sh
nerdctl save -o /tmp/temba-catalog-1.3.3.tar temba-catalog:1.3.3
minikube image load /tmp/temba-catalog-1.3.3.tar
```

The image includes the Rust binary. The ConfigMap created by Helm contains only
MCP server definitions, not code or remote service tokens.

## 2. Define your MCP servers

Create a `values.local.yaml` file:

```yaml
image:
  repository: temba-catalog
  tag: "1.3.3"

catalogueAuth:
  enabled: false

catalog:
  servers:
    - id: documentation
      description: Search and browse the team's documentation
      endpoint: https://docs-mcp.example.com/mcp
      access:
        public: true
```

Replace the example endpoint with the HTTPS address of a real MCP server reachable from
users' machines. Catalog authentication is disabled here;
you can enable groups later using the [Keycloak guide](KEYCLOAK.md).

Default values are in [values.yaml](../chart/temba-catalog/values.yaml).
The [catalog guide](CATALOGUE.md) covers OAuth, headers, and visibility rules in detail.
The repository's `overide.yaml` override belongs to the test environment; do not
reuse it as is for a new installation.

## 3. Install the release

```sh
helm upgrade --install temba chart/temba-catalog \
  --kube-context minikube \
  --namespace temba --create-namespace \
  -f values.local.yaml
```

The chart uses a PVC by default. Check that the cluster has a suitable StorageClass,
or set `persistence.storageClass` / `persistence.existingClaim`.

Check the deployment:

```sh
kubectl --context minikube -n temba rollout status deployment/temba
kubectl --context minikube -n temba logs deployment/temba
```

The administrative API stays disabled until a Secret is referenced.
You can start with Helm definitions alone.

## 4. Reach the catalog during development

Keep this port forwarding session open:

```sh
kubectl --context minikube -n temba port-forward service/temba 18080:8080
```

In the YAML file on the user's machine:

```yaml
allow_insecure_http: true
catalogs:
  - id: team
    url: http://127.0.0.1:18080
```

To check public definitions:

```sh
curl --fail http://127.0.0.1:18080/v1/catalog
```

The YAML flag is enough to reach the catalog through this port forwarding session.
`security.allowInsecureHttp` in the chart applies to **declared MCP endpoints**:
enable it if those definitions use HTTP, whether local or remote.
The local client must also enable `allow_insecure_http` to use those definitions.

## Exposing the catalog over HTTPS

For a shared installation, expose the catalog through an HTTPS Ingress:

```yaml
ingress:
  enabled: true
  className: nginx
  host: catalogue.example.com
  tls:
    - secretName: temba-tls
      hosts: [catalogue.example.com]

security:
  networkPolicy:
    ingress:
      namespace: ingress-nginx
```

Adjust the controller, namespace, domain, and TLS Secret names
to your cluster. The TLS Secret must be supplied through your usual certificate management process.
The chart's Ingress exposes `/v1`, used by both the user API and the admin API.

Then share `https://catalogue.example.com` with users as a `catalogs` entry’s `url`.
Each remote MCP server must also be accessible from their machines.
The catalog does not proxy their calls.

NetworkPolicies are enabled by default: allow your Ingress controller's
namespace and, if Keycloak is enabled, its outbound address ranges.
See [allowed traffic](CONFIGURATION.md#kubernetes-networking) and
[container hardening](../SECURITY_CONTAINER.md).

## Updates and storage

Administrative additions are persisted on the PVC; Helm definitions remain
in your values. The PVC is retained when Helm uninstalls the release.

With the volume, the Deployment uses **Recreate**: expect a brief interruption
during updates. Only one process can write persistent additions.
Do not change Helm definitions to reuse the identifier of an existing API addition.

### Migrating an old configuration

Old keys are rejected. To update an installation from earlier versions:

| Old setting | Current setting |
| --- | --- |
| Global client `labels` | `labels` inside each remote `catalogs` entry; no label selection for local/dynamic MCPs |
| Global `auth` | `catalogueAuth` |
| Client `temba.toml` | `temba.yaml`; the client now accepts YAML only |
| `catalog_url` or `[[catalogs]]` in TOML | YAML `catalogs` list with `id` and `url` |
| `[local_catalog]` with `enabled` / `max_servers` | YAML `dynamic_catalog` |
| `[tool.<catalog>.<id>]` | `tools.<id>` inside the matching `catalogs` entry |
| Executable `temba-client` | `temba` (package remains `temba-client`) |
| `catalogAccess` | An `access` rule |
| `adminApi.mutable` | `adminApi.readOnly`, with inverted logic |
| `auth.type` on an MCP server | `oauth` and/or `headers` |
| `url` on an MCP definition | `endpoint` |
| Display `name` on an MCP definition | Remove it; keep the existing unique `id` |
| `headers[].prefix` | Complete value supplied by the client |
| Credential alias keys in client headers | Actual HTTP header names in `headers` or `headersFromEnv[].name` |
| `{ env = "VARIABLE" }` in TOML | `headersFromEnv` list with `name` and `valueFrom.envVarRef.name` |
| `allow_loopback_http` / `allow_localhost_http` in TOML | `allow_insecure_http` (also permits remote HTTP) |
| `refresh_seconds` in TOML | `refresh_minutes`, default `60`, range `1..1440` |
| `security.allowLoopbackHttp` in Helm | `security.allowInsecureHttp` |

Before using client 1.4.9, update the catalog to 1.3.2 and remove server-level `name` from Helm values, raw catalog files and persisted API definitions. Keep the existing IDs so client mappings and tool IDs remain stable. Header `name` fields are unchanged.

The catalog format is `schemaVersion: 2`. The Python gateway, Redis, token envelopes,
and old HTTP MCP routes are no longer part of the architecture.
The current client is a local stdio MCP server; there is no V1 compatibility layer.

The legacy `/session/callback` and `/oidc/callback` routes are replaced by
the native callback on the user's machine, described in the [Keycloak guide](KEYCLOAK.md).

When migrating an old Deployment to `Recreate`, if Helm retains
the `rollingUpdate` section, remove it once before retrying the update:

```sh
kubectl --context minikube -n temba patch deployment temba \
  --type=merge \
  -p '{"spec":{"strategy":{"type":"Recreate","rollingUpdate":null}}}'
```

Adjust the context, namespace, and Deployment name to your installation.
