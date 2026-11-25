"""Formats des paquets, partagés par l'émetteur (portable) et le récepteur (fixe).

Principe : chaque paquet transporte un ÉTAT complet, jamais un événement isolé.
Un paquet UDP perdu est donc corrigé automatiquement par le suivant
(aucune touche ne peut rester "coincée" à cause d'une perte).

1) Manette  "CG"  (19 octets)
   magic 2s | version B | seq I | buttons H (bits XInput) |
   lx ly rx ry h (sticks, Y vers le haut = positif) | lt rt B (gâchettes 0..255)

2) Clavier + souris  "KM"  (84 octets)
   magic 2s | version B | seq I |
   keys 64s   : 512 bits, 1 bit par touche = scancode (0..255) + 256 si touche "étendue"
   mbuttons B : bit0 gauche, bit1 droit, bit2 milieu, bit3 X1, bit4 X2
   mx my mw I : COMPTEURS CUMULÉS du déplacement souris et de la molette depuis le démarrage.
                Le récepteur applique la différence avec le dernier paquet reçu :
                un paquet perdu ne fait perdre aucun mouvement.
"""
import struct

VERSION = 1

# ---------- Manette ----------
PAD_MAGIC = b"CG"
PAD_FMT = "<2sBIHhhhhBB"
PAD_SIZE = struct.calcsize(PAD_FMT)  # 19

DPAD_UP        = 0x0001
DPAD_DOWN      = 0x0002
DPAD_LEFT      = 0x0004
DPAD_RIGHT     = 0x0008
START          = 0x0010
BACK           = 0x0020
LEFT_THUMB     = 0x0040
RIGHT_THUMB    = 0x0080
LEFT_SHOULDER  = 0x0100
RIGHT_SHOULDER = 0x0200
GUIDE          = 0x0400
A              = 0x1000
B              = 0x2000
X              = 0x4000
Y              = 0x8000

# ---------- Clavier / souris ----------
KM_MAGIC = b"KM"
KM_FMT = "<2sBI64sBIII"
KM_SIZE = struct.calcsize(KM_FMT)  # 84

MB_LEFT, MB_RIGHT, MB_MIDDLE, MB_X1, MB_X2 = 1, 2, 4, 8, 16

U32 = 0xFFFFFFFF


# ---------- Audio ----------
# 3) Audio  "AU"  : magic 2s | version B | seq I | sample_rate I | channels B | PCM int16 entrelacé
#    Un paquet = 5 ms de son (240 échantillons à 48 kHz, stéréo = 960 octets) : sous la taille
#    d'un paquet Ethernet, donc jamais fragmenté.
AU_MAGIC = b"AU"
AU_HDR_FMT = "<2sBIIB"
AU_HDR_SIZE = struct.calcsize(AU_HDR_FMT)  # 12


def pack_audio(seq, rate, channels, pcm):
    return struct.pack(AU_HDR_FMT, AU_MAGIC, VERSION, seq & U32, rate, channels) + pcm


def unpack_audio(data):
    """Renvoie (seq, rate, channels, pcm) ou None."""
    if len(data) <= AU_HDR_SIZE or data[:2] != AU_MAGIC:
        return None
    _, ver, seq, rate, ch = struct.unpack(AU_HDR_FMT, data[:AU_HDR_SIZE])
    if ver != VERSION:
        return None
    return seq, rate, ch, data[AU_HDR_SIZE:]


def pack_pad(seq, buttons, lx, ly, rx, ry, lt, rt):
    return struct.pack(PAD_FMT, PAD_MAGIC, VERSION, seq & U32, buttons, lx, ly, rx, ry, lt, rt)


def pack_km(seq, keys, mbuttons, mx, my, mw):
    return struct.pack(KM_FMT, KM_MAGIC, VERSION, seq & U32, keys, mbuttons,
                       mx & U32, my & U32, mw & U32)


def unpack(data):
    """Renvoie ("pad", dict), ("km", dict) ou (None, None) si le paquet est invalide."""
    if len(data) == PAD_SIZE and data[:2] == PAD_MAGIC:
        m, ver, seq, b, lx, ly, rx, ry, lt, rt = struct.unpack(PAD_FMT, data)
        if ver == VERSION:
            return "pad", dict(seq=seq, buttons=b, lx=lx, ly=ly, rx=rx, ry=ry, lt=lt, rt=rt)
    elif len(data) == KM_SIZE and data[:2] == KM_MAGIC:
        m, ver, seq, keys, mb, mx, my, mw = struct.unpack(KM_FMT, data)
        if ver == VERSION:
            return "km", dict(seq=seq, keys=keys, mbuttons=mb, mx=mx, my=my, mw=mw)
    return None, None


def seq_newer(a, b):
    """True si a est plus récent que b (gère le retour à zéro du compteur 32 bits)."""
    return a != b and ((a - b) & U32) < 0x80000000


def delta32(new, old):
    """Différence signée entre deux compteurs cumulés 32 bits."""
    d = (new - old) & U32
    return d - 0x100000000 if d >= 0x80000000 else d
