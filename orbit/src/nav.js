// Navigation « console » : flèches du clavier ET manette (Gamepad API).
//
// Tout élément focusable marqué data-nav participe. Le déplacement est SPATIAL :
// on choisit l'élément le plus proche dans la direction demandée, d'après sa position
// à l'écran. Pas de grille à déclarer : ça marche pour n'importe quelle mise en page.
//
// Manette (mapping standard) : croix / stick gauche = déplacer, A = valider,
// B = retour, LB / RB = onglet précédent / suivant, Y = action secondaire.
import { useEffect, useRef } from "react";

const DIRS = { ArrowUp: "up", ArrowDown: "down", ArrowLeft: "left", ArrowRight: "right" };

function visible(el) {
  const r = el.getBoundingClientRect();
  return r.width > 0 && r.height > 0 && !el.disabled && getComputedStyle(el).visibility !== "hidden";
}

export function focusables() {
  return [...document.querySelectorAll("[data-nav]")].filter(visible);
}

export function focusFirst(selector = "[data-autofocus]") {
  const el = document.querySelector(selector) || focusables()[0];
  el?.focus({ preventScroll: false });
}

export function moveFocus(dir) {
  const els = focusables();
  const cur = document.activeElement;
  if (!els.includes(cur)) return els[0]?.focus();
  const a = cur.getBoundingClientRect();
  const ax = a.left + a.width / 2;
  const ay = a.top + a.height / 2;
  let best = null;
  let bestScore = Infinity;
  for (const el of els) {
    if (el === cur) continue;
    const b = el.getBoundingClientRect();
    const bx = b.left + b.width / 2;
    const by = b.top + b.height / 2;
    const dx = bx - ax;
    const dy = by - ay;
    const main = { up: -dy, down: dy, left: -dx, right: dx }[dir];
    if (main <= 4) continue; // pas dans la bonne direction
    const side = dir === "up" || dir === "down" ? Math.abs(dx) : Math.abs(dy);
    const score = main + side * 2.2; // on préfère rester aligné
    if (score < bestScore) {
      bestScore = score;
      best = el;
    }
  }
  if (best) {
    best.focus({ preventScroll: true });
    best.scrollIntoView({ block: "nearest", inline: "nearest", behavior: "smooth" });
  }
}

const isTyping = () => {
  const el = document.activeElement;
  return el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA");
};

/**
 * handlers : { onBack, onPrevTab, onNextTab, onSecondary }
 * enabled  : false pendant une session (la manette pilote alors le jeu, pas l'interface)
 */
export function useNav(handlers, enabled = true) {
  const h = useRef(handlers);
  h.current = handlers;
  const on = useRef(enabled);
  on.current = enabled;

  // ---- clavier
  useEffect(() => {
    const onKey = (e) => {
      if (!on.current) return;
      if (DIRS[e.key] && !(isTyping() && (e.key === "ArrowLeft" || e.key === "ArrowRight"))) {
        e.preventDefault();
        moveFocus(DIRS[e.key]);
      } else if (e.key === "Escape" || (e.key === "Backspace" && !isTyping())) {
        h.current.onBack?.();
      } else if (e.key === "PageUp") {
        h.current.onPrevTab?.();
      } else if (e.key === "PageDown") {
        h.current.onNextTab?.();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // ---- manette
  useEffect(() => {
    let raf;
    const prev = {};
    const repeatAt = {};
    const press = (id, now, fn, repeat = false) => {
      if (!prev[id]) {
        fn();
        repeatAt[id] = now + 380;
      } else if (repeat && now >= repeatAt[id]) {
        fn();
        repeatAt[id] = now + 110;
      }
    };
    const loop = (now) => {
      raf = requestAnimationFrame(loop);
      const pad = [...(navigator.getGamepads?.() || [])].find(Boolean);
      if (!pad || !on.current || !document.hasFocus()) return;
      const b = (i) => pad.buttons[i]?.pressed;
      const ax = pad.axes[0] || 0;
      const ay = pad.axes[1] || 0;
      const state = {
        up: b(12) || ay < -0.6,
        down: b(13) || ay > 0.6,
        left: b(14) || ax < -0.6,
        right: b(15) || ax > 0.6,
        a: b(0),
        b: b(1),
        y: b(3),
        lb: b(4),
        rb: b(5),
      };
      for (const d of ["up", "down", "left", "right"]) {
        if (state[d]) press(d, now, () => moveFocus(d), true);
      }
      if (state.a) press("a", now, () => document.activeElement?.click());
      if (state.b) press("b", now, () => h.current.onBack?.());
      if (state.y) press("y", now, () => h.current.onSecondary?.());
      if (state.lb) press("lb", now, () => h.current.onPrevTab?.());
      if (state.rb) press("rb", now, () => h.current.onNextTab?.());
      Object.assign(prev, state);
    };
    raf = requestAnimationFrame(loop);
    return () => cancelAnimationFrame(raf);
  }, []);
}

// Présence d'une manette (pour l'icône de la barre du haut)
export function useGamepadConnected(setConnected) {
  useEffect(() => {
    const update = () => setConnected([...(navigator.getGamepads?.() || [])].some(Boolean));
    window.addEventListener("gamepadconnected", update);
    window.addEventListener("gamepaddisconnected", update);
    update();
    return () => {
      window.removeEventListener("gamepadconnected", update);
      window.removeEventListener("gamepaddisconnected", update);
    };
  }, [setConnected]);
}
