# Local MCP programs through Temba

A single `local_catalog` can contain HTTP MCPs and programs that speak MCP over
stdin/stdout. Your AI client starts Temba, which loads this catalog and manages
the programs automatically. No additional command is needed.

## Configure the programs

Run `temba add mcp`, select `local`, and leave the MCP endpoint empty to configure
a stdio program. Enter the executable and each argument separately; the assistant
also supports working directories and environment variables. Or edit the YAML:

```yaml
local_catalog:
  enabled: true
  servers:
    - name: documentation
      endpoint: https://mcp.example.com/mcp
      access:
        public: true

    - name: workspace
      description: Tools for my workspace
      command: /absolute/path/to/my-mcp
      args:
        - --directory
        - /absolute/path/to/workspace
      cwd: /absolute/path/to/workspace
      timeoutSeconds: 60
      env:
        APPLICATION_MODE: "development"
      envFromEnv:
        - name: API_TOKEN
          valueFrom:
            envVarRef:
              name: WORKSPACE_API_TOKEN
```

Use `endpoint` for HTTP or `command` for stdio, never both. The executable must
speak MCP; a regular shell command is not automatically converted into a tool.
The program runs on the machine hosting Temba. Commands can be executable
paths or names available on its `PATH`, such as `uvx`; keep each argument as a
separate list item. Temba does not evaluate a shell command string. If using a
Windows command shim, an explicit name such as `npx.cmd` can be used.

| Setting for a command entry | Purpose |
| --- | --- |
| `name` | Unique MCP name across both HTTP and stdio entries |
| `description` | Optional searchable description |
| `command` | Executable path or name on `PATH` |
| `args` | Argument list; defaults to `[]` |
| `cwd` | Optional absolute working directory; otherwise inherits the directory of Temba |
| `timeoutSeconds` | Startup and request timeout: 60 by default, between 1 and 300 seconds |
| `env` | Literal environment values, supplied as strings |
| `envFromEnv` | Read named variables from the environment of Temba; missing references prevent startup |

Environment values are sent exactly as configured. They do not belong in
`local_catalog.tools`, which remains for private HTTP headers on HTTP entries.
OAuth and HTTP headers are not supported on command entries.

Children inherit a small execution environment (`PATH`, home, temporary-directory,
locale and Windows system-directory settings), rather than all of Temba's
variables. Pass other required values, including proxy settings, explicitly.
The variables referenced by `envFromEnv` must exist when **Temba** starts.

## Start and use

Launch your AI client with its usual Temba MCP configuration. It starts Temba,
which includes `local_catalog` automatically. Both remote HTTP entries and local
programs work without another process to start manually. For a terminal listing:

```sh
temba --config /absolute/path/to/temba.yaml catalog
```

Each Temba instance owns its local catalog and programs. An internal HTTP gateway
uses a free loopback port automatically; concurrent AI clients do not compete
for a configured port. No listening address needs to be configured.

Commands start lazily when their MCP endpoint is first used, not when the catalog
is listed. One process per MCP entry is shared by the connections to that endpoint;
each Temba instance has its own processes, rather than one process per conversation.
Tools are discovered and called through the usual search and call tools, with
IDs such as `local.workspace.search`.

A failed call is never automatically replayed. A process that crashed is restarted
on the next connection/request. A timed-out call stops its process; the next
request can start it again. Ctrl+C, or SIGTERM on Unix, stops Temba and cleans
up its MCP processes, including descendants in their process group/job object.
Restart your AI client's Temba process after editing the file.

## What is exposed

The internal gateway binds only to loopback. MCP endpoints validate
Host and reject browser Origin headers. These checks do not constitute a sandbox:
a configured program runs with the same OS permissions as Temba.

The internal catalog response contains only MCP metadata and generated endpoints such
as `http://127.0.0.1:<automatic-port>/mcp/workspace`. It never includes `command`, `args`, `cwd`,
`env` or `envFromEnv`. Ordinary tool results remain the output chosen by the MCP.
There is no API for changing or supplying a command. Central catalogs and the
LLM's dynamic registration still accept HTTP definitions only.

The automatically configured local client permits only these exact generated
HTTP endpoints without `allow_insecure_http`; other HTTP MCPs still require that
explicit opt-in. Local MCPs are available without a label filter.
