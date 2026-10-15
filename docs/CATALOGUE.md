# Décrire et administrer le catalogue

Le catalogue partagé contient des **définitions de MCP** : leurs identifiants,
leurs adresses, leurs options de connexion et leurs règles de visibilité.
Les appels aux MCP et leurs sessions OAuth sont gérés par le client local.

Pour installer le service, suivez le [guide de déploiement](DEPLOYMENT.md).

## Déclarer un premier MCP

Dans votre fichier de valeurs Helm :

```yaml
catalog:
  servers:
    - id: documentation
      name: Documentation
      description: Rechercher et consulter la documentation de l’équipe
      endpoint: https://docs-mcp.example.com/mcp
      access:
        public: true
```

Remplacez le domaine d’exemple par un MCP réel. L’adresse doit être joignable
**depuis les postes utilisateurs**, généralement via un Ingress HTTPS.
Une adresse de Service Kubernetes n’est pas une adresse utilisable par ces postes.

| Champ | Rôle |
| --- | --- |
| `id` | Identifiant stable, repris dans le TOML et les `tool_id` |
| `name` | Nom lisible du MCP |
| `description` | Aide l’assistant à identifier les usages du service |
| `endpoint` | URL du transport MCP Streamable HTTP |
| `labels` | Catégories que les utilisateurs peuvent activer localement |
| `timeoutSeconds` | Délai maximal des connexions et appels au MCP, entre 1 et 300 secondes, 60 par défaut |
| `oauth` | Active et configure la connexion OAuth au MCP |
| `headers` | Déclare les headers privés ou publics attendus |
| `access` | Définit qui peut voir le MCP dans le catalogue |

Sans `oauth` ni `headers`, aucune authentification distante n’est configurée.
Sans `access`, l’entrée est publique.

Hors Helm, le fichier chargé par le binaire catalogue utilise le schéma suivant :

```yaml
schemaVersion: 2
servers:
  - id: documentation
    endpoint: https://docs-mcp.example.com/mcp
```

## OAuth et headers : deux options combinables

### Une connexion OAuth

```yaml
catalog:
  servers:
    - id: service
      endpoint: https://mcp.example.com/mcp
      oauth:
        allowedOrigins:
          - https://identity.example.com
```

Le client découvre le fournisseur et utilise le navigateur avec PKCE S256.
L’origine du MCP est autorisée implicitement. Toute origine supplémentaire du
fournisseur OAuth doit être déclarée dans `allowedOrigins`, en HTTPS.

Pour un fournisseur sans enregistrement dynamique de client (DCR), ajoutez :

```yaml
oauth:
  clientId: temba-service
  callbackPort: 19878
  scopes: [read]
  allowedOrigins: [https://identity.example.com]
```

Le `clientId` et les scopes dépendent du fournisseur. Enregistrez auprès de celui-ci
la redirect URI exacte `http://127.0.0.1:19878/oauth/callback`.
Le callback se trouve sur le poste utilisateur. Sans port imposé pour un MCP,
le client choisit un port local disponible.

Cette connexion est indépendante de l’authentification du catalogue Temba.
Les tokens et leur renouvellement restent dans le processus local.

### Un token ou plusieurs headers

```yaml
catalog:
  servers:
    - id: service
      endpoint: https://mcp.example.com/mcp
      headers:
        - name: Authorization
          credential: api_token
          mandatory: true
        - name: X-Tenant
          credential: tenant
        - name: X-Client
          value: temba
```

Chaque header a **exactement une source** :

- `credential` référence une valeur privée que l’utilisateur fournit localement ;
- `value` contient une valeur publique, identique pour tous les clients.

`mandatory` vaut `false` par défaut. Une valeur privée optionnelle absente est omise.
Si une valeur obligatoire manque, le client annonce `credentials_required` avant
de contacter le MCP. Voir [le TOML des credentials](CLIENT.md#fournir-des-tokens-et-des-headers-privés).

Les valeurs publiques sont visibles dans l’API utilisateur et peuvent être persistées.
**N’y placez pas de token ni de clé API.** Les headers privés ne sont pas envoyés au catalogue.

Les noms autorisés sont `Authorization` et les noms `X-*`.
Un header public `value` doit être un `X-*` ; les doublons de noms sont rejetés,
y compris lorsqu’ils diffèrent seulement par la casse.

### OAuth avec des headers supplémentaires

Un même MCP peut déclarer `oauth` et plusieurs headers `X-*`.
OAuth fournit déjà `Authorization` : il ne peut donc pas être combiné avec un
second header `Authorization` statique.

Le client envoie les valeurs complètes, sans ajouter de préfixe.
Il ne transmet pas les headers privés aux endpoints de découverte ou d’échange OAuth.

## Rendre un MCP visible pour les bons utilisateurs

Choisissez une règle `access` par entrée :

| Règle | Visibilité |
| --- | --- |
| `public: true` | Tout le monde, même sans connexion au catalogue |
| `authenticated: true` | Tous les utilisateurs authentifiés auprès du catalogue |
| `groupsAnyOf: ["/equipe-a", "/equipe-b"]` | Les membres d’au moins un des groupes |

Les règles portent sur **tous les outils d’un MCP**, pas sur les opérations métier
que le service autorise ensuite. Connaître directement l’adresse d’un MCP ne donne
pas de droit supplémentaire sur ce service.

Les entrées protégées nécessitent `catalogueAuth.enabled: true`.
Si cette option est désactivée, seules les entrées publiques sont visibles.
Le [tutoriel Keycloak](KEYCLOAK.md) décrit les groupes, les mappers et l’audience.

Les labels sont indépendants des groupes : même public, un MCP portant un label
doit être activé dans la [sélection locale](CLIENT.md#choisir-les-mcp-par-labels).

Si la découverte Keycloak annonce des endpoints incorrects, les valeurs Helm
`catalogueAuth.jwksUri` et `catalogueAuth.tokenEndpoint` permettent de les préciser.
Ces URLs doivent rester en HTTPS et sur la même origine que `catalogueAuth.issuer`.
`jwksUri` sert à la vérification des signatures côté catalogue ; `tokenEndpoint`
est communiqué au client natif pour l’échange du code et le renouvellement.


## Modifier le catalogue partagé

Il existe deux sources de définitions :

| Source | Modification | Conservation |
| --- | --- | --- |
| `catalog.servers` dans les valeurs Helm | Mise à jour Helm | Fichier de valeurs / configuration déployée |
| API administrative | Ajout ou suppression par bearer administratif | Fichier sur volume persistant |

Les définitions Helm restent gérées par Helm : l’API ne peut ni les remplacer ni les supprimer.
Les identifiants doivent être uniques entre les deux sources.

### Activer l’API administrative

Par défaut, `adminApi.secretName` est vide : l’API administrative n’autorise aucun accès.
Créez un Secret Kubernetes contenant un token administratif aléatoire, puis référencez-le :

```yaml
adminApi:
  secretName: temba-admin
  tokenKey: token
  readOnly: false
persistence:
  enabled: true
  size: 100Mi
```

Le Secret doit exister dans le namespace du catalogue. La chart ne le génère pas.
`readOnly: true` permet seulement de consulter les définitions ; `false` permet
les ajouts et suppressions, avec une seule réplique et un volume persistant.

Pour le stockage, les NetworkPolicies et la protection réseau de cette API, voir
[les options du catalogue](CONFIGURATION.md).

### Ajouter une définition par API

Créez un fichier `server.json` :

```json
{
  "id": "service",
  "name": "Service partagé",
  "endpoint": "https://mcp.example.com/mcp",
  "access": {"public": true}
}
```

Puis envoyez-le avec le bearer administratif fourni par votre opérateur :

```sh
curl --fail-with-body \
  -H "Authorization: Bearer ${TEMBA_ADMIN_TOKEN}" \
  -H 'Content-Type: application/json' \
  --data @server.json \
  https://catalogue.example.com/v1/admin/servers
```

Ces ajouts sont partagés selon leur règle `access`. Pour un besoin personnel et
éphémère, utilisez plutôt [le tool local `add_mcp`](CLIENT.md#ajouts-locaux-éphémères).

### Que se passe-t-il au redémarrage ?

Les ajouts API sont sauvegardés dans `/var/lib/temba/additions.json`, puis chargés au démarrage.
L’API confirme une modification après sauvegarde et synchronisation du fichier.
Un échec d’écriture ne publie pas la modification en RAM.

Un fichier invalide ou une collision avec une définition Helm bloque le démarrage
pour éviter une perte silencieuse. Sauvegardez le PVC : il est conservé lors d’une
désinstallation Helm, et sa suppression définitive reste une action d’administration.

Le volume ne contient pas les tokens OAuth ni les headers privés des utilisateurs.

## Routes exposées

| Route | Accès et rôle |
| --- | --- |
| `GET /healthz`, `GET /readyz` | Sondes de vie et de disponibilité |
| `GET /v1/bootstrap` | Paramètres OAuth publics et non secrets du catalogue |
| `GET /v1/catalog` | API utilisateur : définitions filtrées, ETag et cache privé |
| `GET /v1/admin/servers` | Bearer administratif : toutes les définitions |
| `POST /v1/admin/servers` | Ajout persistant si l’écriture est activée |
| `DELETE /v1/admin/servers/{id}` | Suppression persistante d’un ajout API |

`GET /v1/catalog` expose les déclarations `headers`, y compris `mandatory`,
`credential` ou `value`, aux clients autorisés. Le client local en déduit les besoins
de configuration sans exposer inutilement ces déclarations au LLM.

Le serveur vérifie les access tokens du catalogue : signature RS256, issuer,
audience, expiration, `nbf`, sujet et type `Bearer`. Aucun scope personnalisé n’est imposé.
