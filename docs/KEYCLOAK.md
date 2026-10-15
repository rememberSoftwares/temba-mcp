# Réserver des MCP à des groupes Keycloak

Un **groupe** rassemble des utilisateurs. Temba peut réserver un MCP à un groupe, à plusieurs groupes, ou le rendre public. Les noms de groupes sont libres ; ce guide utilise `equipe-a` et `equipe-b` comme exemples.

Ces règles concernent **tous les outils d'un MCP**. Elles contrôlent sa visibilité dans le catalogue ; chaque MCP conserve ses propres permissions et son éventuelle authentification.

## 1. Configurer le client Keycloak

Dans le realm choisi (l'espace contenant vos utilisateurs), ouvrir **Clients → temba**, ou créer un client OpenID Connect nommé `temba`.

- **Client authentication** : Off ; **Standard flow** : On.
- **Require PKCE** : On, méthode **S256** si le choix est proposé.
- **Valid redirect URIs** : `http://127.0.0.1:19877/oauth/callback`.
- Root URL, Home URL, Web origins et les URL de logout peuvent rester vides.

Ce callback revient sur le **poste utilisateur**, pas dans Kubernetes. Adapter le port si `catalogueAuth.callbackPort` change.

## 2. Ajouter les deux mappers

Un **mapper** ajoute des informations au token de connexion envoyé à Temba.

Ouvrir **Clients → temba → Client scopes → temba-dedicated → Mappers → Add mapper → By configuration**. Créer puis sauvegarder :

| Type de mapper | Champs à renseigner |
|---|---|
| **Group Membership** | Name : `temba-groups` ; Token Claim Name : `groups` ; Full group path : **On** ; Add to access token : **On** |
| **Audience** | Name : `temba-audience` ; Included Client Audience : **vide** ; Included Custom Audience : `temba-mcp` ; Add to access token : **On** ; Add to ID token : **Off** |

Les **groupes** indiquent ce que l'utilisateur peut voir. L'**audience** indique que le token est destiné au catalogue Temba. Aucun scope spécial `temba:use` n'est demandé.

Si le scope dédié n'apparaît pas, placer ces mappers dans un client scope attaché à `temba` en **Default**, pour qu'ils soient toujours appliqués.

## 3. Créer les groupes et affecter les utilisateurs

1. **Groups → Create group** : créer `equipe-a` au premier niveau. Son chemin sera `/equipe-a`. Répéter pour `equipe-b` (`/equipe-b`).
2. **Users → utilisateur → Groups → Join group** : sélectionner un ou plusieurs de ces groupes.

Le mapper transmet les appartenances ; il n'ajoute pas les utilisateurs aux groupes.

## 4. Configurer la surcharge Helm

Activer l'authentification du catalogue :

```yaml
catalogueAuth:
  enabled: true
  issuer: https://keycloak.example.com/realms/mon-realm
  clientId: temba
  audience: temba-mcp
  callbackPort: 19877
```

Dans la définition de chaque MCP, choisir **une seule** règle :

| Règle | Qui voit le MCP ? |
|---|---|
| `access: {public: true}` | Tout le monde, sans connexion au catalogue |
| `access: {authenticated: true}` | Tous les utilisateurs connectés au catalogue |
| `access: {groupsAnyOf: ["/equipe-a"]}` | Les membres de ce groupe |
| `access: {groupsAnyOf: ["/equipe-a", "/equipe-b"]}` | Les membres de l’une **ou** l’autre équipe |

Sans `access`, le MCP est public. Les chemins doivent correspondre exactement, casse comprise. Le groupe `/equipe-a/enfant` ne correspond pas automatiquement à `/equipe-a`.

L'authentification propre au MCP (bloc `oauth` et/ou liste `headers`) reste indépendante de ces règles. Désactiver `catalogueAuth.enabled` rend les entrées protégées invisibles à tous.

## 5. Tester

Après déploiement, relancer le client Temba, demander une recherche d’outils à l’agent et terminer la connexion au catalogue dans le navigateur. Un membre de `/equipe-a` voit les MCP réservés à ce groupe ; un utilisateur hors du groupe ne les voit pas. Les labels locaux doivent également autoriser leur affichage.

Si un MCP attendu manque : vérifier l’appartenance au groupe, les labels locaux et les deux mappers. **Client scopes → Evaluate → sélectionner l'utilisateur → Generated access token** doit contenir `groups: ["/equipe-a"]` et une audience `aud` contenant `temba-mcp`.

Un changement de groupe prend effet avec un **nouveau token**, puis le rafraîchissement du catalogue. Redémarrer le client Temba puis demander une recherche d’outils permet de refaire ce test. Si le client LLM utilise un serveur persistant, redémarrer le processus Temba côté serveur ; rouvrir une interface ne suffit pas. La connexion au catalogue ne connecte pas automatiquement aux MCP distants.

Référence : [mappers Keycloak](https://www.keycloak.org/admin-api/protocol-mappers).
