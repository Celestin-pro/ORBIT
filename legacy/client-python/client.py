"""Client en ligne de commande du PC PORTABLE.

    python client.py discover               liste les PC fixes détectés sur le réseau
    python client.py pair [N | IP]           s'appaire (le PIN s'affiche sur le PC fixe)
    python client.py list                    PC déjà appairés
    python client.py status                  état du PC fixe (dépendances, session)
    python client.py install all             installe FFmpeg, ViGEmBus, libs Python sur le fixe
    python client.py install ffmpeg          ... ou un composant précis
    python client.py video --fps 60 --bitrate 20 --codec h264 --scale 1920x1080
    python client.py games                   jeux détectés sur le PC fixe
    python client.py play                    lance une session complète (image + son + contrôles)
    python client.py play --game hades       ... et démarre directement un jeu (id ou nom)
    python client.py unpair                  oublie le PC

S'il y a plusieurs PC appairés : --host <nom ou id>.
"""
import argparse
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

import cgclient as cg

HERE = Path(__file__).resolve().parent


def pick_host(name):
    hosts = cg.load_hosts()
    if not hosts:
        sys.exit("Aucun PC appairé : lance d'abord  python client.py discover  puis  pair")
    if name:
        for hid, h in hosts.items():
            if name in (hid, h["name"], h["host"]):
                return hid, h
        sys.exit(f"PC '{name}' inconnu. Appairés : {', '.join(h['name'] for h in hosts.values())}")
    if len(hosts) > 1:
        sys.exit("Plusieurs PC appairés : précise --host <nom>")
    return next(iter(hosts.items()))


# ---------------------------------------------------------------- commandes
def cmd_discover(args):
    print("Recherche sur le réseau local (3 s)...")
    found = cg.discover()
    if not found:
        print("Aucun PC trouvé. Vérifie que l'agent tourne sur le fixe et que les deux réseaux "
              "sont en profil Privé.")
        return
    paired = {hid for hid in cg.load_hosts()}
    for i, h in enumerate(found, 1):
        mark = "  (déjà appairé)" if h["id"] in paired else ""
        print(f"  {i}. {h['name']}  {', '.join(h['addresses'])}:{h['port']}  v{h['version']}{mark}")


def cmd_pair(args):
    target, port = args.target, 47800
    if target is None or target.isdigit():
        found = cg.discover()
        if not found:
            sys.exit("Aucun PC détecté. Tu peux donner son IP directement : python client.py pair 192.168.1.107")
        idx = int(target) - 1 if target else 0
        if len(found) > 1 and target is None:
            for i, h in enumerate(found, 1):
                print(f"  {i}. {h['name']}  {', '.join(h['addresses'])}")
            idx = int(input("Numéro du PC : ")) - 1
        h = found[idx]
        target, port = h["addresses"][0], h["port"]
        print(f"Appairage avec {h['name']} ({target})...")
    name = args.name or socket.gethostname()
    hid, entry = cg.pair(target, port, name,
                         ask_pin=lambda: input("Code affiché sur le PC fixe : "))
    print(f"Appairé avec « {entry['name']} ». Empreinte : {entry['fingerprint'][:16]}…")


def cmd_list(args):
    for hid, h in cg.load_hosts().items():
        print(f"  {h['name']}  {h['host']}:{h['port']}  id={hid}")


def cmd_status(args):
    _, h = pick_host(args.host)
    s = cg.agent_for(h).request("GET", "/status")
    print(f"{s['name']}  (agent v{s['version']})")
    print("Dépendances :")
    for k, ok in s["deps"].items():
        print(f"  {'OK ' if ok else '-- '} {k}")
    sess = s["session"]
    print(f"Session : {'ACTIVE vers ' + sess['client_ip'] if sess['active'] else 'aucune'}")
    for name, st in sess["processes"].items():
        print(f"  {name} : {st}")
    if sess.get("last_end_reason") and not sess["active"]:
        print(f"  dernière fin : {sess['last_end_reason']}")
    for name, j in s["installs"].items():
        print(f"Installation {name} : {j['state']}" + (f" (code {j['returncode']})" if j["returncode"] else ""))


def cmd_install(args):
    _, h = pick_host(args.host)
    agent = cg.agent_for(h)
    comps = ["ffmpeg", "vigembus", "python-deps"] if args.component == "all" else [args.component]
    for c in comps:
        agent.request("POST", f"/install/{c}")
        print(f"[{c}] installation lancée sur le PC fixe...", end="", flush=True)
        while True:
            time.sleep(2)
            j = agent.request("GET", "/status")["installs"][c]
            if j["state"] != "running":
                break
            print(".", end="", flush=True)
        print(f" {j['state'].upper()}")
        if j["state"] != "ok":
            print("  fin du journal :\n    " + j["log_tail"].replace("\n", "\n    "))
    print("Pense à redémarrer l'agent après l'installation de ViGEmBus ou de FFmpeg "
          "(ou redémarre le PC fixe).")


def cmd_video(args):
    _, h = pick_host(args.host)
    body = {k: v for k, v in dict(fps=args.fps, bitrate_mbps=args.bitrate, codec=args.codec,
                                  scale=args.scale).items() if v is not None}
    print(cg.agent_for(h).request("PATCH", "/config/video", body))


def find_game(agent, query):
    games = agent.request("GET", "/games")
    q = query.lower()
    exact = [g for g in games if q in (g["id"].lower(), g["name"].lower())]
    partial = [g for g in games if q in g["name"].lower()]
    hits = exact or partial
    if not hits:
        sys.exit(f"Aucun jeu ne correspond à « {query} » (python client.py games pour la liste)")
    if len(hits) > 1:
        sys.exit("Plusieurs jeux correspondent : " + ", ".join(f"{g['name']} ({g['id']})" for g in hits))
    return hits[0]


def cmd_games(args):
    _, h = pick_host(args.host)
    agent = cg.agent_for(h)
    games = agent.request("GET", f"/games{'?refresh=true' if args.refresh else ''}")
    running = agent.request("GET", "/games/running")["running"]
    for g in games:
        flag = "  <- en cours" if g["id"] == running else ""
        print(f"  {g['source']:<8} {g['name']:<40} {g['id']}{flag}")
    print(f"{len(games)} jeux")


def cmd_unpair(args):
    hid, h = pick_host(args.host)
    try:
        cg.agent_for(h).request("DELETE", "/pair")
    except Exception as e:
        print(f"(l'agent n'a pas pu être prévenu : {e})")
    hosts = cg.load_hosts()
    hosts.pop(hid, None)
    cg.save_hosts(hosts)
    print("PC oublié.")


def cmd_play(args):
    _, h = pick_host(args.host)
    agent = cg.agent_for(h)
    host_ip = h["host"]
    py = sys.executable
    procs = {}

    def spawn(name, cmd):
        procs[name] = subprocess.Popen(cmd, cwd=HERE)

    try:
        info = agent.request("POST", "/session/start")
        ports = info["ports"]
        print(f"[session] démarrée sur {h['name']} : {info['video']}")

        spawn("audio", [py, str(HERE / "audio_receiver.py"), "--allow", host_ip,
                        "--port", str(ports["audio"])])
        spawn("input", [py, str(HERE / "input_sender.py"), host_ip, "--port", str(ports["input"])])
        spawn("video", ["ffplay", "-hide_banner", "-loglevel", "warning",
                        "-window_title", f"Cloud gaming – {h['name']}",
                        "-x", "1280", "-y", "720",
                        "-fflags", "nobuffer", "-flags", "low_delay", "-probesize", "32",
                        "-analyzeduration", "0", "-framedrop", "-vf", "setpts=0", "-f", "mpegts",
                        f"udp://0.0.0.0:{ports['video']}?fifo_size=5000000&overrun_nonfatal=1"
                        f"&buffer_size=8388608"])
        if args.game:
            g = find_game(agent, args.game)
            time.sleep(1.5)       # laisse le flux démarrer pour voir le lancement
            agent.request("POST", f"/games/{g['id']}/launch")
            print(f"[jeu] lancement de {g['name']}")
        print("[session] ferme la fenêtre vidéo (touche Q ou Échap) pour terminer.")
        print("          Ctrl+Alt+Shift+Z = capturer / rendre le clavier et la souris.")

        stop = threading.Event()

        def heartbeat():
            while not stop.wait(3):
                try:
                    st = agent.request("POST", "/session/heartbeat", timeout=5)
                    dead = [n for n, s in st["processes"].items() if s != "en cours"]
                    if dead:
                        print(f"[fixe] processus arrêté(s) : {', '.join(dead)}  (voir les logs de l'agent)")
                except cg.AgentError as e:
                    print(f"[session] l'agent a terminé la session : {e.detail}")
                    stop.set()
                except OSError as e:
                    print(f"[session] agent injoignable : {e}")

        threading.Thread(target=heartbeat, daemon=True).start()
        while not stop.is_set() and procs["video"].poll() is None:
            time.sleep(0.5)
    except KeyboardInterrupt:
        pass
    finally:
        # Ordre important : d'abord les entrées (le fixe relâche tout faute de paquets),
        # puis la session sur le fixe, puis le reste en local.
        if "input" in procs and procs["input"].poll() is None:
            procs["input"].terminate()
        time.sleep(0.7)
        try:
            agent.request("POST", "/session/stop", timeout=5)
        except Exception as e:
            print(f"[session] arrêt côté fixe impossible ({e}) : il s'arrêtera seul dans 15 s")
        for p in procs.values():
            if p.poll() is None:
                p.terminate()
        print("[session] terminée")


def main():
    ap = argparse.ArgumentParser(description="Client cloud gaming (PC portable)")
    ap.add_argument("--host", help="nom, id ou IP du PC fixe (si plusieurs appairés)")
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("discover").set_defaults(fn=cmd_discover)
    p = sub.add_parser("pair")
    p.add_argument("target", nargs="?", help="numéro dans 'discover' ou IP du PC fixe")
    p.add_argument("--name", help="nom de ce portable (défaut : nom du PC)")
    p.set_defaults(fn=cmd_pair)
    sub.add_parser("list").set_defaults(fn=cmd_list)
    sub.add_parser("status").set_defaults(fn=cmd_status)
    p = sub.add_parser("install")
    p.add_argument("component", choices=["all", "ffmpeg", "vigembus", "python-deps"])
    p.set_defaults(fn=cmd_install)
    p = sub.add_parser("video")
    p.add_argument("--fps", type=int)
    p.add_argument("--bitrate", type=int, help="Mbps")
    p.add_argument("--codec", choices=["h264", "hevc"])
    p.add_argument("--scale", help="ex. 1920x1080, ou '' pour la résolution native")
    p.set_defaults(fn=cmd_video)
    p = sub.add_parser("games")
    p.add_argument("--refresh", action="store_true", help="relance la détection")
    p.set_defaults(fn=cmd_games)
    p = sub.add_parser("play")
    p.add_argument("--game", help="id ou nom du jeu à lancer")
    p.set_defaults(fn=cmd_play)
    sub.add_parser("unpair").set_defaults(fn=cmd_unpair)
    args = ap.parse_args()
    try:
        args.fn(args)
    except cg.AgentError as e:
        sys.exit(f"Erreur de l'agent : {e}")
    except (ConnectionRefusedError, TimeoutError, socket.timeout, OSError) as e:
        sys.exit(f"PC fixe injoignable : {e}")


if __name__ == "__main__":
    main()
