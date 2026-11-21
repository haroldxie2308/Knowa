// Boot, the shared chrome (views, space, view controls) and the hash router.

import * as api from "./api.js";
import { announce, append, h } from "./dom.js";
import { KINDS, RouteError, STATUSES, controls, href, parse, withControls } from "./route.js";
import { emptyState, errorState, loading } from "./ui.js";
import * as entity from "./views/entity.js";
import * as gallery from "./views/gallery.js";
import * as review from "./views/review.js";
import * as search from "./views/search.js";
import * as timeline from "./views/timeline.js";

const VIEWS = { search, timeline, gallery, review, entity };
const LOADING_TEXT = { search: "Loading entities…", timeline: "Loading the timeline…", gallery: "Loading photos…", review: "Loading proposed claims…", entity: "Loading…" };

const main = document.getElementById("main");
const VIEW_HINT = document.getElementById("view-hint").textContent;
const spaceSelect = document.getElementById("space");
let registry = [];
let shown = null;
let controller = null;
let objectUrls = [];

/** The page cannot call the API: no token, or the server no longer accepts it. */
function needsLink(expired) {
  document.title = "Knowa";
  document.body.dataset.locked = "";
  main.replaceChildren(
    h("h1", { tabindex: "-1" }, expired ? "This page’s access key has expired" : "Open the link from knowa serve"),
    emptyState(
      expired ? "knowa serve was restarted, and each start prints a new address." : "This page needs the address knowa serve printed in your terminal.",
      "Open the address that ends in #k= followed by the access key. To get a new one, restart knowa serve.",
    ),
  );
}

function isAbort(e) {
  return e?.name === "AbortError";
}

/**
 * Runs `fn` and puts what it returns into `container`: a loading line while it runs,
 * an error state with Retry when the API fails.
 */
async function load(container, errorTitle, fn, loadingText = "Loading…") {
  container.replaceChildren(loading(loadingText));
  container.setAttribute("aria-busy", "true");
  try {
    container.replaceChildren(...[await fn()].flat().filter(Boolean));
  } catch (e) {
    if (isAbort(e)) return;
    if (e instanceof api.ApiError && e.status === 401) {
      api.forgetToken();
      needsLink(true);
      return;
    }
    if (!(e instanceof api.ApiError)) {
      // A fault in the app itself: say so instead of loading forever, and still raise it.
      container.replaceChildren(errorState(errorTitle, { code: "app", message: `The app failed: ${e.message}` }, () => location.reload()));
      throw e;
    }
    container.replaceChildren(errorState(errorTitle, e, () => load(container, errorTitle, fn, loadingText)));
  } finally {
    container.removeAttribute("aria-busy");
  }
}

function objectUrl(blob) {
  const url = URL.createObjectURL(blob);
  objectUrls.push(url);
  return url;
}

// ---------- chrome ----------

function buildSpaceOptions() {
  spaceSelect.replaceChildren();
  append(spaceSelect, [
    h("option", { value: "", disabled: true }, "Choose a space"),
    registry.map((r) => h("option", { value: r.alias }, r.readonly ? `${r.alias} (read-only)` : r.alias)),
    registry.length > 1 ? h("option", { value: "*" }, "All spaces") : null,
  ]);
  spaceSelect.disabled = registry.length === 0;
}

function buildViewOptions() {
  const box = (group, value, label) =>
    h("label", { class: "check" }, h("input", { type: "checkbox", name: group, value }), label);
  document.getElementById("status-options").replaceChildren(...STATUSES.map((s) => box("status", s, s)));
  document.getElementById("kind-options").replaceChildren(...Object.keys(KINDS).map((k) => box("kinds", k, k)));
}

function summary(c) {
  const statuses = c.statuses.join(", ");
  const kinds = c.kinds.length === Object.keys(KINDS).length ? "all author kinds" : `${c.kinds.join(", ")} authors`;
  return `${statuses} · ${kinds}`;
}

function syncChrome(route, c) {
  for (const a of document.querySelectorAll(".tabs a")) {
    a.href = href(c, a.dataset.view);
    if (a.dataset.view === route.view) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  }
  const current = registry.find((r) => r.current)?.alias ?? "";
  spaceSelect.value = c.space ?? current;
  for (const input of document.querySelectorAll("#status-options input")) input.checked = c.statuses.includes(input.value);
  for (const input of document.querySelectorAll("#kind-options input")) input.checked = c.kinds.includes(input.value);
  document.getElementById("view-summary").textContent = summary(c);
  document.getElementById("view-hint").textContent = VIEW_HINT;
}

/** The view controls as the chrome shows them; the space stays as the link has it. */
function chromeControls(route) {
  const checked = (id) => [...document.querySelectorAll(`#${id} input:checked`)].map((i) => i.value);
  return { space: route.params.get("space"), statuses: checked("status-options"), kinds: checked("kind-options") };
}

function onControlsChange(event) {
  const route = parse(location.hash);
  const next = chromeControls(route);
  const input = event.target;
  if (next.statuses.length === 0 || next.kinds.length === 0) {
    input.checked = true;
    document.getElementById("view-hint").textContent =
      input.name === "status" ? "Keep at least one status: a view without one shows nothing." : "Keep at least one author kind: a view without one shows nothing.";
    return;
  }
  location.hash = withControls(route, next);
}

// ---------- router ----------

async function render() {
  const route = parse(location.hash);
  controller?.abort();
  controller = new AbortController();
  for (const url of objectUrls) URL.revokeObjectURL(url);
  objectUrls = [];
  const changedView = shown === null || shown.view !== route.view || shown.id !== route.id;
  const first = shown === null;
  shown = route;

  let c;
  try {
    c = controls(route.params);
  } catch (e) {
    if (!(e instanceof RouteError)) throw e;
    document.title = "Invalid link · Knowa";
    main.replaceChildren(
      h("h1", { tabindex: "-1" }, "This link’s view is not valid"),
      emptyState(e.message, "Open the view with its default statuses and author kinds instead.", h("a", { class: "btn", href: `#/${route.view === "entity" ? "search" : route.view}` }, "Open the default view")),
    );
    return;
  }
  syncChrome(route, c);

  const module = VIEWS[route.view];
  if (!module) {
    document.title = "Page not found · Knowa";
    main.replaceChildren(
      h("h1", { tabindex: "-1" }, "Page not found"),
      emptyState("This address is not one of Knowa’s views.", null, h("a", { class: "btn", href: href(c, "search") }, "Go to Search")),
    );
    return;
  }

  main.replaceChildren();
  const env = {
    controls: c,
    registry,
    route,
    signal: controller.signal,
    load: (container, errorTitle, fn) => load(container, errorTitle, fn, LOADING_TEXT[route.view]),
    objectUrl,
    rerender: render,
    title: (t) => {
      document.title = `${t} · Knowa`;
    },
  };
  const noSpace = c.space === null && registry.length > 0 && !registry.some((r) => r.current);
  const done = noSpace ? chooseSpace() : module.render(main, route, env);
  if (changedView && !first) {
    window.scrollTo(0, 0);
    main.querySelector("h1")?.focus();
  }
  try {
    await done;
  } catch (e) {
    if (!isAbort(e)) throw e;
    return;
  }
  // Same view, new parameters (a search, a view control): focus stays put, so say what changed.
  const count = main.querySelector(".count");
  if (!changedView && count && shown === route) announce(count.textContent);
}

/** No space in the link and none current in the home: the server would not know which to read. */
function chooseSpace() {
  document.title = "Choose a space · Knowa";
  main.replaceChildren(
    h("h1", { tabindex: "-1" }, "Choose a space"),
    emptyState("No space is current in this home, so there is nothing to read yet.", "Choose a space in the Space menu above, or All spaces."),
  );
}

async function boot() {
  document.getElementById("skip-link").addEventListener("click", (e) => {
    e.preventDefault();
    main.focus();
  });
  if (!api.loadToken()) {
    needsLink(false);
    return;
  }
  try {
    registry = await api.getJson("/api/spaces");
  } catch (e) {
    if (e instanceof api.ApiError && e.status === 401) {
      api.forgetToken();
      needsLink(true);
      return;
    }
    if (!(e instanceof api.ApiError)) throw e;
    main.replaceChildren(h("h1", { tabindex: "-1" }, "Knowa"), errorState("Unable to load the spaces", e, () => location.reload()));
    return;
  }
  buildSpaceOptions();
  buildViewOptions();
  spaceSelect.addEventListener("change", () => {
    const route = parse(location.hash);
    location.hash = withControls(route, { ...chromeControls(route), space: spaceSelect.value });
  });
  document.getElementById("status-options").addEventListener("change", onControlsChange);
  document.getElementById("kind-options").addEventListener("change", onControlsChange);
  window.addEventListener("hashchange", render);
  await render();
}

boot();
