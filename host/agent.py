"""Agent du PC FIXE : API HTTPS que le portable (client CLI aujourd'hui, appli React demain)
utilise pour s'appairer, installer les dépendances, configurer et piloter les sessions.

Lancement manuel (pour tester) :   py agent.py
Lancement normal                :   tâche planifiée créée par host_setup.ps1 (au démarrage de session)
Gestion des appareils appairés  :   py agent.py --list-clients   |   py agent.py --revoke <id>

L'agent DOIT tourner dans ta session Windows (pas comme service) : la capture d'écran
et l'injection clavier/souris ne marchent que dans la session de l'utilisateur.
"""
import argparse
import datetime
import importlib.util
import json
import os
import re
import secrets
import shutil
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

BASE = Path(__file__).resolve().parent
DATA = Path(os.environ.get("APPDATA", str(BASE))) / "CloudGamingAgent"
LOGS = DATA / "logs"
LOGS.mkdir(parents=True, exist_ok=True)

# Lancé avec pythonw (sans console) : stdout/stderr n'existent pas -> on écrit dans un fichier
if sys.stdout is None or sys.stderr is None:
    _log = open(LOGS / "agent.log", "a", buffering=1, encoding="utf-8")
    sys.stdout = sys.stderr = _log

import uvicorn  # noqa: E402
from fastapi import Depends, FastAPI, Header, HTTPException, Request  # noqa: E402
from fastapi.responses import FileResponse  # noqa: E402
from pydantic import BaseModel  # noqa: E402

import games as gamelib  # noqa: E402
import security  # noqa: E402

VERSION = "0.1.0"
SERVICE_TYPE = "_cloudgaming._tcp.local."
WINDOWS = os.name == "nt"
NO_WINDOW = 0x08000000 if WINDOWS else 0          # CREATE_NO_WINDOW
HEARTBEAT_TIMEOUT = 15                             # s sans signe de vie du portable -> fin de session

DEFAULT_CONFIG = {
    "name": socket.gethostname(),
    "api_port": 47800,
    "ports": {"video": 5000, "input": 5001, "audio": 5002},
    "video": {"fps": 60, "bitrate_mbps": 20, "codec": "h264", "scale": "1920x1080"},
}


def log(msg):
    print(f"{datetime.datetime.now():%H:%M:%S} {msg}", flush=True)


# ================================================================ configuration
def load_config():
    path = DATA / "config.json"
    cfg = json.loads(json.dumps(DEFAULT_CONFIG))
    if path.exists():
        saved = json.loads(path.read_text("utf-8"))
        for k, v in saved.items():
            if isinstance(v, dict) and isinstance(cfg.get(k), dict):
                cfg[k].update(v)
            else:
                cfg[k] = v
    return cfg


def save_config(cfg):
    (DATA / "config.json").write_text(json.dumps(cfg, indent=2), "utf-8")


def host_id():
    p = DATA / "host_id"
    if not p.exists():
        p.write_text(secrets.token_hex(6))
    return p.read_text().strip()


# ================================================================ dépendances
def find_ffmpeg():
    found = shutil.which("ffmpeg")
    if found:
        return found
    # La tâche planifiée n'a pas toujours le PATH à jour après une installation winget
    links = Path(os.environ.get("LOCALAPPDATA", "")) / "Microsoft" / "WinGet" / "Links" / "ffmpeg.exe"
    return str(links) if links.exists() else None


def vigem_installed():
    if not WINDOWS:
        return False
    r = subprocess.run(["sc", "query", "ViGEmBus"], capture_output=True, creationflags=NO_WINDOW)
    return r.returncode == 0


def deps_status():
    mod = lambda m: importlib.util.find_spec(m) is not None  # noqa: E731
    return {
        "ffmpeg": find_ffmpeg() is not None,
        "vigembus": vigem_installed(),
        "vgamepad": mod("vgamepad"),
        "pyaudiowpatch": mod("pyaudiowpatch"),
        "numpy": mod("numpy"),
        "psutil": mod("psutil"),
    }


# Liste FERMÉE de ce que le portable peut faire installer : aucune commande libre.
INSTALLABLE = {
    "ffmpeg": ["winget", "install", "-e", "--id", "Gyan.FFmpeg", "--silent",
               "--accept-package-agreements", "--accept-source-agreements"],
    "vigembus": ["winget", "install", "-e", "--id", "ViGEm.ViGEmBus", "--silent",
                 "--accept-package-agreements", "--accept-source-agreements"],
    "python-deps": [sys.executable.replace("pythonw.exe", "python.exe"), "-m", "pip", "install",
                    "--upgrade", "vgamepad", "PyAudioWPatch", "numpy", "psutil"],
}


class Installer:
    def __init__(self):
        self.lock = threading.Lock()
        self.jobs = {}   # nom -> {state, returncode, started, finished}

    def start(self, name):
        if name not in INSTALLABLE:
            raise HTTPException(404, f"composant inconnu : {name}")
        with self.lock:
            if self.jobs.get(name, {}).get("state") == "running":
                raise HTTPException(409, "installation déjà en cours")
            self.jobs[name] = dict(state="running", returncode=None,
                                   started=time.time(), finished=None)
        threading.Thread(target=self._run, args=(name,), daemon=True).start()

    def _run(self, name):
        logf = LOGS / f"install_{name}.log"
        log(f"[install] {name} : démarrage")
        try:
            with open(logf, "w", encoding="utf-8", errors="replace") as f:
                rc = subprocess.run(INSTALLABLE[name], stdout=f, stderr=subprocess.STDOUT,
                                    creationflags=NO_WINDOW).returncode
        except FileNotFoundError as e:
            rc = -1
            logf.write_text(f"Programme introuvable : {e}", "utf-8")
        with self.lock:
            self.jobs[name].update(state="ok" if rc == 0 else "failed",
                                   returncode=rc, finished=time.time())
        log(f"[install] {name} : code {rc}")

    def status(self):
        out = {}
        with self.lock:
            for name, j in self.jobs.items():
                tail = ""
                p = LOGS / f"install_{name}.log"
                if p.exists():
                    tail = "\n".join(p.read_text("utf-8", errors="replace").splitlines()[-8:])
                out[name] = dict(j, log_tail=tail)
        return out


# ================================================================ sessions
def build_commands(client_ip, cfg):
    v, ports = cfg["video"], cfg["ports"]
    ffmpeg = find_ffmpeg()
    if not ffmpeg:
        raise HTTPException(412, "FFmpeg n'est pas installé sur le PC fixe (POST /install/ffmpeg)")
    py = sys.executable.replace("pythonw.exe", "python.exe")
    enc = "hevc_nvenc" if v["codec"] == "hevc" else "h264_nvenc"
    br = f"{v['bitrate_mbps']}M"
    vf = []
    if v.get("scale"):
        w, h = v["scale"].split("x")
        vf = ["-vf", f"hwdownload,format=bgra,scale={w}:{h},format=nv12"]
    video = [ffmpeg, "-hide_banner", "-loglevel", "warning", "-stats",
             "-f", "lavfi", "-i", f"ddagrab=framerate={v['fps']}", *vf,
             "-c:v", enc, "-preset", "p1", "-tune", "ull", "-zerolatency", "1",
             "-delay", "0", "-bf", "0", "-rc", "cbr", "-b:v", br, "-maxrate", br,
             "-bufsize", "1M", "-g", str(v["fps"] * 2),
             "-flush_packets", "1", "-muxdelay", "0", "-muxpreload", "0",
             "-f", "mpegts", f"udp://{client_ip}:{ports['video']}?pkt_size=1316&buffer_size=8388608"]
    audio = [py, str(BASE / "audio_sender.py"), client_ip, "--port", str(ports["audio"])]
    inputs = [py, str(BASE / "input_receiver.py"), "--allow", client_ip, "--port", str(ports["input"])]
    return {"video": video, "audio": audio, "input": inputs}


class SessionManager:
    def __init__(self):
        self.lock = threading.Lock()
        self.procs = {}
        self.client_ip = self.client_id = None
        self.started = self.last_heartbeat = 0.0
        self.end_reason = None
        threading.Thread(target=self._watchdog, daemon=True).start()

    def start(self, client_ip, client_id, cfg):
        cmds = build_commands(client_ip, cfg)
        with self.lock:
            self._stop_locked("remplacée par une nouvelle session")
            env = dict(os.environ, PYTHONUNBUFFERED="1", PYTHONIOENCODING="utf-8")
            for name, cmd in cmds.items():
                f = open(LOGS / f"session_{name}.log", "w", encoding="utf-8", errors="replace")
                self.procs[name] = (subprocess.Popen(cmd, cwd=BASE, stdout=f, stderr=subprocess.STDOUT,
                                                     env=env, creationflags=NO_WINDOW), f)
            self.client_ip, self.client_id = client_ip, client_id
            self.started = self.last_heartbeat = time.time()
            self.end_reason = None
        log(f"[session] démarrée vers {client_ip}")

    def heartbeat(self, client_id):
        with self.lock:
            if self.procs and client_id == self.client_id:
                self.last_heartbeat = time.time()
                return True
        return False

    def stop(self, reason="arrêt demandé"):
        with self.lock:
            self._stop_locked(reason)

    def _stop_locked(self, reason):
        if not self.procs:
            return
        for name, (p, f) in self.procs.items():
            if p.poll() is None:
                p.terminate()
        for name, (p, f) in self.procs.items():
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()
            f.close()
        self.procs.clear()
        self.end_reason = reason
        log(f"[session] arrêtée : {reason}")

    def info(self):
        with self.lock:
            procs = {}
            for name, (p, _) in self.procs.items():
                rc = p.poll()
                procs[name] = "en cours" if rc is None else f"arrêté (code {rc})"
            return dict(active=bool(self.procs), client_ip=self.client_ip if self.procs else None,
                        since=self.started if self.procs else None, processes=procs,
                        last_end_reason=self.end_reason)

    def _watchdog(self):
        while True:
            time.sleep(2)
            with self.lock:
                if self.procs and time.time() - self.last_heartbeat > HEARTBEAT_TIMEOUT:
                    self._stop_locked("le portable ne répond plus")


# ================================================================ mDNS
def lan_ipv4s():
    import ifaddr  # dépendance de zeroconf
    ips = []
    for adapter in ifaddr.get_adapters():
        for ip in adapter.ips:
            if isinstance(ip.ip, str) and not ip.ip.startswith(("127.", "169.254.")):
                ips.append(ip.ip)
    return ips


def advertise(cfg, fingerprint):
    from zeroconf import ServiceInfo, Zeroconf
    ips = lan_ipv4s()
    safe = re.sub(r"[^A-Za-z0-9-]", "-", cfg["name"])[:40]
    info = ServiceInfo(
        SERVICE_TYPE, f"{safe}.{SERVICE_TYPE}",
        addresses=[socket.inet_aton(ip) for ip in ips], port=cfg["api_port"],
        properties={"id": host_id(), "name": cfg["name"], "ver": VERSION, "fp": fingerprint[:16]},
        server=f"{safe}.local.",
    )
    zc = Zeroconf()
    zc.register_service(info)
    log(f"[mDNS] annoncé sur {', '.join(ips)} port {cfg['api_port']}")
    return zc, info


# ================================================================ API
class ExchangeIn(BaseModel):
    session_id: str
    msg_a: str            # hex


class ConfirmIn(BaseModel):
    session_id: str
    client_name: str
    confirm: str          # hex


class VideoPatch(BaseModel):
    fps: int | None = None
    bitrate_mbps: int | None = None
    codec: str | None = None
    scale: str | None = None    # "1920x1080", ou "" pour la résolution native


def create_app(cfg, pairing, fingerprint):
    app = FastAPI(title="Cloud gaming agent", version=VERSION)
    sessions, installer = SessionManager(), Installer()
    library = gamelib.Library(DATA / "custom_games.json")
    pin_proc = {"p": None}

    def auth(authorization: str = Header(default="")):
        token = authorization.removeprefix("Bearer ").strip()
        cid = pairing.check(token) if token else None
        if not cid:
            raise HTTPException(401, "appareil non appairé")
        return cid

    # ---- public
    @app.get("/info")
    def info():
        return dict(name=cfg["name"], id=host_id(), version=VERSION, fingerprint=fingerprint)

    @app.post("/pair/begin")
    def pair_begin():
        try:
            return {"session_id": pairing.begin(), "expires_in": security.PIN_LIFETIME}
        except security.PairingError as e:
            raise HTTPException(429, str(e))

    @app.post("/pair/exchange")
    def pair_exchange(body: ExchangeIn):
        try:
            return {"msg_b": pairing.exchange(body.session_id, bytes.fromhex(body.msg_a)).hex()}
        except (security.PairingError, ValueError) as e:
            raise HTTPException(400, str(e))

    @app.post("/pair/confirm")
    def pair_confirm(body: ConfirmIn, request: Request):
        try:
            cid, token, server_confirm = pairing.confirm(body.session_id, body.client_name, body.confirm)
        except security.PairingError as e:
            log(f"[appairage] refusé depuis {request.client.host} : {e}")
            raise HTTPException(403, str(e))
        log(f"[appairage] OK : « {body.client_name} » ({request.client.host}) -> id {cid}")
        return dict(client_id=cid, token=token, server_confirm=server_confirm)

    # ---- appareils appairés uniquement
    @app.delete("/pair")
    def unpair(cid: str = Depends(auth)):
        pairing.revoke(cid)
        return {"ok": True}

    @app.get("/status")
    def status(cid: str = Depends(auth)):
        try:
            running = library.running()
        except Exception:
            running = None
        return dict(name=cfg["name"], version=VERSION, deps=deps_status(),
                    session=sessions.info(), installs=installer.status(), running_game=running)

    @app.get("/config")
    def get_config(cid: str = Depends(auth)):
        return cfg

    @app.patch("/config/video")
    def patch_video(body: VideoPatch, cid: str = Depends(auth)):
        v = cfg["video"]
        if body.fps is not None:
            if not 24 <= body.fps <= 144:
                raise HTTPException(422, "fps entre 24 et 144")
            v["fps"] = body.fps
        if body.bitrate_mbps is not None:
            if not 2 <= body.bitrate_mbps <= 100:
                raise HTTPException(422, "débit entre 2 et 100 Mbps")
            v["bitrate_mbps"] = body.bitrate_mbps
        if body.codec is not None:
            if body.codec not in ("h264", "hevc"):
                raise HTTPException(422, "codec : h264 ou hevc")
            v["codec"] = body.codec
        if body.scale is not None:
            if body.scale and not re.fullmatch(r"\d{3,4}x\d{3,4}", body.scale):
                raise HTTPException(422, "scale au format 1920x1080, ou vide pour natif")
            v["scale"] = body.scale or None
        save_config(cfg)
        return v

    @app.post("/install/{component}")
    def install(component: str, cid: str = Depends(auth)):
        installer.start(component)
        return {"started": component}

    @app.post("/session/start")
    def session_start(request: Request, cid: str = Depends(auth)):
        sessions.start(request.client.host, cid, cfg)
        return dict(ports=cfg["ports"], video=cfg["video"])

    @app.post("/session/heartbeat")
    def session_heartbeat(cid: str = Depends(auth)):
        if not sessions.heartbeat(cid):
            raise HTTPException(409, "aucune session active pour cet appareil")
        return sessions.info()

    @app.post("/session/stop")
    def session_stop(cid: str = Depends(auth)):
        sessions.stop("arrêt demandé par le portable")
        return sessions.info()

    @app.get("/session")
    def session_info(cid: str = Depends(auth)):
        return sessions.info()

    # ---- jeux
    @app.get("/games")
    def games_list(refresh: bool = False, cid: str = Depends(auth)):
        if refresh:
            library.scan(force=True)
        return library.public_list()

    @app.get("/games/running")
    def games_running(cid: str = Depends(auth)):
        return {"running": library.running()}

    @app.get("/games/{game_id}/art/{kind}")
    def game_art(game_id: str, kind: str, cid: str = Depends(auth)):
        g = library.get(game_id)
        path = g and g["local_art"].get(kind)
        if not path or not Path(path).is_file():
            raise HTTPException(404, "pas d'image locale")
        return FileResponse(path, headers={"Cache-Control": "max-age=86400"})

    @app.post("/games/{game_id}/launch")
    def game_launch(game_id: str, cid: str = Depends(auth)):
        try:
            g = library.launch(game_id)
        except KeyError:
            raise HTTPException(404, "jeu inconnu")
        except OSError as e:
            raise HTTPException(500, f"lancement impossible : {e}")
        log(f"[jeux] lancement : {g['name']}")
        return {"launched": game_id, "name": g["name"]}

    @app.post("/games/{game_id}/stop")
    def game_stop(game_id: str, cid: str = Depends(auth)):
        try:
            n = library.stop(game_id)
        except KeyError:
            raise HTTPException(404, "jeu inconnu")
        log(f"[jeux] arrêt de {game_id} : {n} processus")
        return {"stopped": n}

    app.state.pin_proc = pin_proc
    app.state.sessions = sessions
    return app


# ================================================================ main
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--list-clients", action="store_true")
    ap.add_argument("--revoke", metavar="ID")
    ap.add_argument("--no-mdns", action="store_true")
    ap.add_argument("--list-games", action="store_true", help="affiche les jeux détectés")
    ap.add_argument("--add-game", nargs=2, metavar=("NOM", "EXE"), help="ajoute un jeu perso")
    ap.add_argument("--args", default="", help="arguments pour --add-game")
    ap.add_argument("--cover", help="image de couverture pour --add-game")
    ap.add_argument("--remove-game", metavar="ID")
    args = ap.parse_args()

    cfg = load_config()
    save_config(cfg)
    cert, key, fp = security.ensure_cert(DATA, cfg["name"])

    pin_proc = {"p": None}

    def show_pin(pin, seconds):
        log(f"[appairage] PIN affiché : {pin}")
        popup = BASE / "pin_popup.py"
        pyw = sys.executable.replace("python.exe", "pythonw.exe")
        try:
            pin_proc["p"] = subprocess.Popen([pyw, str(popup), pin, str(seconds)])
        except Exception as e:
            log(f"[appairage] impossible d'afficher la fenêtre du PIN : {e}")

    def hide_pin():
        p = pin_proc["p"]
        if p and p.poll() is None:
            p.terminate()
        pin_proc["p"] = None

    pairing = security.Pairing(DATA / "clients.json", fp, show_pin, hide_pin)

    if args.list_clients:
        for cid, c in pairing.clients.items():
            print(f"{cid}  {c['name']}  (appairé le {c['paired_at']})")
        return
    if args.revoke:
        print("révoqué" if pairing.revoke(args.revoke) else "id inconnu")
        return
    store = DATA / "custom_games.json"
    if args.add_game:
        print("ajouté :", gamelib.add_custom(store, args.add_game[0], args.add_game[1], args.args, args.cover))
        return
    if args.remove_game:
        print("supprimé" if gamelib.remove_custom(store, args.remove_game) else "id inconnu")
        return
    if args.list_games:
        for g in gamelib.Library(store).scan().values():
            print(f"  {g['id']:<28} {g['name']}")
        return

    log(f"[agent] v{VERSION}  données : {DATA}")
    log(f"[agent] empreinte du certificat : {fp}")
    zc = None
    if not args.no_mdns:
        try:
            zc, _ = advertise(cfg, fp)
        except Exception as e:
            log(f"[mDNS] désactivé : {e}")

    app = create_app(cfg, pairing, fp)
    try:
        uvicorn.run(app, host="0.0.0.0", port=cfg["api_port"],
                    ssl_certfile=cert, ssl_keyfile=key, log_level="warning")
    finally:
        app.state.sessions.stop("arrêt de l'agent")
        if zc:
            zc.unregister_all_services()
            zc.close()


if __name__ == "__main__":
    main()
