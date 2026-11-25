"""Détection des jeux installés sur le PC fixe + lancement / suivi / arrêt.

Sources : Steam, Epic Games, GOG, Ubisoft Connect, Rockstar Games Launcher,
jeux perso (custom_games.json).
Chaque jeu a un id stable "<source>:<identifiant>" que l'interface utilise.
"""
import json
import os
import re
import time
from pathlib import Path

try:
    import winreg
except ImportError:          # permet de tester le module hors Windows
    winreg = None

STEAM_TOOLS = {228980, 1070560, 1391110, 1628350, 1493710, 1826330, 2180100, 961940}
STEAM_TOOL_WORDS = ("Steamworks", "Redistributable", "Proton", "Steam Linux Runtime",
                    "Soundtrack", "Dedicated Server", "SDK")
STEAM_CDN = "https://cdn.cloudflare.steamstatic.com/steam/apps/{appid}/{file}"


# ============================================================ utilitaires
def reg_get(root, path, name):
    if winreg is None:
        return None
    for view in (winreg.KEY_WOW64_64KEY, winreg.KEY_WOW64_32KEY):
        try:
            with winreg.OpenKey(root, path, 0, winreg.KEY_READ | view) as k:
                return winreg.QueryValueEx(k, name)[0]
        except OSError:
            continue
    return None


def reg_subkeys(root, path):
    """Renvoie {nom_sous_clé: {valeur: donnée}} (vide si la clé n'existe pas)."""
    out = {}
    if winreg is None:
        return out
    for view in (winreg.KEY_WOW64_32KEY, winreg.KEY_WOW64_64KEY):
        try:
            with winreg.OpenKey(root, path, 0, winreg.KEY_READ | view) as k:
                i = 0
                while True:
                    try:
                        sub = winreg.EnumKey(k, i)
                    except OSError:
                        break
                    i += 1
                    vals = {}
                    with winreg.OpenKey(k, sub) as sk:
                        j = 0
                        while True:
                            try:
                                n, v, _ = winreg.EnumValue(sk, j)
                            except OSError:
                                break
                            vals[n] = v
                            j += 1
                    out.setdefault(sub, vals)
        except OSError:
            continue
    return out


def parse_vdf(text):
    """Parseur minimal du format KeyValues de Valve (fichiers .vdf / .acf)."""
    tokens = re.findall(r'"((?:[^"\\]|\\.)*)"|([{}])', text)
    pos = 0

    def parse_obj():
        nonlocal pos
        obj = {}
        while pos < len(tokens):
            s, brace = tokens[pos]
            pos += 1
            if brace == "}":
                return obj
            key = s
            s2, brace2 = tokens[pos]
            pos += 1
            obj[key] = parse_obj() if brace2 == "{" else s2.replace("\\\\", "\\")
        return obj

    return parse_obj()


def slug(s):
    return re.sub(r"[^a-z0-9]+", "-", s.lower()).strip("-")[:60]


# ============================================================ Steam
def steam_root():
    p = reg_get(winreg.HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath") if winreg else None
    p = p or reg_get(winreg.HKEY_LOCAL_MACHINE, r"SOFTWARE\Valve\Steam", "InstallPath") if winreg else p
    return Path(p) if p else None


def steam_art(root, appid):
    """Images en cache local de Steam (anciennes et nouvelles dispositions du dossier)."""
    cache = root / "appcache" / "librarycache"
    found = {}
    wanted = {"cover": "library_600x900", "hero": "library_hero", "logo": "logo", "header": "header"}
    for kind, stem in wanted.items():
        old = cache / f"{appid}_{stem}.jpg"
        if old.exists():
            found[kind] = str(old)
            continue
        sub = cache / str(appid)
        if sub.is_dir():
            hits = sorted(sub.rglob(f"{stem}*.jpg")) + sorted(sub.rglob(f"{stem}*.png"))
            if hits:
                found[kind] = str(hits[0])
    return found


def steam_games(root=None):
    root = root or steam_root()
    if not root or not root.exists():
        return []
    libs = [root]
    lf = root / "steamapps" / "libraryfolders.vdf"
    if lf.exists():
        data = parse_vdf(lf.read_text("utf-8", errors="replace")).get("libraryfolders", {})
        for v in data.values():
            if isinstance(v, dict) and v.get("path"):
                libs.append(Path(v["path"]))
    games, seen = [], set()
    for lib in libs:
        apps = lib / "steamapps"
        for acf in apps.glob("appmanifest_*.acf"):
            st = parse_vdf(acf.read_text("utf-8", errors="replace")).get("AppState", {})
            try:
                appid = int(st.get("appid", 0))
            except ValueError:
                continue
            name = st.get("name", "")
            flags = int(st.get("StateFlags", "0") or 0)
            if (appid in seen or appid in STEAM_TOOLS or not flags & 4
                    or any(w.lower() in name.lower() for w in STEAM_TOOL_WORDS)):
                continue
            seen.add(appid)
            games.append(dict(
                id=f"steam:{appid}", name=name, source="steam",
                install_dir=str(apps / "common" / st.get("installdir", "")),
                launch=f"steam://rungameid/{appid}",
                local_art=steam_art(root, appid),
                remote_art={"cover": STEAM_CDN.format(appid=appid, file="library_600x900.jpg"),
                            "hero": STEAM_CDN.format(appid=appid, file="library_hero.jpg"),
                            "header": STEAM_CDN.format(appid=appid, file="header.jpg")},
                last_played=int(st.get("LastPlayed", "0") or 0),
            ))
    return games


def steam_running_appid():
    v = reg_get(winreg.HKEY_CURRENT_USER, r"Software\Valve\Steam", "RunningAppID") if winreg else None
    return int(v) if v else 0


# ============================================================ Epic
def epic_games(manifests=None):
    manifests = manifests or Path(os.environ.get("PROGRAMDATA", r"C:\ProgramData")) / \
        "Epic" / "EpicGamesLauncher" / "Data" / "Manifests"
    games = []
    if not manifests.exists():
        return games
    for item in manifests.glob("*.item"):
        try:
            m = json.loads(item.read_text("utf-8", errors="replace"))
        except json.JSONDecodeError:
            continue
        if m.get("bIsIncompleteInstall") or "games" not in m.get("AppCategories", ["games"]):
            continue
        if m.get("MainGameAppName") and m.get("MainGameAppName") != m.get("AppName"):
            continue   # DLC
        ns, cat, app = m.get("CatalogNamespace"), m.get("CatalogItemId"), m.get("AppName")
        games.append(dict(
            id=f"epic:{app}", name=m.get("DisplayName", app), source="epic",
            install_dir=m.get("InstallLocation", ""),
            launch=f"com.epicgames.launcher://apps/{ns}%3A{cat}%3A{app}?action=launch&silent=true",
            local_art={}, remote_art={}, last_played=0,
        ))
    return games


# ============================================================ GOG
def gog_games():
    games = []
    if winreg is None:
        return games
    for gid, v in reg_subkeys(winreg.HKEY_LOCAL_MACHINE, r"SOFTWARE\GOG.com\Games").items():
        exe, path = v.get("exe") or v.get("launchCommand"), v.get("path", "")
        if not exe or not v.get("gameName"):
            continue
        games.append(dict(id=f"gog:{gid}", name=v["gameName"], source="gog", install_dir=path,
                          launch=exe, local_art={}, remote_art={}, last_played=0))
    return games


# ============================================================ Ubisoft
def ubisoft_games():
    games = []
    if winreg is None:
        return games
    for uid, v in reg_subkeys(winreg.HKEY_LOCAL_MACHINE, r"SOFTWARE\Ubisoft\Launcher\Installs").items():
        d = v.get("InstallDir")
        if not d or not Path(d).exists():
            continue
        games.append(dict(id=f"ubi:{uid}", name=Path(d.rstrip("/\\")).name, source="ubisoft",
                          install_dir=d, launch=f"uplay://launch/{uid}/0",
                          local_art={}, remote_art={}, last_played=0))
    return games


# ============================================================ Rockstar
ROCKSTAR_NOT_GAMES = {"launcher", "social club", "rockstar games social club", "steam", "epic"}


def rockstar_games():
    r"""Chaque jeu installé par le Rockstar Games Launcher a sa clé
    HKLM\SOFTWARE\(WOW6432Node\)Rockstar Games\<Titre> avec une valeur InstallFolder.
    Lancement : LauncherPatcher.exe -launchTitleInFolder "<dossier du jeu>"."""
    games = []
    if winreg is None:
        return games
    base = r"SOFTWARE\Rockstar Games"
    launcher_dir = reg_get(winreg.HKEY_LOCAL_MACHINE, base + r"\Launcher", "InstallFolder")
    patcher = Path(launcher_dir) / "LauncherPatcher.exe" if launcher_dir else None
    if not patcher or not patcher.exists():
        return games
    for title, v in reg_subkeys(winreg.HKEY_LOCAL_MACHINE, base).items():
        folder = v.get("InstallFolder")
        if title.lower() in ROCKSTAR_NOT_GAMES or not folder or not Path(folder).is_dir():
            continue
        games.append(dict(
            id=f"rockstar:{slug(title)}", name=title, source="rockstar", install_dir=folder,
            launch=str(patcher), launch_cmd=[str(patcher), "-launchTitleInFolder", folder],
            local_art={}, remote_art={}, last_played=0,
        ))
    return games


# ============================================================ jeux perso
def custom_games(store: Path):
    if not store.exists():
        return []
    out = []
    for g in json.loads(store.read_text("utf-8")):
        exe = Path(g["exe"])
        out.append(dict(id=f"custom:{slug(g['name'])}", name=g["name"], source="custom",
                        install_dir=str(exe.parent), launch=str(exe), args=g.get("args", ""),
                        local_art={"cover": g["cover"]} if g.get("cover") else {},
                        remote_art={}, last_played=0))
    return out


def add_custom(store: Path, name, exe, args="", cover=None):
    exe = Path(exe).resolve()
    if not exe.is_file() or exe.suffix.lower() not in (".exe", ".bat", ".lnk", ".url"):
        raise ValueError(f"fichier introuvable ou type non pris en charge : {exe}")
    items = json.loads(store.read_text("utf-8")) if store.exists() else []
    items = [g for g in items if slug(g["name"]) != slug(name)]
    items.append(dict(name=name, exe=str(exe), args=args, cover=cover))
    store.write_text(json.dumps(items, indent=2, ensure_ascii=False), "utf-8")
    return f"custom:{slug(name)}"


def remove_custom(store: Path, game_id):
    if not store.exists():
        return False
    items = json.loads(store.read_text("utf-8"))
    kept = [g for g in items if f"custom:{slug(g['name'])}" != game_id]
    store.write_text(json.dumps(kept, indent=2, ensure_ascii=False), "utf-8")
    return len(kept) != len(items)


# ============================================================ catalogue
class Library:
    """Cache des jeux détectés + lancement / suivi du jeu en cours."""

    def __init__(self, custom_store: Path):
        self.custom_store = custom_store
        self.games, self.scanned_at = {}, 0.0
        self.current = None            # id du dernier jeu lancé par l'agent

    def scan(self, force=False):
        if force or not self.games or time.time() - self.scanned_at > 60:
            found = []
            for fn in (steam_games, epic_games, gog_games, ubisoft_games, rockstar_games,
                       lambda: custom_games(self.custom_store)):
                try:
                    found += fn()
                except Exception as e:             # un launcher cassé ne bloque pas les autres
                    print(f"[jeux] erreur de détection ({fn}) : {e}", flush=True)
            # Un jeu Rockstar acheté sur Steam/Epic apparaît aussi dans le registre Rockstar :
            # on garde la version Steam/Epic (lancée par son propre launcher).
            norm = lambda d: os.path.normcase(os.path.abspath(d)) if d else ""  # noqa: E731
            others = {norm(g["install_dir"]) for g in found if g["source"] != "rockstar"}
            found = [g for g in found if g["source"] != "rockstar" or norm(g["install_dir"]) not in others]
            self.games = {g["id"]: g for g in sorted(found, key=lambda g: g["name"].lower())}
            self.scanned_at = time.time()
        return self.games

    def public_list(self):
        out = []
        for g in self.scan().values():
            out.append(dict(id=g["id"], name=g["name"], source=g["source"],
                            art={k: f"/games/{g['id']}/art/{k}" for k in g["local_art"]},
                            remote_art=g["remote_art"], last_played=g["last_played"]))
        return out

    def get(self, game_id):
        return self.scan().get(game_id)

    def launch(self, game_id):
        g = self.get(game_id)
        if not g:
            raise KeyError(game_id)
        if g.get("launch_cmd"):
            import subprocess
            subprocess.Popen(g["launch_cmd"])
        elif g.get("args"):
            import subprocess
            subprocess.Popen(f'"{g["launch"]}" {g["args"]}', cwd=g["install_dir"] or None)
        else:
            os.startfile(g["launch"])   # gère les .exe ET les URI (steam://, uplay://...)
        self.current = game_id
        return g

    def _game_processes(self, g):
        import psutil
        root = os.path.normcase(os.path.abspath(g["install_dir"])) if g["install_dir"] else None
        if not root or len(root) < 8:      # évite de considérer "C:\" comme un dossier de jeu
            return []
        procs = []
        for p in psutil.process_iter(["exe"]):
            exe = p.info.get("exe")
            if exe and os.path.normcase(exe).startswith(root + os.sep):
                procs.append(p)
        return procs

    def running(self):
        """Jeu en cours : Steam le dit directement, sinon on cherche un processus
        dont l'exécutable est dans le dossier du jeu."""
        appid = steam_running_appid()
        if appid:
            return f"steam:{appid}"
        if self.current:
            g = self.get(self.current)
            if g and self._game_processes(g):
                return self.current
        return None

    def stop(self, game_id):
        g = self.get(game_id)
        if not g:
            raise KeyError(game_id)
        procs = self._game_processes(g)
        for p in procs:
            try:
                p.terminate()
            except Exception:
                pass
        return len(procs)
