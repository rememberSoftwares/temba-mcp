# Test Minikube : groupes et tokens

Les groupes Keycloak sont créés à la racine : `dev` et `pm`. Le mapper `groups`
doit inclure leur chemin complet dans l’access token (`/dev`, `/pm`). Voir
[la configuration Keycloak](KEYCLOAK.md).

| Groupe dans le token | MCP visibles | Label |
| --- | --- | --- |
| Tous, même sans connexion | DeepWiki | `tech` |
| `/dev` | GitLab par token | — |
| `/dev` | Jira par token | `support` |
| `/pm` | Jira OAuth Atlassian | — |

Avec les deux groupes, les quatre MCP sont visibles. Après un changement de
groupe, reconnecter le catalogue pour obtenir un nouveau token Keycloak ;
redémarrer OpenCode relance cette connexion. Un token déjà délivré conserve
ses anciens groupes jusqu’à son renouvellement.

## Déploiements locaux

Les copies dans `deploy/minikube-test/` ne modifient pas les exemples des dépôts
Jira et GitLab. Elles utilisent les images construites depuis leurs sources,
chargées dans Minikube (`*-mcp:temba-test-20261013`), car le registre privé était
injoignable. Ces images doivent être présentes avant de réappliquer les copies :

```sh
kubectl --context minikube apply -f deploy/minikube-test/
```

Les paramètres fixes sont dans l’ENV des pods :

- Jira : `JIRA_URL=https://numsproduct.atlassian.net` et
  `JIRA_EMAIL=emilien.lancelot@numspot.com`.
- GitLab : `GITLAB_PROJECTS=https://gitlab.tooling.cloudgouv-eu-west-1.numspot.internal/cloud-compute/eclused-compute`
  et `GITLAB_VERIFY_SSL=false` pour ce test.

Aucun token n’est défini dans ces manifests : chaque client fournit son token
par header. Pour Jira, ce token doit correspondre au compte email configuré.

## Accès depuis le poste

Pour ce test local, maintenir ces trois port-forwards ouverts, chacun dans son
terminal (ou via k9s) :

```sh
kubectl --context minikube port-forward service/temba 18080:8080
kubectl --context minikube -n agent port-forward service/jira-mcp 18081:8000
kubectl --context minikube -n agent port-forward service/gitlab-mcp 18082:8000
```

En dehors de ce test, remplacer les endpoints loopback par les ingress HTTPS
accessibles depuis les postes clients.

## Configuration du client

Le fichier local `temba.toml` contient uniquement les références des tokens :

```toml
[tool.jira-token]
expected_endpoint = "http://127.0.0.1:18081/mcp"
[tool.jira-token.headers]
token = { env = "JIRA_TOKEN" }

[tool.gitlab]
expected_endpoint = "http://127.0.0.1:18082/mcp"
[tool.gitlab.headers]
token = { env = "GITLAB_TOKEN" }
```

Définir ces deux variables dans l’environnement qui lance OpenCode. Les valeurs
sont les tokens bruts, sans préfixe `Bearer `. Une valeur littérale locale dans
le TOML fonctionne aussi. `expected_endpoint` limite la destination autorisée
pour l’envoi du token.

Les labels sont un filtre local, distinct des permissions Keycloak : `labels = []`
inclut uniquement les MCP sans label (GitLab pour `/dev`, Jira OAuth pour `/pm`).
`labels = ["support"]` ajoute Jira par token ; `labels = ["tech"]` ajoute DeepWiki.
Pour inclure les deux, utiliser `labels = ["support", "tech"]`. Les MCP sans
label restent visibles. Si un MCP porte plusieurs labels, tous doivent être
activés dans le TOML. Même public, DeepWiki nécessite donc le label `tech`
pour être inclus par le client local.

Sans token métier, un MCP autorisé reste annoncé avec le statut
`credentials_required`. Les appels Jira/GitLab nécessitent ensuite un token valide.
