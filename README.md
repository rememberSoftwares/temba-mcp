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

#### Configuring the catalog

This walkthrough assumes a catalog is already deployed and you know its URL.



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
