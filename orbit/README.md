# ORBIT – interface console (PC portable)

```
orbit/
├─ src/            interface React (écrans, navigation manette)
├─ src-tauri/      coque Tauri 2 (fenêtre + lancement du pont)
└─ bridge/         pont local Python : parle à l'agent du PC fixe, lance ffplay / entrées / son
```

L'interface n'appelle que le pont (`http://127.0.0.1:47900`, protégé par un jeton généré
à chaque lancement). Le pont réutilise `cgclient.py` : détection mDNS, appairage SPAKE2,
HTTPS avec empreinte épinglée, Wake-on-LAN.

## 1. Prérequis (une seule fois, sur le portable)

1. **Python 3.11+** (déjà installé) puis, dans le dossier `bridge` :
   ```powershell
   python -m pip install -r requirements.txt
   ```
2. **FFmpeg** (pour ffplay) : déjà installé lors des étapes précédentes.
3. **Node.js LTS** : `winget install OpenJS.NodeJS.LTS`
4. **Rust + outils de compilation Microsoft** (pour Tauri) :
   ```powershell
   winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
   winget install Rustlang.Rustup
   ```
   Ferme puis rouvre le terminal. WebView2 est déjà présent sous Windows 10/11.
5. Dans le dossier `orbit` :
   ```powershell
   npm install
   ```

## 2. Lancer en mode développement

```powershell
npm run tauri dev
```
La première compilation Rust prend quelques minutes, les suivantes quelques secondes.
Le pont Python est lancé automatiquement par Tauri.

Sans Tauri (navigateur, pratique pour retoucher l'interface) :
```powershell
cd bridge; python bridge.py --token dev      # terminal 1
npm run dev                                   # terminal 2 -> http://localhost:5173
```

## 3. Construire l'exécutable

```powershell
npm run tauri build
```
Installeur : `src-tauri\target\release\bundle\nsis\ORBIT_0.1.0_x64-setup.exe`.
Les fichiers du pont sont copiés dans l'installation ; Python et les bibliothèques de
`requirements.txt` doivent rester installés sur le portable.
Si `python` n'est pas dans le PATH : variable d'environnement `ORBIT_PYTHON` (ex. `py`).

## Utilisation

| Manette | Clavier | Action |
|---|---|---|
| Croix / stick gauche | Flèches | Se déplacer |
| A | Entrée / clic | Valider |
| B | Échap | Retour / terminer la session |
| LB / RB | Page préc. / Page suiv. | Changer d'onglet |
| – | F11 | Plein écran de l'interface |

- **Premier lancement** : écran « Choisir un PC » : le PC fixe détecté apparaît, choisis-le
  et entre le code qui s'affiche sur le fixe (pavé numérique à la manette ou clavier).
- **Jouer** : l'écran de lancement montre les étapes (réveil, agent, flux, jeu). La vidéo
  s'ouvre dans la fenêtre de streaming (plein écran par défaut, réglable dans Paramètres).
  Ctrl+Alt+Shift+Z capture / rend le clavier et la souris.
- **Fin** : ferme la fenêtre de streaming (Échap / Q) ou « Terminer la session ».
- **Wi-Fi faible** : Paramètres > Débit 10–15 Mbps et Résolution 720p.

## Réveil à distance (Wake-on-LAN)

ORBIT réveille le PC fixe s'il est éteint ou en veille, à condition :
- que le PC fixe ait été **appairé avec cette version** (l'agent donne son adresse MAC) :
  sinon « Oublier ce PC » puis ré-appairer ;
- que le Wake-on-LAN soit activé dans le **BIOS** du fixe (souvent « Power On by PCI-E »)
  et dans Windows : propriétés de la carte réseau > Avancé > *Wake on Magic Packet* ;
- que le fixe soit en **Ethernet** (le réveil par Wi-Fi est rarement supporté) ;
- que le *Démarrage rapide* de Windows soit désactivé (Options d'alimentation).

## Fichiers à retoucher pour personnaliser

| Fichier | Contenu |
|---|---|
| `src/styles.css` | couleurs, tailles, mise en page (variables en haut) |
| `src/screens/*.jsx` | un fichier par écran |
| `src/nav.js` | navigation spatiale + mapping manette |
| `src/api.js` | appels au pont |
| `bridge/bridge.py` | routes du pont, déroulé d'une session (`Runner`) |
