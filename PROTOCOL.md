# Protocole vidéo ORBIT (OVP) – version 1

Remplace MPEG-TS sur UDP/SRT entre le PC fixe et le PC portable.
Objectif : tenir un flux 1080p60 sur un Wi-Fi qui perd des paquets, comme Moonlight.

Constat terrain qui motive ce protocole (24/11) : en UDP brut sur Wi-Fi, 1080p 20 Mbps, une image
(~41 Ko) part en rafale de ~30 paquets ; la borne Wi-Fi en jette une partie et **un seul paquet
perdu abîme toute l'image** → 15 à 25 images/s exploitables sur 60.

Trois mécanismes répondent à ce constat :
1. **Pacing** : les paquets d'une image sont étalés dans le temps au lieu de partir en rafale.
2. **FEC Reed-Solomon** par image : des paquets de parité permettent de reconstruire les paquets
   perdus sans aller-retour réseau.
3. **Canal retour** : le portable rapporte ses statistiques et demande une image clé quand une
   image est irrécupérable.

---

## Vue d'ensemble

```
PC FIXE                                              PC PORTABLE
ffmpeg (ddagrab + nvenc, -f h264 -)                  orbit-player
   │  H.264/HEVC Annex B sur stdout                    ▲
   ▼                                                   │ OrbitProtocolSource
orbit-sender ── UDP 5000 : paquets "OV" ─────────────► réassemblage + FEC ─► décodeur D3D11VA
   ▲                                                   │
   └──────── UDP 5003 : paquets "OF" (retour) ◄────────┘
```

- `orbit-sender` lance lui-même ffmpeg (capture + encodage) et lit son flux brut. Plus tard, il
  remplacera ffmpeg par sa propre capture DXGI + NVENC ; le protocole ne changera pas.
- Le portable **écoute** sur 5000 ; le fixe envoie. Le portable envoie son retour sur fixe:5003.
- Les deux côtés n'acceptent que l'IP de l'autre (`--client` / `--host`).

Le code du protocole vit dans la crate **`orbit-protocol`** (Rust pur, sans dépendance Windows,
entièrement testable), utilisée par `orbit-sender` et par `orbit-player`.

---

## Découpage d'une image

Une **image** = une unité d'accès H.264/HEVC complète (Annex B, avec ses codes de démarrage),
y compris SPS/PPS(/VPS) quand ils précèdent une image clé.

1. `n = ceil(frame_len / SHARD_SIZE_MAX)` fragments de données au total.
2. Parité d'un bloc de `d` fragments de données :
   - `fec_percent = 0` → **aucune parité** (`parity_shards = 0`, Reed-Solomon n'est pas appelé) ;
   - sinon `parity = max(fec_min_parity, ceil(d × fec_percent / 100))`, plancher
     `fec_min_parity` = 2 par défaut (une image de 2 Ko n'aurait sinon qu'un fragment de parité).
3. `d_max` = plus grand `d ≤ MAX_DATA_SHARDS` tel que `d + parity(d) ≤ 255` (limite de GF(2^8)) ;
   182 à 40 %, 127 à 100 %.
4. L'image est découpée en `block_count = ceil(n / d_max)` **blocs FEC** (au plus 255) ; chaque
   bloc reçoit `ceil(frame_len / block_count)` octets, le dernier le reste. Le récepteur
   **concatène les blocs dans l'ordre** de `block_index` ; la somme des `block_len` vaut `frame_len`.
5. Dans un bloc de `block_len` octets : `data_shards = ceil(block_len / SHARD_SIZE_MAX)`,
   `shard_size = ceil(block_len / data_shards)` (même taille dans tout le bloc, donc les petites
   images ont de petits fragments) ; le dernier fragment de données est complété par des zéros.
6. Ordre d'émission : bloc par bloc, fragments de données puis de parité.

**Code Reed-Solomon (fait partie du format)** : corps GF(2^8) de polynôme 0x11D, matrice de
Vandermonde `(d+p) × d` rendue systématique (multipliée par l'inverse de son carré supérieur,
construction Backblaze), implémentée par la crate `reed-solomon-erasure` 6.0. La première ligne
de parité est le XOR des données. Un vecteur de référence figé est testé dans `fec.rs`.

Constantes v1 :

| Constante | Valeur | Raison |
|---|---|---|
| `SHARD_SIZE_MAX` | 1200 octets | 28 (IP+UDP) + 32 (en-tête) + 1200 = 1260 < MTU 1280 (Tailscale, VPN) |
| `MAX_DATA_SHARDS` | 200 | laisse de la place à la parité sous 255 |
| `fec_percent` | 20 % par défaut (option, 0 à 100) | réglable selon le réseau |
| `fec_min_parity` | 2 par défaut (option) | plancher de parité par bloc quand `fec_percent > 0` |
| `MAX_FRAME_LEN` | 16 Mio | au-delà, le paquet est rejeté (aucune allocation sur des valeurs hostiles) |

Une image clé 1080p de ~200 Ko = ~170 fragments → 1 bloc ; une image clé 4K → plusieurs blocs.

---

## Paquet vidéo `"OV"` (fixe → portable, UDP 5000)

Little-endian, en-tête de **32 octets**, puis `shard_size` octets de charge utile.

| Offset | Taille | Champ | Description |
|---|---|---|---|
| 0 | 2 | magic | `b"OV"` |
| 2 | 1 | version | `1` |
| 3 | 1 | flags | bit0 = image clé, bit1 = fragment de parité, bits 2-7 = 0 |
| 4 | 4 | frame_id | u32, +1 par image, rebouclage géré (comparaison modulo 2^32) |
| 8 | 1 | block_index | u8, index du bloc FEC dans l'image |
| 9 | 1 | block_count | u8, nombre de blocs de l'image |
| 10 | 1 | shard_index | u8, 0..data_shards-1 = données, data_shards.. = parité |
| 11 | 1 | data_shards | u8, fragments de données de CE bloc |
| 12 | 1 | parity_shards | u8, fragments de parité de CE bloc |
| 13 | 1 | codec | 1 = H.264, 2 = HEVC |
| 14 | 2 | shard_size | u16, taille de la charge utile de chaque fragment de ce bloc |
| 16 | 4 | block_len | u32, octets utiles de ce bloc (pour retirer le bourrage) |
| 20 | 4 | frame_len | u32, octets utiles de l'image entière |
| 24 | 8 | send_ts_us | u64, horloge monotone du fixe (µs) à l'arrivée de l'image depuis l'encodeur |

Règles :
- Tous les fragments d'un bloc ont les mêmes `frame_id`, `block_index`, `block_count`,
  `data_shards`, `parity_shards`, `shard_size`, `block_len`, `frame_len`, `codec`, `flags` (hors bit1).
- Un paquet dont un champ est incohérent (tailles, index hors bornes) est **rejeté et compté**,
  jamais une panique : ces octets viennent du réseau. Vérifications : magic, version, bits de
  flags réservés, codec, `block_index < block_count`, `1 ≤ data_shards`,
  `data_shards + parity_shards ≤ 255`, `shard_index < data_shards + parity_shards`, bit1 égal à
  `shard_index ≥ data_shards`, `1 ≤ shard_size ≤ 1200`, longueur de charge = `shard_size`,
  `1 ≤ block_len ≤ data_shards × shard_size`, `block_len ≤ frame_len ≤ MAX_FRAME_LEN`, et
  cohérence avec les fragments déjà reçus du même bloc et de la même image (`send_ts_us` compris).

---

## Paquet retour `"OF"` (portable → fixe, UDP 5003)

En-tête commun : `magic b"OF"` (2), `version` u8 = 1, `type` u8, `seq` u32.

| type | Nom | Charge utile | Fréquence |
|---|---|---|---|
| 1 | HELLO | aucune | toutes les 500 ms tant que la session vit (le sender arrête d'envoyer après 3 s sans HELLO ni STATS) |
| 2 | STATS | `last_frame_id` u32, `frames_ok` u32, `frames_fec_recovered` u32, `frames_lost` u32, `shards_received` u32, `shards_lost` u32, `jitter_us` u32 (compteurs depuis le dernier STATS ; `jitter_us` est la valeur courante) | toutes les 250 ms |
| 3 | KEYFRAME_REQUEST | `lost_frame_id` u32 | à chaque image irrécupérable, au plus 1 toutes les 200 ms |

`jitter_us` : gigue RFC 3550 (`J += (|D| − J) / 16`), où `D` est la variation, d'une image à
la suivante, de `arrivée du premier fragment − send_ts_us`. Les deux horloges n'ont pas à être
synchronisées : seule la variation compte.

`shards_lost` est estimé sur les blocs dont au moins un fragment est arrivé (pour une image
dont rien n'est arrivé, le nombre de fragments est inconnu).

**Démarrage du sender** : il n'envoie rien tant qu'il n'a pas reçu un premier HELLO ou STATS de
`--client` (ffmpeg tourne et son flux est lu, mais jeté). Il se met en pause après 3 s sans
retour, et reprend au premier HELLO **à partir de la prochaine image clé**.

Côté sender v1 (ffmpeg en ligne de commande) : une demande d'image clé ne peut pas être
transmise à ffmpeg. Elle est **journalisée et comptée** ; le flux garde une image clé
périodique (`--keyframe-interval`, 1 s par défaut). Quand le sender encodera lui-même (NVENC),
la demande forcera une image clé immédiate.

---

## Pacing (sender)

- Les fragments d'une image (données puis parité, blocs dans l'ordre) sont envoyés avec un
  **débit plafonné** : `rate = max(pacing_factor × bitrate, frame_bytes / (0.75 × frame_interval))`,
  `pacing_factor` = 1.5 par défaut.
- Envoi en **micro-rafales** de `burst` paquets (4 par défaut) séparées par l'attente calculée,
  avec un timer haute résolution (`CreateWaitableTimerExW` + `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`,
  repli sur une attente active courte en dessous de 200 µs).
- Si l'image suivante arrive avant la fin de l'envoi : terminer l'image en cours sans pacing
  (ne jamais accumuler de retard), compter l'événement.
- Option `--no-pacing` pour comparer.

---

## Réassemblage (player, `OrbitProtocolSource`)

- Un tampon par `frame_id` en cours, au plus 8 images : une image à 8 `frame_id` ou plus de la
  plus récente reçue est déclarée perdue sans attendre le délai.
- État de départ : **attente d'une image clé** (le décodeur ne peut pas démarrer sans SPS/PPS) ;
  les images non clés complètes reçues avant sont jetées. Même chose après une resynchronisation
  (`frame_id` à plus de 64 de l'attendu : émetteur redémarré).
- Dès qu'un bloc a `data_shards` fragments reçus (données ou parité), il est **reconstruit**
  (Reed-Solomon) ; l'image est livrée au décodeur quand tous ses blocs le sont.
- **Délai d'attente** : une image incomplète est déclarée perdue si une image plus récente est
  complète **et** si `frame_lost_timeout_ms` (défaut 20 ms) s'est écoulé depuis son premier fragment
  (si aucun de ses fragments n'est arrivé : depuis le premier fragment de l'image suivante).
- Images livrées **dans l'ordre** des `frame_id` ; une image arrivée après une plus récente déjà
  livrée est jetée (comptée « en retard »).
- Après une image perdue, politique `--on-loss` :
  - `freeze` (défaut) : ne plus livrer que des images clés jusqu'à la prochaine image clé
    (pas d'artefacts, image figée) + KEYFRAME_REQUEST ;
  - `show` : continuer à livrer (artefacts possibles jusqu'à la prochaine image clé).
- Jeux de paramètres (SPS/PPS/VPS) : extraits de la première image clé (Annex B) et passés au
  décodeur via `VideoSource::configure`, comme le fait déjà `LavfSource`.
- Statistiques exposées à l'overlay F3 : images ok / récupérées par FEC / perdues / en retard,
  fragments perdus, gigue, débit, demandes d'image clé envoyées.

---

## Interfaces en ligne de commande

### orbit-sender (PC fixe)
```
orbit-sender --client <IP portable>
             [--port 5000] [--feedback-port 5003]
             [--codec h264|hevc] [--fps 60] [--bitrate-mbps 20]
             [--scale 1920x1080 | --scale native]
             [--keyframe-interval-s 1] [--fec-percent 20] [--fec-min-parity 2]
             [--split silence|aud] [--split-silence-us 1000]
             [--pacing-factor 1.5] [--burst 4] [--no-pacing]
             [--source desktop|test]        test = mire testsrc2, pour développer sans capture
             [--ffmpeg <chemin>]
             [--simulate-loss <pct>]        jette volontairement des paquets à l'envoi (tests FEC)
```
Commande ffmpeg lancée (source desktop, H.264) :
`ffmpeg -hide_banner -loglevel warning -f lavfi -i ddagrab=framerate=<fps> [-vf hwdownload,format=bgra,scale=W:H,format=nv12]
 -c:v h264_nvenc -preset p1 -tune ull -zerolatency 1 -delay 0 -bf 0 -rc cbr -b:v <B>M -maxrate <B>M
 -bufsize <2 images> -g <fps × keyframe_interval> -forced-idr 1 -aud 1 -flush_packets 1 -f h264 -`
(HEVC : `hevc_nvenc` et `-f hevc`). Les délimiteurs d'unité d'accès (`-aud 1`, NAL type 9 en H.264,
35 en HEVC) servent à découper les images dans le flux stdout. Pour les encodeurs sans option
`-aud` (`libopenh264`, `*_mf`, utilisés par `--source test` quand NVENC manque), le filtre
`-bsf:v h264_metadata=aud=insert` (`hevc_metadata` en HEVC, LGPL) les ajoute.

**Découpage et latence** : en Annex B, la fin d'une image n'est connue qu'à l'arrivée de l'AUD
suivant, soit une image de retard. Par défaut (`--split silence`), le sender émet l'image dès que
ffmpeg n'a rien écrit depuis `--split-silence-us` (1 ms) si elle contient déjà une tranche ; l'AUD
suivant confirme la coupure (sinon l'erreur est comptée et journalisée). `--split aud` attend
l'AUD suivant (mode sûr, une image de latence en plus).

### orbit-player (PC portable) – ajouts
```
--transport orbit            (en plus de udp et srt)
--feedback-port 5003
--on-loss freeze|show        (défaut freeze)
--frame-lost-timeout-ms 20
--simulate-loss <pct>        jette volontairement des paquets à la réception (tests)
```

---

## Ports et pare-feu

| Port | Sens | Règle à ajouter |
|---|---|---|
| UDP 5000 | fixe → portable | déjà ouverte sur le portable |
| UDP 5003 | portable → fixe | **nouvelle** règle entrante sur le fixe (agent `host_setup.ps1`) |

---

## Évolutions prévues (hors v1)

- v2 : sender avec capture DXGI + NVENC intégrés → vraies images clés à la demande, invalidation
  de références, débit adaptatif piloté par STATS.
- Chiffrement (ChaCha20-Poly1305) avec une clé de session fournie par l'agent.
