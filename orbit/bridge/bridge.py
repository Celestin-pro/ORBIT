"""Pont local ORBIT (PC PORTABLE).

L'interface React (dans Tauri) ne parle qu'à ce pont, en HTTP sur 127.0.0.1.
Le pont fait tout le travail « système » avec le code déjà validé :
  - découverte mDNS, appairage SPAKE2, HTTPS épinglé vers l'agent (cgclient.py)
  - réveil du PC fixe (Wake-on-LAN)
  - lancement local de ffplay + input_sender + audio_receiver pendant une session

Lancement : géré par Tauri. Pour développer sans Tauri :
    python bridge.py --token dev          (puis npm run dev dans le dossier orbit)
"""
import argparse
import ctypes
import os
import secrets
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

import uvicorn
from fastapi import Depends, FastAPI, Header, HTTPException, Query
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse
from pydantic import BaseModel

import cgclient as cg

HERE = Path(__file__).resolve().parent
ART_CACHE = cg.STORE.parent / "art"
WINDOWS = os.name == "nt"
NO_WINDOW = 0x08000000 if WINDOWS else 0
TOKEN = "dev"


def auth(x_orbit_token: str = Header(default=""), t: str = Query(default="")):
    # Jeton en en-tête (fetch) ou en paramètre ?t= (balises <img>, qui ne savent pas envoyer d'en-tête)
    if not secrets.compare_digest(x_orbit_token or t, TOKEN):
        raise HTTPException(401, "jeton du pont invalide")


def host_entry(hid):
    h = cg.load_hosts().get(hid)
    if not h:
        raise HTTPException(404, "PC inconnu")
    return h


def call(hid, method, path, body=None, timeout=10):
    try:
        return cg.agent_for(host_entry(hid)).request(method, path, body, timeout=timeout)
    except cg.AgentError as e:
        raise HTTPException(502 if e.status in (0, 500) else e.status, e.detail)
    except OSError as e:
        raise HTTPException(503, f"PC fixe injoignable : {e}")


# ======================================================== lecteur vidéo
def _fresh_path():
    """PATH relu dans le registre : un logiciel installé APRÈS l'ouverture du terminal
    (ou de l'appli) n'apparaît pas dans le PATH hérité par ce processus."""
    parts = [os.environ.get("PATH", "")]
    if WINDOWS:
        import winreg
        for root, key in ((winreg.HKEY_LOCAL_MACHINE,
                           r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment"),
                          (winreg.HKEY_CURRENT_USER, r"Environment")):
            try:
                with winreg.OpenKey(root, key) as k:
                    parts.append(os.path.expandvars(winreg.QueryValueEx(k, "Path")[0]))
            except OSError:
                pass
    return os.pathsep.join(parts)


def find_mpv():
    """mpv = décodage GPU (D3D11VA) + profil faible latence. ffplay en secours.
    Ordre : variable ORBIT_MPV, PATH (relu dans le registre), dossiers d'installation courants."""
    import shutil
    forced = os.environ.get("ORBIT_MPV")
    if forced and Path(forced).is_file():
        return forced
    found = shutil.which("mpv", path=_fresh_path())
    if found:
        return found
    env = os.environ.get
    candidates = []
    for base in (env("ProgramFiles", r"C:\Program Files"), env("ProgramFiles(x86)", ""),
                 env("LOCALAPPDATA", ""), env("LOCALAPPDATA", "") and str(Path(env("LOCALAPPDATA")) / "Programs")):
        if base:
            candidates += [Path(base) / "mpv" / "mpv.exe", Path(base) / "MPV Player" / "mpv.exe"]
    if env("USERPROFILE"):
        candidates.append(Path(env("USERPROFILE")) / "scoop" / "apps" / "mpv" / "current" / "mpv.exe")
    for c in candidates:
        if c.is_file():
            return str(c)
    # winget en mode "portable" : %LOCALAPPDATA%\Microsoft\WinGet\Packages\shinchiro.mpv_...\mpv.exe
    pkgs = Path(env("LOCALAPPDATA", "")) / "Microsoft" / "WinGet" / "Packages"
    if pkgs.is_dir():
        for d in pkgs.glob("*mpv*"):
            hits = list(d.rglob("mpv.exe"))
            if hits:
                return str(hits[0])
    return None


def player_command(video_cfg, port, title, fullscreen):
    """Commande du lecteur + nom du lecteur. Le portable ÉCOUTE, le fixe envoie."""
    if video_cfg.get("transport") == "srt":
        lat_us = int(video_cfg.get("srt_latency_ms", 40)) * 1000
        url = f"srt://0.0.0.0:{port}?mode=listener&latency={lat_us}&transtype=live"
    else:
        url = f"udp://0.0.0.0:{port}?fifo_size=5000000&overrun_nonfatal=1&buffer_size=8388608"
    mpv = find_mpv()
    if mpv:
        return [
            mpv, "--no-config", f"--input-conf={HERE / 'mpv-input.conf'}",
            "--profile=low-latency",            # pas d'interpolation, tampons minimaux
            "--untimed",                        # affiche chaque image dès qu'elle est décodée
            "--no-cache", "--hwdec=auto-safe",  # décodage par la carte graphique
            "--demuxer-lavf-o-add=fflags=+nobuffer", "--demuxer-lavf-analyzeduration=0.1",
            "--no-audio", "--osc=no", "--osd-level=0", "--force-window=immediate",
            f"--title={title}",
            *(["--fs"] if fullscreen else ["--geometry=1280x720"]),
            url,
        ], "mpv"
    return [
        "ffplay", "-hide_banner", "-loglevel", "error", "-window_title", title,
        "-fflags", "nobuffer", "-flags", "low_delay", "-probesize", "32",
        "-analyzeduration", "0", "-framedrop", "-vf", "setpts=0", "-f", "mpegts",
        *(["-fs"] if fullscreen else ["-x", "1280", "-y", "720"]), url,
    ], "ffplay"


# ======================================================== Wake-on-LAN
def wake_on_lan(macs):
    """Paquet magique : 6 x FF puis 16 x l'adresse MAC, en broadcast UDP (ports 9 et 7)."""
    sent = 0
    for mac in macs:
        raw = bytes.fromhex(mac.replace("-", "").replace(":", ""))
        if len(raw) != 6:
            continue
        packet = b"\xff" * 6 + raw * 16
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
            for port in (9, 7):
                s.sendto(packet, ("255.255.255.255", port))
        sent += 1
    return sent


def ping_agent(h, timeout=1.5):
    """(en ligne ?, latence ms) en interrogeant /info."""
    t0 = time.perf_counter()
    try:
        cg.Agent(h["host"], h["port"], h["fingerprint"]).request("GET", "/info", timeout=timeout)
        return True, round((time.perf_counter() - t0) * 1000)
    except Exception:
        return False, None


# ======================================================== session
STEP_NAMES = {"wake": "Réveil du PC fixe", "stream": "Ouverture du lecteur vidéo",
              "agent": "Démarrage du flux sur le PC fixe", "game": "Lancement du jeu"}


class Runner:
    """Déroule une session et expose son avancement à l'interface (écran de lancement)."""

    def __init__(self):
        self.lock = threading.Lock()
        self.procs = {}
        self.stop_evt = threading.Event()
        self.thread = None
        self._reset()

    def _reset(self):
        self.state = dict(phase="idle", host_id=None, host_name=None, game=None,
                          steps=[], message=None, started=None, warnings=[])

    def snapshot(self):
        with self.lock:
            return {k: (list(v) if isinstance(v, list) else v) for k, v in self.state.items()}

    def _step(self, key, status, detail=None):
        with self.lock:
            for s in self.state["steps"]:
                if s["key"] == key:
                    s.update(status=status, detail=detail)

    def start(self, hid, game_id, fullscreen):
        with self.lock:
            if self.state["phase"] not in ("idle", "error", "ended"):
                raise HTTPException(409, "une session est déjà en cours")
        h = host_entry(hid)
        game = None
        if game_id:
            games = call(hid, "GET", "/games")
            game = next((g for g in games if g["id"] == game_id), None)
            if not game:
                raise HTTPException(404, "jeu introuvable sur le PC fixe")
        keys = ["wake", "stream", "agent"] + (["game"] if game else [])
        with self.lock:
            self._reset()
            self.state.update(phase="starting", host_id=hid, host_name=h["name"], game=game,
                              steps=[dict(key=k, label=STEP_NAMES[k], status="pending", detail=None)
                                     for k in keys])
        self.stop_evt.clear()
        self.thread = threading.Thread(target=self._run, args=(hid, h, game, fullscreen), daemon=True)
        self.thread.start()

    def _fail(self, key, msg):
        self._step(key, "error", msg)
        with self.lock:
            self.state.update(phase="error", message=msg)
        self._teardown(h=None)

    def _run(self, hid, h, game, fullscreen):
        agent = cg.agent_for(h)
        # 1. réveil
        self._step("wake", "active")
        online, _ = ping_agent(h)
        if online:
            self._step("wake", "done", "déjà allumé")
        else:
            macs = h.get("macs") or []
            if not macs:
                return self._fail("wake", "PC fixe éteint ou injoignable (adresse MAC inconnue : "
                                          "ré-appaire le PC pour activer le réveil à distance)")
            wake_on_lan(macs)
            self._step("wake", "active", "paquet de réveil envoyé, démarrage du PC...")
            t0 = time.time()
            while not online and time.time() - t0 < 120 and not self.stop_evt.is_set():
                time.sleep(3)
                wake_on_lan(macs)
                online, _ = ping_agent(h)
            if not online:
                return self._fail("wake", "le PC fixe ne s'est pas réveillé (Wake-on-LAN activé dans le BIOS ?)")
            self._step("wake", "done", f"réveillé en {int(time.time() - t0)} s")
        if self.stop_evt.is_set():
            return self._teardown(h)

        # 2. lecteur vidéo d'abord : il doit écouter AVANT que le fixe commence à envoyer
        self._step("stream", "active")
        try:
            cfg = agent.request("GET", "/config")
        except Exception as e:
            return self._fail("stream", f"configuration illisible : {getattr(e, 'detail', e)}")
        ports, v = cfg["ports"], cfg["video"]
        cmd, player = player_command(v, ports["video"], f"ORBIT – {h['name']}", fullscreen)
        try:
            self.procs["video"] = subprocess.Popen(cmd, cwd=HERE)
        except FileNotFoundError:
            return self._fail("stream", "aucun lecteur vidéo : installe mpv "
                                        "(winget install --id shinchiro.mpv --exact)")

        # 3. session côté agent
        self._step("agent", "active")
        try:
            agent.request("POST", "/session/start")
        except Exception as e:
            return self._fail("agent", f"l'agent a refusé : {getattr(e, 'detail', e)}")
        self._step("agent", "done", f"{v['codec'].upper()} · {v['bitrate_mbps']} Mbps · {v['fps']} fps · "
                                    f"{v.get('transport', 'udp').upper()}")

        py = sys.executable.replace("pythonw.exe", "python.exe")
        try:
            self.procs["audio"] = subprocess.Popen(
                [py, str(HERE / "audio_receiver.py"), "--allow", h["host"], "--port", str(ports["audio"])],
                cwd=HERE, creationflags=NO_WINDOW)
            self.procs["input"] = subprocess.Popen(
                [py, str(HERE / "input_sender.py"), h["host"], "--port", str(ports["input"])],
                cwd=HERE, creationflags=NO_WINDOW)
        except FileNotFoundError as e:
            return self._fail("stream", f"programme introuvable : {e.filename}")
        time.sleep(1.0)
        if self.procs["video"].poll() is not None:
            return self._fail("stream", f"{player} s'est fermé tout de suite "
                                        "(transport SRT non pris en charge par ce lecteur ?)")
        self._step("stream", "done", f"{player} · {'décodage GPU' if player == 'mpv' else 'décodage processeur'}")

        # 4. jeu
        if game:
            self._step("game", "active")
            time.sleep(1.5)        # le flux s'ouvre d'abord : on voit le jeu démarrer
            try:
                agent.request("POST", f"/games/{game['id']}/launch")
                self._step("game", "done", game["name"])
            except Exception as e:
                self._step("game", "error", f"lancement impossible : {getattr(e, 'detail', e)}")

        with self.lock:
            self.state.update(phase="streaming", started=time.time())

        # 5. boucle de vie : heartbeat + surveillance de la fenêtre vidéo
        reported = set()
        while not self.stop_evt.wait(3):
            if self.procs["video"].poll() is not None:
                break          # fenêtre vidéo fermée par l'utilisateur
            try:
                st = agent.request("POST", "/session/heartbeat", timeout=5)
                for name, s in st["processes"].items():
                    if not s.startswith("en cours") and name not in reported:
                        reported.add(name)
                        with self.lock:
                            self.state["warnings"].append(
                                f"« {name} » arrêté sur le PC fixe : "
                                + (st.get("errors", {}).get(name, "").splitlines() or [s])[-1])
            except cg.AgentError as e:
                with self.lock:
                    self.state["warnings"].append(f"session terminée par le PC fixe : {e.detail}")
                break
            except OSError:
                with self.lock:
                    self.state["warnings"].append("PC fixe momentanément injoignable")
        self._teardown(h)

    def stop(self):
        self.stop_evt.set()
        with self.lock:
            if self.state["phase"] in ("starting", "streaming"):
                self.state["phase"] = "stopping"

    def _teardown(self, h):
        with self.lock:
            if self.state["phase"] not in ("error",):
                self.state["phase"] = "stopping"
        # Ordre : entrées d'abord (le fixe relâche tout faute de paquets), puis l'agent, puis le reste
        p = self.procs.get("input")
        if p and p.poll() is None:
            p.terminate()
        time.sleep(0.7)
        hid = self.state.get("host_id")
        if hid:
            try:
                cg.agent_for(host_entry(hid)).request("POST", "/session/stop", timeout=5)
            except Exception:
                pass      # l'agent s'arrêtera seul faute de heartbeat
        for p in self.procs.values():
            if p.poll() is None:
                p.terminate()
        self.procs.clear()
        with self.lock:
            if self.state["phase"] != "error":
                self.state["phase"] = "ended"


runner = Runner()


# ======================================================== API
class PairBegin(BaseModel):
    host: str
    port: int = 47800


class PairFinish(BaseModel):
    pair_id: str
    pin: str
    client_name: str | None = None


class PlayIn(BaseModel):
    game_id: str | None = None
    fullscreen: bool = True


class VideoPatch(BaseModel):
    fps: int | None = None
    bitrate_mbps: int | None = None
    codec: str | None = None
    scale: str | None = None
    transport: str | None = None
    srt_latency_ms: int | None = None


app = FastAPI(title="ORBIT bridge")
app.add_middleware(CORSMiddleware, allow_origins=["*"], allow_methods=["*"], allow_headers=["*"])
pairings = {}


@app.get("/health")
def health():
    return {"ok": True, "pc": socket.gethostname()}


@app.get("/player", dependencies=[Depends(auth)])
def player():
    mpv = find_mpv()
    return {"name": "mpv" if mpv else "ffplay", "path": mpv,
            "install_hint": None if mpv else "winget install --id shinchiro.mpv --exact"}


@app.get("/hosts", dependencies=[Depends(auth)])
def hosts():
    items = list(cg.load_hosts().items())
    results = [None] * len(items)

    def check(i, hid, h):
        online, ms = ping_agent(h)
        results[i] = dict(id=hid, name=h["name"], host=h["host"], online=online, latency_ms=ms,
                          can_wake=bool(h.get("macs")))

    threads = [threading.Thread(target=check, args=(i, hid, h)) for i, (hid, h) in enumerate(items)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return results


@app.get("/discover", dependencies=[Depends(auth)])
def discover(timeout: float = 3.0):
    paired = cg.load_hosts()
    return [dict(d, paired=d["id"] in paired) for d in cg.discover(min(timeout, 8))]


@app.post("/pair/begin", dependencies=[Depends(auth)])
def pair_begin(body: PairBegin):
    agent = cg.Agent(body.host, body.port)
    try:
        info = agent.request("GET", "/info")
        fp = agent.last_seen_fp
        sid = agent.request("POST", "/pair/begin")["session_id"]
    except cg.AgentError as e:
        raise HTTPException(e.status or 502, e.detail)
    except OSError as e:
        raise HTTPException(503, f"PC injoignable : {e}")
    pid = secrets.token_hex(8)
    pairings[pid] = dict(agent=agent, host=body.host, port=body.port, sid=sid, fp=fp, info=info,
                         created=time.time())
    return dict(pair_id=pid, name=info["name"], expires_in=120)


@app.post("/pair/finish", dependencies=[Depends(auth)])
def pair_finish(body: PairFinish):
    from spake2 import SPAKE2_A
    p = pairings.pop(body.pair_id, None)
    if not p or time.time() - p["created"] > 120:
        raise HTTPException(410, "appairage expiré : recommence")
    agent, fp = p["agent"], p["fp"]
    pin = body.pin.replace(" ", "").strip()
    if not (pin.isdigit() and len(pin) == 6):
        raise HTTPException(422, "le code fait 6 chiffres")
    try:
        a = SPAKE2_A(pin.encode())
        msg_b = bytes.fromhex(agent.request("POST", "/pair/exchange",
                                            {"session_id": p["sid"], "msg_a": a.start().hex()})["msg_b"])
        key = a.finish(msg_b)
        res = agent.request("POST", "/pair/confirm", {
            "session_id": p["sid"], "client_name": body.client_name or socket.gethostname(),
            "confirm": cg.confirm_tag(key, b"client", fp)})
    except cg.AgentError as e:
        raise HTTPException(403 if e.status == 403 else 502, e.detail)
    if not secrets.compare_digest(res["server_confirm"], cg.confirm_tag(key, b"server", fp)):
        raise HTTPException(403, "le PC n'a pas prouvé qu'il connaît le code")
    info = p["info"]
    hosts_ = cg.load_hosts()
    hosts_[info["id"]] = dict(name=info["name"], host=p["host"], port=p["port"], fingerprint=fp,
                              token=res["token"], client_id=res["client_id"], macs=info.get("macs", []))
    cg.save_hosts(hosts_)
    return dict(id=info["id"], name=info["name"])


@app.delete("/hosts/{hid}", dependencies=[Depends(auth)])
def unpair(hid: str):
    h = host_entry(hid)
    try:
        cg.agent_for(h).request("DELETE", "/pair", timeout=4)
    except Exception:
        pass
    hosts_ = cg.load_hosts()
    hosts_.pop(hid, None)
    cg.save_hosts(hosts_)
    return {"ok": True}


@app.post("/hosts/{hid}/wake", dependencies=[Depends(auth)])
def wake(hid: str):
    n = wake_on_lan(host_entry(hid).get("macs") or [])
    if not n:
        raise HTTPException(409, "adresse MAC inconnue : ré-appaire le PC")
    return {"sent": n}


@app.get("/hosts/{hid}/status", dependencies=[Depends(auth)])
def status(hid: str):
    return call(hid, "GET", "/status")


@app.get("/hosts/{hid}/config", dependencies=[Depends(auth)])
def config(hid: str):
    return call(hid, "GET", "/config")


@app.patch("/hosts/{hid}/config/video", dependencies=[Depends(auth)])
def patch_video(hid: str, body: VideoPatch):
    return call(hid, "PATCH", "/config/video", body.model_dump(exclude_none=True))


@app.post("/hosts/{hid}/install/{component}", dependencies=[Depends(auth)])
def install(hid: str, component: str):
    return call(hid, "POST", f"/install/{component}")


@app.get("/hosts/{hid}/games", dependencies=[Depends(auth)])
def games(hid: str, refresh: bool = False):
    return call(hid, "GET", f"/games{'?refresh=true' if refresh else ''}", timeout=30)


@app.get("/hosts/{hid}/games/{gid}/art/{kind}", dependencies=[Depends(auth)])
def art(hid: str, gid: str, kind: str):
    safe = "".join(c if c.isalnum() else "_" for c in f"{gid}_{kind}")
    path = ART_CACHE / hid / f"{safe}.jpg"
    if not path.exists():
        try:
            data = cg.agent_for(host_entry(hid)).request_raw(f"/games/{gid}/art/{kind}")
        except Exception:
            raise HTTPException(404, "pas d'image")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    return FileResponse(path, headers={"Cache-Control": "max-age=604800"})


@app.post("/hosts/{hid}/play", dependencies=[Depends(auth)])
def play(hid: str, body: PlayIn):
    runner.start(hid, body.game_id, body.fullscreen)
    return runner.snapshot()


@app.post("/hosts/{hid}/games/{gid}/stop", dependencies=[Depends(auth)])
def stop_game(hid: str, gid: str):
    return call(hid, "POST", f"/games/{gid}/stop")


@app.get("/session", dependencies=[Depends(auth)])
def session():
    return runner.snapshot()


@app.post("/session/stop", dependencies=[Depends(auth)])
def session_stop():
    runner.stop()
    return runner.snapshot()


@app.post("/session/dismiss", dependencies=[Depends(auth)])
def session_dismiss():
    with runner.lock:
        if runner.state["phase"] in ("ended", "error"):
            runner._reset()
    return runner.snapshot()


# ======================================================== main
def watch_parent(pid):
    """Quand l'appli Tauri se ferme, on termine proprement la session puis on quitte."""
    if WINDOWS:
        k32 = ctypes.windll.kernel32
        h = k32.OpenProcess(0x00100000, False, pid)          # SYNCHRONIZE
        if not h:
            return
        k32.WaitForSingleObject(h, 0xFFFFFFFF)
    else:
        while True:
            try:
                os.kill(pid, 0)
            except OSError:
                break
            time.sleep(1)
    runner.stop()
    if runner.thread:
        runner.thread.join(timeout=6)
    os._exit(0)


def main():
    global TOKEN
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=47900)
    ap.add_argument("--token", default=os.environ.get("ORBIT_TOKEN", "dev"))
    ap.add_argument("--parent-pid", type=int)
    args = ap.parse_args()
    TOKEN = args.token
    if args.parent_pid:
        threading.Thread(target=watch_parent, args=(args.parent_pid,), daemon=True).start()
    uvicorn.run(app, host="127.0.0.1", port=args.port, log_level="warning", ws="none")


if __name__ == "__main__":
    main()
