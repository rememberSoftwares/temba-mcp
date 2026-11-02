# Minikube test: groups and tokens

The Keycloak groups are created at the top level: `dev` and `pm`. The `groups` mapper
must include their full paths in the access token (`/dev`, `/pm`). See
[Keycloak configuration](KEYCLOAK.md).

| Group in the token | Visible MCP servers | Label |
| --- | --- | --- |
| Everyone, even without a connection | DeepWiki | `tech` |
| `/dev` | GitLab with a token | — |
| `/dev` | Jira with a token | `support` |
| `/pm` | Jira with Atlassian OAuth | — |

With both groups, all four MCP servers are visible. After a group
change, reconnect to the catalog to obtain a new Keycloak token;
restarting OpenCode restarts this connection. An existing token keeps
its old groups until it is renewed.

## Local deployments

The copies in `deploy/minikube-test/` do not change the examples in the
Jira and GitLab repositories. They use images built from their source code,
loaded into Minikube (`*-mcp:temba-test-20261013`), because the private registry was
unreachable. These images must be present before reapplying the copies:

```sh
kubectl --context minikube apply -f deploy/minikube-test/
```

Fixed settings are in the pods' environment variables:

- Jira: `JIRA_URL=https://numsproduct.atlassian.net` and
  `JIRA_EMAIL=emilien.lancelot@numspot.com`.
- GitLab: `GITLAB_PROJECTS=https://gitlab.tooling.cloudgouv-eu-west-1.numspot.internal/cloud-compute/eclused-compute`
  and `GITLAB_VERIFY_SSL=false` for this test.

No token is defined in these manifests: each client provides its token
through a header. For Jira, this token must match the configured email account.

## Access from the user's machine

For this local test, keep these three port forwarding sessions open, each in its
own terminal (or through k9s):

```sh
kubectl --context minikube port-forward service/temba 18080:8080
kubectl --context minikube -n agent port-forward service/jira-mcp 18081:8000
kubectl --context minikube -n agent port-forward service/gitlab-mcp 18082:8000
```

Outside this test, replace the loopback endpoints with HTTPS ingress addresses
accessible from client machines.

## Client configuration

The local `temba.yaml` file contains only token references:

```yaml
catalogs:
  - id: team
    url: https://catalogue.example.com
    tools:
      jira-token:
        expected_endpoint: http://127.0.0.1:18081/mcp
        headersFromEnv:
          - name: X-Jira-Token
            valueFrom:
              envVarRef:
                name: JIRA_TOKEN
      gitlab:
        expected_endpoint: http://127.0.0.1:18082/mcp
        headersFromEnv:
          - name: X-Gitlab-Token
            valueFrom:
              envVarRef:
                name: GITLAB_TOKEN
```

Set these two variables in the environment that launches OpenCode. The values
are raw tokens, without the `Bearer ` prefix. A local literal value in
the YAML file also works. `expected_endpoint` restricts the allowed destination
for sending the token.

Labels are a per-catalog client filter, separate from Keycloak permissions.
Set `labels` inside the relevant remote `catalogs` entry: `labels: []`
includes only MCP servers without labels (GitLab for `/dev`, Jira OAuth for `/pm`).
`labels: ["support"]` adds Jira with a token; `labels: ["tech"]` adds DeepWiki.
To include both, use `labels: ["support", "tech"]`. MCP servers without
labels remain visible. If an MCP server has multiple labels, all must be
enabled in the YAML file. Even though DeepWiki is public, it therefore requires the `tech` label
to be included by the local client.

Without a service token, an allowed MCP server is still listed with the
`credentials_required` status. Jira/GitLab calls then require a valid token.
