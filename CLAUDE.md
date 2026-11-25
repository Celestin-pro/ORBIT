# orbit-player – récepteur de streaming d'ORBIT

Ce dépôt contient `orbit-player.exe`, le **récepteur côté PC portable** du projet de cloud gaming
maison ORBIT. Il remplace mpv (vidéo), `input_sender.py` (entrées) et `audio_receiver.py` (son)
par **un seul programme Rust, une seule fenêtre**.

Le reste du système existe déjà et fonctionne : ne pas le réécrire ici.

```
PC FIXE (hôte)                                   PC PORTABLE (client)
agent.py (FastAPI, HTTPS)  <-- API -------------  ORBIT (Tauri + React) + bridge.py
  ffmpeg ddagrab -> h264_nvenc -> MPEG-TS  ----->  orbit-player.exe   (CE DÉPÔT)
    UDP ou SRT, port 5000                              vidéo
  input_receiver.py (vgamepad + SendInput) <-----      entrées  (UDP 5001)
  audio_sender.py (WASAPI loopback)        ----->      son      (UDP 5002)
```

Le pont (`bridge.py`) gère l'agent (démarrage/arrêt de session, heartbeat) et **lance
`orbit-player.exe`** avec les bons arguments. Le player ne parle jamais à l'API de l'agent
(jusqu'à nouvel ordre) : il reçoit, affiche, joue, envoie les entrées, et se ferme quand
l'utilisateur ferme la fenêtre.

---

## Règles non négociables

1. **Aucune copie GPU → CPU de l'image.** Le décodage se fait en D3D11VA ; l'image décodée reste
   une texture D3D11 jusqu'à l'écran. Une copie GPU → GPU (`CopySubresourceRegion`) est acceptée.
   Toute lecture de pixels côté CPU (`av_hwframe_transfer_data`, `Map` sur la texture décodée...)
   est interdite dans le chemin vidéo.
2. **Un seul `ID3D11Device`**, créé par le player, partagé entre FFmpeg (décodage) et le rendu.
   Activer la protection multithread (`ID3D10Multithread::SetMultithreadProtected(TRUE)`), ou
   utiliser le verrou de `AVD3D11VADeviceContext`.
3. **Toujours afficher l'image la plus récente.** Entre le décodeur et le rendu : une « boîte aux
   lettres » d'une seule place (la nouvelle image remplace l'ancienne non affichée). Jamais de file
   d'attente qui grossit.
4. **FFmpeg LGPL uniquement, en DLL** (builds `lgpl-shared` de BtbN). Aucune dépendance GPL
   (pas de x264/x265, pas de mpv, pas de libav* compilé en `--enable-gpl`). Le projet doit pouvoir
   être publié sous une licence permissive.
5. **`unsafe` confiné** aux modules FFI (`decoder/`, `render/`) avec un commentaire `// SAFETY:`
   sur chaque bloc. Le reste du code est en Rust sûr.
6. **Pas de `panic!` / `unwrap()` dans les boucles temps réel** (réception, décodage, rendu, son,
   entrées). Les erreurs remontent, sont journalisées, et le player continue ou se ferme proprement.
7. **Compatibilité réseau totale** avec l'existant : les formats de paquets ci-dessous sont figés.
   Tout changement de protocole se fait aussi côté hôte, dans un autre dépôt, et doit être demandé.
8. Code et commentaires **en français** (noms d'identifiants en anglais). Le propriétaire du projet
   doit pouvoir lire et étendre le code : préférer la clarté à l'astuce.

---

## Stack

| Rôle | Crate |
|---|---|
| Fenêtre, clavier/souris (mode relatif), manette, son | `sdl3` (feature `build-from-source` ou SDL3 précompilé) |
| Direct3D 11 / DXGI | `windows` (crate officielle Microsoft) |
| Réception SRT/UDP, démultiplexage MPEG-TS, décodage | `ffmpeg-sys-next` 8.x (bindings bruts) liés à FFmpeg 8.1 LGPL |
| Canaux entre threads | `crossbeam-channel` |
| Arguments | `clap` (derive) |
| Journalisation | `log` + `env_logger` |
| Erreurs | `anyhow` (binaire) / `thiserror` (modules) |

Prérequis de compilation (Windows) :
- Rust stable MSVC, Visual Studio Build Tools (C++).
- LLVM/Clang (`winget install LLVM.LLVM`) : requis par `bindgen` pour `ffmpeg-sys-next`.
- FFmpeg LGPL shared : `ffmpeg-n8.1-latest-win64-lgpl-shared-8.1.zip` (github.com/BtbN/FFmpeg-Builds),
  décompressé dans `third_party/ffmpeg`, variable `FFMPEG_DIR` pointant dessus.
  La version majeure doit correspondre à celle de `ffmpeg-sys-next` (8.x <-> FFmpeg 8.1).
  La branche n7.x n'est plus publiée par BtbN : 8.1 est la plus ancienne encore disponible.
  Le build doit fournir le protocole **SRT** (`ffmpeg -protocols`) ; s'il manque, on reste en
  UDP — **on ne prend jamais un build GPL** pour obtenir SRT.
  Le script `scripts/setup.ps1` doit faire ce téléchargement + vérifier les prérequis.
- Les DLL FFmpeg (et SDL3 si dynamique) sont copiées à côté de l'exe par `build.rs` ou le script.

---

## Architecture

### FFmpeg est remplaçable (contrainte structurante)

À terme, FFmpeg sera retiré du chemin réseau, et peut-être du décodage. Le code est donc
organisé autour de deux traits, définis dès la v0.1, et **aucun type FFmpeg ne traverse leur
frontière** : le reste du programme (boîte aux lettres, rendu, stats, entrées, son) ne dépend
jamais de `ffmpeg-sys-next`.

| Trait | Contrat | Implémentation v0.1 | Plus tard |
|---|---|---|---|
| `VideoSource` | fournit des **unités d'accès** H.264/HEVC brutes : octets du paquet compressé, horodatage, drapeau « image clé », plus le codec et la résolution annoncés | `LavfSource` (libavformat : UDP/SRT, démultiplexage MPEG-TS) | `OrbitProtocolSource` (notre protocole UDP maison) |
| `HwDecoder` | consomme ces unités d'accès et rend une **texture D3D11 NV12 sur NOTRE device** (jamais de pixels côté CPU) | `AvcodecD3d11Decoder` (libavcodec + D3D11VA) | `MediaFoundationDecoder` |

Conséquences pratiques :
- `ffmpeg-sys-next` n'est importé que par `video/lavf_source.rs` et `video/avcodec_decoder.rs`.
  Tout autre module qui en aurait besoin est un signe que l'abstraction fuit.
- Les types d'échange (`AccessUnit`, `DecodedFrame`, `StreamInfo`, `VideoCodec`) sont en Rust sûr,
  sans pointeur FFmpeg, et testables unitairement.
- Les erreurs des implémentations sont traduites en erreurs du domaine (`thiserror`) avant de
  sortir : pas de code d'erreur `AVERROR` brut dans le reste du programme.

```
src/
  main.rs            arguments, création fenêtre SDL, démarrage des threads, boucle d'événements
  config.rs          struct Config (issue de clap)
  video/
    source.rs        trait VideoSource + AccessUnit + StreamInfo (aucun type FFmpeg)
    decoder.rs       trait HwDecoder (aucun type FFmpeg)
    lavf_source.rs   impl VideoSource : avformat_open_input(udp/srt) -> av_read_frame -> AccessUnit
    avcodec_decoder.rs  impl HwDecoder : contexte d3d11va sur NOTRE device, get_format -> AV_PIX_FMT_D3D11
    frame.rs         DecodedFrame { slot, taille, pts, t_received, t_decoded } + boîte aux lettres
  render/
    d3d11.rs         device, swapchain flip, SRV NV12 (R8 + R8G8), shader plein écran, présentation
    shaders.hlsl     conversion NV12 -> RGB (BT.709, plage limitée), compilé au build (fxc) ou à l'exécution
  input/
    capture.rs       état manette (SDL gamepad) + clavier (scancodes) + souris (relatif) -> paquets
    sender.rs        thread : envoi 250 Hz vers hôte:5001
  audio/
    receiver.rs      thread : UDP 5002 -> jitter buffer
    output.rs        flux SDL3 audio, callback qui tire dans le jitter buffer
  net/protocol.rs    formats de paquets (identiques à protocol.py), tests unitaires d'aller-retour
  stats.rs           compteurs + overlay (touche F3)
scripts/setup.ps1    prérequis + FFmpeg LGPL
scripts/test-send.ps1  émetteur de test (mire FFmpeg) pour développer sans le PC fixe
```

### Threads

| Thread | Rôle |
|---|---|
| principal | événements SDL (fenêtre, clavier, souris, manette), rendu + présentation |
| vidéo | réception + décodage ; dépose la dernière image dans la boîte aux lettres |
| entrées | lit l'état partagé (mis à jour par le thread principal), envoie à 250 Hz |
| son | réception UDP -> jitter buffer (le callback SDL tire dedans) |

### Chemin vidéo (le cœur du projet)

1. `avformat_open_input` sur l'URL (voir CLI) avec options basse latence :
   `fflags=nobuffer`, `probesize=32768`, `analyzeduration=100000`, `flags=low_delay`.
   UDP : `fifo_size=5000000`, `overrun_nonfatal=1`, `buffer_size=8388608`.
   SRT : `mode=listener`, `latency=<µs>`, `transtype=live` (**le portable écoute, le fixe appelle**).
2. Décodeur `h264` ou `hevc` (selon le flux), `AV_CODEC_FLAG_LOW_DELAY`, `hw_device_ctx` =
   `AVHWDeviceContext` de type `AV_HWDEVICE_TYPE_D3D11VA` construit **à partir de notre device**
   (`AVD3D11VADeviceContext.device` = notre `ID3D11Device`, avec `AddRef`), puis `av_hwdevice_ctx_init`.
3. Callback `get_format` : choisir `AV_PIX_FMT_D3D11`. Si indisponible : erreur claire (pas de repli CPU
   silencieux ; un repli logiciel explicite pourra être ajouté plus tard derrière une option).
4. Image décodée : `frame.data[0]` = `ID3D11Texture2D*` (tableau de textures), `frame.data[1]` = index.
   Copier la tranche (`CopySubresourceRegion`) dans une texture NV12 du renderer créée avec
   `D3D11_BIND_SHADER_RESOURCE` (v0.1). Optimisation ultérieure : demander `BindFlags` shader dans le
   `hw_frames_ctx` et lire la tranche directement.
5. Rendu : deux SRV sur la texture NV12 (`DXGI_FORMAT_R8_UNORM` luminance, `DXGI_FORMAT_R8G8_UNORM`
   chrominance), triangle plein écran, conversion BT.709. Respect du ratio 16:9 (bandes noires).
6. Swapchain : `DXGI_SWAP_EFFECT_FLIP_DISCARD`, 2 buffers, `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT`
   + `SetMaximumFrameLatency(1)`, `DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING` si supporté.
   Par défaut `--vsync off` : `Present(0, DXGI_PRESENT_ALLOW_TEARING)` dès qu'une nouvelle image arrive.
7. Si le flux se coupe : garder la dernière image, afficher « Signal perdu » après 2 s, et rouvrir
   l'entrée automatiquement (l'hôte relance ffmpeg en cas de plantage).

---

## Interface en ligne de commande (contrat avec bridge.py)

```
orbit-player.exe --host <IP du PC fixe>
                 [--transport udp|srt]        (défaut udp)
                 [--video-port 5000] [--input-port 5001] [--audio-port 5002]
                 [--srt-latency-ms 40]
                 [--fullscreen] [--vsync on|off] (défaut off)
                 [--no-input] [--no-audio]    (v0.1 : entrées/son désactivables)
                 [--audio-buffer-ms 30]
                 [--title "ORBIT – PC-FIXE"]
```

- Code de sortie 0 : fenêtre fermée par l'utilisateur. Autre : erreur (message sur stderr).
- `--host` sert aux entrées (destination) et au filtrage du son (seule IP acceptée).
- Touches réservées : **Ctrl+Alt+Shift+Q** quitter, **Ctrl+Alt+Shift+Z** libérer/capturer la souris,
  **F3** overlay de stats, **F11** plein écran. Ces combinaisons ne sont jamais envoyées à l'hôte.

---

## Protocoles réseau (FIGÉS – identiques à protocol.py côté hôte)

Tout en little-endian. `VERSION = 1`. Chaque paquet transporte un **état complet**, jamais un
événement : un paquet perdu est corrigé par le suivant.

### Manette `"CG"` – 19 octets – client -> hôte:5001
| Champ | Type | Contenu |
|---|---|---|
| magic | 2 octets | `b"CG"` |
| version | u8 | 1 |
| seq | u32 | compteur croissant (débordement géré par l'hôte) |
| buttons | u16 | bits XInput : DPAD_UP 0x0001, DOWN 0x0002, LEFT 0x0004, RIGHT 0x0008, START 0x0010, BACK 0x0020, LEFT_THUMB 0x0040, RIGHT_THUMB 0x0080, LEFT_SHOULDER 0x0100, RIGHT_SHOULDER 0x0200, GUIDE 0x0400, A 0x1000, B 0x2000, X 0x4000, Y 0x8000 |
| lx, ly, rx, ry | i16 ×4 | sticks, convention XInput : **Y vers le haut = positif** (SDL est inversé : `-y - 1`, borné) |
| lt, rt | u8 ×2 | gâchettes 0..255 |

### Clavier + souris `"KM"` – 84 octets – client -> hôte:5001
| Champ | Type | Contenu |
|---|---|---|
| magic | 2 octets | `b"KM"` |
| version | u8 | 1 |
| seq | u32 | |
| keys | 64 octets | 512 bits ; bit `i` = touche enfoncée ; `i = scancode PS/2 set 1 (0..255) + 256 si touche étendue (E0)`. Convertir les scancodes SDL (USB HID) vers set 1 avec une table dédiée, testée. |
| mbuttons | u8 | bit0 gauche, bit1 droit, bit2 milieu, bit3 X1, bit4 X2 |
| mx, my, mw | u32 ×3 | **compteurs cumulés** depuis le démarrage (déplacement relatif X, Y, molette en unités de 120), modulo 2^32. L'hôte applique la différence avec le dernier paquet reçu. |

Envoi : à chaque tick (250 Hz), un paquet CG **et** un paquet KM, même `seq`.
Quand la capture est inactive ou la fenêtre perd le focus : clavier et boutons **tous relâchés**,
compteurs inchangés. À la fermeture : 5 paquets « tout relâché ».

### Son `"AU"` – hôte -> client:5002
En-tête 12 octets : magic `b"AU"`, version u8, seq u32, sample_rate u32, channels u8 ;
puis PCM **s16le entrelacé**, 5 ms par paquet (240 échantillons à 48 kHz stéréo = 960 octets).
Jitter buffer : cible `--audio-buffer-ms` (30), maximum 3× la cible (au-delà : jeter le plus ancien),
vide -> silence puis attendre de revenir à la cible avant de rejouer. Paquets en retard (seq plus
ancien) : ignorés. Accepter uniquement les paquets venant de `--host`.

---

## Étapes (une à la fois, chacune livrée testée)

### v0.1 – Image
- Fenêtre SDL3 + device D3D11 + swapchain flip ; décodage D3D11VA sans copie CPU ; UDP et SRT.
- Overlay F3 : fps reçus/affichés, temps de décodage, temps réception -> présentation, images
  jetées, résolution, codec, transport.
- `scripts/test-send.ps1` : `ffmpeg -re -f lavfi -i testsrc2=size=1920x1080:rate=60 -c:v h264_nvenc
  -preset p1 -tune ull -zerolatency 1 -bf 0 -g 120 -f mpegts "udp://127.0.0.1:5000?pkt_size=1316"`
  (+ variante SRT `mode=caller`).
- **Critère** : flux 1080p60 affiché, CPU du player < 10 %, aucune copie CPU, latence inférieure à mpv
  (mesure : chronomètre à l'écran côté hôte, photo des deux écrans).

### v0.2 – Entrées
- Manette (SDL gamepad, mapping standard), clavier (scancodes set 1), souris relative.
- Capture active quand la fenêtre a le focus ; Ctrl+Alt+Shift+Z libère/reprend la souris.
- Tests unitaires : encodage des paquets octet par octet (comparer à des vecteurs générés par
  `protocol.py`), table de scancodes.
- **Critère** : `joy.cpl` sur l'hôte réagit ; un jeu se contrôle au clavier/souris sans touche coincée.

### v0.3 – Son
- Réception + jitter buffer + sortie SDL3.
- **Critère** : son continu, latence audio ≈ cible, pas de dérive après 30 min.

### v0.4 – Intégration ORBIT
- `bridge.py` lance `orbit-player.exe` (s'il est présent) au lieu de mpv + scripts Python.
- Le player est embarqué dans l'installeur Tauri (ressource) avec ses DLL.

### Plus tard (ne pas commencer sans demande)
- **Protocole vidéo ORBIT** (remplace MPEG-TS sur UDP brut ; `OrbitProtocolSource`, changement
  côté hôte requis). Constat terrain qui le motive : en UDP brut sur Wi-Fi, 1080p à 20 Mbit/s,
  les ~30 paquets UDP d'une image partent en rafale -> pertes massives (15 à 25 images/s reçues
  sur 60, erreurs « Packet corrupt » du démultiplexeur). Ce protocole doit **obligatoirement** :
  1. **Étaler l'envoi (pacing)** : répartir les paquets d'une image sur l'intervalle entre deux
     images (≈ 16,7 ms à 60 i/s) au lieu de les envoyer en rafale.
  2. **FEC Reed-Solomon par image** : paquets de redondance calculés sur les paquets de chaque
     image, pour reconstruire les paquets perdus sans retransmission.
  3. **Canal retour** client -> hôte : demander une image clé quand une image reste
     irrécupérable malgré la FEC.
- Débit adaptatif.
- HDR, multi-écran, décodage logiciel de secours.

---

## Méthode de travail attendue

- Avant de coder une étape : courte proposition de plan (fichiers, structures, points risqués).
- Petits commits, chacun compilable (`cargo build`, `cargo clippy -- -D warnings`, `cargo fmt`).
- Tests unitaires pour tout ce qui est pur (protocoles, jitter buffer, scancodes, calculs de ratio).
- Ce qui ne peut pas être testé automatiquement (GPU, fenêtre) : expliquer exactement comment le
  tester à la main et quoi observer.
- En cas de doute sur une API FFmpeg/D3D11 : s'appuyer sur la documentation officielle et les
  exemples C de FFmpeg (`doc/examples/hw_decode.c`), et le signaler plutôt que d'inventer.
- Licence prévue : MIT (à confirmer). Garder la liste des dépendances et de leurs licences dans
  `THIRD_PARTY.md`.

---

## Travail à plusieurs agents (git)

Plusieurs agents peuvent travailler en même temps. Règle de base : **un agent = une tâche =
une branche `agent/<nom>` = un worktree**. Personne ne modifie directement `main` dans le
dossier principal pendant que des agents tournent.

### Démarrer un agent
- À la main (un terminal par agent) : `./scripts/agent.ps1 new <nom>` puis
  `cd .claude/worktrees/<nom>; claude`. Le script copie `.cargo/config.toml` (FFmpeg/SDL3
  restent partagés dans le `third_party` du dossier principal : pas de nouveau téléchargement).
- Depuis Claude Code : `claude --worktree <nom>`, ou un sous-agent avec `isolation: "worktree"`.
  `.worktreeinclude` fait copier `.cargo/config.toml` automatiquement.
- Chaque worktree a son propre `target/` (~1 Go, première compilation longue) : c'est voulu,
  deux `cargo build` sur le même `target/` se bloqueraient mutuellement.

### Règles pour chaque agent
1. Ne travailler **que dans son worktree**, sur sa branche. Ne jamais faire `checkout`, `reset`,
   `rebase` ou `push --force` sur `main` ni sur la branche d'un autre agent.
2. Rester dans le périmètre de sa tâche. Découpage par modules pour limiter les conflits :
   `video/`, `render/`, `input/`, `audio/`, `net/`, `stats.rs`, `scripts/`.
3. **Fichiers partagés** (`main.rs`, `config.rs`, `Cargo.toml`, `Cargo.lock`, `CLAUDE.md`,
   `THIRD_PARTY.md`) : modifications minimales et localisées (ajouter une ligne, un module, une
   option), pas de réorganisation ni de reformatage global.
4. Les traits `VideoSource` / `HwDecoder` et `net/protocol.rs` sont des contrats : ne pas les
   modifier sans que la tâche le demande explicitement.
5. Petits commits, chacun passant `cargo fmt --check`, `cargo clippy -- -D warnings`,
   `cargo test`. Message : `<module>: <quoi>` en français (ex. `audio: jitter buffer`).
6. Avant de déclarer la tâche finie : `git rebase main` dans son worktree et relancer les
   vérifications. En cas de conflit sur `Cargo.lock` : prendre la version de `main`, puis
   `cargo build` pour la régénérer.
7. Ne pas fusionner soi-même dans `main` : c'est le propriétaire (ou l'agent coordinateur à sa
   demande) qui le fait.

### Intégrer le travail (propriétaire / coordinateur)
- `./scripts/agent.ps1 list` : branches des agents, avance/retard sur `main`.
- `./scripts/agent.ps1 merge <nom>` : rebase sur `main`, fmt + clippy + tests, puis fusion en
  avance rapide (`--ff-only`). Fusionner une branche à la fois ; les autres agents font ensuite
  `git rebase main`.
- `./scripts/agent.ps1 remove <nom>` : supprime le worktree et la branche (refusé si non fusionnée).
