"""PC PORTABLE : envoie manette + clavier + souris au PC fixe en UDP.

Usage :
    python input_sender.py 192.168.1.20             (IP du PC FIXE, pas celle de la box)
    python input_sender.py 192.168.1.20 --no-kbm    (manette uniquement)

Ctrl+Alt+Shift+Z : active / désactive la capture clavier + souris.
Ctrl+C dans ce terminal : arrêter (capture désactivée d'abord).
"""
import os
# Sans ça, SDL ignore la manette dès qu'une autre fenêtre (ffplay, jeu...) a le focus
os.environ["SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS"] = "1"
os.environ["PYGAME_HIDE_SUPPORT_PROMPT"] = "1"

import argparse
import socket
import time

import pygame
from pygame._sdl2 import controller  # mapping standard Xbox / PlayStation / Switch

import protocol as P

BUTTON_MAP = {
    pygame.CONTROLLER_BUTTON_A: P.A,
    pygame.CONTROLLER_BUTTON_B: P.B,
    pygame.CONTROLLER_BUTTON_X: P.X,
    pygame.CONTROLLER_BUTTON_Y: P.Y,
    pygame.CONTROLLER_BUTTON_BACK: P.BACK,
    pygame.CONTROLLER_BUTTON_GUIDE: P.GUIDE,
    pygame.CONTROLLER_BUTTON_START: P.START,
    pygame.CONTROLLER_BUTTON_LEFTSTICK: P.LEFT_THUMB,
    pygame.CONTROLLER_BUTTON_RIGHTSTICK: P.RIGHT_THUMB,
    pygame.CONTROLLER_BUTTON_LEFTSHOULDER: P.LEFT_SHOULDER,
    pygame.CONTROLLER_BUTTON_RIGHTSHOULDER: P.RIGHT_SHOULDER,
    pygame.CONTROLLER_BUTTON_DPAD_UP: P.DPAD_UP,
    pygame.CONTROLLER_BUTTON_DPAD_DOWN: P.DPAD_DOWN,
    pygame.CONTROLLER_BUTTON_DPAD_LEFT: P.DPAD_LEFT,
    pygame.CONTROLLER_BUTTON_DPAD_RIGHT: P.DPAD_RIGHT,
}
BUTTON_NAMES = {P.A: "A", P.B: "B", P.X: "X", P.Y: "Y", P.BACK: "Back", P.GUIDE: "Guide",
                P.START: "Start", P.LEFT_THUMB: "L3", P.RIGHT_THUMB: "R3", P.LEFT_SHOULDER: "LB",
                P.RIGHT_SHOULDER: "RB", P.DPAD_UP: "Haut", P.DPAD_DOWN: "Bas",
                P.DPAD_LEFT: "Gauche", P.DPAD_RIGHT: "Droite"}
NEUTRAL = (0, 0, 0, 0, 0, 0, 0)


def clamp16(v):
    return max(-32768, min(32767, v))


def open_first_controller():
    for i in range(controller.get_count()):
        if controller.is_controller(i):
            pad = controller.Controller(i)
            print(f"[manette] connectée : {pad.name}")
            return pad
    return None


def read_pad(pad):
    buttons = 0
    for sdl_btn, bit in BUTTON_MAP.items():
        if pad.get_button(sdl_btn):
            buttons |= bit
    lx = pad.get_axis(pygame.CONTROLLER_AXIS_LEFTX)
    ly = clamp16(-pad.get_axis(pygame.CONTROLLER_AXIS_LEFTY) - 1)   # SDL : Y bas = + ; XInput : Y haut = +
    rx = pad.get_axis(pygame.CONTROLLER_AXIS_RIGHTX)
    ry = clamp16(-pad.get_axis(pygame.CONTROLLER_AXIS_RIGHTY) - 1)
    lt = max(0, pad.get_axis(pygame.CONTROLLER_AXIS_TRIGGERLEFT)) * 255 // 32767
    rt = max(0, pad.get_axis(pygame.CONTROLLER_AXIS_TRIGGERRIGHT)) * 255 // 32767
    return buttons, lx, ly, rx, ry, lt, rt


def names(mask):
    return "+".join(n for bit, n in BUTTON_NAMES.items() if mask & bit) or "aucun"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("host", help="IP du PC fixe")
    ap.add_argument("--port", type=int, default=5001)
    ap.add_argument("--rate", type=int, default=250, help="envois par seconde")
    ap.add_argument("--no-kbm", action="store_true", help="désactive clavier + souris")
    args = ap.parse_args()

    pygame.init()
    controller.init()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    dest = (args.host, args.port)
    period = 1.0 / args.rate

    kbm = None
    if not args.no_kbm:
        from kbm_capture import KbmCapture
        kbm = KbmCapture(on_toggle=lambda on: print(
            "[clavier/souris] CAPTURE ACTIVE -> envoyés au PC fixe (Ctrl+Alt+Shift+Z pour rendre la main)"
            if on else "[clavier/souris] capture désactivée -> le portable reprend la main"))
        kbm.start()
        print("[clavier/souris] prêt : Ctrl+Alt+Shift+Z pour capturer")

    pad = open_first_controller()
    if pad is None:
        print("[manette] aucune manette détectée (branche-en une quand tu veux)")

    seq = 0
    last_buttons = 0
    last_retry = time.perf_counter()
    sent, stats_t = 0, time.perf_counter()
    next_t = time.perf_counter()
    print(f"[envoi] vers {dest[0]}:{dest[1]} à {args.rate} Hz  (Ctrl+C pour arrêter)")

    try:
        while True:
            for ev in pygame.event.get():
                if ev.type == pygame.CONTROLLERDEVICEADDED and pad is None:
                    pad = open_first_controller()
                elif ev.type == pygame.CONTROLLERDEVICEREMOVED:
                    # L'événement peut concerner un AUTRE périphérique : on vérifie le nôtre
                    if pad is not None and not pad.attached():
                        print("[manette] débranchée (Bluetooth coupé, mise en veille, "
                              "ou Steam qui la capture sur ce portable ?)")
                        pad = None
            # Reconnexion automatique : on réessaie toutes les 2 s
            if pad is None and time.perf_counter() - last_retry > 2.0:
                last_retry = time.perf_counter()
                controller.quit()
                controller.init()
                pad = open_first_controller()

            state = read_pad(pad) if pad is not None else NEUTRAL
            sock.sendto(P.pack_pad(seq, *state), dest)
            if kbm is not None:
                sock.sendto(P.pack_km(seq, *kbm.snapshot()), dest)
            seq += 1
            sent += 1

            if state[0] != last_buttons:
                print(f"[manette] boutons : {names(state[0])}")
                last_buttons = state[0]

            now = time.perf_counter()
            if now - stats_t >= 5.0:
                print(f"[envoi] {sent / (now - stats_t):.0f} envois/s")
                sent, stats_t = 0, now

            next_t += period
            delay = next_t - time.perf_counter()
            if delay > 0:
                time.sleep(delay)   # précis sous Windows avec Python >= 3.11
            else:
                next_t = time.perf_counter()
    except KeyboardInterrupt:
        pass
    finally:
        if kbm is not None:
            kbm.stop()
        # Quelques paquets "tout relâché" pour ne rien laisser coincé côté fixe
        for _ in range(5):
            sock.sendto(P.pack_pad(seq, *NEUTRAL), dest)
            if kbm is not None:
                _, _, mx, my, mw = kbm.snapshot()
                sock.sendto(P.pack_km(seq, bytes(64), 0, mx, my, mw), dest)
            seq += 1
        print("\n[envoi] arrêté")


if __name__ == "__main__":
    main()
