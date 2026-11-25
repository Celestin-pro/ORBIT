import { useEffect, useMemo, useState } from "react";
import { focusFirst } from "../nav.js";
import { GameArt, Glyph, Hints, SOURCE_LABEL, Tile } from "../ui.jsx";

function lastPlayedText(ts) {
  if (!ts) return "Jamais lancé";
  const days = Math.floor((Date.now() / 1000 - ts) / 86400);
  if (days <= 0) return "Dernière session aujourd'hui";
  if (days === 1) return "Dernière session hier";
  if (days < 30) return `Dernière session il y a ${days} jours`;
  return `Dernière session le ${new Date(ts * 1000).toLocaleDateString("fr-FR")}`;
}

export default function Home({ hostId, games, onPlay, onOpenLibrary }) {
  // Les jeux joués récemment d'abord, puis le reste par ordre alphabétique
  const row = useMemo(
    () => [...games].sort((a, b) => (b.last_played || 0) - (a.last_played || 0) || a.name.localeCompare(b.name)).slice(0, 8),
    [games],
  );
  const [selId, setSelId] = useState(row[0]?.id);
  const sel = row.find((g) => g.id === selId) || row[0];

  useEffect(() => {
    focusFirst();
  }, []);

  if (!sel) {
    return (
      <main className="empty">
        <h1>Aucun jeu détecté sur ce PC</h1>
        <p>Installe un jeu Steam, Epic, GOG, Ubisoft ou Rockstar sur le PC fixe, puis actualise dans Paramètres.</p>
      </main>
    );
  }

  return (
    <main className="home">
      <div className="hero-art" key={sel.id}>
        <GameArt hostId={hostId} game={sel} kind="hero" showTitle={false} />
      </div>
      <div className="hero-scrim" aria-hidden="true" />

      <section className="hero">
        <span className="eyebrow">{SOURCE_LABEL[sel.source] || sel.source}</span>
        <h1>{sel.name}</h1>
        <p className="muted">{lastPlayedText(sel.last_played)}</p>
        <div className="hero-actions">
          <button data-nav className="btn btn-primary" onClick={() => onPlay(sel)}>
            <Glyph k="A" /> Jouer
          </button>
        </div>
      </section>

      <section className="row">
        <div className="row-head">
          <h2>Récemment joués</h2>
          <button data-nav className="link" onClick={onOpenLibrary}>
            Toute la bibliothèque · {games.length} jeux
          </button>
        </div>
        <div className="row-tiles">
          {row.map((g, i) => (
            <Tile
              key={g.id}
              hostId={hostId}
              game={g}
              selected={g.id === sel.id}
              onFocus={() => setSelId(g.id)}
              onClick={() => onPlay(g)}
              {...(i === 0 ? { "data-autofocus": true } : {})}
            />
          ))}
        </div>
      </section>

      <Hints items={[["A", "Jouer"], ["LB", "Onglet"], ["RB", "Onglet"], ["B", "Retour"]]} />
    </main>
  );
}
