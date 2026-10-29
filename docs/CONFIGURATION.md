# Catalog options

## Allowing HTTP in development

HTTPS is required by default. `security.allowInsecureHttp` permits HTTP in declared
MCP endpoints and OAuth origins, whether local or remote:

| MCP server URL | With `security.allowInsecureHttp: true` |
|---|---|
| `http://localhost:9000/mcp` | Accepted |
| `http://127.0.0.1:9000/mcp` | Accepted |
| `http://mcp.entreprise/mcp` | Accepted |
| `https://mcp.entreprise/mcp` | Accepted even without the option |

The MCP server address is used by the **client on the user's machine**. A localhost address therefore refers to an MCP server on that machine, not a service in Kubernetes.

This option configures neither the Ingress nor Keycloak certificates. To reach **the catalog** through port forwarding, the local YAML file is what matters:

```yaml
allow_insecure_http: true
catalogs:
  - id: team
    url: http://127.0.0.1:18080
```

The chart flag controls which definitions the server accepts. The YAML flag controls
which destinations the local client accepts. Enable both if MCP definitions use HTTP;
only the YAML flag is needed for an HTTP catalog with HTTPS MCP definitions.
Neither flag configures TLS termination on the Ingress or disables HTTPS certificate checks.
OAuth destination approval (`oauth.allowedOrigins`) is independent: private destinations
announced automatically remain blocked even when HTTP is enabled.

When upgrading, replace `security.allowLoopbackHttp` with `security.allowInsecureHttp`
in Helm overrides, and `allow_localhost_http` or `allow_loopback_http` with
`allow_insecure_http` in YAML. The new flags also permit remote HTTP.

## Kubernetes networking

Temba clients and administrative calls go through the same Ingress to the catalog. The policy allows pods from the controller's namespace on TCP 8080:

```yaml
security:
  networkPolicy:
    enabled: true
    ingress:
      namespace: ingress-nginx
      podSelector: {}  # optional: labels selecting only the controller pods
    authCidrs: ["203.0.113.0/24"]  # replace this example with your Keycloak CIDRs
```

This policy rejects other incoming traffic. Kubernetes port forwarding is not an ordinary connection from another pod and does not validate this filtering.

Outbound traffic: DNS to kube-dns, and HTTPS/TCP 443 to `authCidrs` only if `catalogueAuth.enabled`. This policy rejects everything else. Without these exceptions, Keycloak validation would no longer work. MCP servers are contacted by the user's machine; their URLs do not need to be allowed in this policy.

The CNI must enforce NetworkPolicies. Connections from nodes, controllers using hostNetwork, and other policies require checking the cluster's behavior. This policy filters addresses and ports, not HTTP routes: the administrative bearer token is still required for the admin API.

## Mutable catalog and storage

`adminApi.readOnly: true` means read access only. To allow additions and deletions:

```yaml
adminApi:
  readOnly: false
  secretName: temba-admin
  tokenKey: token
persistence:
  enabled: true
  size: 100Mi
  storageClass: ""
```

First create the Kubernetes Secret containing the administrative bearer token. API definitions are saved on the PVC and reloaded after restart; Helm definitions remain managed by Helm. OAuth tokens and private header values stay on users' machines.

Only one replica writes the file. A save failure returns an error and does not change the catalog in RAM. A corrupt file or a conflict with a Helm identifier blocks startup to prevent silent data loss. The PVC is retained when Helm uninstalls the release; remember to back it up.

References: [NetworkPolicies](https://kubernetes.io/docs/concepts/services-networking/network-policies/), [persistent volumes](https://kubernetes.io/docs/concepts/storage/persistent-volumes/).
