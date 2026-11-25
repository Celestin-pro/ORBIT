"""PC PORTABLE : reçoit le son du PC fixe et le joue avec une faible latence.

Usage :
    python audio_receiver.py --allow 192.168.1.20        (IP du PC fixe)
    python audio_receiver.py --allow 192.168.1.20 --buffer 40

--buffer = latence visée en ms (défaut 30). Plus bas = plus réactif mais risque de
craquements sur un Wi-Fi instable ; plus haut = plus stable.

Ctrl+C pour arrêter.
"""
import argparse
import socket
import threading
import time

import sounddevice as sd

import protocol as P


class JitterBuffer:
    """Tampon FIFO d'octets PCM, borné pour garder une latence basse.

    - Trop vide  -> on joue du silence (le son "saute" au lieu de bloquer).
    - Trop plein -> on jette le plus ancien (le son rattrape le temps réel).
    Ça compense aussi la petite dérive d'horloge entre les deux cartes son.
    """

    def __init__(self, target_bytes, max_bytes):
        self.buf = bytearray()
        self.lock = threading.Lock()
        self.target, self.max = target_bytes, max_bytes
        self.started = False       # on attend d'avoir 'target' octets avant de jouer
        self.underruns = self.trims = 0

    def push(self, pcm):
        with self.lock:
            self.buf += pcm
            if len(self.buf) > self.max:
                del self.buf[:len(self.buf) - self.target]
                self.trims += 1
            if not self.started and len(self.buf) >= self.target:
                self.started = True

    def pull(self, n):
        with self.lock:
            if not self.started:
                return bytes(n)
            if len(self.buf) >= n:
                out = bytes(self.buf[:n])
                del self.buf[:n]
                return out
            # Plus assez de données : on vide ce qui reste et on complète avec du silence
            out = bytes(self.buf) + bytes(n - len(self.buf))
            self.buf.clear()
            self.started = False   # on re-remplit jusqu'à 'target' avant de rejouer
            self.underruns += 1
            return out

    def level_ms(self, bytes_per_ms):
        with self.lock:
            return len(self.buf) / bytes_per_ms


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--allow", required=True, help="IP du PC fixe")
    ap.add_argument("--port", type=int, default=5002)
    ap.add_argument("--buffer", type=int, default=30, help="latence visée en ms")
    args = ap.parse_args()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1 << 20)
    sock.bind(("0.0.0.0", args.port))
    sock.settimeout(1.0)
    print(f"[audio] écoute UDP {args.port}, autorisé : {args.allow}  (Ctrl+C pour arrêter)")

    stream = jb = None
    fmt = None          # (rate, channels) du flux en cours
    last_seq = None
    lost = late = 0
    t0 = time.perf_counter()

    def open_stream(rate, channels):
        nonlocal stream, jb
        if stream is not None:
            stream.stop()
            stream.close()
        bpms = rate * channels * 2 / 1000          # octets par milliseconde
        jb = JitterBuffer(int(args.buffer * bpms) // 4 * 4, int(args.buffer * 3 * bpms) // 4 * 4)
        frame_bytes = channels * 2

        def callback(outdata, frames, time_info, status):
            outdata[:] = jb.pull(frames * frame_bytes)

        stream = sd.RawOutputStream(samplerate=rate, channels=channels, dtype="int16",
                                    blocksize=rate // 200, latency="low", callback=callback)
        stream.start()
        print(f"[audio] lecture : {rate} Hz, {channels} canaux, tampon visé {args.buffer} ms")
        return bpms

    bpms = 1
    try:
        while True:
            try:
                data, (ip, _) = sock.recvfrom(4096)
            except socket.timeout:
                continue
            except ConnectionResetError:
                continue
            if ip != args.allow:
                continue
            pkt = P.unpack_audio(data)
            if pkt is None:
                continue
            seq, rate, ch, pcm = pkt

            if fmt != (rate, ch):
                fmt = (rate, ch)
                bpms = open_stream(rate, ch)
                last_seq = None

            if last_seq is not None:
                if not P.seq_newer(seq, last_seq):
                    late += 1
                    continue               # paquet en retard : déjà dépassé, on le jette
                gap = (seq - last_seq - 1) & P.U32
                if 0 < gap < 1000:
                    lost += gap            # paquets perdus : on ne les attend pas
            last_seq = seq
            jb.push(pcm)

            now = time.perf_counter()
            if now - t0 >= 5:
                print(f"[audio] tampon {jb.level_ms(bpms):.0f} ms  perdus {lost}  en retard {late}  "
                      f"coupures {jb.underruns}  rattrapages {jb.trims}")
                lost = late = jb.underruns = jb.trims = 0
                t0 = now
    except KeyboardInterrupt:
        pass
    finally:
        if stream is not None:
            stream.stop()
            stream.close()
        print("\n[audio] arrêté")


if __name__ == "__main__":
    main()
