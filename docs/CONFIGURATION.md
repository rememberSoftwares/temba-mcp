# Options du catalogue

## HTTP sur le poste de développement

HTTPS est requis par défaut. `allowLoopbackHttp` accepte une exception pour une adresse locale :

| URL d'un MCP | Avec `security.allowLoopbackHttp: true` |
|---|---|
| `http://localhost:9000/mcp` | Acceptée |
| `http://127.0.0.1:9000/mcp` | Acceptée |
| `http://mcp.entreprise/mcp` | Refusée |
| `https://mcp.entreprise/mcp` | Acceptée même sans l'option |

L'adresse du MCP est utilisée par le **client sur le poste utilisateur**. Une adresse localhost désigne donc un MCP sur ce poste, pas un service dans Kubernetes.

Cette option ne configure ni l'Ingress ni les certificats de Keycloak. Pour joindre **le catalogue** via un port-forward, c'est le TOML local qui compte :

```toml
catalog_url = "http://127.0.0.1:18080"
allow_loopback_http = true
```

Le `security.allowLoopbackHttp` de la chart peut rester `false` si tous les MCP distants sont en HTTPS.

## Réseau Kubernetes

OpenCode et les appels administratifs passent par le même Ingress vers le catalogue. La policy autorise les pods du namespace du contrôleur, sur TCP 8080 :

```yaml
security:
  networkPolicy:
    enabled: true
    ingress:
      namespace: ingress-nginx
      podSelector: {}  # facultatif : labels des seuls pods du contrôleur
    authCidrs: ["203.0.113.0/24"]  # exemple à remplacer par les CIDR de votre Keycloak
```

Les autres entrées sont refusées par cette policy. Un port-forward Kubernetes n'est pas une connexion ordinaire depuis un autre pod et ne valide pas ce filtrage.

En sortie : DNS vers kube-dns, et HTTPS/TCP 443 vers `authCidrs` seulement si `catalogueAuth.enabled`. Tout le reste est refusé par cette policy. Sans ces exceptions, la validation Keycloak ne fonctionnerait plus. Les MCP sont contactés par le poste utilisateur ; leurs URL n'ont pas à être autorisées dans cette policy.

Le CNI doit appliquer les NetworkPolicies. Les connexions depuis les nœuds, les contrôleurs en hostNetwork et les autres policies nécessitent de vérifier le comportement du cluster. Cette policy filtre des adresses et ports, pas les routes HTTP : le bearer administratif reste nécessaire pour l'API admin.

## Catalogue modifiable et stockage

`adminApi.readOnly: true` signifie consultation seulement. Pour autoriser les ajouts et suppressions :

```yaml
adminApi:
  readOnly: false
  secretName: temba-admin
  tokenKey: token
persistence:
  enabled: true
  size: 100Mi
  storageClass: ""
```

Créer d'abord le Secret Kubernetes contenant le bearer administratif. Les définitions API sont enregistrées sur le PVC et rechargées après redémarrage ; les définitions Helm restent gérées par Helm. Les tokens OAuth et valeurs privées de headers restent sur les postes utilisateurs.

Une seule réplique écrit le fichier. Un échec de sauvegarde renvoie une erreur et ne modifie pas le catalogue en RAM. Un fichier corrompu ou une collision avec un identifiant Helm bloque le démarrage pour éviter une perte silencieuse. Le PVC est conservé à la désinstallation Helm ; penser à le sauvegarder.

Références : [NetworkPolicies](https://kubernetes.io/docs/concepts/services-networking/network-policies/), [volumes persistants](https://kubernetes.io/docs/concepts/storage/persistent-volumes/).
