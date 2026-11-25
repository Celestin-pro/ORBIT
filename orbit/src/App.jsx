import { useCallback, useEffect, useState } from "react";
import { api, initBridge, toggleFullscreen } from "./api.js";
import { useGamepadConnected, useNav } from "./nav.js";
import Home from "./screens/Home.jsx";
import Hosts from "./screens/Hosts.jsx";
import Launch from "./screens/Launch.jsx";
import Library from "./screens/Library.jsx";
import Settings from "./screens/Settings.jsx";
import { Clock, PadIcon, Spinner } from "./ui.jsx";

const TABS = [
  ["home", "Accueil"],
  ["library", "Bibliothèque"],
  ["settings", "Paramètres"],
];
const ACTIVE_PHASES = ["starting", "streaming", "stopping", "ended", "error"];

function loadPrefs() {
  try {
    return { fullscreen: true, ...JSON.parse(localStorage.getItem("orbit.prefs") || "{}") };
  } catch {
    return { fullscreen: true };
  }
}

export default function App() {
  const [ready, setReady] = useState(false);
  const [fatal, setFatal] = useState(null);
  const [hosts, setHosts] = useState([]);
  const [hostId, setHostId] = useState(() => localStorage.getItem("orbit.host"));
  const [view, setView] = useState("loading");
  const [games, setGames] = useState(null);
  const [gamesError, setGamesError] = useState(null);
  const [session, setSession] = useState({ phase: "idle" });
  const [pad, setPad] = useState(false);
  const [prefs, setPrefsState] = useState(loadPrefs);

  const host = hosts.find((h) => h.id === hostId);
  const inSession = ACTIVE_PHASES.includes(session.phase);
  useGamepadConnected(setPad);

  const setPrefs = (p) => {
    setPrefsState(p);
    localStorage.setItem("orbit.prefs", JSON.stringify(p));
  };

  // ---------------------------------------------------------------- données
  const loadHosts = useCallback(async () => {
    const list = await api("/hosts");
    setHosts(list);
    return list;
  }, []);

  const loadGames = useCallback(
    async (refresh = false) => {
      if (!hostId) return;
      setGamesError(null);
      try {
        setGames(await api(`/hosts/${hostId}/games${refresh ? "?refresh=true" : ""}`));
      } catch (e) {
        setGames(null);
        setGamesError(e.message);
      }
    },
    [hostId],
  );

  useEffect(() => {
    (async () => {
      try {
        await initBridge();
        const list = await loadHosts();
        setReady(true);
        const known = list.find((h) => h.id === hostId) || list[0];
        if (known) {
          setHostId(known.id);
          setView("home");
        } else {
          setView("hosts");
        }
      } catch (e) {
        setFatal(e.message);
      }
    })();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (hostId) localStorage.setItem("orbit.host", hostId);
    setGames(null);
    if (ready && hostId) loadGames();
  }, [hostId, ready, loadGames]);

  // État du PC (latence) toutes les 10 s, état de la session toutes les secondes
  useEffect(() => {
    if (!ready) return;
    const t = setInterval(() => loadHosts().catch(() => {}), 10000);
    return () => clearInterval(t);
  }, [ready, loadHosts]);

  useEffect(() => {
    if (!ready) return;
    const tick = () =>
      api("/session")
        .then(setSession)
        .catch(() => {});
    tick();
    const t = setInterval(tick, inSession ? 700 : 3000);
    return () => clearInterval(t);
  }, [ready, inSession]);

  // ---------------------------------------------------------------- actions
  const play = async (game) => {
    try {
      setSession(await api(`/hosts/${hostId}/play`, { method: "POST", body: { game_id: game?.id ?? null, fullscreen: prefs.fullscreen } }));
    } catch (e) {
      setSession({ phase: "error", steps: [], message: e.message, host_name: host?.name, warnings: [] });
    }
  };
  const stop = () => api("/session/stop", { method: "POST" }).then(setSession);
  const dismiss = useCallback(() => {
    api("/session/dismiss", { method: "POST" })
      .then(setSession)
      .catch(() => setSession({ phase: "idle" }));
    loadHosts().catch(() => {});
    loadGames();
  }, [loadHosts, loadGames]);

  const tabIndex = TABS.findIndex(([k]) => k === view);
  useNav(
    {
      onPrevTab: () => tabIndex >= 0 && setView(TABS[(tabIndex + TABS.length - 1) % TABS.length][0]),
      onNextTab: () => tabIndex >= 0 && setView(TABS[(tabIndex + 1) % TABS.length][0]),
      onBack: () => {
        if (inSession) return session.phase === "error" ? dismiss() : stop();
        if (view === "hosts" && host) setView("home");
        else if (view !== "home" && host) setView("home");
      },
    },
    // Pas de conflit avec le jeu : la manette n'agit sur l'interface que si la fenêtre ORBIT
    // a le focus (pendant le jeu, c'est la fenêtre de streaming qui l'a)
    true,
  );

  useEffect(() => {
    const k = (e) => e.key === "F11" && (e.preventDefault(), toggleFullscreen());
    window.addEventListener("keydown", k);
    return () => window.removeEventListener("keydown", k);
  }, []);

  // ---------------------------------------------------------------- rendu
  if (fatal) {
    return (
      <main className="empty">
        <h1>ORBIT ne démarre pas</h1>
        <p className="error-msg">{fatal}</p>
        <p className="muted">Vérifie que Python et les dépendances du pont sont installés (voir le README).</p>
      </main>
    );
  }
  if (!ready || view === "loading") {
    return (
      <main className="empty">
        <span className="brand big">ORBIT</span>
        <p className="muted">
          <Spinner /> Démarrage…
        </p>
      </main>
    );
  }

  if (inSession) {
    return <Launch hostId={hostId} session={session} onStop={stop} onDismiss={dismiss} />;
  }

  if (view === "hosts" || !host) {
    return (
      <Hosts
        hosts={hosts}
        currentId={hostId}
        onBack={host ? () => setView("home") : null}
        onSelect={(id) => {
          setHostId(id);
          setView("home");
        }}
        onPaired={async (id) => {
          await loadHosts();
          setHostId(id);
          setView("home");
        }}
      />
    );
  }

  return (
    <div className="app">
      <header className="topbar">
        <div className="topbar-left">
          <span className="brand">ORBIT</span>
          <nav className="tabs">
            {TABS.map(([k, label]) => (
              <button key={k} data-nav className={`tab ${view === k ? "is-on" : ""}`} onClick={() => setView(k)}>
                {label}
              </button>
            ))}
          </nav>
        </div>
        <div className="topbar-right">
          <button data-nav className="host-pill" onClick={() => setView("hosts")} title="Changer de PC">
            <span className={`status-dot ${host.online ? "on" : ""}`} aria-hidden="true" />
            <strong>{host.name}</strong>
            <span className="muted">{host.online ? `En ligne · ${host.latency_ms} ms` : host.can_wake ? "Éteint · réveil au lancement" : "Hors ligne"}</span>
          </button>
          <PadIcon connected={pad} />
          <Clock />
        </div>
      </header>

      {view !== "settings" && !games && !gamesError && (
        <main className="empty">
          <p className="muted">
            <Spinner /> Récupération des jeux de {host.name}…
          </p>
        </main>
      )}
      {view !== "settings" && gamesError && (
        <main className="empty">
          <h1>{host.online ? "Impossible de lire les jeux" : `${host.name} est hors ligne`}</h1>
          <p className="muted">{gamesError}</p>
          <div className="hero-actions">
            <button data-nav data-autofocus className="btn btn-primary" onClick={() => play(null)}>
              {host.can_wake && !host.online ? "Réveiller et ouvrir le bureau" : "Ouvrir le bureau à distance"}
            </button>
            <button data-nav className="btn btn-ghost" onClick={() => loadGames()}>
              Réessayer
            </button>
          </div>
        </main>
      )}

      {games && view === "home" && <Home hostId={hostId} games={games} onPlay={play} onOpenLibrary={() => setView("library")} />}
      {games && view === "library" && <Library hostId={hostId} hostName={host.name} games={games} onPlay={play} />}
      {view === "settings" && (
        <Settings
          host={host}
          prefs={prefs}
          setPrefs={setPrefs}
          onRefreshGames={() => loadGames(true)}
          onChangeHost={() => setView("hosts")}
          onUnpaired={async () => {
            const list = await loadHosts();
            setHostId(list[0]?.id ?? null);
            setView(list.length ? "home" : "hosts");
          }}
        />
      )}
    </div>
  );
}
