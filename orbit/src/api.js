// Accès au pont local (bridge.py).
// Dans Tauri : l'adresse et le jeton viennent de la commande Rust `bridge_info`.
// Dans un navigateur (npm run dev seul) : http://127.0.0.1:47900 avec le jeton "dev".

let base = "http://127.0.0.1:47900";
let token = "dev";

export async function initBridge() {
  if ("__TAURI_INTERNALS__" in window) {
    const { invoke } = await import("@tauri-apps/api/core");
    const info = await invoke("bridge_info");
    base = info.url;
    token = info.token;
  }
  // Le pont met une ou deux secondes à démarrer : on attend qu'il réponde
  for (let i = 0; i < 40; i++) {
    try {
      const r = await fetch(`${base}/health`);
      if (r.ok) return true;
    } catch {
      /* pas encore prêt */
    }
    await new Promise((res) => setTimeout(res, 250));
  }
  throw new Error("Le pont local ne répond pas (bridge.py).");
}

export class ApiError extends Error {
  constructor(status, detail) {
    super(typeof detail === "string" ? detail : JSON.stringify(detail));
    this.status = status;
  }
}

export async function api(path, { method = "GET", body } = {}) {
  const r = await fetch(`${base}${path}`, {
    method,
    headers: { "X-Orbit-Token": token, "Content-Type": "application/json" },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const data = await r.json().catch(() => null);
  if (!r.ok) throw new ApiError(r.status, data?.detail ?? r.statusText);
  return data;
}

// <img> ne peut pas envoyer d'en-tête : le jeton passe en paramètre
export function artUrl(hostId, game, kind = "cover") {
  if (!game.art?.[kind]) return null;
  return `${base}/hosts/${hostId}/games/${encodeURIComponent(game.id)}/art/${kind}?t=${token}`;
}

export async function toggleFullscreen() {
  if ("__TAURI_INTERNALS__" in window) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("toggle_fullscreen");
  }
  if (document.fullscreenElement) return document.exitFullscreen();
  return document.documentElement.requestFullscreen();
}
