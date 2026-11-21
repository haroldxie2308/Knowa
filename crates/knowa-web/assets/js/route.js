// Hash routes: #/search?q=…, #/e/<id>, #/timeline, #/gallery, #/review.
// Every route also carries the view controls that apply everywhere:
//   space=<alias>|*     absent: the server's current space
//   status=a,b          absent: accepted,proposed (DESIGN §4.7 default view)
//   kinds=human,agent   absent: every author kind

export const VIEWS = ["search", "timeline", "gallery", "review"];
export const STATUSES = ["accepted", "proposed", "rejected", "retracted", "superseded"];
const DEFAULT_STATUSES = ["accepted", "proposed"];
export const KINDS = { human: "knowa:Human", agent: "knowa:Agent", worker: "knowa:Worker" };
const COMMON = ["space", "status", "kinds"];

export class RouteError extends Error {}

/** `{ view, id?, params }`, where view is one of VIEWS, "entity" or "unknown". */
export function parse(hash) {
  const raw = hash.replace(/^#/, "");
  const q = raw.indexOf("?");
  const path = q === -1 ? raw : raw.slice(0, q);
  const params = new URLSearchParams(q === -1 ? "" : raw.slice(q + 1));
  if (path === "" || path === "/") return { view: "search", params };
  const parts = path.replace(/^\//, "").split("/");
  if (parts.length === 1 && VIEWS.includes(parts[0])) return { view: parts[0], params };
  if (parts.length === 2 && parts[0] === "e" && parts[1] !== "") {
    return { view: "entity", id: decodeURIComponent(parts[1]), params };
  }
  return { view: "unknown", params };
}

/** The view controls in `params`, checked. Unknown values are a RouteError. */
export function controls(params) {
  const list = (name) => {
    const v = params.get(name);
    return v === null ? null : v.split(",").filter((s) => s !== "");
  };
  const statuses = list("status") ?? DEFAULT_STATUSES;
  const badStatus = statuses.find((s) => !STATUSES.includes(s));
  if (badStatus !== undefined) throw new RouteError(`The link names an unknown status “${badStatus}”.`);
  if (statuses.length === 0) throw new RouteError("The link selects no status.");
  const kinds = list("kinds") ?? Object.keys(KINDS);
  const badKind = kinds.find((k) => !(k in KINDS));
  if (badKind !== undefined) throw new RouteError(`The link names an unknown author kind “${badKind}”.`);
  if (kinds.length === 0) throw new RouteError("The link selects no author kind.");
  return { space: params.get("space"), statuses, kinds };
}

/** The query document `view` for these controls. */
export function viewDoc(c) {
  const view = { status: c.statuses };
  if (c.kinds.length < Object.keys(KINDS).length) view.actorKinds = c.kinds.map((k) => KINDS[k]);
  return view;
}

/** A query document's `spaces` for these controls; [] reads the server's current space. */
export function spacesDoc(c) {
  return c.space === null ? [] : [c.space];
}

/** Controls as URL parameters, leaving defaults out so links stay short. */
function controlParams(c) {
  const p = new URLSearchParams();
  if (c.space !== null) p.set("space", c.space);
  if (c.statuses.join(",") !== DEFAULT_STATUSES.join(",")) p.set("status", c.statuses.join(","));
  if (c.kinds.length < Object.keys(KINDS).length) p.set("kinds", c.kinds.join(","));
  return p;
}

/** A link to `view` (or an entity id) that keeps the current view controls. */
export function href(c, view, extra = {}) {
  const p = controlParams(c);
  for (const [k, v] of Object.entries(extra)) if (v !== null && v !== undefined && v !== "") p.set(k, v);
  const path = view === "entity" ? `/e/${encodeURIComponent(extra.id)}` : `/${view}`;
  p.delete("id");
  const qs = p.toString();
  return `#${path}${qs ? `?${qs}` : ""}`;
}

/** The current route with some controls replaced, keeping its own parameters. */
export function withControls(route, c) {
  const p = controlParams(c);
  for (const [k, v] of route.params) if (!COMMON.includes(k)) p.set(k, v);
  const path = route.view === "entity" ? `/e/${encodeURIComponent(route.id)}` : `/${route.view}`;
  const qs = p.toString();
  return `#${path}${qs ? `?${qs}` : ""}`;
}

/** The current route with its own (non-control) parameters replaced. */
export function withParams(route, c, own) {
  const extra = { ...own };
  if (route.view === "entity") extra.id = route.id;
  return href(c, route.view, extra);
}
