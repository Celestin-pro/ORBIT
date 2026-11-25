import { useEffect, useMemo, useState } from "react";
import { focusFirst } from "../nav.js";
import { Hints, SOURCE_LABEL, Tile } from "../ui.jsx";

export default function Library({ hostId, hostName, games, onPlay }) {
  const sources = useMemo(() => ["all", ...new Set(games.map((g) => g.source))], [games]);
  const [filter, setFilter] = useState("all");
  const shown = filter === "all" ? games : games.filter((g) => g.source === filter);

  useEffect(() => {
    focusFirst(".grid .tile");
  }, []);

  return (
    <main className="library">
      <div className="library-head">
        <div className="title-line">
          <h1>Bibliothèque</h1>
          <span className="muted">
            {games.length} jeux sur {hostName}
          </span>
        </div>
        <div className="filters" role="group" aria-label="Filtrer par launcher">
          {sources.map((s) => (
            <button
              key={s}
              data-nav
              className={`pill ${filter === s ? "is-on" : ""}`}
              aria-pressed={filter === s}
              onClick={() => setFilter(s)}
            >
              {s === "all" ? "Tous" : SOURCE_LABEL[s] || s}
            </button>
          ))}
        </div>
      </div>

      <div className="grid">
        {shown.map((g) => (
          <div className="grid-item" key={g.id}>
            <Tile hostId={hostId} game={g} wide onClick={() => onPlay(g)} />
            <span className="grid-name">{g.name}</span>
            <span className="grid-src">{SOURCE_LABEL[g.source] || g.source}</span>
          </div>
        ))}
      </div>

      <Hints items={[["A", "Jouer"], ["LB", "Onglet"], ["RB", "Onglet"], ["B", "Retour"]]} />
    </main>
  );
}
