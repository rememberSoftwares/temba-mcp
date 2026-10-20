# Temba

**A shared MCP catalog. Connections that stay on your machine.**

Temba is a catalog of MCP servers that let LLMs discover and call tools.

[Getting started](#getting-started) · [Deploying the catalog](#deploying-the-catalog) · [Documentation](#documentation)

## Why Temba?

- **One catalog for every team** : Use keycloak groups to assign tools to your organisation's teams.
- **Simple catalog management** : Use Infra As Code or REST API to manage catalog entries.
- **Secured by design** : OIDC sessions from remote MCPs stays on the machine.
- **Dynamic MCP registration** : Optionnally let the LLM register MCPs it finds to its catalog.

## How it works

>> show a mermaid like graph of a local MCP tool that retieves a list of MCP urls from the remote catalogue and then connects to theses urls
>> Also add a link showing the admin api adding new tools.

## Getting started

Temba is made of two components :
* The server side catalog installed using Helm.
* The local MCP tool that connects to the catalog.

### Installing the catalog

The catalog must be installed on Kubernetes using the helm chart in `./chart/temba-catalog`.

```bash
cd ./chart/temba-catalog
helm upgrade --install temba chart/temba-catalog
```


* Opencode MCP config to add Temba (or any other LLM client like Codex or Claude Code):
```json
"mcp": {
    "temba": {
      "type": "local",
      "command": [
        "uvx",
        "--from",
        "/home/elancelot/Documents/leTigre/dynmcp/dist/temba_client-1.4.9-py3-none-manylinux_2_34_x86_64.whl",
        "temba-client",
        "--config",
        "<path-to-your-toml-file>"
      ],
      "enabled": true
    }
}
```




## Learn by example

### Adding an unauthenticated MCP to the catalog

Let's add the DeepWiki MCP to the catalog. It does not require any kind of authentication.  

* Temba catalog:  
```yaml
catalog:
  servers:
  - id: deepwiki
    description: Public technical documentation tools.
    endpoint: https://mcp.deepwiki.com/mcp
    timeoutSeconds: 45
    access:
      public: true
```

* .toml on the client desktop:
```toml
catalog_url = "http://127.0.0.1:18080" # URL of the catalog
# allow_insecure_http = true          # if using an HTTP URL for the catalog
```

=> Ask your LLM to search something about a Github repo.

### Adding an OIDC MCP to the catalog

Let's add the official Jira MCP that uses an oauth connexion

* Temba catalog:  
```yaml
catalog:
  servers:
  - id: jira
    description: Atlassian remote Jira MCP ; sign in using its browser OAuth flow.
    endpoint: https://mcp.atlassian.com/v2/mcp?tools=all
    oauth: {}
    #  allowedOrigins:    # Optional restriction on OAuth destinations announced by this MCP. If unset, public destinations are discovered automatically.
    #  - https://auth.atlassian.com
```

* .toml on the client desktop:
```toml
catalog_url = "http://127.0.0.1:18080" # URL of the catalog
# allow_insecure_http = true          # if using an HTTP URL for the catalog
```

### Sending a custom header

Some MCPs need to recieve somes specific headers to work.  
Let's say that e have a Gitlab MCP that needs two custom headers :
* **Non-sensitive** string to scope the MCP to a particular project.
* **Sensitive** string containing a user defined token.

The non-sensitive header can be set in the catalog directly. On the other hand the sensitive token, being a user defined value, will be set in the client's toml file.

* Temba catalog:  
```yaml
catalog:
  servers:
  - id: gitlab
    description: GitLab tools for project and instance configured on the MCP server.
    endpoint: http://<GITLAB-MCP-URL>/mcp
    headers:

    - name: X-Gitlab-Project # For non sensitive values 
      value: <REPO/PROJECT> 


    - name: X-Gitlab-Token   # For sensitive values
      credential: token      # The credential name must match the one in the .toml ; 'token' in this example
      mandatory: true
```

* .toml on the client desktop:
```toml
[tool.gitlab]                                      # The string after 'tool' must match the 'id' in the catalog 
expected_endpoint = "http://127.0.0.1:18082/mcp"
[tool.gitlab.headers]
token = "glpat-<SECRET-TOKEN-OF-USER>"             # Note the 'token' key that must be identical to the one in the catalog (cf 'credential' above)
```

If you don't want to store the secret inside the toml, you can use the ENV like so:
```
token = {env = <ENV_KEY_NAME>}"
```
:warning: Note that some LLM clients may not forward local ENV to their subprocess.


### Partitioning tools using labels

You can add labels on MCPs. These MCPs will only be available to clients with the same labels.  

Let's add a Gitlab and Jira MCP. The Gitlab MCP is for developpers and use the label "dev". The jira MCP is for product managers and use the labels "pm". 

* Temba catalog:  
```yaml
catalog:
  servers:
  - id: gitlab
    description: GitLab tools for project and instance configured on the MCP server.
    endpoint: http://<GITLAB-MCP-URL>/mcp
  - id: jira
    description: Atlassian remote Jira MCP ; sign in using its browser OAuth flow.
    endpoint: https://mcp.atlassian.com/v2/mcp?tools=all
    oauth: {}
```

* .toml on the client desktop for a developer:
```toml
labels = ["dev"]

...
```
=> This client can discover the Gitlab MCP but not Jira.  

* .toml on the client desktop for a product manager:  
```toml
labels = ["pm"]

...
```
=> This client can discover the Jira MCP but not Gitlab  

:warning: Labels are not a security feature. It's a simple way to partition tools but if you need true separation then use the Keycloak feature (see bellow).  


### Partitioning tools using Keycloak

The Temba MCP supports OIDC. This allows to assign tools to users using Keycloak groups.

#### 1. Configure the Keycloak client

In your chosen realm (the space containing your users), create a new client: **Clients → temba**, or create an OpenID Connect client named `temba`.

- **Client authentication**: Off; **Standard flow**: On.
- **Require PKCE**: On, with method **S256** if the choice is offered.
- **Valid redirect URIs**: `http://127.0.0.1:19877/oauth/callback`.
- Root URL, Home URL, Web origins, and logout URLs can remain empty.

:information_source: After login, Keycloak instructs the user's browser to open the redirect URI. The browser connects to the local Temba client on the same machine; Kubernetes is not involved in this callback. The port must match catalogueAuth.callbackPort.

#### 2. Add the two mappers

A **mapper** adds information to the authentication token sent to Temba.

Open **Clients → temba → Client scopes → temba-dedicated → Mappers → Add mapper → By configuration**. Create and save:

| Mapper type | Fields to fill in |
|---|---|
| **Group Membership** | Name: `temba-groups`; Token Claim Name: `groups`; Full group path: **On**; Add to access token: **On** |
| **Audience** | Name: `temba-audience`; Included Client Audience: **empty**; Included Custom Audience: `temba-mcp`; Add to access token: **On**; Add to ID token: **Off** |

**Groups** indicate what the user can see. The **audience** indicates that the token is intended for the Temba catalog. No special `temba:use` scope is requested.

If the dedicated scope does not appear, put these mappers in a client scope attached to `temba` as **Default**, so they are always applied.

#### 3. Create groups and assign users

1. **Groups → Create group**: create `team-a` at the top level. Its path will be `/team-a`. Repeat for `team-b` (`/team-b`).
2. **Users → user → Groups → Join group**: select one or more of these groups.

The mapper passes along group memberships; it does not add users to groups.

#### 4. Configure the Helm override

Enable catalog authentication:

```yaml
catalogueAuth:
  enabled: true
  issuer: https://keycloak.example.com/realms/my-realm
  clientId: temba
  audience: temba-mcp
  callbackPort: 19877
```

In each MCP server definition, choose **only one** rule:

| Rule | Who can see the MCP server? |
|---|---|
| `access: {public: true}` | Everyone, without a catalog connection |
| `access: {authenticated: true}` | All users connected to the catalog |
| `access: {groupsAnyOf: ["team-a"]}` | Members of this group |
| `access: {groupsAnyOf: ["team-a", "team-b"]}` | Members of either team |

:information_source: Without `access`, the MCP server is public. The initial `/` is optional in catalog rules: `team-a` and `/team-a` are equivalent; `teams/team-a` means `/teams/team-a`. Paths and case must otherwise match exactly: `team-a/child` does not match `team-a`. Keep **Full group path: On** in the Keycloak mapper so nested groups remain distinct.


**Back to the catalog !**

Let's add a Gitlab and Jira MCP. The Gitlab MCP is for users part of `team-a`. The jira MCP is for users part of `team-b`. 

* Temba catalog:  
```yaml
catalog:
  servers:
  - id: gitlab
    description: GitLab tools for project and instance configured on the MCP server.
    endpoint: http://<GITLAB-MCP-URL>/mcp
  - id: jira
    description: Atlassian remote Jira MCP ; sign in using its browser OAuth flow.
    endpoint: https://mcp.atlassian.com/v2/mcp?tools=all
    oauth: {}
```

* .toml on the client desktop for a developer:
```toml
labels = ["dev"]

...
```
=> This client can discover the Gitlab MCP but not Jira.  

* .toml on the client desktop for a product manager:  
```toml
labels = ["pm"]

...
```
=> This client can discover the Jira MCP but not Gitlab  


#### Configuring the catalog

The catalog is a list of remote MCPs that follows this structure:

```yaml
catalog:
  servers:
  - id: <some-unique-id>                 # ie gitlab
    description: <description of what this MCP does> # ie Gitlab management tools for MR and users.
    endpoint: http://127.0.0.1:18082/mcp # Remote MCP URL must be accessible from the computer
    headers:                             # Optionnal headers (see Setting headers)
    - name: X-<Heeader-Name>
      credential: <some-key>             # Optionnal setting that takes the header value from the client's .toml config 
      mandatory: true
    oauth:                               # Optionnal setting to add OIDC MCPs (ie official Jira) 
      allowedOrigins:                    # Optional restriction; omit or use [] for automatic public OAuth discovery.
        - https://auth.example.com
      clientId: my-client
      scopes:
        - openid
      callbackPort: 19878
    labels: ["some-label"]               # Optionnal setting to distribute tools to clients that have the same label in their .toml. (See Using labels)
    access:                              # Use Keycloak to manage tools per team (see Using OIDC for catalog access)
      groupsAnyOf:
      - <group-name>                     # User's registered to this group in keycloak will have access to this tool
```

#### Catalog config examples:

* Example with a Gitlab MCP using only a header as token :

```yaml
catalog:
  servers:
  - id: gitlab
    description: GitLab tools for the dev group; project and instance configured on the MCP server. Requires a client-provided raw GitLab token.
    endpoint: http://gtlab-mcp/mcp # Change this to target your own Gitlab MCP 
    headers:
    - name: X-Gitlab-Token         # This is the header name that this Gitlab MCP awaits
      credential: token            # The token is set under the key'token' in the client's .toml config. (see Setting headers)
      mandatory: true
    access:
      groupsAnyOf:
      - dev                        # If the user is in the group keycloak group 'dev', this tool will be made available to him
```

* Example with the official Jira MCP using OIDC (to Atlasian itself)

``` yaml
catalog:
  servers:
  - id: jira
    description: Atlassian remote Jira MCP for the project managers group; sign in using its browser OAuth flow.
    endpoint: https://mcp.atlassian.com/v2/mcp?tools=all
    oauth:
      allowedOrigins:
      - https://auth.atlassian.com
    access:
      groupsAnyOf:
      - /pm
```

### 1. Get the client

Install [uv](https://docs.astral.sh/uv/getting-started/installation/), which provides the
`uvx` command. You do not need to install Rust, Cargo, or a compiler.

Then get the **Temba wheel for your system** from your administrator
or from the [CI artifacts](.github/workflows/ci.yaml). This `.whl` file contains
the precompiled Rust binary.

CI targets Linux x86_64, macOS Apple Silicon and Intel, and Windows x86_64.
**The package is not yet published to a registry**: use a wheel file for now.

### 2. Create your configuration

Copy [temba.example.toml](temba.example.toml) to a file named `temba.toml`, then set
the catalog URL. These two lines are enough to get started:

```toml
catalog_url = "https://catalogue.example.com"
labels = []
```

`labels = []` shows MCP servers without labels. If your team uses labels to
make some MCP servers optional, add the ones you want:

```toml
labels = ["support", "documentation"]
```

MCP servers without labels remain visible. If an MCP server has multiple labels,
all of them must appear in this list.

### 3. Connect Temba to your AI client

The client must launch the following command as a **local MCP server**:

```sh
uvx --from /absolute/path/to/client.whl temba-client --config /absolute/path/to/temba.toml
```

Replace both paths with the paths to your wheel and configuration file.
Use absolute paths: the AI client's working directory may change from one project to another.

<details>
<summary>OpenCode configuration example</summary>

Add a local MCP entry to your OpenCode configuration:

```json
{
  "mcp": {
    "temba": {
      "type": "local",
      "command": [
        "uvx",
        "--from",
        "/absolute/path/to/client.whl",
        "temba-client",
        "--config",
        "/absolute/path/to/temba.toml"
      ],
      "enabled": true
    }
  }
}
```

</details>

For another MCP client, use the same command in its configuration format.
To check only that the binary is installed, run:

```sh
uvx --from /absolute/path/to/client.whl temba-client --help
```

### 4. Make your first request

Launch your AI client and try asking:

> List the services you have access to.

The assistant uses `tool_search` to discover tools, then `call_tool` to execute them.
You do not need to handle their identifiers manually.

If authentication is required, Temba opens the browser **when you use the service**.
An existing SSO session may authorize the connection immediately: Temba briefly waits
for the result and continues with the request. If you still need to take action, the assistant lets you know.

> Changed the TOML file, environment variables, or client version?
> **Restart the Temba process.** With a persistent AI server such as `opencode serve`,
> restarting only the interface may not reload the MCP servers.

## Connecting to MCP servers

Authentication methods can be combined:

| The MCP server requires… | What you configure |
| --- | --- |
| No credentials | Nothing else on the user side |
| A token or private headers | Their values in your TOML file or the Temba process environment |
| An OAuth / OIDC connection | Authorization in the browser; the session stays in RAM |
| OAuth and additional headers | Both mechanisms together |

For a static token, the administrator declares the expected header in the catalog.
You provide its value locally. For example:

```toml
[tool.service]
expected_endpoint = "https://mcp.example.com/mcp"

[tool.service.headers]
api_token = { env = "SERVICE_TOKEN" }
```

The `service` identifier, `api_token` reference, and URL must match the catalog
definition. A direct value such as `api_token = "Bearer …"` also works; keep
that file private. Temba sends the full value without adding a prefix.

The [client configuration guide](docs/CLIENT.md) explains headers, labels, and the connection flow.

### Do you need Keycloak?

**No, Keycloak is optional.** It controls who can see entries in the shared catalog.
Authentication with a remote MCP server remains independent: a public catalog can contain OAuth MCP servers.

To restrict MCP servers to teams, follow the [step-by-step Keycloak tutorial](docs/KEYCLOAK.md).
Labels provide convenient local filtering; Keycloak groups control visibility on the server.

## Adding an MCP server for a temporary need

The local catalog lets the assistant add an MCP server itself without changing the team's catalog.
Enable it in your TOML file to expose the third tool, `add_mcp`:

```toml
[local_catalog]
enabled = true
idle_timeout_seconds = 1800
max_servers = 16
```

The added entry stays **in memory in the local process**. It receives a secret identifier that
the assistant must provide to find and use it. This also works without authentication to the central catalog.

Each entry expires after **30 minutes without its ID being used**, or when the process stops.
The default limit is **16 simultaneous entries**. Expiration is useful when an AI server
keeps the Temba process running across multiple conversations.

Possession of the identifier grants access to the entry: it does not authenticate a conversation.
Values provided to `add_mcp` may also appear in the AI client's history.

See the [local addition walkthrough](docs/CLIENT.md#temporary-local-additions) for examples and OAuth rules.

## Deploying the catalog

Do you manage shared MCP servers? Deploy the catalog using a container image and
the [temba-catalog](chart/temba-catalog) chart. Define the addresses, descriptions,
and, if needed, authorized groups there.

1. Build the image and make it available to the cluster.
2. Prepare a Helm values file containing your MCP servers.
3. Deploy the catalog, then share its URL with users.

The [deployment guide](docs/DEPLOYMENT.md) covers these steps, including a Minikube walkthrough and HTTPS Ingress settings.
The [catalog guide](docs/CATALOGUE.md) explains how to describe MCP servers and manage persistent additions.

## Repository layout

```text
crates/temba-client/     Local MCP server, search, and OAuth connections
crates/temba-catalog/    REST catalog and access control
crates/temba-core/       Shared schemas and validation
chart/temba-catalog/    Kubernetes deployment
docs/                  User and administration guides
```

To build the project or wheels, see the [development guide](docs/DEVELOPMENT.md).

## Documentation

| You want to… | Read |
| --- | --- |
| Configure TOML, tokens, or labels | [Local client](docs/CLIENT.md) |
| Describe MCP servers, their OAuth, and their headers | [Shared catalog](docs/CATALOGUE.md) |
| Install and expose the service | [Deployment](docs/DEPLOYMENT.md) |
| Configure Keycloak groups and mappers | [Keycloak tutorial](docs/KEYCLOAK.md) |
| Understand local HTTP, NetworkPolicies, and storage | [Catalog options](docs/CONFIGURATION.md) |
| Understand local session protection | [Client security](SECURITY_CLIENT.md) |
| Understand Kubernetes and container hardening | [Container security](SECURITY_CONTAINER.md) |
| Build wheels or contribute code | [Development](docs/DEVELOPMENT.md) |

The [Minikube scenario](docs/MINIKUBE_TEST.md) describes a specific test environment.
The [V2 specification](SPECIFICATION_TEMBA_V2.md) also contains ideas for future development;
the guides above describe the implemented behavior.
