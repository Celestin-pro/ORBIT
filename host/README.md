# Cloud gaming maison – agent + client

```
host/    -> à copier sur le PC FIXE
client/  -> à copier sur le PC PORTABLE
```

## 1. PC fixe (une seule fois, sur place)

1. Copie le dossier `host` où tu veux (ex. `Documents\Cloud-gaming\host`).
   Ne le déplace plus ensuite : la tâche planifiée pointe vers ce chemin.
2. PowerShell **administrateur** dans ce dossier :
   ```powershell
   powershell -ExecutionPolicy Bypass -File .\host_setup.ps1
   ```
   Le script installe les bibliothèques de l'agent, ouvre le pare-feu, crée la tâche
   planifiée « CloudGamingAgent » (démarrage à l'ouverture de session) et lance l'agent.

C'est la seule chose à faire sur le fixe.

## 2. PC portable (une seule fois)

PowerShell **administrateur** dans le dossier `client` :
```powershell
powershell -ExecutionPolicy Bypass -File .\client_setup.ps1
```

## 3. Depuis le portable, tout le reste

```powershell
python client.py discover        # le PC fixe doit apparaître
python client.py pair            # une fenêtre avec un code à 6 chiffres s'ouvre sur le fixe -> tape-le
python client.py status          # dépendances manquantes ?
python client.py install all     # FFmpeg + ViGEmBus + libs Python, installés SUR LE FIXE
python client.py play            # session : image + son + contrôles
```

Après `install all`, redémarre l'agent pour qu'il voie FFmpeg et ViGEmBus :
redémarre le PC fixe, ou sur le fixe : `Stop-ScheduledTask CloudGamingAgent; Start-ScheduledTask CloudGamingAgent`.

Jeux du PC fixe (Steam, Epic, GOG, Ubisoft Connect, Rockstar Games Launcher + jeux perso) :
```powershell
python client.py games                 # liste
python client.py games --refresh       # relance la détection après une installation
python client.py play --game hades     # session + lancement direct du jeu
```
Ajouter un jeu perso (à faire **sur le PC fixe**, volontairement pas depuis le réseau) :
```powershell
py agent.py --add-game "Mon jeu" "D:\Jeux\MonJeu\MonJeu.exe"
py agent.py --list-games
py agent.py --remove-game custom:mon-jeu
```

Réglages vidéo (enregistrés sur le fixe) :
```powershell
python client.py video --bitrate 20 --fps 60 --codec h264 --scale 1920x1080
python client.py video --scale ""          # résolution native (sans conversion CPU)
```

Pendant `play` : **Ctrl+Alt+Shift+Z** capture/rend le clavier et la souris ;
fermer la fenêtre vidéo termine la session des deux côtés.

## Sécurité

- HTTPS avec un certificat propre à l'agent ; le portable mémorise son empreinte et
  refuse de parler à un autre PC qui se ferait passer pour lui.
- Appairage par PIN avec SPAKE2 : le PIN ne circule jamais et ne peut pas être deviné
  en écoutant le réseau. Un essai par PIN affiché.
- Chaque portable a son propre jeton ; révocation : `python client.py unpair` (portable)
  ou `py agent.py --list-clients` / `py agent.py --revoke <id>` (fixe).
- L'agent n'exécute **aucune commande libre** : seulement les installations et la session prévues.
- Si la connexion au portable est perdue plus de 15 s, la session s'arrête toute seule.

## Où sont les journaux (PC fixe)

`%APPDATA%\CloudGamingAgent\logs\` : `agent.log`, `session_video.log`,
`session_audio.log`, `session_input.log`, `install_*.log`.

## API (pour l'appli React)

| Méthode | Route | Auth | Rôle |
|---|---|---|---|
| GET | `/info` | – | nom, id, version, empreinte |
| POST | `/pair/begin` | – | affiche le PIN sur le fixe -> `session_id` |
| POST | `/pair/exchange` | – | `{session_id, msg_a}` -> `{msg_b}` (SPAKE2) |
| POST | `/pair/confirm` | – | `{session_id, client_name, confirm}` -> `{client_id, token, server_confirm}` |
| DELETE | `/pair` | oui | révoque ce portable |
| GET | `/status` | oui | dépendances, session, installations |
| GET | `/config` | oui | configuration complète |
| PATCH | `/config/video` | oui | `{fps, bitrate_mbps, codec, scale}` |
| POST | `/install/{ffmpeg\|vigembus\|python-deps}` | oui | lance une installation |
| GET | `/games?refresh=true` | oui | jeux détectés `[{id, name, source, art, remote_art, last_played}]` |
| GET | `/games/{id}/art/{cover\|hero\|logo\|header}` | oui | image locale (Steam) |
| GET | `/games/running` | oui | `{running: id ou null}` |
| POST | `/games/{id}/launch` | oui | lance le jeu |
| POST | `/games/{id}/stop` | oui | ferme les processus du jeu |
| POST | `/session/start` | oui | démarre vidéo + audio + entrées vers l'IP appelante |
| POST | `/session/heartbeat` | oui | toutes les 3 s pendant la session |
| POST | `/session/stop` | oui | arrête la session |

Auth = en-tête `Authorization: Bearer <token>`.
