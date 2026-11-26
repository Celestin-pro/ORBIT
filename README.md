# orbit-player

Récepteur de streaming du projet ORBIT, côté PC portable : il reçoit la vidéo du PC fixe
(protocole maison OVP avec `orbit-sender`, ou MPEG-TS H.264/HEVC en UDP ou SRT), la décode en D3D11VA sans jamais copier l'image vers le
processeur, et l'affiche dans une seule fenêtre. Les entrées et le son viendront aux étapes
suivantes (voir `CLAUDE.md`, qui décrit aussi l'architecture et les règles du projet).

## Compiler et lancer

```powershell
powershell -ExecutionPolicy Bypass -File scripts\setup.ps1   # prérequis + FFmpeg LGPL + SDL3
cargo build --release
target\release\orbit-player.exe --host 192.168.1.20
```

Pour développer sans le PC fixe, une mire locale :

```powershell
cargo run -- --host 127.0.0.1
powershell -ExecutionPolicy Bypass -File scripts\test-send.ps1   # dans un autre terminal
```

Touches réservées (jamais envoyées à l'hôte) : **F3** overlay de stats, **F11** plein écran,
**Ctrl+Alt+Shift+Z** souris, **Ctrl+Alt+Shift+Q** quitter.

Journal : niveau `info` par défaut, `$env:ORBIT_LOG = "debug"` pour le détail (réception
paquet par paquet résumée chaque seconde, tentatives de reconnexion).

## Protocole maison OVP (`--transport orbit`)

Avec `--transport orbit`, le player reçoit le protocole décrit dans `PROTOCOL.md` au lieu du
MPEG-TS de ffmpeg : FEC Reed-Solomon (les paquets perdus sont reconstruits sans
retransmission), pacing côté émetteur, et canal retour vers le PC fixe (HELLO, STATS toutes
les 250 ms, demande d'image clé après une image irrécupérable). L'émetteur est
`orbit-sender`, sur le PC fixe. Les modes `udp` (défaut) et `srt` restent disponibles.

```powershell
# PC fixe (règle de pare-feu UDP 5003 entrante nécessaire) :
target\release\orbit-sender.exe --client 192.168.1.72 --bitrate-mbps 15 --fec-percent 20
# PC portable :
target\release\orbit-player.exe --host 192.168.1.107 --transport orbit
```

L'émetteur n'envoie rien tant qu'il n'a pas reçu le premier HELLO du player : l'ordre de
lancement n'a pas d'importance. Options propres à ce transport : `--feedback-port 5003`,
`--on-loss freeze|show` (image figée ou artefacts après une perte, jusqu'à l'image clé
suivante), `--frame-lost-timeout-ms 20`, `--simulate-loss <pct>` (jette des paquets à la
réception pour éprouver la FEC).

Pour développer sans le PC fixe, tout sur le portable (mire, encodeur Intel) :

```powershell
cargo run --release -- --host 127.0.0.1 --transport orbit
cargo run --release -p orbit-sender -- --client 127.0.0.1 --source test --encoder qsv
```

En OVP, l'overlay F3 ajoute : images ok / réparées par la FEC / perdues / en retard par
seconde, fragments perdus (et taux), débit reçu, gigue, étalement à l'arrivée (premier
fragment -> image complète : proche de la cible du pacing), nombre de demandes d'image clé,
et, s'il y en a, paquets rejetés, paquets d'autres sources, débordements et pertes simulées.
« Réception->écran » part ici de l'arrivée du **premier fragment** de l'image : l'attente du
réassemblage est comprise.

## Ce que montre l'overlay F3

Les mêmes chiffres sont écrits une fois par seconde dans le journal, sur une ligne
`stats | Libellé=valeur | ...` facile à copier dans un rapport.

| Ligne | Signification |
|---|---|
| Transport | entrée réseau écoutée (`udp://0.0.0.0:5000`, `srt://...`) |
| Codec | codec du flux et méthode de décodage (`H.264 / D3D11VA`) |
| Résolution | taille des images décodées |
| Reçues | unités d'accès (images compressées) reçues par seconde |
| Décodées | images sorties du décodeur par seconde |
| Affichées | images présentées à l'écran par seconde |
| Débit | débit compressé reçu |
| Décodage | temps de décodage d'une image (moyenne et maximum sur la seconde) |
| Réception->écran | latence interne du player, voir ci-dessous |
| Jetées | images décodées remplacées par une plus récente avant d'être affichées (normal si l'écran rafraîchit moins vite que le flux) |
| Refusées | unités rejetées à l'entrée du décodeur (avant la première image clé, après une perte) |
| Erreurs décodage | images abîmées par des pertes (affichées quand même, le flux se rétablit à l'image clé suivante) |
| Reconnexions | réouvertures de l'entrée après une coupure |

`+n (total m)` : `n` pendant la dernière seconde, `m` depuis le démarrage.

## Coupures et reconnexion

- Avant la première image : bandeau « En attente du flux… ». Le player réessaie d'ouvrir
  l'entrée toutes les 500 ms, sans limite ; le PC fixe peut démarrer après lui.
- Sans nouvelle image pendant 2 s : la dernière image reste affichée, assombrie, avec le
  bandeau « Signal perdu ». Le voile disparaît après 100 ms d'images continues.
- En parallèle, une lecture réseau muette pendant 2 s échoue (option `timeout` de l'URL) : le
  player referme et rouvre l'entrée, puis reprend à la prochaine image clé. En UDP, la
  coupure est détectée environ 2,5 s après le dernier paquet ; en SRT, environ 2,2 s après un
  arrêt brutal de l'émetteur (mesuré avec un délai de 1 s : 1,5 s et 1,2 s).
- Une erreur que réessayer ne réglera pas (décodeur matériel absent, codec non pris en
  charge) ferme le player avec un message sur stderr et un code de sortie non nul.

## Mesurer la latence

### Ce que mesure « Réception->écran »

C'est la **latence interne du player** : de l'instant où une image compressée complète sort
du démultiplexeur MPEG-TS jusqu'à l'appel `Present` qui l'envoie à l'écran. Elle comprend le
décodage, la copie GPU vers la texture d'affichage, l'attente de la chaîne d'échange et le
rendu. Ordre de grandeur sur la mire locale en 720p60 : 1,5 ms en moyenne.

Elle **ne comprend pas** :

- la capture et l'encodage sur le PC fixe ;
- le transport réseau ;
- l'attente dans le démultiplexeur MPEG-TS : une image n'en sort qu'au début de la suivante
  (environ 16,7 ms à 60 images/s). On le constate à chaque coupure, où la dernière image
  n'est livrée qu'au moment où la coupure est détectée. C'est une des raisons du futur
  protocole maison ;
- le compositeur de Windows et le balayage de l'écran.

C'est donc le bon chiffre pour comparer deux réglages du player entre eux, pas pour connaître
la latence que ressent le joueur.

### Mesure de bout en bout (critère de la v0.1)

Le principe : filmer en une seule photo un chronomètre affiché sur le PC fixe **et** son
image retransmise sur le portable. La différence entre les deux valeurs est la latence
totale, de l'écran du fixe à l'écran du portable.

1. Sur le PC fixe, afficher un chronomètre à la milliseconde, grand et contrasté (une page
   web de chronomètre en plein écran convient), sur l'écran capturé par `ddagrab`.
2. Lancer la session ORBIT ; sur le portable, `orbit-player` en plein écran (F11), avec
   `--vsync off` (le défaut).
3. Placer les deux écrans côte à côte et les photographier ensemble avec un téléphone, en
   vitesse d'obturation rapide (1/500 s ou plus, mode sport ou rafale) pour que les chiffres
   ne soient pas flous.
4. Sur chaque photo : latence = chronomètre du fixe − chronomètre vu sur le portable.
5. Prendre au moins 10 photos (une rafale suffit) et noter la médiane et le maximum. Une
   photo isolée ne suffit pas : chaque écran ne se rafraîchit que toutes les 16,7 ms à 60 Hz,
   donc une mesure seule peut varier d'une image dans un sens ou dans l'autre.
6. Pour comparer avec mpv, refaire la même série dans les mêmes conditions (même débit,
   même encodeur, même réseau), en alternant les deux lecteurs.

Pendant la mesure, garder l'overlay F3 ouvert : si « Jetées » ou « Erreurs décodage »
augmentent, ou si « Réception->écran » a des maxima élevés, la série n'est pas
représentative.

La mire `scripts\test-send.ps1 -Chrono` incruste un chronomètre dans l'image, mais c'est le
temps *du flux* : il ne se compare pas à une horloge réelle, et ne remplace donc pas cette
mesure.
