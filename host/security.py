"""Sécurité de l'agent : certificat TLS auto-signé + appairage par PIN (SPAKE2).

Pourquoi SPAKE2 et pas « le client envoie le PIN » ?
  Un PIN à 6 chiffres se casse en une fraction de seconde si un attaquant capture
  quoi que ce soit qui en dépend (un hash, un HMAC...). SPAKE2 est un échange de clé
  authentifié par mot de passe (PAKE) : les deux côtés obtiennent la même clé secrète
  seulement s'ils connaissent le même PIN, et rien de ce qui passe sur le réseau ne
  permet de tester des PIN hors ligne. Un attaquant n'a droit qu'à UN essai par PIN affiché.

Liaison au certificat TLS :
  La confirmation finale est un HMAC(clé SPAKE2, empreinte du certificat). Si quelqu'un
  s'intercale avec son propre certificat (attaque de l'homme du milieu), les empreintes
  vues par le client et par l'agent diffèrent et l'appairage échoue.
"""
import datetime
import hashlib
import hmac
import json
import secrets
import threading
import time
from pathlib import Path

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import NameOID
from spake2 import SPAKE2_B

PIN_LIFETIME = 120        # s pendant lesquelles un PIN est valable
MIN_BEGIN_INTERVAL = 5    # s minimum entre deux demandes d'appairage


# ---------------------------------------------------------------- certificat
def ensure_cert(data_dir: Path, common_name: str):
    """Crée (au premier lancement) puis charge le certificat de l'agent.
    Renvoie (chemin_cert, chemin_clé, empreinte_sha256_hex)."""
    cert_p, key_p = data_dir / "agent_cert.pem", data_dir / "agent_key.pem"
    if not cert_p.exists() or not key_p.exists():
        key = ec.generate_private_key(ec.SECP256R1())
        name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, common_name)])
        now = datetime.datetime.now(datetime.timezone.utc)
        cert = (x509.CertificateBuilder()
                .subject_name(name).issuer_name(name)
                .public_key(key.public_key())
                .serial_number(x509.random_serial_number())
                .not_valid_before(now - datetime.timedelta(days=1))
                .not_valid_after(now + datetime.timedelta(days=3650))
                .sign(key, hashes.SHA256()))
        key_p.write_bytes(key.private_bytes(serialization.Encoding.PEM,
                                            serialization.PrivateFormat.PKCS8,
                                            serialization.NoEncryption()))
        cert_p.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
    cert = x509.load_pem_x509_certificate(cert_p.read_bytes())
    fp = hashlib.sha256(cert.public_bytes(serialization.Encoding.DER)).hexdigest()
    return str(cert_p), str(key_p), fp


def confirm_tag(key: bytes, role: bytes, fingerprint: str) -> str:
    return hmac.new(key, b"cg-" + role + b"|" + fingerprint.encode(), hashlib.sha256).hexdigest()


# ---------------------------------------------------------------- appairage
class PairingError(Exception):
    pass


class Pairing:
    def __init__(self, store: Path, fingerprint: str, show_pin, hide_pin):
        self.store = store
        self.fp = fingerprint
        self.show_pin, self.hide_pin = show_pin, hide_pin
        self.lock = threading.Lock()
        self.sessions = {}           # sid -> {pin, created, key, state}
        self.last_begin = 0.0
        self.clients = json.loads(store.read_text("utf-8")) if store.exists() else {}

    def _save(self):
        self.store.write_text(json.dumps(self.clients, indent=2), "utf-8")

    def _purge(self):
        now = time.time()
        for sid in [s for s, v in self.sessions.items() if now - v["created"] > PIN_LIFETIME]:
            del self.sessions[sid]
            self.hide_pin()

    # Étape 1 : le portable demande un appairage -> le fixe affiche un PIN
    def begin(self):
        with self.lock:
            self._purge()
            now = time.time()
            if now - self.last_begin < MIN_BEGIN_INTERVAL:
                raise PairingError("trop de demandes, réessaie dans quelques secondes")
            self.last_begin = now
            self.sessions.clear()                 # un seul appairage à la fois
            pin = f"{secrets.randbelow(10 ** 6):06d}"
            sid = secrets.token_hex(8)
            self.sessions[sid] = dict(pin=pin, created=now, key=None, state="new")
        self.show_pin(pin, PIN_LIFETIME)
        return sid

    # Étape 2 : échange SPAKE2 (le portable a tapé le PIN)
    def exchange(self, sid: str, msg_a: bytes) -> bytes:
        with self.lock:
            self._purge()
            s = self.sessions.get(sid)
            if not s or s["state"] != "new":
                raise PairingError("session d'appairage inconnue ou expirée")
            b = SPAKE2_B(s["pin"].encode())
            msg_b = b.start()
            try:
                s["key"] = b.finish(msg_a)
            except Exception:
                del self.sessions[sid]
                raise PairingError("message d'appairage invalide")
            s["state"] = "exchanged"
            return msg_b

    # Étape 3 : confirmation (prouve le même PIN + le même certificat), usage unique
    def confirm(self, sid: str, client_name: str, client_confirm: str):
        with self.lock:
            s = self.sessions.pop(sid, None)
        self.hide_pin()
        if not s or s["state"] != "exchanged":
            raise PairingError("session d'appairage inconnue ou expirée")
        expected = confirm_tag(s["key"], b"client", self.fp)
        if not hmac.compare_digest(expected, client_confirm):
            raise PairingError("PIN incorrect (ou connexion interceptée)")
        token = secrets.token_urlsafe(32)
        client_id = secrets.token_hex(4)
        with self.lock:
            self.clients[client_id] = dict(
                name=client_name[:64],
                token_sha256=hashlib.sha256(token.encode()).hexdigest(),
                paired_at=datetime.datetime.now().isoformat(timespec="seconds"),
            )
            self._save()
        return client_id, token, confirm_tag(s["key"], b"server", self.fp)

    # Authentification des requêtes suivantes
    def check(self, token: str):
        h = hashlib.sha256(token.encode()).hexdigest()
        for cid, c in self.clients.items():
            if hmac.compare_digest(h, c["token_sha256"]):
                return cid
        return None

    def revoke(self, client_id: str) -> bool:
        with self.lock:
            ok = self.clients.pop(client_id, None) is not None
            self._save()
        return ok
