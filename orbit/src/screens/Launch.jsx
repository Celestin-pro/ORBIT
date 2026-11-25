// Écran de lancement / en jeu : suit l'état de la session renvoyé par le pont (/session).
import { useEffect } from "react";
import { focusFirst } from "../nav.js";
import { GameArt, Glyph, Spinner } from "../ui.jsx";

function StepDot({ status }) {
  if (status === "done")
    return (
      <span className="dot dot-done">
        <svg aria-hidden="true" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#06121A" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round">
          <path d="M5 12.5l4.5 4.5L19 7.5" />
        </svg>
      </span>
    );
  if (status === "error") return <span className="dot dot-error">!</span>;
  if (status === "active") return <span className="dot dot-active" />;
  return <span className="dot" />;
}

export default function Launch({ hostId, session, onStop, onDismiss }) {
  const { phase, steps, game, host_name, message, warnings } = session;
  const done = steps.filter((s) => s.status === "done").length;
  const pct = steps.length ? Math.round((done / steps.length) * 100) : 0;

  useEffect(() => {
    focusFirst();
  }, [phase]);

  // Fin normale : retour automatique au menu
  useEffect(() => {
    if (phase === "ended") {
      const t = setTimeout(onDismiss, 1200);
      return () => clearTimeout(t);
    }
  }, [phase, onDismiss]);

  const title = game?.name || `Bureau de ${host_name}`;

  return (
    <main className="launch">
      {game && (
        <div className="launch-art">
          <GameArt hostId={hostId} game={game} kind="hero" showTitle={false} />
        </div>
      )}
      <div className="launch-scrim" aria-hidden="true" />

      <section className="launch-panel">
        <span className="brand">ORBIT</span>

        <div className="launch-title">
          <span className="eyebrow">
            {phase === "streaming" ? `En jeu sur ${host_name}` : `Lancement sur ${host_name}`}
          </span>
          <h1>{title}</h1>
        </div>

        {phase !== "streaming" && (
          <>
            <ol className="steps">
              {steps.map((s) => (
                <li key={s.key} className={`step step-${s.status}`}>
                  <StepDot status={s.status} />
                  <span>
                    <span className="step-label">{s.label}</span>
                    {s.detail && <span className="step-detail">{s.detail}</span>}
                  </span>
                </li>
              ))}
            </ol>
            <div className="progress" role="progressbar" aria-label="Progression du lancement" aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct}>
              <div style={{ width: `${pct}%` }} />
            </div>
          </>
        )}

        {phase === "streaming" && (
          <div className="ingame">
            <p>
              Le jeu s'affiche dans la fenêtre de streaming. <strong>Ctrl+Alt+Shift+Z</strong> capture ou rend le clavier et la souris.
            </p>
            <p className="muted">Ferme la fenêtre de streaming (Échap ou Q) ou utilise le bouton ci-dessous pour terminer.</p>
          </div>
        )}

        {warnings?.length > 0 && (
          <ul className="warnings">
            {warnings.slice(-3).map((w, i) => (
              <li key={i}>{w}</li>
            ))}
          </ul>
        )}

        {phase === "error" && <p className="error-msg">{message}</p>}

        <div className="launch-actions">
          {(phase === "starting" || phase === "streaming") && (
            <button data-nav data-autofocus className="btn btn-ghost" onClick={onStop}>
              <Glyph k="B" /> {phase === "streaming" ? "Terminer la session" : "Annuler"}
            </button>
          )}
          {phase === "stopping" && (
            <span className="muted">
              <Spinner /> Fermeture de la session…
            </span>
          )}
          {phase === "error" && (
            <button data-nav data-autofocus className="btn btn-ghost" onClick={onDismiss}>
              <Glyph k="B" /> Revenir au menu
            </button>
          )}
          {phase === "ended" && <span className="muted">Session terminée</span>}
        </div>
      </section>
    </main>
  );
}
