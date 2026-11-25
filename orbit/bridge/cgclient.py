"""Bibliothèque client : découverte mDNS, appairage SPAKE2, appels HTTPS à l'agent.

Tout ce que fait l'appli React plus tard passera par les mêmes routes HTTP ;
ce module sert de référence (et de client de test).
"""
import hashlib
import hmac
import http.client
import json
import os
import socket
import ssl
import time
from pathlib import Path

SERVICE_TYPE = "_cloudgaming._tcp.local."
STORE = Path(os.environ.get("APPDATA", Path.home())) / "CloudGaming" / "hosts.json"


class AgentError(Exception):
    def __init__(self, status, detail):
        super().__init__(f"{status} : {detail}")
        self.status, self.detail = status, detail


# ---------------------------------------------------------------- découverte
def discover(timeout=3.0):
    """Liste les agents annoncés sur le réseau local."""
    from zeroconf import ServiceBrowser, Zeroconf

    found = {}

    class Listener:
        def add_service(self, zc, type_, name):
            info = zc.get_service_info(type_, name, timeout=1500)
            if info:
                props = {k.decode(): (v.decode() if v else "") for k, v in info.properties.items()}
                found[name] = dict(name=props.get("name", name.split(".")[0]),
                                   id=props.get("id"), version=props.get("ver"),
                                   addresses=info.parsed_addresses(), port=info.port,
                                   fp_prefix=props.get("fp"))

        def update_service(self, zc, type_, name):
            self.add_service(zc, type_, name)

        def remove_service(self, zc, type_, name):
            found.pop(name, None)

    zc = Zeroconf()
    try:
        ServiceBrowser(zc, SERVICE_TYPE, Listener())
        time.sleep(timeout)
    finally:
        zc.close()
    return list(found.values())


# ---------------------------------------------------------------- stockage local
def load_hosts():
    return json.loads(STORE.read_text("utf-8")) if STORE.exists() else {}


def save_hosts(hosts):
    STORE.parent.mkdir(parents=True, exist_ok=True)
    STORE.write_text(json.dumps(hosts, indent=2), "utf-8")


# ---------------------------------------------------------------- HTTPS épinglé
class Agent:
    """Client HTTPS vers un agent.

    Le certificat de l'agent est auto-signé : on ne le vérifie pas avec une autorité,
    on compare son EMPREINTE à celle enregistrée lors de l'appairage (épinglage).
    """

    def __init__(self, host, port=47800, fingerprint=None, token=None):
        self.host, self.port = host, port
        self.fingerprint, self.token = fingerprint, token
        self._ctx = ssl.create_default_context()
        self._ctx.check_hostname = False
        self._ctx.verify_mode = ssl.CERT_NONE     # remplacé par la vérification d'empreinte ci-dessous

    def request(self, method, path, body=None, timeout=10):
        status, data = self._do(method, path, body, timeout)
        data = json.loads(data or b"null")
        if status >= 400:
            detail = data.get("detail") if isinstance(data, dict) else data
            raise AgentError(status, detail)
        return data

    def request_raw(self, path, timeout=10):
        """GET qui renvoie des octets (images)."""
        status, data = self._do("GET", path, None, timeout)
        if status >= 400:
            raise AgentError(status, "introuvable")
        return data

    def _do(self, method, path, body, timeout):
        conn = http.client.HTTPSConnection(self.host, self.port, context=self._ctx, timeout=timeout)
        try:
            conn.connect()
            seen = hashlib.sha256(conn.sock.getpeercert(binary_form=True)).hexdigest()
            if self.fingerprint and not hmac.compare_digest(seen, self.fingerprint):
                raise AgentError(0, "EMPREINTE DIFFÉRENTE : ce n'est pas le PC appairé "
                                    "(réinstallation de l'agent ou interception). Ré-appaire-le.")
            self.last_seen_fp = seen
            headers = {"Content-Type": "application/json"}
            if self.token:
                headers["Authorization"] = f"Bearer {self.token}"
            conn.request(method, path, body=json.dumps(body) if body is not None else None,
                         headers=headers)
            resp = conn.getresponse()
            return resp.status, resp.read()
        finally:
            conn.close()

    def peer_fingerprint(self):
        self.request("GET", "/info")
        return self.last_seen_fp


# ---------------------------------------------------------------- appairage
def confirm_tag(key, role, fingerprint):
    return hmac.new(key, b"cg-" + role + b"|" + fingerprint.encode(), hashlib.sha256).hexdigest()


def pair(host, port, client_name, ask_pin):
    """Appaire ce portable avec l'agent. ask_pin() est appelé quand le PIN est affiché
    sur le PC fixe et doit renvoyer les 6 chiffres saisis."""
    from spake2 import SPAKE2_A

    agent = Agent(host, port)
    info = agent.request("GET", "/info")
    fp = agent.last_seen_fp                       # empreinte VUE par nous (pas celle annoncée)
    sid = agent.request("POST", "/pair/begin")["session_id"]

    pin = ask_pin().replace(" ", "").strip()
    a = SPAKE2_A(pin.encode())
    msg_a = a.start()
    msg_b = bytes.fromhex(agent.request("POST", "/pair/exchange",
                                        {"session_id": sid, "msg_a": msg_a.hex()})["msg_b"])
    key = a.finish(msg_b)
    res = agent.request("POST", "/pair/confirm", {"session_id": sid, "client_name": client_name,
                                                   "confirm": confirm_tag(key, b"client", fp)})
    if not hmac.compare_digest(res["server_confirm"], confirm_tag(key, b"server", fp)):
        raise AgentError(0, "l'agent n'a pas prouvé qu'il connaît le PIN : appairage annulé")

    hosts = load_hosts()
    hosts[info["id"]] = dict(name=info["name"], host=host, port=port, fingerprint=fp,
                             token=res["token"], client_id=res["client_id"])
    save_hosts(hosts)
    return info["id"], hosts[info["id"]]


def agent_for(entry):
    return Agent(entry["host"], entry["port"], entry["fingerprint"], entry["token"])


def resolve_lan_ip(target):
    """IP locale de CE PC utilisée pour joindre 'target' (sert à l'affichage)."""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect((target, 9))
        return s.getsockname()[0]
    finally:
        s.close()
