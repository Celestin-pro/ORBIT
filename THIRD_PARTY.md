# Dépendances et licences

Licence prévue pour `orbit-player` : **MIT** (à confirmer).

Objectif : rester publiable sous licence permissive. Aucune dépendance sous GPL, et FFmpeg
uniquement en build **LGPL** lié dynamiquement (règle 4 de CLAUDE.md).

## Bibliothèques natives

| Composant | Version | Licence | Lien | Remarques |
|---|---|---|---|---|
| SDL3 | 3.4.20 | Zlib | dynamique (`SDL3.dll`) | release officielle « VC », téléchargée par `scripts/setup.ps1` dans `third_party/sdl3` |
| FFmpeg (libavformat, libavcodec, libavutil) | n8.1.3 LGPL *shared* | LGPL-2.1-or-later | dynamique (DLL) | builds `lgpl-shared` de [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds) ; `setup.ps1` refuse tout build `--enable-gpl` et vérifie la présence du protocole SRT |
| Direct3D 11 / DXGI | système | — | système | fourni par Windows |

Le lien dynamique vers FFmpeg LGPL est ce qui autorise une licence permissive côté
`orbit-player` : les DLL sont distribuées telles quelles, remplaçables par l'utilisateur, et
aucun code FFmpeg n'est incorporé dans l'exécutable.

## Crates Rust (dépendances directes)

| Crate | Version | Licence |
|---|---|---|
| `anyhow` | 1.0 | MIT OR Apache-2.0 |
| `clap` | 4.6 | MIT OR Apache-2.0 |
| `env_logger` | 0.11 | MIT OR Apache-2.0 |
| `ffmpeg-sys-next` | 8.1 | WTFPL OR MIT (bindings ; FFmpeg lui-même reste LGPL) |
| `log` | 0.4 | MIT OR Apache-2.0 |
| `sdl3` (+ `sdl3-sys`) | 0.22 / 0.7 | MIT OR Apache-2.0 (bindings) ; SDL3 lui-même en Zlib |
| `thiserror` | 2.0 | MIT OR Apache-2.0 |
| `windows` | 0.62 | MIT OR Apache-2.0 |

`crossbeam-channel` est annoncé dans CLAUDE.md mais pas encore nécessaire : la boîte aux
lettres vidéo est un `Mutex` + `Condvar`, et le thread vidéo se pilote par un `AtomicBool`
partagé avec le rappel d'interruption de FFmpeg. Il sera ajouté quand un canal servira
réellement (probablement pour le son).

## Vérification

```powershell
cargo tree --edges normal        # arbre des dépendances
cargo install cargo-deny         # une fois
cargo deny check licenses        # contrôle automatique (à mettre en place)
```

`scripts/setup.ps1` vérifie en plus, à chaque exécution, que le build FFmpeg téléchargé
n'a pas été compilé avec `--enable-gpl`.
