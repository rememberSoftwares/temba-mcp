# Sécurité du client local

Le processus `temba-client` tourne sur le poste utilisateur. Il reçoit des définitions non secrètes du catalogue et contacte directement les MCP. Le catalogue, l’agent LLM et les résultats des outils ne reçoivent ni access token, ni refresh token, ni clé permettant de récupérer une session distante.

Les sessions OAuth vivent uniquement dans des objets du processus local. Aucun token, code d’autorisation ou vérificateur PKCE n’est écrit dans un fichier ou un trousseau. Le cache de `uvx` contient le binaire distribué, pas les credentials. Une relance du client nécessite une nouvelle authentification.

Le flux OAuth utilise Authorization Code avec PKCE S256, un état aléatoire, un callback loopback et un délai de dix minutes. Le listener est créé avant l’ouverture du navigateur. L’état est vérifié avant consommation du callback ; un callback incorrect ne termine pas le flux attendu. Un callback correct est consommé une seule fois. Un issuer présent dans le callback doit correspondre à celui de la découverte ; il est obligatoire lorsque le fournisseur annonce le support de ce paramètre. La découverte exige un issuer cohérent et des endpoints dont les origines sont approuvées par le catalogue.

Chaque session dispose de son propre verrou de refresh. Le refresh token tournant remplace l’ancien ; l’absence d’un nouveau refresh token conserve le précédent. Un rejet `invalid_grant` supprime les credentials. La marge de renouvellement vaut le minimum de 60 secondes et 10 % de la durée de vie du token. Le transport récupère le token courant pour chaque requête, y compris la reprise d’un stream, sans conserver un ancien bearer dans sa configuration.

Le transport vérifie la destination exacte avant de joindre un token OAuth. TLS 1.3 reste activé, avec des groupes ECDHE classiques (X25519, P-256, P-384) pour éviter les resets constatés sur certains endpoints Atlassian avec les groupes hybrides ML-KEM. La validation des certificats reste active. Les clients HTTP ne suivent pas les redirections et n’utilisent pas implicitement les variables de proxy. Un token statique provient seulement d’une variable d’environnement choisie dans la configuration locale et d’une URL exacte approuvée localement. Le catalogue ne peut pas choisir cette variable ou modifier l’association.

Les appels métier ont un timeout et ne sont pas rejoués automatiquement : après un timeout, le serveur peut avoir exécuté l’opération. L’utilisateur doit vérifier son résultat avant de relancer une écriture.

La suppression d’un serveur, son changement de configuration, une déconnexion ou la fin du processus ferme les connexions concernées. Les buffers possédés par le client pour les secrets utilisent `Zeroizing`; les core dumps sont désactivés sur Unix. **Cela ne garantit pas l’effacement de toutes les copies** : parsing JSON, bibliothèques HTTP, TLS, allocations temporaires et variables d’environnement peuvent en conserver. Rust n’est pas une garantie d’effacement intégral de la RAM.

Le poste reste dans le périmètre de confiance. Un programme capable de lire la mémoire du client ou de s’exécuter sous le compte utilisateur peut exploiter ses credentials. La RAM peut également être paginée ; la protection du swap relève de l’OS. Un token statique fourni par variable d’environnement a la durée de vie de cet environnement, indépendamment de la déconnexion MCP.

Les définitions et les outils distants sont des données non fiables. Le filtre `labels` est une préférence locale, pas une ACL. La V2 ne permet pas au LLM d’ajouter des destinations arbitraires ; les origines OAuth supplémentaires doivent être approuvées par l’administrateur.

Les logs utilisent stderr et indiquent les noms et états des connexions. Aucun subscriber de tracing SDK n’est installé, pour éviter les dumps de requêtes et de secrets. Ne pas partager un dump mémoire ou un environnement contenant des tokens.
