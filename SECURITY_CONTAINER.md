# Sécurité du conteneur catalogue

Le conteneur V2 exécute uniquement `temba-catalog`, un binaire Rust. Il ne contient aucun moteur de proxy MCP et ne manipule aucun token Jira/Grafana. Il traite temporairement les bearers du catalogue pour vérifier les JWT et peut conserver le token administratif fourni par Secret Kubernetes. Le cache JWKS contient des clés publiques.

L’image runtime Distroless contient les bibliothèques système et les certificats CA nécessaires au HTTPS, sans shell, Python, Cargo ou gestionnaire de packages. La chart impose :

- utilisateur et groupe non root `10001` ;
- filesystem racine en lecture seule et catalogue monté en lecture seule ;
- aucune élévation de privilèges, toutes les capabilities retirées ;
- seccomp `RuntimeDefault` ;
- aucun token de ServiceAccount monté ;
- aucune zone de scratch inscriptible et aucun volume de credentials distants ;
- limites de ressources et sondes HTTP.

Le code est embarqué dans l’image. La ConfigMap contient seulement les définitions non secrètes. Les champs inconnus, secrets dans les URLs et anciennes configurations de headers sont rejetés au démarrage ou à l’ajout administratif.

La NetworkPolicy est configurée sous `security.networkPolicy`. Sans authentification du catalogue, seuls les accès DNS sont permis en sortie. Avec Keycloak, une sortie TCP 443 est ajoutée pour récupérer la découverte et les JWKS. Restreindre `authCidrs` aux plages du fournisseur d’identité quand elles sont connues. Le filtrage effectif dépend d’un CNI qui applique les NetworkPolicy ; un Minikube sans ce support ne les applique pas. Le catalogue n’a aucun besoin réseau vers les MCP distants.

La vérification JWT exige RS256, issuer et audience configurés, expiration, `nbf`, sujet et type `Bearer`. Aucun scope personnalisé n’est imposé. La découverte et les overrides d’endpoints Keycloak sont en HTTPS, sans redirection vers un autre endpoint. Le cache JWKS est borné et les rafraîchissements sur un `kid` inconnu sont limités.

Ne pas mettre de secrets de MCP dans les valeurs Helm ou les ConfigMaps. Pour l’API administrative, référencer un Secret préexistant, utiliser un token aléatoire et protéger l’accès réseau aux routes `/v1/admin`. Cette API n’écrit pas sur disque : les ajouts temporaires sont perdus au redémarrage et ne remplacent pas les définitions Helm.

Le filesystem readonly réduit les possibilités de modification persistante, mais ne protège pas une mémoire déjà compromise. Il faut maintenir le runtime et le noyau, limiter l’accès aux pods et au plan de contrôle Kubernetes, et déployer les images validées par digest (`image.digest`). Les core dumps sont désactivés par le binaire sur Unix.

Validation locale possible avec une image construite :

```sh
nerdctl run --rm --read-only --cap-drop=ALL --security-opt=no-new-privileges \
  --user 10001:10001 -p 18080:8080 temba-catalog:1.0.0
```

Le runtime n’écrit aucun fichier pour démarrer ou servir le catalogue. L’authentification navigateur et les callbacks OAuth appartiennent au client local ; aucun callback OAuth n’est exposé par ce conteneur.
