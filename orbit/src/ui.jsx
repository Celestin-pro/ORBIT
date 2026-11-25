// Petits composants partagés : glyphes de manette, tuile de jeu, pied de page, icônes.
import { useEffect, useState } from "react";
import { artUrl } from "./api.js";

const GLYPH_COLORS = { A: "#5EE6A8", B: "#F28C6B", X: "#8FB8FF", Y: "#F5C451" };

export function Glyph({ k, size = 22 }) {
  if (k === "LB" || k === "RB" || k === "≡") {
    return (
      <span className="glyph glyph-shoulder" style={{ height: size }}>
        {k}
      </span>
    );
  }
  return (
    <span className="glyph" style={{ width: size, height: size, background: GLYPH_COLORS[k] }}>
      {k}
    </span>
  );
}

export function Hints({ items }) {
  return (
    <footer className="hints">
      {items.map(([k, label]) => (
        <span key={k + label}>
          <Glyph k={k} /> {label}
        </span>
      ))}
    </footer>
  );
}

// Couleur stable par jeu (placeholder quand il n'y a pas de jaquette)
const PALETTE = ["#7A3B2E", "#3B2A7A", "#2E5A6B", "#1F4D3A", "#6B5A1F", "#5A2E55", "#2E3F6B", "#6B2E3F"];
export function gameColor(name) {
  let h = 0;
  for (const c of name) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return PALETTE[h % PALETTE.length];
}

export const SOURCE_LABEL = {
  steam: "Steam",
  epic: "Epic Games",
  gog: "GOG",
  ubisoft: "Ubisoft Connect",
  rockstar: "Rockstar",
  custom: "Perso",
};

/** Image d'un jeu : image locale (via l'agent) -> image en ligne (Steam) -> couleur + titre. */
export function GameArt({ hostId, game, kind = "cover", className = "", showTitle = true }) {
  const sources = [artUrl(hostId, game, kind), game.remote_art?.[kind]].filter(Boolean);
  const [i, setI] = useState(0);
  const src = sources[i];
  return (
    <div className={`art ${className}`} style={{ background: gameColor(game.name) }}>
      {src && <img src={src} alt="" draggable="false" onError={() => setI(i + 1)} />}
      {(!src || showTitle === "always") && showTitle && <span className="art-title">{game.name}</span>}
    </div>
  );
}

export function Tile({ hostId, game, selected, onFocus, onClick, wide = false, ...rest }) {
  return (
    <button
      {...rest}
      data-nav
      className={`tile ${wide ? "tile-wide" : ""} ${selected ? "is-selected" : ""}`}
      onFocus={onFocus}
      onClick={onClick}
      aria-label={game.name}
    >
      <GameArt hostId={hostId} game={game} kind={wide ? "header" : "cover"} />
    </button>
  );
}

export function Spinner() {
  return <span className="spinner" aria-hidden="true" />;
}

export function PadIcon({ connected }) {
  return (
    <svg
      role="img"
      aria-label={connected ? "Manette connectée" : "Aucune manette"}
      width="26"
      height="26"
      viewBox="0 0 24 24"
      fill="none"
      stroke={connected ? "#C3CAD8" : "#4A5366"}
      strokeWidth="1.6"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d="M6 11h4M8 9v4M15 12h.01M18 10h.01" />
      <path d="M17.3 5H6.7a4 4 0 0 0-3.95 3.4L2 14.6A3 3 0 0 0 7.3 17l1.2-1.5h7l1.2 1.5a3 3 0 0 0 5.3-2.4l-.75-6.2A4 4 0 0 0 17.3 5z" />
    </svg>
  );
}

export function Clock() {
  const [now, setNow] = useState(new Date());
  useEffect(() => {
    const t = setInterval(() => setNow(new Date()), 15000);
    return () => clearInterval(t);
  }, []);
  return <span className="clock">{now.toLocaleTimeString("fr-FR", { hour: "2-digit", minute: "2-digit" })}</span>;
}
