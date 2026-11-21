// Loading, empty and error states, and the confirmation dialog.

import { h } from "./dom.js";

/** Shown only if loading takes longer than a moment (CSS delays it). */
export function loading(text) {
  return h("p", { class: "loading" }, text);
}

/** What this place is and how to fill it, with an optional next step (a node). */
export function emptyState(title, body, action = null) {
  return h("div", { class: "state" }, h("p", { class: "state-title" }, title), body ? h("p", {}, body) : null, action);
}

/** What to do next, per error code; the server's message says what went wrong. */
const ADVICE = {
  network: "Check that knowa serve is still running in your terminal, then retry.",
  usage: "Set what the message names, then retry.",
  not_found: "It may be in another space: choose another space or All spaces.",
  invalid: "Correct the input, then retry.",
  conflict: "Resolve the conflict the message names in a terminal, then retry.",
  unsupported: "Narrow the request as the message suggests.",
  cursor_expired: "The index was rebuilt since this page loaded. Retry to start from the beginning.",
  integrity: "Run knowa verify in a terminal to see what failed.",
  internal: "Retry. If it keeps failing, the terminal running knowa serve shows the cause.",
  app: "Reload the page. If it happens again, the browser console has the details.",
};

// Error codes only grow (DESIGN §6.4): one this app does not know yet gets the generic step.
const advice = (code) => ADVICE[code] ?? ADVICE.internal;

/** An API error: what failed, the server's message, and what to do next. */
export function errorState(title, err, retry) {
  return h(
    "div",
    { class: "state state-error", role: "alert" },
    h("p", { class: "state-title" }, title),
    h("p", {}, err.message),
    h("p", {}, advice(err.code)),
    err.code !== "network" && err.code !== "app" ? h("p", { class: "state-detail" }, `Error code: ${err.code}`) : null,
    retry ? h("button", { type: "button", class: "btn", onclick: retry }, "Retry") : null,
  );
}

/** An error beside the control that caused it, e.g. a review that failed. */
export function inlineError(err) {
  return h("p", { class: "inline-error", role: "alert" }, `${err.message} ${advice(err.code)}`);
}

/**
 * Asks before a destructive action. Resolves true only for the confirm button; Escape
 * and Cancel resolve false. Focus starts on Cancel and returns to the trigger.
 */
export function confirmAction({ title, body, confirmLabel }) {
  const dialog = document.getElementById("confirm");
  const trigger = document.activeElement;
  document.getElementById("confirm-title").textContent = title;
  document.getElementById("confirm-body").textContent = body;
  document.getElementById("confirm-ok").textContent = confirmLabel;
  dialog.returnValue = "";
  dialog.showModal();
  document.getElementById("confirm-cancel").focus();
  return new Promise((resolve) => {
    dialog.addEventListener(
      "close",
      () => {
        if (trigger instanceof HTMLElement && trigger.isConnected) trigger.focus();
        resolve(dialog.returnValue === "confirm");
      },
      { once: true },
    );
  });
}
