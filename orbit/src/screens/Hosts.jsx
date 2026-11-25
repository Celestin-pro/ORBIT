// Choix du PC fixe : PC déjà appairés, PC détectés sur le réseau, appairage par code.
import { useEffect, useState } from "react";
import { api } from "../api.js";
import { focusFirst } from "../nav.js";
import { Hints, Spinner } from "../ui.jsx";

function PinPad({ hostName, onSubmit, onCancel, error, busy }) {
  const [pin, setPin] = useState("");
  const add = (d) => setPin((p) => (p.length < 6 ? p + d : p));
  useEffect(() => {
    focusFirst();
  }, []);
  useEffect(() => {
    if (pin.length === 6 && !busy) onSubmit(pin);
  }, [pin]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (error) setPin("");
  }, [error]);
  const hint = error ? "Nouveau code demandé : entre celui qui vient de s'afficher." : null;
  // Saisie au clavier physique aussi
  useEffect(() => {
    const k = (e) => {
      if (/^\d$/.test(e.key)) add(e.key);
      if (e.key === "Backspace") setPin((p) => p.slice(0, -1));
    };
    window.addEventListener("keydown", k);
    return () => window.removeEventListener("keydown", k);
  }, []);

  return (
    <div className="pinpad">
      <h2>Code affiché sur {hostName}</h2>
      <div className="pin-display" aria-live="polite">
        {[0, 1, 2, 3, 4, 5].map((i) => (
          <span key={i} className={`pin-digit ${i === 3 ? "gap" : ""}`}>
            {pin[i] || ""}
          </span>
        ))}
      </div>
      {error && <p className="error-msg">{error}</p>}
      {hint && <p className="muted">{hint}</p>}
      {busy && (
        <p className="muted">
          <Spinner /> Vérification…
        </p>
      )}
      <div className="pin-keys">
        {["1", "2", "3", "4", "5", "6", "7", "8", "9"].map((d, i) => (
          <button key={d} data-nav {...(i === 0 ? { "data-autofocus": true } : {})} className="key" onClick={() => add(d)}>
            {d}
          </button>
        ))}
        <button data-nav className="key key-muted" onClick={() => setPin((p) => p.slice(0, -1))} aria-label="Effacer">
          ⌫
        </button>
        <button data-nav className="key" onClick={() => add("0")}>
          0
        </button>
        <button data-nav className="key key-muted" onClick={onCancel}>
          Annuler
        </button>
      </div>
    </div>
  );
}

export default function Hosts({ hosts, currentId, onSelect, onPaired, onBack }) {
  const [found, setFound] = useState(null);
  const [scanning, setScanning] = useState(false);
  const [manual, setManual] = useState("");
  const [pairing, setPairing] = useState(null); // { pair_id, name }
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  const scan = async () => {
    setScanning(true);
    setError(null);
    try {
      setFound(await api("/discover?timeout=3"));
    } catch (e) {
      setError(e.message);
    }
    setScanning(false);
  };

  useEffect(() => {
    scan();
    focusFirst();
  }, []);

  const begin = async (host, port = 47800, keepError = false) => {
    if (!keepError) setError(null);
    setBusy(true);
    try {
      const res = await api("/pair/begin", { method: "POST", body: { host, port } });
      setPairing({ ...res, host, port });
    } catch (e) {
      setError(e.message);
    }
    setBusy(false);
  };

  const finish = async (pin) => {
    setBusy(true);
    setError(null);
    try {
      const res = await api("/pair/finish", { method: "POST", body: { pair_id: pairing.pair_id, pin } });
      setPairing(null);
      onPaired(res.id);
    } catch (e) {
      // Code faux : l'agent invalide la session, il faut redemander un code
      setError(`${e.message}. Un nouveau code va s'afficher sur le PC.`);
      const { host, port } = pairing;
      setPairing(null);
      setTimeout(() => begin(host, port, true), 5500); // l'agent impose 5 s entre deux demandes
    }
    setBusy(false);
  };

  if (pairing) {
    return (
      <main className="hosts">
        <PinPad hostName={pairing.name} onSubmit={finish} onCancel={() => setPairing(null)} error={error} busy={busy} />
      </main>
    );
  }

  const others = (found || []).filter((f) => !f.paired);

  return (
    <main className="hosts">
      <h1>Choisir un PC</h1>

      {hosts.length > 0 && (
        <section>
          <h2>Mes PC</h2>
          <div className="host-list">
            {hosts.map((h, i) => (
              <button
                key={h.id}
                data-nav
                {...(i === 0 ? { "data-autofocus": true } : {})}
                className={`host-card ${h.id === currentId ? "is-on" : ""}`}
                onClick={() => onSelect(h.id)}
              >
                <span className={`status-dot ${h.online ? "on" : ""}`} aria-hidden="true" />
                <span className="host-name">{h.name}</span>
                <span className="muted">
                  {h.online ? `En ligne · ${h.latency_ms} ms` : h.can_wake ? "Éteint · réveil possible" : "Hors ligne"}
                </span>
              </button>
            ))}
          </div>
        </section>
      )}

      <section>
        <div className="row-head">
          <h2>Détectés sur le réseau</h2>
          <button data-nav className="link" onClick={scan} disabled={scanning}>
            {scanning ? "Recherche…" : "Relancer la recherche"}
          </button>
        </div>
        {scanning && !found && (
          <p className="muted">
            <Spinner /> Recherche des PC…
          </p>
        )}
        {found && others.length === 0 && <p className="muted">Aucun nouveau PC trouvé. L'agent tourne-t-il sur le PC fixe ? Réseau en profil Privé ?</p>}
        <div className="host-list">
          {others.map((f) => (
            <button
              key={f.id}
              data-nav
              className="host-card"
              onClick={() => begin(f.addresses[0], f.port)}
            >
              <span className="status-dot on" aria-hidden="true" />
              <span className="host-name">{f.name}</span>
              <span className="muted">
                {f.addresses[0]} · Appairer
              </span>
            </button>
          ))}
        </div>
      </section>

      <section className="manual">
        <label htmlFor="manual-ip">Ou par adresse IP</label>
        <div className="manual-line">
          <input id="manual-ip" data-nav value={manual} onChange={(e) => setManual(e.target.value)} placeholder="192.168.1.107" />
          <button
            data-nav
            className="btn btn-ghost"
            disabled={!manual || busy}
            onClick={() => begin(manual.trim())}
          >
            Appairer
          </button>
        </div>
      </section>

      {error && <p className="error-msg">{error}</p>}
      <Hints items={[["A", "Choisir"], ...(onBack ? [["B", "Retour"]] : [])]} />
    </main>
  );
}
