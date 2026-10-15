# Développer et distribuer Temba

## Structure du dépôt

```text
.
├── crates/
│   ├── temba-client/      # MCP stdio, recherche locale et sessions OAuth
│   ├── temba-catalog/     # API REST, contrôle d’accès et stockage des définitions
│   └── temba-core/        # Schémas et validations partagés
├── chart/temba-catalog/   # Déploiement Kubernetes avec Helm
├── docs/                 # Guides utilisateurs et administrateurs
├── .github/workflows/    # Vérifications et construction des wheels
├── Dockerfile            # Image du catalogue central
├── catalog.yaml          # Exemple pour le binaire catalogue hors Helm
└── temba.example.toml    # Configuration de départ du client local
```

Le workspace requiert **Rust 1.99 ou supérieur**, conformément à `Cargo.toml`.
Helm permet de vérifier la chart ; `uv` fournit le lanceur `uvx` utilisé pour la distribution.

## Vérifier les changements

Depuis la racine du dépôt :

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
helm lint chart/temba-catalog
```

La recherche d’outils est locale, avec un classement BM25.
Le protocole MCP utilise stdout ; les diagnostics du client doivent rester sur stderr.

## Construire le client

```sh
uvx --from 'maturin>=1.9,<2' maturin build --release --locked --out dist
```

Le résultat est un wheel dans `dist/`, contenant le binaire Rust natif.
Le backend est configuré dans [pyproject.toml](../pyproject.toml) avec `bindings = "bin"`.
Python sert au packaging et au lanceur ; l’exécution MCP est assurée par le binaire Rust.

La [CI](../.github/workflows/ci.yaml) construit actuellement :

| Système | Architecture |
| --- | --- |
| Linux | x86_64 |
| macOS | Apple Silicon et Intel |
| Windows | x86_64 |

Elle vérifie aussi le lancement des wheels avec `uvx`, puis les conserve comme artefacts.
Les autres architectures nécessitent une cible de build et un wheel supplémentaires.
La publication sur un registre n’est pas configurée.

## Livrer une nouvelle version

Le client et le catalogue peuvent avoir des versions différentes.
Pour le client, garder cohérents la version de `crates/temba-client/Cargo.toml`,
`pyproject.toml` et `Cargo.lock`.

Chaque livraison doit produire un wheel portant un **nouveau nom et une nouvelle version**.
Écraser un wheel au même chemin peut laisser `uvx` réutiliser son ancien binaire en cache.
Les utilisateurs doivent mettre à jour le chemin dans leur client IA, puis redémarrer
le processus Temba réellement utilisé, y compris derrière un serveur persistant.

Pour le catalogue, le [guide de déploiement](DEPLOYMENT.md) décrit la construction de
l’image et son chargement dans Minikube. La configuration de la CI ne publie pas d’image catalogue.

## Sécurité et évolutions

Les limites de protection de la mémoire et le comportement des credentials sont décrits
dans [SECURITY_CLIENT.md](../SECURITY_CLIENT.md). Le runtime du catalogue et la chart sont
décrits dans [SECURITY_CONTAINER.md](../SECURITY_CONTAINER.md).

La [spécification V2](../SPECIFICATION_TEMBA_V2.md) contient aussi des évolutions envisagées.
Pour documenter une fonctionnalité disponible, vérifier d’abord le code et les guides actuels.
