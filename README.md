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
uvx --from /chemin/temba_client-1.3.2-…whl temba-client --config /chemin/temba.toml
```

Après publication du package, la commande sera :

```sh
uvx --from temba-client==1.3.2 temba-client --config /chemin/temba.toml
```

Copier [temba.example.toml](temba.example.toml) et adapter `catalog_url`. Exemple de configuration OpenCode pour un wheel local :

```json
{
  "mcp": {
    "temba": {
      "type": "local",
      "command": ["uvx", "--from", "/chemin/temba_client-1.3.2-…whl", "temba-client", "--config", "/chemin/temba.toml"],
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

`jira.__temba_disconnect__` ferme cette connexion et supprime ses credentials OAuth. `__catalog__.__temba_disconnect__` ferme toutes les connexions et termine la session du catalogue. Les journaux vont sur stderr, avec le nom des MCP et une progression comme `MCP jira: ready; ready=2/4; pending=[grafana:connecting, autre:auth_required]` ; stdout est réservé au protocole MCP.

## OAuth et headers des MCP

Les options sont indépendantes :

| Besoin | Définition du MCP |
|---|---|
| Sans authentification | Ni `oauth`, ni `headers` |
| Un ou plusieurs headers statiques | Liste `headers` ; valeurs lues localement dans des variables d'environnement |
| OAuth / OIDC | Bloc `oauth` ; PKCE S256, sessions en RAM sur le poste |
| OAuth et headers supplémentaires | Bloc `oauth` **et** liste `headers` |

Exemple de catalogue :

```yaml
schemaVersion: 2
servers:
  - id: docs
    endpoint: https://docs-mcp.example.com/mcp
  - id: grafana
    endpoint: https://grafana-mcp.example.com/mcp
    headers:
      - name: Authorization
        credential: api_token
        mandatory: true
  - id: jira
    endpoint: https://mcp.atlassian.com/v2/mcp?tools=all
    oauth:
      allowedOrigins: [https://auth.atlassian.com]
    headers:
      - name: X-Api-Key
        credential: api_key
      - name: X-Tenant
        credential: tenant
      - name: X-App
        value: temba
```

La chart reprend ces définitions sous `catalog.servers`. Un header a exactement une source :

- `credential` : référence à une valeur privée fournie localement ;
- `value` : valeur publique non secrète, identique pour tous les clients.

`mandatory` vaut **false par défaut**. Sans valeur locale, un header optionnel est omis. Avec `mandatory: true`, son absence bloque la connexion avant tout appel distant et le MCP affiche `credentials_required`. Il reste visible pour que l'utilisateur sache quoi configurer.

Le client obtient ces déclarations via **GET /v1/catalog**, l'API utilisateur filtrée selon ses droits Keycloak. Aucun appel admin n'est nécessaire. Chaque header y inclut `mandatory: true/false`, ainsi que `credential` ou `value`. `tool_search` expose aussi ces déclarations dans les états des serveurs ; les valeurs privées ne sont jamais renvoyées au LLM.

Dans le fichier TOML local, `[tool.grafana]` décrit le MCP et `[tool.grafana.headers]` fournit ses valeurs privées :

```toml
[tool.grafana]
expected_endpoint = "https://grafana-mcp.example.com/mcp"
[tool.grafana.headers]
api_token = { env = "GRAFANA_AUTHORIZATION" }

[tool.jira]
expected_endpoint = "https://mcp.atlassian.com/v2/mcp?tools=all"
[tool.jira.headers]
api_key = { env = "JIRA_API_KEY" }
tenant = "engineering"
```

Une chaîne est **la valeur finale**, sans recherche automatique dans l'environnement : `api_token = "GRAFANA_AUTHORIZATION"` enverrait littéralement ces mots. Utiliser `api_token = { env = "GRAFANA_AUTHORIZATION" }` pour lire cette variable, ou `api_token = "Bearer …"` pour une valeur directe. Les clés `api_token`, `api_key` et `tenant` correspondent aux références `credential` du catalogue, pas aux noms HTTP.

Le choix `{ env = "…" }` demande de fournir la variable à l'environnement du processus client. La valeur est envoyée **telle quelle**, sans `prefix` ajouté par Temba. Une valeur directe reste enregistrée dans le TOML : garder ce fichier privé. Par exemple, `GRAFANA_AUTHORIZATION` contient déjà `Bearer …`. Pour les headers X-*, fournir directement la valeur attendue par le MCP. `endpoint` est la destination demandée par le catalogue ; `expected_endpoint` est celle à laquelle le poste autorise l'envoi de ses secrets. Ce second contrôle empêche un changement de catalogue de rediriger un token local vers une autre URL. Il est nécessaire uniquement pour les headers privés configurés localement, pas pour les headers publics du catalogue. Si la destination change, le client refuse d'envoyer les valeurs tant que l'URL approuvée localement ne correspond plus. Elles ne sont pas transmises au catalogue, aux endpoints OAuth ni aux autres MCP. Les redirections HTTP sont désactivées.

OAuth fournit lui-même le header `Authorization` : il peut être combiné avec plusieurs `X-*`, mais pas avec un second `Authorization`. Les doublons de noms de headers, même avec une casse différente, sont rejetés. Sans OAuth, `Authorization` peut contenir un token statique. Les headers autorisés sont `Authorization` et les noms `X-*`. Une `value` publique est autorisée seulement pour les X-* ; ne jamais y placer une clé API ou un autre secret, car elle est lisible dans le catalogue et persistée sur le volume.

Pour un fournisseur sans enregistrement dynamique de client (DCR), renseigner `oauth.clientId` et `oauth.callbackPort`, puis enregistrer la redirect URI `http://127.0.0.1:PORT/oauth/callback` chez ce fournisseur. `oauth.scopes` contient ses permissions OAuth. Pour un serveur d'autorisation sur une autre origine, renseigner `oauth.allowedOrigins`. Ces adresses doivent être en HTTPS.

Les `labels` de chaque serveur dans la chart décrivent ses catégories, par exemple `labels: [jira]`. Le champ `labels` de `temba.toml` choisit les catégories à inclure : `labels = []` inclut tous les MCP accessibles ; `labels = ["jira"]` inclut seulement ceux portant le label `jira`. Avec plusieurs labels demandés, tous doivent être présents sur le serveur. Aucun header OpenCode n’est nécessaire. Ce filtre facilite l’usage ; ce n’est pas une règle d’autorisation serveur.

## Catalogue et Keycloak

`catalogueAuth.enabled` contrôle uniquement la connexion au catalogue ; `oauth` contrôle celle du MCP distant. Le catalogue est public par défaut : **Keycloak est optionnel**, même pour une définition de MCP distant OAuth. L’authentification au catalogue et celle à Jira sont indépendantes.

Chaque MCP peut être public, accessible à tous les utilisateurs connectés ou réservé à un ou plusieurs groupes Keycloak. Par exemple `access: {groupsAnyOf: ["/temba-jira"]}` réserve l'entrée Jira à ce groupe, et `access: {public: true}` reste accessible sans connexion. Les règles sont appliquées par le catalogue à partir du claim signé `groups`, indépendamment des labels locaux. L'ancien champ `catalogAccess` est remplacé par `access`.

Voir [la configuration Keycloak et les exemples de règles](docs/KEYCLOAK.md). La surcharge locale utilise `/temba-jira` : créer ce groupe, y ajouter les utilisateurs et configurer le mapper **Group Membership** sur le client `temba`.

Lorsque l'authentification du catalogue est activée, le client ouvre automatiquement la connexion navigateur au démarrage sans bloquer son initialisation MCP. L'action `__catalog__.__temba_connect__` permet de relancer cette connexion si nécessaire. Les règles portent sur les entrées MCP, pas sur les outils individuels ni sur les permissions des données distantes. Cette action reconnecte uniquement le catalogue Temba : elle ne lance pas l'OAuth de Jira et ne donne aucun droit supplémentaire. Pour connecter un MCP distant visible, utiliser son action `<serveur>.__temba_connect__` renvoyée par `tool_search`. Un MCP absent peut être filtré par les droits du catalogue ou les labels locaux ; le client ne peut pas en déduire la cause exacte.

Configuration du client Keycloak :

- Client public (`Client authentication: Off`), Standard flow activé, PKCE S256 requis.
- Redirect URI exacte : `http://127.0.0.1:19877/oauth/callback` (port configurable dans la chart).
- Audience des access tokens : celle de `catalogueAuth.audience`, par défaut `temba-mcp`. Le mapper d’audience existant peut être conservé.
- **Aucun scope personnalisé `temba:use` n’est requis ou vérifié.** Le login demande seulement le scope OIDC standard `openid`.

Les anciens callbacks `/session/callback` et `/oidc/callback` ne sont plus utilisés. Root URL, Home URL, post-logout redirect URIs et Web origins ne sont pas nécessaires pour ce client natif. L’échange du code se fait localement avec PKCE, sans secret client.

Le catalogue vérifie signature RS256, issuer, audience, expiration, `nbf`, sujet et type de token d’accès. Les overrides `catalogueAuth.jwksUri` et `catalogueAuth.tokenEndpoint` restent possibles si la découverte Keycloak annonce de mauvaises URLs : HTTPS et même origine que l’issuer sont obligatoires.

## Déploiement du catalogue

```sh
nerdctl build -t temba-catalog:1.3.1 .
nerdctl save -o /tmp/temba-catalog-1.3.1.tar temba-catalog:1.3.1
minikube image load /tmp/temba-catalog-1.3.1.tar
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
| `/v1/admin/servers` POST | Ajout persistant si `adminApi.readOnly: false` |
| `/v1/admin/servers/{id}` DELETE | Suppression persistante d'un ajout API |

L'API administrative est désactivée sans `adminApi.secretName` (Secret Kubernetes existant). `adminApi.readOnly: true` autorise uniquement la consultation ; `false` autorise les ajouts et suppressions et impose une seule réplique ainsi qu'un volume persistant. Aucun secret n'est généré automatiquement.

```yaml
adminApi:
  readOnly: false
  secretName: temba-admin
  tokenKey: token
persistence:
  enabled: true
  size: 100Mi
  storageClass: ""  # classe par défaut ; ou existingClaim pour un PVC existant
```

Les ajouts API sont sauvegardés dans `/var/lib/temba/additions.json` et chargés au démarrage. L'API confirme après écriture et synchronisation du fichier, puis remplacement atomique. Une erreur d'écriture ne publie pas la modification en RAM. Un fichier invalide ou un conflit avec une définition Helm bloque le démarrage avec un diagnostic. Les définitions Helm ne peuvent pas être modifiées par l'API.

Le PVC créé est conservé lors d'une désinstallation Helm. Sa sauvegarde et sa suppression définitive relèvent de l'administrateur. Le déploiement utilise `Recreate` avec le volume : il y a une courte interruption lors d'une mise à jour. Aucun token OAuth ni valeur privée de header n'est écrit sur ce volume ; les éventuelles valeurs publiques font partie des définitions.

Voir [les options réseau et HTTP local](docs/CONFIGURATION.md). La version 1.2 utilise le schéma de catalogue **2** : migrer `auth` global vers `catalogueAuth`, remplacer `adminApi.mutable` par `adminApi.readOnly` en inversant sa valeur, et remplacer l'ancien `auth.type` des MCP par `oauth` et/ou `headers`. Mettre à jour catalogue et client ensemble ; les anciennes clés sont rejetées. Depuis le client 1.3.2, remplacer `[credentials.<id>]` par `[tool.<id>]` et `[credentials.<id>.env]` par `[tool.<id>.headers]`, en écrivant explicitement les références `{ env = "…" }`. Depuis 1.3.1, les définitions MCP utilisent `endpoint` à la place de `url` (y compris les définitions API persistées). Depuis 1.3, retirer aussi `headers[].prefix`, fournir la valeur complète côté client et ajouter `mandatory: true` aux headers réellement requis.

Lors d'une migration d'un Deployment existant vers `Recreate`, si Helm conserve l'ancienne section `rollingUpdate`, la retirer une fois avant de relancer l'upgrade (adapter le nom et le contexte) :

```sh
kubectl patch deployment temba --type=merge -p '{"spec":{"strategy":{"type":"Recreate","rollingUpdate":null}}}'
```


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
