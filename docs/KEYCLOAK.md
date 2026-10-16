# Restricting MCP servers to Keycloak groups

A **group** brings users together. Temba can restrict an MCP server to one group, several groups, or make it public. Group names are up to you; this guide uses `team-a` and `team-b` as examples.

These rules apply to **all tools of an MCP server**. They control its visibility in the catalog; each MCP server keeps its own permissions and any authentication it requires.

## 1. Configure the Keycloak client

In your chosen realm (the space containing your users), open **Clients → temba**, or create an OpenID Connect client named `temba`.

- **Client authentication**: Off; **Standard flow**: On.
- **Require PKCE**: On, with method **S256** if the choice is offered.
- **Valid redirect URIs**: `http://127.0.0.1:19877/oauth/callback`.
- Root URL, Home URL, Web origins, and logout URLs can remain empty.

This callback returns to the **user's machine**, not Kubernetes. Adjust the port if `catalogueAuth.callbackPort` changes.

## 2. Add the two mappers

A **mapper** adds information to the authentication token sent to Temba.

Open **Clients → temba → Client scopes → temba-dedicated → Mappers → Add mapper → By configuration**. Create and save:

| Mapper type | Fields to fill in |
|---|---|
| **Group Membership** | Name: `temba-groups`; Token Claim Name: `groups`; Full group path: **On**; Add to access token: **On** |
| **Audience** | Name: `temba-audience`; Included Client Audience: **empty**; Included Custom Audience: `temba-mcp`; Add to access token: **On**; Add to ID token: **Off** |

**Groups** indicate what the user can see. The **audience** indicates that the token is intended for the Temba catalog. No special `temba:use` scope is requested.

If the dedicated scope does not appear, put these mappers in a client scope attached to `temba` as **Default**, so they are always applied.

## 3. Create groups and assign users

1. **Groups → Create group**: create `team-a` at the top level. Its path will be `/team-a`. Repeat for `team-b` (`/team-b`).
2. **Users → user → Groups → Join group**: select one or more of these groups.

The mapper passes along group memberships; it does not add users to groups.

## 4. Configure the Helm override

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
| `access: {groupsAnyOf: ["/team-a"]}` | Members of this group |
| `access: {groupsAnyOf: ["/team-a", "/team-b"]}` | Members of either team |

Without `access`, the MCP server is public. Paths must match exactly, including case. The group `/team-a/child` does not automatically match `/team-a`.

The MCP server's own authentication (`oauth` block and/or `headers` list) remains independent of these rules. Disabling `catalogueAuth.enabled` makes protected entries invisible to everyone.

## 5. Test

After deployment, restart the Temba client, ask the agent to search for tools, and complete the catalog connection in the browser. A member of `/team-a` sees MCP servers restricted to that group; a user outside the group does not. Local labels must also allow them to be displayed.

If an expected MCP server is missing, check group membership, local labels, and both mappers. **Client scopes → Evaluate → select the user → Generated access token** must contain `groups: ["/team-a"]` and an `aud` audience containing `temba-mcp`.

A group change takes effect with a **new token**, followed by a catalog refresh. Restarting the Temba client and then requesting a tool search lets you repeat this test. If the LLM client uses a persistent server, restart the Temba process on the server; reopening an interface is not enough. Connecting to the catalog does not automatically connect to remote MCP servers.

Reference: [Keycloak mappers](https://www.keycloak.org/admin-api/protocol-mappers).
