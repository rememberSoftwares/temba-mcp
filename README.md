# Temba

Temba est composé d’un **catalogue REST central** et d’un **MCP local en Rust**. Le catalogue fournit les adresses et les descriptions des MCP distants. Le client local recherche leurs outils et les appelle directement, via leurs ingress HTTPS.

Les tokens OAuth des MCP distants restent **uniquement en RAM sur le poste utilisateur**. Le catalogue ne les reçoit pas, ne les renouvelle pas et ne proxifie aucun appel métier. Fermer le processus local supprime ses sessions ; une nouvelle authentification sera nécessaire au prochain lancement.

```text
OpenCode ── stdio ── temba-client ── HTTPS ── MCP Jira / Grafana / …
                          │
                          └── HTTPS ── temba-catalog (définitions uniquement)
```

## Utilisation

Le client est distribué sous forme de wheels contenant un binaire Rust précompilé. L’utilisateur installe `uv`, qui fournit la commande `uvx` ; il n’a pas besoin de Rust, Cargo ou d’un compilateur. Python est provisionné par uv si nécessaire pour le lanceur du wheel.

Le package n’est pas encore publié. Avec un wheel fourni par la CI ou construit localement :

```sh
uvx --from /chemin/temba_client-1.0.1-…whl temba-client --config /chemin/temba.toml
```

Après publication du package, la commande sera :

```sh
uvx --from temba-client==1.0.1 temba-client --config /chemin/temba.toml
```

Copier [temba.example.toml](temba.example.toml) et adapter `catalog_url`. Exemple de configuration OpenCode pour un wheel local :

```json
{
  "mcp": {
    "temba": {
      "type": "local",
      "command": ["uvx", "--from", "/chemin/temba_client-1.0.1-…whl", "temba-client", "--config", "/chemin/temba.toml"],
      "enabled": true
    }
  }
}
```

Utiliser des chemins absolus : le répertoire de travail d’OpenCode dépend du projet ouvert. Le client expose deux outils :

- `tool_search` : recherche BM25 locale, schémas des outils et état de chaque connexion.
- `call_tool` : exécute un `tool_id` retourné par la recherche.

Chaque nouvelle livraison du client doit avoir une version et un nom de wheel distincts : remplacer un wheel au même chemin peut laisser `uvx` utiliser son ancien binaire en cache. Après une mise à jour, modifier le chemin dans OpenCode et relancer OpenCode pour recharger le processus MCP.

Un serveur nécessitant une connexion propose `jira.__temba_connect__`. L’action ouvre le navigateur et renvoie rapidement `auth_in_progress` avec un lien local de secours. Terminer l’autorisation, puis relancer la recherche. Les callbacks écoutent uniquement sur `127.0.0.1` ; ils expirent après dix minutes et ne fonctionnent qu’une fois.

`jira.__temba_disconnect__` ferme cette connexion et supprime ses credentials OAuth. `__catalog__.__temba_disconnect__` ferme toutes les connexions et termine la session du catalogue. Les journaux vont sur stderr, avec le nom des MCP et une progression comme `MCP jira: ready (2/4)` ; stdout est réservé au protocole MCP.

## Trois types de MCP

| Type | Définition dans le catalogue | Comportement local |
|---|---|---|
| Sans authentification | `auth: {type: none}` | Connexion directe, sans Keycloak |
| Token statique | `auth: {type: token}` | Token lu dans une variable d’environnement du poste, associée à une URL exacte |
| OAuth / OIDC | `auth: {type: oauth}` | Connexion navigateur directe au fournisseur distant, PKCE S256, access et refresh tokens en RAM |

Exemple de catalogue :

```yaml
schemaVersion: 1
servers:
  - id: docs
    url: https://docs-mcp.example.com/mcp
    auth: {type: none}
  - id: grafana
    url: https://grafana-mcp.example.com/mcp
    labels: [observability]
    auth: {type: token, headerName: Authorization, prefix: "Bearer "}
  - id: jira
    name: Jira
    description: Search Jira tickets and projects
    url: https://mcp.atlassian.com/v2/mcp?tools=all
    labels: [jira]
    auth: {type: oauth, allowedOrigins: ["https://auth.atlassian.com"]}
```

Pour Grafana, ajouter dans **le fichier local** :

```toml
[credentials.grafana]
token_env = "GRAFANA_MCP_TOKEN"
expected_endpoint = "https://grafana-mcp.example.com/mcp"
```

La variable est fournie à l’environnement du processus local. Elle ne figure jamais dans le catalogue. Si l’URL change, le client refuse d’envoyer le token tant que l’URL approuvée n’a pas été modifiée localement. Un token Grafana n’est jamais ajouté aux requêtes Jira. Les redirections HTTP sont désactivées.

Pour un fournisseur OAuth sans enregistrement dynamique de client (DCR), renseigner `auth.clientId` et `auth.callbackPort`, puis enregistrer la redirect URI `http://127.0.0.1:PORT/oauth/callback` chez ce fournisseur. `auth.scopes` permet de demander les permissions propres au MCP distant. Cela ne crée aucune contrainte de scope Temba.

La découverte OAuth est limitée à l’origine du MCP. Si le serveur d’autorisation utilise une autre origine, l’administrateur doit l’ajouter explicitement dans `auth.allowedOrigins`, par exemple `["https://auth.example.com"]`. Toutes ces adresses doivent être en HTTPS. Les tokens sont rafraîchis à la demande, sous verrou par session, avec une marge adaptée à leur durée de vie.

`labels` filtre localement les définitions, la recherche et les appels. Ce filtre facilite l’usage ; ce n’est pas une règle d’autorisation serveur.

## Catalogue et Keycloak

Le catalogue est public par défaut : **Keycloak est optionnel**, même pour une définition de MCP distant OAuth. L’authentification au catalogue et celle à Jira sont indépendantes.

Pour protéger certaines définitions, activer `auth.enabled` dans la chart et marquer ces serveurs `catalogAccess: authenticated`. Les autres restent `public`. Le client local peut se connecter au catalogue via `__catalog__.__temba_connect__`.

Configuration du client Keycloak :

- Client public (`Client authentication: Off`), Standard flow activé, PKCE S256 requis.
- Redirect URI exacte : `http://127.0.0.1:19877/oauth/callback` (port configurable dans la chart).
- Audience des access tokens : celle de `auth.audience`, par défaut `temba-mcp`. Le mapper d’audience existant peut être conservé.
- **Aucun scope personnalisé `temba:use` n’est requis ou vérifié.** Le login demande seulement le scope OIDC standard `openid`.

Les anciens callbacks `/session/callback` et `/oidc/callback` ne sont plus utilisés. Root URL, Home URL, post-logout redirect URIs et Web origins ne sont pas nécessaires pour ce client natif. L’échange du code se fait localement avec PKCE, sans secret client.

Le catalogue vérifie signature RS256, issuer, audience, expiration, `nbf`, sujet et type de token d’accès. Les overrides `auth.jwksUri` et `auth.tokenEndpoint` restent possibles si la découverte Keycloak annonce de mauvaises URLs : HTTPS et même origine que l’issuer sont obligatoires.

## Déploiement du catalogue

```sh
nerdctl build -t temba-catalog:1.0.0 .
minikube image load temba-catalog:1.0.0
helm upgrade --install temba chart/temba-catalog -f chart/temba-catalog/overide.yaml --reset-values
kubectl port-forward service/temba 18080:8080
```

La surcharge fournie conserve l’issuer et le client Keycloak `temba` de l’environnement de développement. Elle publie Jira ; **Grafana devra être ajouté avec son ingress HTTPS**, car l’adresse Kubernetes `http://mcp-grafana:8000/mcp` n’est pas accessible depuis les postes.

En développement, le fichier TOML peut utiliser `catalog_url = "http://127.0.0.1:18080"` et `allow_loopback_http = true`. En production, utiliser un ingress HTTPS pour le catalogue et chacun des MCP distants. `security.allowLoopbackHttp` dans la chart autorise uniquement des définitions HTTP de loopback pour les tests ; il n’autorise pas un serveur HTTP distant arbitraire. La terminaison TLS du catalogue est assurée par l’ingress.

Endpoints :

| Route | Accès |
|---|---|
| `/healthz`, `/readyz` | Sondes |
| `/v1/bootstrap` | Public, paramètres OAuth non secrets |
| `/v1/catalog` | Définitions publiques, puis protégées si bearer valide ; ETag et cache privé |
| `/v1/admin/servers` GET | Bearer administratif, liste des définitions |
| `/v1/admin/servers` POST | Ajout temporaire si `adminApi.mutable: true` |
| `/v1/admin/servers/{id}` DELETE | Suppression d’un ajout temporaire |

L’API administrative est désactivée sans `adminApi.secretName`. Les mutations sont en RAM, perdues au redémarrage, et imposent un seul replica. Les définitions Helm restent la source durable et ne peuvent pas être remplacées par l’API. Le catalogue refuse les champs secrets et inconnus, notamment `headers`, les tokens et les secrets OAuth.

Le client recharge le catalogue toutes les 60 secondes. Une modification de destination ou une suppression ferme la connexion et efface sa session. Une réponse invalide ne remplace pas le dernier catalogue valide.

## Développement et distribution

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
uvx --from 'maturin>=1.9,<2' maturin build --release --locked --out dist
helm lint chart/temba-catalog
```

La CI construit des wheels Linux x86_64, macOS Apple Silicon / Intel et Windows x86_64, puis vérifie leur lancement avec `uvx`. Aucune compilation Rust n’est demandée aux utilisateurs. Les autres architectures nécessitent un wheel supplémentaire. La publication sur un registre est une étape distincte ; aucune publication automatique n’est configurée.

Cette V2 retire la gateway Python, les enveloppes de tokens, Redis, les anciennes routes MCP HTTP et la recherche par embeddings serveur. La recherche est locale. L’ajout de serveurs par le LLM est également retiré : les définitions viennent du catalogue administré. Il n’y a pas de couche de compatibilité V1.

Voir [sécurité du client local](SECURITY_CLIENT.md) et [sécurité du conteneur](SECURITY_CONTAINER.md). [La spécification V2](SPECIFICATION_TEMBA_V2.md) décrit aussi des évolutions envisagées ; la documentation ci-dessus correspond aux fonctionnalités implémentées.
