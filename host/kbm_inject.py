"""PC FIXE : rejoue clavier + souris avec SendInput (API Windows).

Les touches sont injectées par SCANCODE : les jeux (DirectInput, Raw Input) les voient
comme un vrai clavier, quelle que soit la disposition AZERTY/QWERTY.
"""
import ctypes
from ctypes import wintypes

import protocol as P

user32 = ctypes.WinDLL("user32", use_last_error=True)

INPUT_MOUSE, INPUT_KEYBOARD = 0, 1
KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE = 0x0001, 0x0002, 0x0008
MOUSEEVENTF_MOVE = 0x0001
MOUSEEVENTF_WHEEL = 0x0800
MOUSE_FLAGS = {  # bit -> (appui, relâchement, mouseData)
    P.MB_LEFT:   (0x0002, 0x0004, 0),
    P.MB_RIGHT:  (0x0008, 0x0010, 0),
    P.MB_MIDDLE: (0x0020, 0x0040, 0),
    P.MB_X1:     (0x0080, 0x0100, 1),
    P.MB_X2:     (0x0080, 0x0100, 2),
}
ULONG_PTR = ctypes.c_size_t


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", wintypes.LONG), ("dy", wintypes.LONG), ("mouseData", wintypes.DWORD),
                ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD), ("dwExtraInfo", ULONG_PTR)]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", wintypes.WORD), ("wScan", wintypes.WORD), ("dwFlags", wintypes.DWORD),
                ("time", wintypes.DWORD), ("dwExtraInfo", ULONG_PTR)]


class HARDWAREINPUT(ctypes.Structure):
    _fields_ = [("uMsg", wintypes.DWORD), ("wParamL", wintypes.WORD), ("wParamH", wintypes.WORD)]


class _INPUTUNION(ctypes.Union):
    _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT), ("hi", HARDWAREINPUT)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", wintypes.DWORD), ("u", _INPUTUNION)]


def _key(idx, down):
    i = INPUT(type=INPUT_KEYBOARD)
    i.u.ki.wScan = idx & 0xFF
    i.u.ki.dwFlags = (KEYEVENTF_SCANCODE
                      | (KEYEVENTF_EXTENDEDKEY if idx >= 256 else 0)
                      | (0 if down else KEYEVENTF_KEYUP))
    return i


def _mouse(flags, dx=0, dy=0, data=0):
    i = INPUT(type=INPUT_MOUSE)
    i.u.mi.dx, i.u.mi.dy = dx, dy
    i.u.mi.mouseData = data & 0xFFFFFFFF
    i.u.mi.dwFlags = flags
    return i


def _send(inputs):
    if inputs:
        arr = (INPUT * len(inputs))(*inputs)
        user32.SendInput(len(inputs), arr, ctypes.sizeof(INPUT))


class KbmInjector:
    def __init__(self):
        self.keys = bytes(64)
        self.mbuttons = 0
        self.counters = None   # (mx, my, mw) du dernier paquet appliqué

    def apply(self, s):
        inputs = []

        # Clavier : on n'envoie que les touches dont l'état a changé
        new = s["keys"]
        for byte_i in range(64):
            diff = new[byte_i] ^ self.keys[byte_i]
            if diff:
                for bit in range(8):
                    if diff >> bit & 1:
                        inputs.append(_key(byte_i * 8 + bit, bool(new[byte_i] >> bit & 1)))
        self.keys = new

        # Souris : déplacement = différence des compteurs cumulés
        if self.counters is None:
            self.counters = (s["mx"], s["my"], s["mw"])   # premier paquet : simple référence
        dx = P.delta32(s["mx"], self.counters[0])
        dy = P.delta32(s["my"], self.counters[1])
        dw = P.delta32(s["mw"], self.counters[2])
        self.counters = (s["mx"], s["my"], s["mw"])
        if dx or dy:
            inputs.append(_mouse(MOUSEEVENTF_MOVE, dx, dy))
        if dw:
            inputs.append(_mouse(MOUSEEVENTF_WHEEL, data=dw))

        # Boutons souris
        changed = s["mbuttons"] ^ self.mbuttons
        for bit, (f_down, f_up, data) in MOUSE_FLAGS.items():
            if changed & bit:
                inputs.append(_mouse(f_down if s["mbuttons"] & bit else f_up, data=data))
        self.mbuttons = s["mbuttons"]

        _send(inputs)

    def release_all(self):
        """Relâche toutes les touches et tous les boutons (perte de connexion, arrêt)."""
        if self.counters is None:
            return
        mx, my, mw = self.counters
        self.apply(dict(keys=bytes(64), mbuttons=0, mx=mx, my=my, mw=mw))
