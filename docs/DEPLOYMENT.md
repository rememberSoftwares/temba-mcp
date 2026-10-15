# Déployer le catalogue

Ce guide installe **le catalogue REST central**. Les utilisateurs exécutent ensuite
[le client local](../README.md#premiers-pas) sur leur poste.
Les MCP distants doivent déjà exister et être accessibles depuis ces postes.

Le catalogue utilise une image conteneur et la chart [temba-catalog](../chart/temba-catalog).
Les versions du catalogue et du client évoluent indépendamment : actuellement,
le catalogue est en **1.3.1** et le client en **1.4.8**.

## Prérequis

Pour le parcours Minikube :

- un Minikube démarré et un contexte `kubectl` nommé `minikube` ;
- Helm et `nerdctl` ;
- les sources du dépôt et l’accès aux images de base utilisées par le Dockerfile.

Pour un autre cluster, construisez et publiez l’image dans votre registre habituel,
puis renseignez `image.repository`, `image.tag` ou `image.digest` dans les valeurs Helm.
Adaptez également le contexte et le namespace des commandes.

## 1. Construire l’image

Depuis la racine du dépôt :

```sh
nerdctl build -t temba-catalog:1.3.1 .
```

Dans Minikube, chargez explicitement une archive de l’image :

```sh
nerdctl save -o /tmp/temba-catalog-1.3.1.tar temba-catalog:1.3.1
minikube image load /tmp/temba-catalog-1.3.1.tar
```

L’image embarque le binaire Rust. La ConfigMap créée par Helm contient uniquement
les définitions de MCP, pas le code ni des tokens de services distants.

## 2. Décrire vos MCP

Créez un fichier `values.local.yaml` :

```yaml
image:
  repository: temba-catalog
  tag: "1.3.1"

catalogueAuth:
  enabled: false

catalog:
  servers:
    - id: documentation
      name: Documentation
      description: Rechercher et consulter la documentation de l’équipe
      endpoint: https://docs-mcp.example.com/mcp
      access:
        public: true
```

Remplacez l’endpoint d’exemple par l’adresse HTTPS d’un MCP réel, joignable depuis
les postes utilisateurs. L’authentification du catalogue est ici désactivée ;
vous pourrez activer les groupes ensuite avec le [guide Keycloak](KEYCLOAK.md).

Les valeurs par défaut sont dans [values.yaml](../chart/temba-catalog/values.yaml).
Le [guide du catalogue](CATALOGUE.md) détaille OAuth, headers et règles de visibilité.
La surcharge `overide.yaml` du dépôt appartient à l’environnement de test ; ne la
reprenez pas telle quelle pour une nouvelle installation.

## 3. Installer la release

```sh
helm upgrade --install temba chart/temba-catalog \
  --kube-context minikube \
  --namespace temba --create-namespace \
  -f values.local.yaml
```

La chart utilise un PVC par défaut. Vérifiez que le cluster dispose d’une StorageClass
adaptée, ou renseignez `persistence.storageClass` / `persistence.existingClaim`.

Vérifiez le déploiement :

```sh
kubectl --context minikube -n temba rollout status deployment/temba
kubectl --context minikube -n temba logs deployment/temba
```

L’API administrative reste désactivée tant qu’aucun Secret n’est référencé.
Vous pouvez commencer avec les définitions Helm seules.

## 4. Joindre le catalogue en développement

Gardez ce port-forward ouvert :

```sh
kubectl --context minikube -n temba port-forward service/temba 18080:8080
```

Dans le TOML du poste utilisateur :

```toml
catalog_url = "http://127.0.0.1:18080"
allow_loopback_http = true
```

Pour vérifier les définitions publiques :

```sh
curl --fail http://127.0.0.1:18080/v1/catalog
```

Le flag du TOML suffit pour joindre le catalogue via ce port-forward.
`security.allowLoopbackHttp` dans la chart concerne les **endpoints de MCP déclarés** :
activez-le seulement si ces définitions utilisent elles aussi du HTTP local.
Il n’autorise jamais un endpoint HTTP distant arbitraire.

## Exposer le catalogue en HTTPS

Pour une installation partagée, exposez le catalogue par un Ingress HTTPS :

```yaml
ingress:
  enabled: true
  className: nginx
  host: catalogue.example.com
  tls:
    - secretName: temba-tls
      hosts: [catalogue.example.com]

security:
  networkPolicy:
    ingress:
      namespace: ingress-nginx
```

Les noms du contrôleur, du namespace, du domaine et du Secret TLS sont à adapter
à votre cluster. Le Secret TLS doit être fourni par votre gestion habituelle des certificats.
L’Ingress de la chart expose `/v1`, utilisé par l’API utilisateur et l’API admin.

Communiquez ensuite `https://catalogue.example.com` aux utilisateurs comme `catalog_url`.
Chaque MCP distant doit, lui aussi, être accessible depuis leurs postes.
Le catalogue ne proxifie pas leurs appels.

Les NetworkPolicies sont activées par défaut : autorisez le namespace de votre
contrôleur Ingress et, si Keycloak est activé, ses plages d’adresses de sortie.
Voir [les flux autorisés](CONFIGURATION.md#réseau-kubernetes) et le
[durcissement du conteneur](../SECURITY_CONTAINER.md).

## Mises à jour et stockage

Les ajouts administratifs sont persistés sur le PVC ; les définitions Helm restent
dans vos valeurs. Le PVC est conservé lors d’une désinstallation Helm.

Avec le volume, le Deployment utilise **Recreate** : prévoyez une courte interruption
pendant la mise à jour. Un seul processus peut écrire les ajouts persistants.
Ne modifiez pas des définitions Helm pour réutiliser l’identifiant d’un ajout API existant.

### Migrer une ancienne configuration

Les anciennes clés sont rejetées. Pour mettre à jour une installation issue des premières versions :

| Ancien réglage | Réglage actuel |
| --- | --- |
| `auth` global | `catalogueAuth` |
| `catalogAccess` | Une règle `access` |
| `adminApi.mutable` | `adminApi.readOnly`, avec la logique inversée |
| `auth.type` sur un MCP | `oauth` et/ou `headers` |
| `url` sur une définition MCP | `endpoint` |
| `headers[].prefix` | Valeur complète fournie côté client |
| `[credentials.<id>]` dans le TOML | `[tool.<id>]` et `[tool.<id>.headers]` |
| Référence ENV implicite | Objet explicite `{ env = "VARIABLE" }` |

Le format du catalogue est `schemaVersion: 2`. La gateway Python, Redis, les enveloppes
de tokens et les anciennes routes MCP HTTP ne font plus partie de l’architecture.
Le client actuel est un MCP local stdio ; il n’y a pas de couche de compatibilité V1.

Les callbacks historiques `/session/callback` et `/oidc/callback` sont remplacés par
le callback natif sur le poste, décrit dans le [guide Keycloak](KEYCLOAK.md).

Lors de la migration d’un ancien Deployment vers `Recreate`, si Helm conserve
la section `rollingUpdate`, retirez-la une fois avant de relancer la mise à jour :

```sh
kubectl --context minikube -n temba patch deployment temba \
  --type=merge \
  -p '{"spec":{"strategy":{"type":"Recreate","rollingUpdate":null}}}'
```

Adaptez le contexte, le namespace et le nom du Deployment à votre installation.
