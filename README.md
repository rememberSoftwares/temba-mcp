# Temba

Stop WAISTING tokens with MCP servers that you won't use!  
Temba is a **catalog** of MCP servers that let LLMs discover and use tools only when needed.  

[Getting started](#getting-started) · [Deploying the catalog](#deploying-the-catalog) · [Documentation](#documentation)

## Why Temba?

Using the official Jira MCP will add 20k tokens to your context from the start and disturb the LLM.  
You may not even use these MCP in every conversations.  
The same applies with all MCPs that ar loaded by default in your context. Do you really need this Gitlab MCP ? You're paying for its token .

- **Don't waste tokens** : The catalog's search feature prevents loading all your MCPs in the context from the start.
- **Manage tools access using Keycloak** : Use keycloak groups to assign tools to users.
- **Simple catalog management** : Use Infra As Code or REST API to manage catalog entries.
- **Secured by design** : OIDC sessions from remote MCPs stays on the machine.
- **Dynamic MCP registration** : Optionnally let the LLM register MCPs it finds to its own catalog.

## How it works

>> show a mermaid like graph of a local MCP tool that retieves a list of MCP urls from the remote catalogue and then connects to theses urls
>> Also add a link showing the admin api adding new tools.

## Getting started

Temba is made of two components :
* The server side catalog installed on Kubernetes using Helm.
* The local MCP tool that connects to the catalog used by the client (Opencode, Codex, Claude Code, etc)

### Installing the catalog

The catalog must be installed on Kubernetes using the helm chart in `./chart/temba-catalog`.

```bash
cd ./chart/temba-catalog
helm upgrade --install temba chart/temba-catalog
```

### 1. Get the client

Install [uv](https://docs.astral.sh/uv/getting-started/installation/), which provides the
`uvx` command. You do not need to install Rust, Cargo, or a compiler.

Get the **Temba wheel for your system** from
[GitHub Releases](https://github.com/rememberSoftwares/temba-mcp/releases).
A release is published automatically when a numeric version tag is pushed.
If no release is available yet, get a wheel from your administrator or the
[CI artifacts](.github/workflows/ci.yaml).

The `.whl` contains the precompiled Rust binary. You can use its downloaded path
or its GitHub Release URL with `uvx --from`. Native archives are also available
for running without uv. See [Temba releases and installation](docs/RELEASES.md).

CI targets Linux x86_64, macOS Apple Silicon and Intel, and Windows x86_64.
**PyPI publishing is not configured**; GitHub Release downloads work without it.

### 2. Create your configuration

Copy [temba.example.yaml](temba.example.yaml) to a private file named `temba.yaml`, then set
the catalog URL and a local alias:

```yaml
labels: []
catalogs:
  - id: team
    url: https://catalogue.example.com
```

Inspect the catalog to find MCP IDs, endpoints, labels and required headers:

```sh
uvx --from /absolute/path/to/client.whl temba --config /absolute/path/to/temba.yaml catalog
```

Add `--json` for JSON output. If the catalog is protected, Temba opens the browser
for login. See [inspecting the catalog](docs/CLIENT.md#inspecting-the-catalog).

### 3. Connect Temba to your AI client

The client must launch the following command as a **local MCP server**:

```sh
uvx --from /absolute/path/to/client.whl temba --config /absolute/path/to/temba.yaml
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
        "temba",
        "--config",
        "/absolute/path/to/temba.yaml"
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
uvx --from /absolute/path/to/client.whl temba --help
```

### 4. Make your first request

Launch your AI client and try asking:

> List the services you have access to.

The assistant uses `tool_search` to discover tools, then `call_tool` to execute them.

If authentication is required, Temba opens the browser **when you use the service**.
An existing SSO session may authorize the connection immediately: Temba briefly waits
for the result and continues with the request. If you still need to take action, the assistant lets you know.

> Changed the YAML file, environment variables, or client version?
> Restarting the UI of your AI client may not restart the process in charge of loading MCPs.
> For instance, opencode needs "opencode service restart".



## The catalog : Learn by example

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

* .yaml on the client desktop:
```yaml
allow_insecure_http: true
catalogs:
  - id: team
    url: http://127.0.0.1:18080
```

=> Ask your LLM to search something about a specific Github repo.

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

* .yaml on the client desktop:
```yaml
allow_insecure_http: true
catalogs:
  - id: team
    url: http://127.0.0.1:18080
```

### Sending a custom header

Some MCPs need to recieve somes specific headers to work.  
Let's say that e have a Gitlab MCP that needs two custom headers :
* **Non-sensitive** string to scope the MCP to a particular project.
* **Sensitive** string containing a user defined token.

The non-sensitive header can be set in the catalog directly. On the other hand the sensitive token, being a user defined value, will be set in the client's YAML file.

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
      credential: token      # Marks this header as a private client-supplied value
      mandatory: true
```

* .yaml on the client desktop:
```yaml
allow_insecure_http: true
catalogs:
  - id: team
    url: http://127.0.0.1:18080
    tools:
      gitlab:
        expected_endpoint: http://127.0.0.1:18082/mcp
        headers:
          X-Gitlab-Token: glpat-<SECRET-TOKEN-OF-USER>
```

If you don't want to store the secret inside the YAML, you can use the ENV like so:
```yaml
# Inside the MCP configuration, replace headers with:
headersFromEnv:
  - name: X-Gitlab-Token
    valueFrom:
      envVarRef:
        name: GITLAB_TOKEN
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

* .yaml on the client desktop for a developer:
```yaml
labels:
- dev

# Other settings…
```
=> This client can discover the Gitlab MCP but not Jira.  

* .yaml on the client desktop for a product manager:
```yaml
labels:
- pm

# Other settings…
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

Let's add a Gitlab and Jira MCP. The Gitlab MCP is for users part of group `team-a`. The jira MCP is for users part of group `team-b`. 

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

* .yaml on the client desktop:
```yaml
allow_insecure_http: true
catalogs:
  - id: team
    url: http://127.0.0.1:18080
```

=> Depending on the user's group, it will access either one of the tools or both if the user is present in both groups.  


## Several catalogs and a local IaC catalog

Add one `catalogs` entry per source. Each contributes up to `search_limit`
results, with IDs such as `team.service.search` and `local.service.search`.
Credentials and OAuth sessions stay separate, and an unavailable source does not
block the others.

`temba serve` exposes the static definitions in `local_catalog` without a management
API. Set `local_catalog.enabled: true`: when it contains entries, the client
includes it automatically as `local`. No URL needs to be added to `catalogs`.
Private headers belong in `local_catalog.tools`; `listen` is optional. The LLM's optional third tool is configured separately in `dynamic_catalog`.
See [multiple catalogs and local serving](docs/MULTI_CATALOG.md) for the setup.

## Let the LLM manage the catalog

We're about to live in a world where LLMs will do a lot of our work. To do so, they need to interact with many services, therefore many MCPs will be needed...
And because you can't know for sure which one will be required, the best solution is to let the LLM add them to the catalog itself.

Enable the feature in your YAML file:

```yaml
dynamic_catalog:
  enabled: true
  max_servers: 16
```

To test, ask the LLM to register the official Jira MCP. It will look for it on the internet and add the URL to the catalog.

:warning: This is an experimental feature that give LLMs a lot of power. In a near futur this will be very useful but maybe it's also too soon. You should keep an eye on your AI client when using this.


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
| Configure YAML, tokens, or labels | [Local client](docs/CLIENT.md) |
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
