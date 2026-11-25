"""PC FIXE : reçoit manette + clavier + souris et les rejoue localement.

Usage :
    python input_receiver.py --allow 192.168.1.42     (IP du PC portable)
    python input_receiver.py --allow 192.168.1.42 --no-pad

Ctrl+C pour arrêter.
Si un jeu est lancé en administrateur, lance aussi ce script en administrateur
(Windows bloque l'injection d'entrées d'un programme normal vers un programme admin).
"""
import argparse
import socket
import time

import protocol as P
from kbm_inject import KbmInjector

TIMEOUT_NEUTRAL = 0.5  # s sans paquet -> tout relâcher


def make_pad():
    import vgamepad as vg  # pilote ViGEmBus requis
    pad = vg.VX360Gamepad()
    print("[manette virtuelle] Xbox 360 créée")
    return pad


def apply_pad(pad, s):
    r = pad.report
    r.wButtons = s["buttons"]
    r.bLeftTrigger, r.bRightTrigger = s["lt"], s["rt"]
    r.sThumbLX, r.sThumbLY = s["lx"], s["ly"]
    r.sThumbRX, r.sThumbRY = s["rx"], s["ry"]
    pad.update()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--allow", required=True, help="seule IP autorisée (le PC portable)")
    ap.add_argument("--bind", default="0.0.0.0")
    ap.add_argument("--port", type=int, default=5001)
    ap.add_argument("--no-pad", action="store_true", help="pas de manette virtuelle")
    args = ap.parse_args()

    pad = None if args.no_pad else make_pad()
    kbm = KbmInjector()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind((args.bind, args.port))
    sock.settimeout(TIMEOUT_NEUTRAL)
    print(f"[écoute] UDP {args.bind}:{args.port}, autorisé : {args.allow}  (Ctrl+C pour arrêter)")

    last_seq = {"pad": None, "km": None}
    active = False
    last_buttons = 0
    stats_t, received, late, rejected = time.perf_counter(), 0, 0, 0

    try:
        while True:
            try:
                data, (ip, _) = sock.recvfrom(256)
            except socket.timeout:
                if active:
                    if pad is not None:
                        pad.reset()
                        pad.update()
                    kbm.release_all()
                    active = False
                    print("[sécurité] plus de paquets -> tout est relâché")
                continue
            except ConnectionResetError:
                continue  # bruit ICMP sous Windows

            if ip != args.allow:
                rejected += 1
                continue
            kind, s = P.unpack(data)
            if kind is None:
                continue
            prev = last_seq[kind]
            if prev is not None and not P.seq_newer(s["seq"], prev):
                late += 1
                continue
            last_seq[kind] = s["seq"]

            if kind == "pad" and pad is not None:
                apply_pad(pad, s)
                if s["buttons"] != last_buttons:
                    print(f"[manette] boutons=0x{s['buttons']:04X}")
                    last_buttons = s["buttons"]
            elif kind == "km":
                kbm.apply(s)
            active = True
            received += 1

            now = time.perf_counter()
            if now - stats_t >= 5.0:
                print(f"[réception] {received / (now - stats_t):.0f} paquets/s  "
                      f"en retard : {late}  IP refusées : {rejected}")
                stats_t, received, late, rejected = now, 0, 0, 0
    except KeyboardInterrupt:
        pass
    finally:
        if pad is not None:
            pad.reset()
            pad.update()
        kbm.release_all()
        print("\n[réception] arrêtée, tout est relâché")


if __name__ == "__main__":
    main()
