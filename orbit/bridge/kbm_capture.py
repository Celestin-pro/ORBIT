"""PC PORTABLE : capture clavier + souris au niveau système (hooks bas niveau Windows).

- Ctrl+Alt+Shift+Z active / désactive la capture.
- Capture ACTIVE  : les touches et la souris ne vont plus au portable, elles partent
                    vers le PC fixe (le curseur du portable reste figé au centre de l'écran).
- Capture INACTIVE : le portable fonctionne normalement, rien n'est envoyé.

On lit les SCANCODES (position physique des touches) et non les caractères :
c'est ce qu'attendent les jeux, et ça évite les problèmes AZERTY/QWERTY.
"""
import ctypes
import threading

from pynput import keyboard, mouse

user32 = ctypes.windll.user32
try:
    # Coordonnées en vrais pixels, même avec une mise à l'échelle Windows à 125/150 %
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except Exception:
    pass

WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP = 0x100, 0x101, 0x104, 0x105
LLKHF_EXTENDED, LLKHF_INJECTED = 0x01, 0x10
LLMHF_INJECTED = 0x01

WM_MOUSEMOVE = 0x200
WM_MOUSEWHEEL = 0x20A
MOUSE_BUTTON_MSG = {   # message -> (bit, appuyé ?)
    0x201: (1, True), 0x202: (1, False),    # gauche
    0x204: (2, True), 0x205: (2, False),    # droit
    0x207: (4, True), 0x208: (4, False),    # milieu
}
WM_XBUTTONDOWN, WM_XBUTTONUP = 0x20B, 0x20C

VK_SHIFT = {0x10, 0xA0, 0xA1}
VK_CTRL = {0x11, 0xA2, 0xA3}
VK_ALT = {0x12, 0xA4, 0xA5}
VK_Z = 0x5A


class KbmCapture:
    def __init__(self, on_toggle=None):
        self.lock = threading.Lock()
        self.capturing = False
        self.on_toggle = on_toggle
        # État envoyé au PC fixe
        self.keys = bytearray(64)
        self.mbuttons = 0
        self.mx = self.my = self.mw = 0   # compteurs cumulés
        # État local
        self._down_vk = set()      # touches physiquement enfoncées (pour le raccourci)
        self._local_down = set()   # touches dont l'appui est parti au portable : leur relâchement doit y aller aussi
        self._anchor = (0, 0)
        self._kb = keyboard.Listener(win32_event_filter=self._kb_filter)
        self._ms = mouse.Listener(win32_event_filter=self._ms_filter)

    def start(self):
        self._kb.start()
        self._ms.start()

    def stop(self):
        self._kb.stop()
        self._ms.stop()

    def snapshot(self):
        with self.lock:
            return bytes(self.keys), self.mbuttons, self.mx, self.my, self.mw

    # ------------------------------------------------------------------
    def _toggle(self):
        self.capturing = not self.capturing
        with self.lock:
            self.keys = bytearray(64)   # tout relâché côté fixe au changement de mode
            self.mbuttons = 0
        if self.capturing:
            w, h = user32.GetSystemMetrics(0), user32.GetSystemMetrics(1)
            self._anchor = (w // 2, h // 2)
            user32.SetCursorPos(*self._anchor)
        if self.on_toggle:
            self.on_toggle(self.capturing)

    # Les filtres tournent dans le thread du hook Windows : ils doivent rester TRÈS rapides,
    # sinon Windows désinstalle le hook silencieusement.
    def _kb_filter(self, msg, data):
        if data.flags & LLKHF_INJECTED:
            return True
        down = msg in (WM_KEYDOWN, WM_SYSKEYDOWN)
        vk = data.vkCode
        if down:
            self._down_vk.add(vk)
        else:
            self._down_vk.discard(vk)

        # Raccourci Ctrl+Alt+Shift+Z
        if (down and vk == VK_Z and self._down_vk & VK_CTRL
                and self._down_vk & VK_ALT and self._down_vk & VK_SHIFT):
            self._toggle()
            self._kb.suppress_event()

        if not self.capturing:
            if down:
                self._local_down.add(vk)
            else:
                self._local_down.discard(vk)
            return True

        # Relâchement d'une touche enfoncée AVANT la capture : on le laisse au portable
        if not down and vk in self._local_down:
            self._local_down.discard(vk)
            return True

        idx = (data.scanCode & 0xFF) + (256 if data.flags & LLKHF_EXTENDED else 0)
        with self.lock:
            if down:
                self.keys[idx >> 3] |= 1 << (idx & 7)
            else:
                self.keys[idx >> 3] &= ~(1 << (idx & 7)) & 0xFF
        self._kb.suppress_event()

    def _ms_filter(self, msg, data):
        if not self.capturing or data.flags & LLMHF_INJECTED:
            return True
        with self.lock:
            if msg == WM_MOUSEMOVE:
                # Le curseur est bloqué sur l'ancre : la position "demandée" moins l'ancre = le mouvement
                self.mx += data.pt.x - self._anchor[0]
                self.my += data.pt.y - self._anchor[1]
            elif msg in MOUSE_BUTTON_MSG:
                bit, pressed = MOUSE_BUTTON_MSG[msg]
                self.mbuttons = self.mbuttons | bit if pressed else self.mbuttons & ~bit
            elif msg in (WM_XBUTTONDOWN, WM_XBUTTONUP):
                bit = 8 if (data.mouseData >> 16) == 1 else 16
                self.mbuttons = self.mbuttons | bit if msg == WM_XBUTTONDOWN else self.mbuttons & ~bit
            elif msg == WM_MOUSEWHEEL:
                self.mw += ctypes.c_short((data.mouseData >> 16) & 0xFFFF).value
        self._ms.suppress_event()
