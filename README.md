# 🐯 Temba

Stop waisting tokens in every chats by using MCP servers that you don't use!  
Temba is a **catalog** of MCP servers that let LLMs discover and use tools only when needed.  

[Getting started](#getting-started) · [Deploying the catalog](#deploying-the-catalog) · [Documentation](#documentation)

## 💡 Why Temba?

Using the official Jira MCP will add 20k tokens to your context from the start and disturb the LLM.  
You may not even use these MCP in every conversations.  
The same applies with all MCPs that are loaded by default in your context. Do you really need this Gitlab MCP ? You're paying for its token .  

- **Stop wasting tokens** : The catalog's search feature prevents loading all your MCPs in the context from the start.
- **Manage tools access using Keycloak** : Use keycloak groups to assign tools to users (use labels otherwise).
- **Local MCP catalog** : Create your own catalog of MCPs without needing any backend
- **Dynamic MCP registration** : Optionnally let the LLM register MCPs it finds to its own catalog.
- **Simple catalog management** : Use Infra As Code or REST API to manage catalog entries.

## ⚙️ How it works

>> show a mermaid like graph of a local MCP tool that retieves a list of MCP urls from the remote catalogue and then connects to theses urls
>> Also add a link showing the admin api adding new tools.

<a id="getting-started"></a>

## 🚀 Getting started

Temba is made of two components :
* A server side catalog installed on Kubernetes using Helm (optionnal).
* A local MCP tool that connects to catalogs (use with AI client like Opencode, Codex, Claude, ...).


### 📦 1. Install Temba

Download the **native archive for your system** from
[GitHub Releases](https://github.com/rememberSoftwares/temba-mcp/releases)
and extract it.  

On Linux and macOS, install the extracted binary on your `PATH`:

```sh
sudo install -m 0755 temba /usr/local/bin/temba
temba --version
```

On Windows, put `temba.exe` in a directory on your user `PATH`.
See [installation for each platform](docs/RELEASES.md#install-the-native-binary)
for archive names, extraction and checksum verification.

If the version you need is not released yet, use the native archive from the
[CI artifacts](.github/workflows/ci.yaml) or ask your administrator for it.

### 📝 2. Create your configuration

Run the short setup assistant:

```sh
temba add catalog
```

Enter the catalog URL, choose a unique name and optionally select labels. Leave
the URL empty to enable your local catalog. Add its MCPs with the interactive
assistant:

```sh
temba add mcp
```

Choose `local` for an HTTP MCP or a local stdio program. Choosing a remote catalog
configures local headers for an already published MCP; it does not modify the
shared catalog. The assistant saves the configuration to
`~/.temba/temba.yaml` (`%USERPROFILE%\.temba\temba.yaml` on Windows).
You can also start from [temba.example.yaml](temba.example.yaml).

Update basic settings without editing YAML:

```sh
temba set search_limit 10
temba set dynamic_catalog true
```

See `temba --help` and `temba set --help` for settings and examples.

Inspect the catalog to find MCP names, endpoints, labels and required headers:

```sh
temba catalog
```

Add `--json` for JSON output. If the catalog is protected, Temba opens the browser
for login. See [inspecting the catalog](docs/CLIENT.md#inspecting-the-catalog).

### 🔌 3. Connect Temba to your AI client

The client must launch the following command as a **local MCP server**:

```sh
temba
```

Temba reads the configuration from your home directory,
independently of the AI client's working directory. To use another file, add
`--config /absolute/path/to/temba.yaml` or set `TEMBA_CONFIG`.

<details>
<summary>OpenCode configuration example</summary>

Add a local MCP entry to your OpenCode configuration:

```json
{
  "mcp": {
    "temba": {
      "type": "local",
      "command": ["temba"],
      "enabled": true
    }
  }
}
```

</details>

For another MCP client, use the same command in its configuration format.
If Temba is not on that client’s `PATH`, use the absolute installed binary path.
To check only that the binary is installed, run:

```sh
temba --help
```

### 💬 4. Make your first request

Launch your AI client and try asking:

> List the services you have access to.

The assistant uses `tool_search` with an empty query to list MCPs without loading
tool schemas into its context. Keyword queries return matching tool details;
`call_tool` executes a selected tool.

If authentication is required, Temba opens the browser **when you use the service**.
An existing SSO session may authorize the connection immediately: Temba briefly waits
for the result and continues with the request. If you still need to take action, the assistant lets you know.


<a id="deploying-the-catalog"></a>

## ☸️ Kubernetes catalog install

Installing the catalog on k8s allows to:
* serve tools based on keycloak groups
* update the tools list by admins using IAC or API

The catalog must be installed on Kubernetes using the helm chart in `./chart/temba-catalog`.

```bash
cd ./chart/temba-catalog
helm upgrade --install temba chart/temba-catalog
```

**Learn by example ! Look at the different usecases bellow:**

### 🌐 Adding an unauthenticated MCP to the catalog

Let's add the DeepWiki MCP to the catalog. It does not require any kind of authentication.  

* Temba catalog:  
```yaml
catalog:
  servers:
  - name: deepwiki
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
  - name: team-name
    url: http://127.0.0.1:8080
```

=> Ask your LLM to search something about a specific Github repo.

### 🔐 Adding an OIDC MCP to the catalog

Let's add the official Jira MCP that uses an oauth connexion

* Temba catalog:  
```yaml
catalog:
  servers:
  - name: jira
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
  - name: team-name
    url: http://127.0.0.1:8080
```

### 📨 Sending a custom header

Some MCPs need to recieve specific headers from the client to work.  

Let's say that we have a Gitlab MCP that needs two custom headers :
* A **Non-sensitive** string to scope the MCP to a particular project.
* A **Sensitive** string containing a personnal user token (GLPAT).

The non-sensitive header can be set in the server side catalog directly.  
On the other hand the sensitive token, being a user defined value, must be set in the client's YAML file.  

* Temba catalog:  
```yaml
catalog:
  servers:
  - name: gitlab
    description: GitLab tools for project and instance configured on the MCP server.
    endpoint: http://<GITLAB-MCP-URL>/mcp
    headers:

    - name: X-Gitlab-Project # For non-sensitive values 
      value: <REPO/PROJECT> 


    - name: X-Gitlab-Token   # For sensitive values
      credential: token      # The `credential` key marks this header as a client-supplied value
      mandatory: true
```

* .yaml on the client desktop:
```yaml
allow_insecure_http: true
catalogs:
  - name: team-name
    url: http://127.0.0.1:18080
    tools:
      gitlab: # !! <= Must match the tool name server side.
        expected_endpoint: http://127.0.0.1:18082/mcp
        headers:
          X-Gitlab-Token: glpat-<SECRET-TOKEN-OF-USER>
```

If you don't want to store the secret inside the YAML, you can use the ENV like so:
```yaml
allow_insecure_http: true
catalogs:
  - name: team-name
    url: http://127.0.0.1:18080
    tools:
      gitlab: # !! <= Must match the tool name server side.
        expected_endpoint: http://127.0.0.1:18082/mcp
        headersFromEnv:
          - name: X-Gitlab-Token
            valueFrom:
              envVarRef:
                name: GITLAB_TOKEN # <= ENV var
```
:warning: Note that some LLM clients may not forward local ENV to their subprocess.


### 🏷️ Partitioning tools using labels

You can add labels on MCPs. These MCPs will only be available to clients with the same labels.  

Let's add a Gitlab and Jira MCP. The Gitlab MCP is for developpers and use the label "dev". The jira MCP is for product managers and use the labels "pm". 

* Temba catalog:  
```yaml
catalog:
  servers:
  - name: gitlab
    labels: [dev] # <= Here
    description: GitLab tools for project and instance configured on the MCP server.
    endpoint: http://<GITLAB-MCP-URL>/mcp

  - name: jira
    labels: [pm] # <= And here
    description: Atlassian remote Jira MCP ; sign in using its browser OAuth flow.
    endpoint: https://mcp.atlassian.com/v2/mcp?tools=all
    oauth: {}
```

* .yaml on the client desktop for a developer:
```yaml
catalogs:
  - name: team
    url: https://catalogue.example.com
    labels:
      - dev # <= Here
```
=> This client can discover the Gitlab MCP but not Jira.  

* .yaml on the client desktop for a product manager:
```yaml
catalogs:
  - name: team
    url: https://catalogue.example.com
    labels:
      - pm # <= And here
```
=> This client can discover the Jira MCP but not Gitlab  

:warning: Labels are not a security feature. It's a simple way to partition tools but if you need true separation then use the Keycloak feature bellow.  


### 🔑 Partitioning tools using Keycloak

Temba MCP supports OIDC. Use Keycloak groups to assign tools to users.

#### ⚙️ 1. Configure the Keycloak client

In your chosen realm (the space containing your users), create a new client: **Clients → temba**, or create an OpenID Connect client named `temba`.

- **Client authentication**: Off; **Standard flow**: On.
- **Require PKCE**: On, with method **S256** if the choice is offered.
- **Valid redirect URIs**: `http://127.0.0.1:19877/oauth/callback`.
- Root URL, Home URL, Web origins, and logout URLs can remain empty.

:information_source: After login, Keycloak instructs the user's browser to open the redirect URI. The browser connects to the local Temba client on the same machine; Kubernetes is not involved in this callback. The port must match catalogueAuth.callbackPort.

#### 🔗 2. Add the two mappers

A **mapper** adds information to the authentication token sent to Temba.

Open **Clients → temba → Client scopes → temba-dedicated → Mappers → Add mapper → By configuration**. Create and save:

| Mapper type | Fields to fill in |
|---|---|
| **Group Membership** | Name: `temba-groups`; Token Claim Name: `groups`; Full group path: **On**; Add to access token: **On** |
| **Audience** | Name: `temba-audience`; Included Client Audience: **empty**; Included Custom Audience: `temba-mcp`; Add to access token: **On**; Add to ID token: **Off** |

**Groups** indicate what the user can see. The **audience** indicates that the token is intended for the Temba catalog. No special `temba:use` scope is requested.

If the dedicated scope does not appear, put these mappers in a client scope attached to `temba` as **Default**, so they are always applied.

#### 👥 3. Create groups and assign users

1. **Groups → Create group**: create `team-a` at the top level. Its path will be `/team-a`. Repeat for `team-b` (`/team-b`).
2. **Users → user → Groups → Join group**: select one or more of these groups.

The mapper passes along group memberships; it does not add users to groups.

#### 🛠️ 4. Configure the Helm override

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
| `access: {public: true}` | Everyone, without a catalog connection (default) |
| `access: {authenticated: true}` | All users authenticated to the catalog |
| `access: {groupsAnyOf: ["team-a"]}` | Members of this group |
| `access: {groupsAnyOf: ["team-a", "team-b"]}` | Members of either team |

:information_source: Without `access`, the MCP server is public. The initial `/` is optional in catalog rules: `team-a` and `/team-a` are equivalent; `teams/team-a` means `/teams/team-a`. Paths and case must otherwise match exactly: `team-a/child` does not match `team-a`. Keep **Full group path: On** in the Keycloak mapper so nested groups remain distinct.


**Back to the catalog !**

Let's add a Gitlab and Jira MCP. The Gitlab MCP is for users part of group `team-a`. The jira MCP is for users part of group `team-b`. 

* Temba catalog:  
```yaml
catalog:
  servers:
  - name: gitlab
    description: GitLab tools for project and instance configured on the MCP server.
    endpoint: http://<GITLAB-MCP-URL>/mcp
    access:
      groupsAnyOf: ["team-a"]
  - name: jira
    description: Atlassian remote Jira MCP ; sign in using its browser OAuth flow.
    endpoint: https://mcp.atlassian.com/v2/mcp?tools=all
    access:
      groupsAnyOf: ["team-b"]
    oauth: {}
```

* .yaml on the client desktop:
```yaml
allow_insecure_http: true
catalogs:
  - name: team-name
    url: http://127.0.0.1:8080
```

=> Depending on the user's group, it will access either one of the tools or both if the user is present in both groups.  


## 🤖 Let the LLM manage the catalog

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


<a id="documentation"></a>

## 📚 Documentation

| You want to… | Read |
| --- | --- |
| Configure YAML, tokens, or labels | [Local client](docs/CLIENT.md) |
| Describe MCP servers, their OAuth, and their headers | [Shared catalog](docs/CATALOGUE.md) |
| Install and expose the service | [Deployment](docs/DEPLOYMENT.md) |
| Configure Keycloak groups and mappers | [Keycloak tutorial](docs/KEYCLOAK.md) |
| Understand local HTTP, NetworkPolicies, and storage | [Catalog options](docs/CONFIGURATION.md) |
| Understand local session protection | [Client security](SECURITY_CLIENT.md) |
| Understand Kubernetes and container hardening | [Container security](SECURITY_CONTAINER.md) |
| Build Temba or contribute code | [Development](docs/DEVELOPMENT.md) |

The [Minikube scenario](docs/MINIKUBE_TEST.md) describes a specific test environment.
The [V2 specification](SPECIFICATION_TEMBA_V2.md) also contains ideas for future development;
the guides above describe the implemented behavior.


**Temba, his arms wide.**  
