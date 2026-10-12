# Sécurité du conteneur catalogue

Le conteneur V2 exécute uniquement `temba-catalog`, un binaire Rust. Il ne contient aucun moteur de proxy MCP et ne manipule aucun token Jira/Grafana. Il traite temporairement les bearers du catalogue pour vérifier les JWT et peut conserver le token administratif fourni par Secret Kubernetes. Le cache JWKS contient des clés publiques.

L’image runtime Distroless contient les bibliothèques système et les certificats CA nécessaires au HTTPS, sans shell, Python, Cargo ou gestionnaire de packages. La chart impose :

- utilisateur et groupe non root `10001` ;
- filesystem racine en lecture seule et catalogue monté en lecture seule ;
- aucune élévation de privilèges, toutes les capabilities retirées ;
- seccomp `RuntimeDefault` ;
- aucun token de ServiceAccount monté ;
- seul le volume des définitions API peut être inscriptible, lorsque `adminApi.readOnly: false` ; aucun credential distant sur ce volume ;
- limites de ressources et sondes HTTP.

Le code est embarqué dans l’image. La ConfigMap contient seulement les définitions non secrètes. Les champs inconnus, secrets dans les URLs et anciennes configurations de headers sont rejetés au démarrage ou à l’ajout administratif.

La NetworkPolicy sous `security.networkPolicy` refuse les entrées et sorties par défaut. Seuls les pods du namespace du contrôleur Ingress configuré peuvent joindre TCP 8080 ; `ingress.podSelector` permet de sélectionner précisément ce contrôleur. Les sorties DNS vers kube-dns restent autorisées. Avec `catalogueAuth.enabled`, TCP 443 vers `authCidrs` permet la découverte et les JWKS Keycloak. Restreindre ces CIDR en production. Le catalogue ne contacte jamais les MCP distants. L'application nécessite un CNI compatible ; les flux depuis le nœud et certains contrôleurs en hostNetwork ont un comportement propre au CNI. Les autres NetworkPolicies peuvent ajouter des autorisations. Voir [Kubernetes](https://kubernetes.io/docs/concepts/services-networking/network-policies/).

Les définitions API persistent sur un PVC de 100 Mi par défaut. Le système de fichiers racine reste en lecture seule. Les ajouts sont synchronisés et remplacés atomiquement avant publication ; un seul écrivain est autorisé et les mises à jour utilisent Recreate. Le fichier persistant est validé au démarrage et ne contient pas de valeurs privées de headers, tokens OAuth ni secrets. Sauvegarder le PVC ; il est conservé par Helm à la désinstallation.


La vérification JWT exige RS256, issuer et audience configurés, expiration, `nbf`, sujet et type `Bearer`. Aucun scope personnalisé n’est imposé. La découverte et les overrides d’endpoints Keycloak sont en HTTPS, sans redirection vers un autre endpoint. Le cache JWKS est borné et les rafraîchissements sur un `kid` inconnu sont limités.

Ne pas mettre de secrets de MCP dans les valeurs Helm ou les ConfigMaps. Pour l’API administrative, référencer un Secret préexistant, utiliser un token aléatoire et protéger l’accès réseau aux routes `/v1/admin`. Cette API n’écrit pas sur disque : les ajouts temporaires sont perdus au redémarrage et ne remplacent pas les définitions Helm.

Le filesystem readonly réduit les possibilités de modification persistante, mais ne protège pas une mémoire déjà compromise. Il faut maintenir le runtime et le noyau, limiter l’accès aux pods et au plan de contrôle Kubernetes, et déployer les images validées par digest (`image.digest`). Les core dumps sont désactivés par le binaire sur Unix.

Validation locale possible avec une image construite :

```sh
nerdctl run --rm --read-only --cap-drop=ALL --security-opt=no-new-privileges \
  --user 10001:10001 -p 18080:8080 temba-catalog:1.3.1
```

Le runtime n’écrit aucun fichier pour démarrer ou servir le catalogue. L’authentification navigateur et les callbacks OAuth appartiennent au client local ; aucun callback OAuth n’est exposé par ce conteneur.
