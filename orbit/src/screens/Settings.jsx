import { useCallback, useEffect, useState } from "react";
import { api, toggleFullscreen } from "../api.js";
import { focusFirst } from "../nav.js";
import { Hints, Spinner } from "../ui.jsx";

const DEPS = [
  ["ffmpeg", "FFmpeg (capture + encodage)", "ffmpeg"],
  ["vigembus", "Pilote manette virtuelle (ViGEmBus)", "vigembus"],
  ["vgamepad", "Manette virtuelle (vgamepad)", "python-deps"],
  ["pyaudiowpatch", "Capture du son", "python-deps"],
  ["psutil", "Suivi des jeux", "python-deps"],
];

function Choice({ label, options, value, onChange }) {
  return (
    <div className="setting">
      <span className="setting-label">{label}</span>
      <div className="filters">
        {options.map(([v, text]) => (
          <button key={String(v)} data-nav className={`pill ${value === v ? "is-on" : ""}`} aria-pressed={value === v} onClick={() => onChange(v)}>
            {text}
          </button>
        ))}
      </div>
    </div>
  );
}

export default function Settings({ host, prefs, setPrefs, onRefreshGames, onChangeHost, onUnpaired }) {
  const [status, setStatus] = useState(null);
  const [video, setVideo] = useState(null);
  const [error, setError] = useState(null);
  const [confirmUnpair, setConfirmUnpair] = useState(false);
  const [player, setPlayer] = useState(null);
  useEffect(() => {
    api("/player").then(setPlayer).catch(() => {});
  }, []);

  const load = useCallback(async () => {
    try {
      const [st, cfg] = await Promise.all([api(`/hosts/${host.id}/status`), api(`/hosts/${host.id}/config`)]);
      setStatus(st);
      setVideo(cfg.video);
      setError(null);
    } catch (e) {
      setError(e.message);
    }
  }, [host.id]);

  useEffect(() => {
    load();
  }, [load]);
  useEffect(() => {
    if (video) focusFirst(".settings .card [data-nav]");
  }, [video === null]); // eslint-disable-line react-hooks/exhaustive-deps

  // Rafraîchit pendant une installation
  useEffect(() => {
    const running = status && Object.values(status.installs || {}).some((j) => j.state === "running");
    if (!running) return;
    const t = setInterval(load, 2000);
    return () => clearInterval(t);
  }, [status, load]);

  const patch = async (change) => {
    try {
      setVideo(await api(`/hosts/${host.id}/config/video`, { method: "PATCH", body: change }));
    } catch (e) {
      setError(e.message);
    }
  };

  const install = async (component) => {
    try {
      await api(`/hosts/${host.id}/install/${component}`, { method: "POST" });
      load();
    } catch (e) {
      setError(e.message);
    }
  };

  const unpair = async () => {
    await api(`/hosts/${host.id}`, { method: "DELETE" });
    onUnpaired();
  };

  return (
    <main className="settings">
      <h1>Paramètres</h1>
      {error && <p className="error-msg">{error}</p>}

      <div className="settings-cols">
        <section className="card">
          <h2>Streaming</h2>
          {!video ? (
            <p className="muted">
              <Spinner /> Chargement…
            </p>
          ) : (
            <>
              <Choice label="Images par seconde" value={video.fps} onChange={(fps) => patch({ fps })} options={[[30, "30"], [60, "60"]]} />
              <Choice
                label="Débit"
                value={video.bitrate_mbps}
                onChange={(bitrate_mbps) => patch({ bitrate_mbps })}
                options={[[10, "10 Mbps"], [15, "15"], [20, "20"], [30, "30"]]}
              />
              <Choice label="Codec" value={video.codec} onChange={(codec) => patch({ codec })} options={[["h264", "H.264"], ["hevc", "HEVC"]]} />
              <Choice
                label="Résolution envoyée"
                value={video.scale || ""}
                onChange={(scale) => patch({ scale })}
                options={[["", "Native"], ["1920x1080", "1080p"], ["1280x720", "720p"]]}
              />
              <Choice
                label="Transport réseau"
                value={video.transport || "udp"}
                onChange={(transport) => patch({ transport })}
                options={[["udp", "UDP · latence minimale"], ["srt", "SRT · corrige les pertes"]]}
              />
              {video.transport === "srt" && (
                <Choice
                  label="Fenêtre de correction SRT"
                  value={video.srt_latency_ms}
                  onChange={(srt_latency_ms) => patch({ srt_latency_ms })}
                  options={[[20, "20 ms"], [40, "40 ms"], [80, "80 ms"], [120, "120 ms"]]}
                />
              )}
              <p className="muted small">
                Image qui se dégrade ou se fige : passe en SRT. Wi-Fi faible : baisse le débit (10–15 Mbps) et passe en 720p.
              </p>
            </>
          )}
        </section>

        <section className="card">
          <h2>Ce PC portable</h2>
          <Choice
            label="Fenêtre de jeu"
            value={prefs.fullscreen}
            onChange={(fullscreen) => setPrefs({ ...prefs, fullscreen })}
            options={[[true, "Plein écran"], [false, "Fenêtrée"]]}
          />
          <div className="setting">
            <span className="setting-label">Lecteur vidéo</span>
            {player?.name === "mpv" ? (
              <span className="small">
                <span className="status-dot on inline" aria-hidden="true" /> mpv · décodage par la carte graphique
              </span>
            ) : (
              <span className="small">
                <span className="status-dot off inline" aria-hidden="true" /> ffplay (décodage processeur, plus lent). Installe mpv :{" "}
                <code>{player?.install_hint}</code>
              </span>
            )}
          </div>
          <div className="setting">
            <span className="setting-label">Interface ORBIT</span>
            <div className="filters">
              <button data-nav className="pill" onClick={toggleFullscreen}>
                Basculer plein écran (F11)
              </button>
            </div>
          </div>
        </section>

        <section className="card">
          <h2>PC fixe · {host.name}</h2>
          <ul className="deps">
            {DEPS.map(([key, label, comp]) => {
              const ok = status?.deps?.[key];
              const job = status?.installs?.[comp];
              return (
                <li key={key}>
                  <span className={`status-dot ${ok ? "on" : "off"}`} aria-hidden="true" />
                  <span className="dep-label">{label}</span>
                  {status && !ok && (
                    <button data-nav className="pill" disabled={job?.state === "running"} onClick={() => install(comp)}>
                      {job?.state === "running" ? "Installation…" : job?.state === "failed" ? "Réessayer" : "Installer"}
                    </button>
                  )}
                  {ok && <span className="muted small">OK</span>}
                </li>
              );
            })}
          </ul>
          <p className="muted small">Après l'installation de FFmpeg ou de ViGEmBus, redémarre le PC fixe.</p>
          <div className="card-actions">
            <button data-nav className="pill" onClick={onRefreshGames}>
              Actualiser les jeux
            </button>
            <button data-nav className="pill" onClick={onChangeHost}>
              Changer de PC
            </button>
            {!confirmUnpair ? (
              <button data-nav className="pill pill-danger" onClick={() => setConfirmUnpair(true)}>
                Oublier ce PC
              </button>
            ) : (
              <button data-nav className="pill pill-danger is-on" onClick={unpair}>
                Confirmer : oublier {host.name}
              </button>
            )}
          </div>
        </section>
      </div>

      <Hints items={[["A", "Choisir"], ["LB", "Onglet"], ["RB", "Onglet"], ["B", "Retour"]]} />
    </main>
  );
}
