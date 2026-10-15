# Configurer le client local

Ce guide complète les [premiers pas du README](../README.md#premiers-pas).
Le fichier `temba.toml` appartient au poste qui exécute `temba-client`.
Il est lu au démarrage du processus.

## Les réglages de base

```toml
catalog_url = "https://catalogue.example.com"
labels = []
refresh_seconds = 60
search_limit = 5
```

| Option | Utilité | Valeur par défaut |
| --- | --- | --- |
| `catalog_url` | URL du catalogue REST, sans suffixe `/mcp` ni `/v1/catalog` | Obligatoire |
| `labels` | Labels des MCP que vous choisissez d’inclure | `[]` : MCP sans label |
| `refresh_seconds` | Intervalle de relecture du catalogue central, entre 10 et 3 600 secondes | `60` |
| `search_limit` | Nombre maximum d’outils retournés par recherche, entre 1 et 50 | `5` |
| `allow_loopback_http` | Autorise HTTP pour les adresses de loopback en développement | `false` |

Pour un catalogue accessible via un port-forward :

```toml
catalog_url = "http://127.0.0.1:18080"
allow_loopback_http = true
```

Les MCP distants doivent être joignables **depuis le poste utilisateur**.
Une URL localhost désigne ce poste, pas un pod Kubernetes.
Voir [HTTP local et réseau](CONFIGURATION.md).

## Choisir les MCP par labels

Les labels servent à activer des catégories de MCP sans changer vos permissions.
Pour inclure un MCP, tous ses labels doivent figurer dans votre liste.
Les MCP sans label restent inclus.

| Labels dans le TOML | MCP inclus, parmi ceux autorisés par le catalogue |
| --- | --- |
| `[]` | Ceux qui n’ont aucun label |
| `["support"]` | Ceux sans label, et ceux portant seulement `support` |
| `["support", "documentation"]` | Ceux dont les labels sont un sous-ensemble de cette liste |

Les règles de groupes sont vérifiées séparément par le catalogue central.
Un label ne permet pas de contourner une règle Keycloak.

## Fournir des tokens et des headers privés

Supposons que le catalogue contienne cette définition :

```yaml
id: service
endpoint: https://mcp.example.com/mcp
headers:
  - name: Authorization
    credential: api_token
    mandatory: true
  - name: X-Tenant
    credential: tenant
```

Votre TOML fournit les valeurs pour **cet identifiant de MCP** :

```toml
[tool.service]
expected_endpoint = "https://mcp.example.com/mcp"

[tool.service.headers]
api_token = { env = "SERVICE_TOKEN" }
tenant = "engineering"
```

`api_token` et `tenant` sont les références `credential` du catalogue.
Ce ne sont pas les noms des headers HTTP.

### Valeur directe ou variable d’environnement ?

Une chaîne est toujours une **valeur littérale** :

```toml
[tool.service]
expected_endpoint = "https://mcp.example.com/mcp"

[tool.service.headers]
api_token = "Bearer votre-token"
```

Pour lire une variable, utilisez explicitement `{ env = "SERVICE_TOKEN" }`.
Écrire `api_token = "SERVICE_TOKEN"` enverrait ces mots au serveur.

La valeur est envoyée telle quelle : si le MCP attend `Bearer …`, fournissez
également le préfixe. S’il attend un token brut, fournissez seulement le token.

Une valeur directe reste sur disque dans le TOML ; gardez ce fichier privé et hors du dépôt.
Avec une variable d’environnement, celle-ci doit être présente dans le **processus Temba**.
Un serveur IA déjà démarré n’hérite pas des exports faits ensuite dans un autre terminal.

### Pourquoi `expected_endpoint` ?

`endpoint` est l’adresse annoncée par le catalogue.
`expected_endpoint` est l’adresse à laquelle **vous autorisez l’envoi de vos valeurs privées**.
Les deux doivent correspondre exactement, y compris le chemin et les paramètres de requête.

Si la destination change, Temba refuse d’envoyer vos headers jusqu’à ce que vous
ayez approuvé la nouvelle adresse dans le TOML. Ces valeurs ne sont pas envoyées
au catalogue, aux fournisseurs OAuth ni aux autres MCP. Les redirections HTTP sont désactivées.

Ce réglage est nécessaire pour des headers privés configurés dans le TOML.
Un header public fourni par le catalogue n’en a pas besoin.

### Que voit l’assistant ?

Un header optionnel absent est simplement omis. L’absence d’un header `mandatory: true`
bloque la connexion avant tout appel distant.

Dans ce cas, le MCP reste visible avec `credentials_required` et `missing_headers`
liste seulement les headers obligatoires manquants. Lorsque les credentials sont
configurés, les déclarations de headers ne sont pas exposées par `tool_search`.
Leurs valeurs privées ne sont jamais renvoyées dans ses résultats.

## Comprendre les connexions OAuth

Il existe deux connexions indépendantes :

- **Au catalogue**, si son administrateur a activé `catalogueAuth` : elle détermine
  les définitions visibles pour votre identité.
- **À chaque MCP OAuth** : elle autorise l’accès au service distant avec votre compte.

Un catalogue public peut donc contenir un MCP OAuth. Une inscription locale peut
également utiliser OAuth sans authentification du catalogue central.

La connexion au catalogue démarre automatiquement au premier appel d’outil qui
la nécessite. Pour un MCP distant, l’assistant utilise l’action de connexion renvoyée
par la recherche. Les actions techniques sont `<id>.__temba_connect__`, appelées via `call_tool`.

Temba attend jusqu’à 10 secondes le résultat d’une autorisation lancée. Si votre
session navigateur permet un SSO rapide, la demande peut se poursuivre immédiatement.
Sinon, l’assistant indique qu’une validation reste attendue dans le navigateur.
La recherche peut aussi attendre brièvement la découverte des outils en cours.

Le rafraîchissement périodique ne rouvre pas le navigateur. Le renouvellement par
refresh token est silencieux ; si une nouvelle autorisation devient nécessaire,
elle attend un prochain appel d’outil. Les appels métier échoués ne sont pas rejoués automatiquement.

Les callbacks OAuth écoutent sur `127.0.0.1`, expirent après dix minutes et sont à usage unique.
Les tokens OAuth ne sont écrits ni sur disque ni dans le trousseau système par Temba.
Une nouvelle connexion peut donc être nécessaire après l’arrêt du processus.

### Lire les états du catalogue

`tool_search` renvoie notamment `catalog.status` :

| État | Signification |
| --- | --- |
| `public` | L’authentification du catalogue est désactivée |
| `auth_in_progress` | Une autorisation navigateur est en cours |
| `loading` | L’authentification a réussi ; les définitions sont en cours de chargement |
| `authenticated` | Le catalogue authentifié est chargé |
| `auth_required` | Une connexion est nécessaire ; elle sera déclenchée lors d’un appel concerné |
| `auth_failed` | La connexion a échoué ou a expiré avant validation |
| `catalog_unavailable` | L’authentification a réussi, mais le chargement a échoué |

La réponse contient aussi `catalog.authentication_enabled`, `catalog.authenticated`
et `catalog.automatic_login`. `catalog_connect_tool` propose une relance seulement
après un échec d’authentification, pas pendant une connexion en cours ou déjà établie.

L’action `<id>.__temba_disconnect__` ferme la connexion au MCP et supprime sa session OAuth.
`__catalog__.__temba_disconnect__` termine la session du catalogue et ferme les connexions,
y compris les inscriptions locales.

## Ajouts locaux éphémères

Cette option, désactivée par défaut, ajoute le tool `add_mcp` :

```toml
[local_catalog]
enabled = true
idle_timeout_seconds = 1800
max_servers = 16
```

| Option | Effet |
| --- | --- |
| `enabled` | Expose ou retire le tool d’ajout dynamique |
| `idle_timeout_seconds` | Expiration de chaque inscription sans usage de son ID : 1 800 secondes par défaut, entre 60 et 86 400 |
| `max_servers` | Nombre maximum d’inscriptions locales simultanées par processus : 16 par défaut, entre 1 et 64 |

### Ajouter un MCP avec un token

Voici les arguments de `add_mcp` :

```json
{
  "definition": {
    "id": "service-temporaire",
    "endpoint": "https://mcp.example.com/mcp",
    "headers": [
      {"name": "Authorization", "credential": "api_token", "mandatory": true}
    ]
  },
  "headers": {"api_token": "Bearer votre-token"}
}
```

Les valeurs sont propres à cette inscription ; elle ne lit pas les headers du TOML.
`add_mcp` renvoie un `local_id` secret de 256 bits et un `connect_tool_id`.

1. Conserver le `local_id` dans la conversation.
2. Chercher les outils avec `tool_search`, en passant `local_ids: [local_id]` et une requête.
3. Si le statut est `auth_required`, appeler `connect_tool_id` via `call_tool`.
4. Dès que le statut est `ready`, utiliser les `tool_id` renvoyés par la recherche.

Sans `local_ids`, les inscriptions locales ne sont pas énumérées.
L’action `<local_id>.__temba_disconnect__` supprime l’inscription.

### Ajouter un MCP OAuth

Déclarez les origines HTTPS des fournisseurs externes dans **la définition locale** :

```json
{
  "definition": {
    "id": "service-temporaire",
    "endpoint": "https://mcp.example.com/mcp",
    "oauth": {
      "allowedOrigins": ["https://identity.example.com"]
    }
  }
}
```

L’origine du MCP est déjà autorisée. `allowedOrigins` autorise les destinations OAuth
supplémentaires ; ce champ n’active pas l’authentification du catalogue Temba.
Il empêche un MCP de choisir un fournisseur non déclaré.

Si une origine a été omise, déconnectez l’inscription puis refaites `add_mcp` avec
la définition corrigée. Aucun changement du catalogue central n’est nécessaire.

### Durée de vie et portée

Chaque recherche ou appel présentant un ID valide renouvelle son délai d’inactivité.
Un ID expiré est immédiatement refusé ; un nettoyage toutes les 10 secondes ferme
les connexions expirées et libère les credentials possédés par le client.
L’arrêt du processus supprime toutes les inscriptions.

Deux ajouts portant le même nom reçoivent des IDs et des connexions distincts.
**La possession de l’ID autorise l’utilisation** dans ce processus : il n’existe pas
d’identité universelle de conversation. Le client IA peut conserver les arguments,
les tokens fournis et les réponses dans son propre historique.

## Mettre à jour et dépanner

Après une modification du TOML ou de l’environnement, redémarrez le processus Temba.
Après une mise à jour du client, changez aussi le chemin du wheel dans votre client IA.
Un wheel remplacé au même chemin peut laisser `uvx` utiliser son ancien binaire en cache.

Avec un serveur IA persistant, redémarrez le serveur qui lance les MCP.
Fermer seulement une conversation ou rouvrir le terminal ne suffit pas toujours.

Les logs vont sur **stderr**, tandis que stdout est réservé au protocole MCP.
Ils indiquent le nom des MCP, leur état et la progression des connexions :

```text
MCP service-a: ready; ready=2/3; pending=[service-b:connecting]
```

| Symptôme | Vérification |
| --- | --- |
| MCP absent | Groupes du token Keycloak, labels locaux et définition du catalogue |
| `credentials_required` | Références `credential`, valeurs non vides et environnement du processus Temba |
| Destination refusée | Égalité exacte de `expected_endpoint` et `endpoint` ; vérifier le changement avant d’approuver |
| Origine OAuth refusée | `oauth.allowedOrigins` dans la bonne définition, centrale ou locale |
| Port de callback déjà occupé | Une autre connexion utilise ce port local ; fermer l’ancien processus ou configurer un autre port |
| Timeout d’un appel | Vérifier le résultat de l’opération distante avant de réessayer une écriture |

Le client relit le catalogue toutes les 60 secondes par défaut. Une suppression ou
un changement de définition ferme l’ancienne connexion. Une réponse invalide
ne remplace pas le dernier catalogue valide.

Pour le modèle de protection mémoire et des tokens, voir [la sécurité du client](../SECURITY_CLIENT.md).
