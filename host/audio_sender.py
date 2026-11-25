"""PC FIXE : capture le son du PC (ce qui sort des haut-parleurs) et l'envoie au portable.

Capture en "WASAPI loopback" : on enregistre la sortie audio par défaut de Windows,
sans câble ni périphérique virtuel.

Usage :
    py audio_sender.py 192.168.1.72          (IP du PC portable)

Ctrl+C pour arrêter.
"""
import argparse
import socket
import time

import numpy as np
import pyaudiowpatch as pyaudio

import protocol as P

CHUNK_MS = 5  # 5 ms par paquet


def find_loopback(p):
    """Trouve le périphérique loopback de la sortie audio par défaut."""
    wasapi = p.get_host_api_info_by_type(pyaudio.paWASAPI)
    speakers = p.get_device_info_by_index(wasapi["defaultOutputDevice"])
    if speakers.get("isLoopbackDevice"):
        return speakers
    for lb in p.get_loopback_device_info_generator():
        if speakers["name"] in lb["name"]:
            return lb
    raise RuntimeError("Aucun périphérique loopback trouvé pour la sortie par défaut")


def to_stereo(block, channels):
    """int16 entrelacé, N canaux -> stéréo. Mixage simple pour le 5.1 / 7.1."""
    if channels == 2:
        return block
    a = np.frombuffer(block, dtype=np.int16).reshape(-1, channels).astype(np.float32)
    if channels == 1:
        st = np.repeat(a, 2, axis=1)
    else:
        # Ordre Windows : FL FR C LFE BL BR (SL SR)
        L, R = a[:, 0].copy(), a[:, 1].copy()
        if channels >= 3:
            L += 0.707 * a[:, 2]
            R += 0.707 * a[:, 2]
        if channels >= 6:
            L += 0.5 * a[:, 4]
            R += 0.5 * a[:, 5]
        if channels >= 8:
            L += 0.5 * a[:, 6]
            R += 0.5 * a[:, 7]
        st = np.stack([L, R], axis=1) * 0.6   # évite la saturation après mixage
    return np.clip(st, -32768, 32767).astype(np.int16).tobytes()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("host", help="IP du PC portable")
    ap.add_argument("--port", type=int, default=5002)
    args = ap.parse_args()

    p = pyaudio.PyAudio()
    dev = find_loopback(p)
    rate = int(dev["defaultSampleRate"])
    channels = dev["maxInputChannels"]
    frames = rate * CHUNK_MS // 1000
    print(f"[audio] capture : {dev['name']}  ({rate} Hz, {channels} canaux -> stéréo)")

    stream = p.open(format=pyaudio.paInt16, channels=channels, rate=rate,
                    frames_per_buffer=frames, input=True, input_device_index=dev["index"])

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    dest = (args.host, args.port)
    print(f"[envoi] vers {dest[0]}:{dest[1]}, paquets de {CHUNK_MS} ms  (Ctrl+C pour arrêter)")
    print("        (Windows ne fournit rien quand aucun son ne joue : c'est normal)")

    seq, sent, t0 = 0, 0, time.perf_counter()
    try:
        while True:
            block = stream.read(frames, exception_on_overflow=False)
            sock.sendto(P.pack_audio(seq, rate, 2, to_stereo(block, channels)), dest)
            seq += 1
            sent += 1
            now = time.perf_counter()
            if now - t0 >= 5:
                print(f"[envoi] {sent / (now - t0):.0f} paquets/s")
                sent, t0 = 0, now
    except KeyboardInterrupt:
        print("\n[audio] arrêté")
    finally:
        stream.stop_stream()
        stream.close()
        p.terminate()


if __name__ == "__main__":
    main()
