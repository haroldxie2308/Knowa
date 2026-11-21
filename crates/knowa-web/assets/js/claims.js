// A claim with its provenance (DESIGN §7.4: "provenance is part of every claim"):
// status and stale as text, author and author kind, via, conf, note and src links.

import * as api from "./api.js";
import { h, idButton, recordTime, shortId, tag } from "./dom.js";
import { STATUSES, href, spacesDoc, viewDoc } from "./route.js";

const KIND_NAMES = { "knowa:Human": "human", "knowa:Agent": "agent", "knowa:Worker": "worker" };
const ROLE_NAMES = {
  "knowa:Owner": "owner",
  "knowa:Member": "member",
  "knowa:Agent": "agent role",
  "knowa:Worker": "worker role",
  "knowa:Viewer": "viewer",
};

/** Predicates shown first on an entity, in this order; the rest follow by name. */
const LEADING = ["knowa:label", "knowa:type", "knowa:body", "knowa:summary", "knowa:when", "knowa:where", "knowa:about"];

/**
 * Shared rendering context: the view controls, display labels of ids and what is
 * known about authors. `labels` and `actors` are filled by `loadLabels`/`loadActors`.
 */
export function context(controls, registry) {
  return { controls, registry, labels: new Map(), actors: new Map() };
}

/** A link to an id's page, by its label when one is known. */
export function idLink(ctx, id, label = ctx.labels.get(id)) {
  const target = href(ctx.controls, "entity", { id });
  if (label) return h("a", { href: target, dir: "auto" }, label);
  return h("a", { href: target, class: "mono", title: id }, shortId(id));
}

/** Display labels of `ids` under the current view, from one `find`. */
export async function loadLabels(ctx, ids, signal) {
  const missing = [...new Set(ids)].filter((id) => !ctx.labels.has(id));
  // A find returns at most 1000 entities (DESIGN §6.1).
  for (let i = 0; i < missing.length; i += 1000) {
    const chunk = missing.slice(i, i + 1000);
    const res = await api.query(
      { op: "find", spaces: spacesDoc(ctx.controls), view: viewDoc(ctx.controls), where: { ids: chunk }, limit: chunk.length },
      signal,
    );
    for (const e of res.items) if (e.label) ctx.labels.set(e.id, e.label);
  }
}

/**
 * Label, kind and role of each author. An actor's kind and role are policy (DESIGN
 * §4.6), so they are read with a view of every status: what the reader filters out
 * still has an author.
 */
export async function loadActors(ctx, ids, signal) {
  const missing = [...new Set(ids)].filter((id) => !ctx.actors.has(id));
  await Promise.all(
    missing.map(async (id) => {
      const res = await api.query(
        { op: "context", id, spaces: spacesDoc(ctx.controls), view: { status: STATUSES }, hops: 0, limit: 100 },
        signal,
      );
      const info = res.actors.find((a) => a.id === id);
      ctx.actors.set(id, { label: res.entity.label ?? null, kind: info?.kind ?? null, role: info?.role ?? null });
    }),
  );
}

/** Ids a list of claims refers to: subjects, refs, sources and replaced claims. */
export function referencedIds(claims) {
  const out = [];
  for (const c of claims) {
    out.push(c.s);
    if (c.o.ref) out.push(c.o.ref);
    for (const id of c.src ?? []) out.push(id);
    for (const id of c.replaces ?? []) out.push(id);
  }
  return out;
}

function renderValue(ctx, o) {
  if ("text" in o) {
    const long = o.text.length > 80 || o.text.includes("\n");
    return h("span", { class: long ? "claim-value long" : "claim-value", lang: o.lang ?? null, dir: "auto" }, o.text);
  }
  if ("term" in o) return h("span", { class: "claim-value" }, h("span", { class: "term" }, o.term));
  if ("ref" in o) return h("span", { class: "claim-value" }, idLink(ctx, o.ref));
  if ("int" in o) return h("span", { class: "claim-value" }, String(o.int));
  if ("dec" in o) return h("span", { class: "claim-value" }, o.dec);
  if ("bool" in o) return h("span", { class: "claim-value" }, String(o.bool));
  if ("when" in o) return h("span", { class: "claim-value" }, o.when);
  if ("geo" in o) {
    const g = o.geo;
    const parts = [`${g.lat}, ${g.lon}`];
    if (g.alt) parts.push(`altitude ${g.alt} m`);
    if (g.r) parts.push(`± ${g.r} m`);
    return h("span", { class: "claim-value" }, parts.join(" · "));
  }
  if ("qty" in o) return h("span", { class: "claim-value" }, `${o.qty} ${o.unit}`);
  // A value form this app does not know yet (the contract only grows): show it raw.
  return h("span", { class: "claim-value mono" }, JSON.stringify(o));
}

function qualifiers(ctx, q) {
  if (!q) return null;
  return Object.entries(q).map(([p, values]) => h("div", { class: "meta" }, tag(p), values.map((v) => renderValue(ctx, v))));
}

/** "agent", "human, owner": the kind, and the role when it says more than the kind. */
export function actorKindText(ctx, actorId) {
  const a = ctx.actors.get(actorId);
  const kind = a?.kind ? (KIND_NAMES[a.kind] ?? a.kind) : "kind not declared";
  const role = a?.role && !(a.role === a.kind) ? (ROLE_NAMES[a.role] ?? a.role) : null;
  return role ? `${kind}, ${role}` : kind;
}

function authorText(ctx, actorId) {
  const a = ctx.actors.get(actorId);
  const who = a?.label ? h("bdi", { title: actorId }, a.label) : h("span", { class: "mono", title: actorId }, shortId(actorId));
  return h("span", {}, "by ", who, ` (${actorKindText(ctx, actorId)})`);
}

function statusBadges(cv) {
  return [h("span", { class: `badge badge-${cv.status}` }, cv.status), cv.stale ? h("span", { class: "badge badge-stale", title: "An input in src is no longer active" }, "stale") : null];
}

function spaceAlias(ctx, spaceId) {
  return ctx.registry.find((r) => r.space === spaceId)?.alias ?? shortId(spaceId);
}

/**
 * One claim. `show` picks the parts the surrounding group does not already say:
 * "subject" (s and p), "predicate" (p only) or "value" (just the value).
 */
export function renderClaim(ctx, cv, { show = "value", actions = null } = {}) {
  const head =
    show === "subject"
      ? h("div", { class: "meta" }, h("span", { class: "claim-subject" }, idLink(ctx, cv.s)), tag(cv.p))
      : show === "predicate"
        ? h("div", { class: "meta" }, tag(cv.p))
        : null;
  const via = Object.entries(cv.via ?? {}).map(([k, v]) => `${k} ${v}`);
  const meta = h(
    "div",
    { class: "meta" },
    statusBadges(cv),
    authorText(ctx, cv.actor),
    recordTime(cv.at),
    cv.valid ? h("span", {}, `valid ${cv.valid}`) : null,
    via.length ? h("span", {}, `via ${via.join(", ")}`) : null,
    cv.conf ? h("span", {}, `confidence ${cv.conf}`) : null,
    ctx.controls.space === "*" ? h("span", {}, `in ${spaceAlias(ctx, cv.space)}`) : null,
    idButton(cv.id),
  );
  const src = cv.src?.length
    ? h("div", { class: "meta claim-src" }, "Derived from", cv.src.map((id) => idLink(ctx, id)))
    : null;
  const replaces = cv.replaces?.length
    ? h("div", { class: "meta claim-src" }, "Replaces", cv.replaces.map((id) => idLink(ctx, id)))
    : null;
  return h(
    "article",
    { class: "claim", "aria-label": `${cv.p} claim, ${cv.status}${cv.stale ? ", stale" : ""}` },
    head,
    renderValue(ctx, cv.o),
    qualifiers(ctx, cv.q),
    meta,
    cv.note ? h("p", { class: "claim-note" }, h("bdi", {}, cv.note)) : null,
    src,
    replaces,
    actions,
  );
}

/** Claims grouped by predicate, leading predicates first, in record order within. */
export function byPredicate(claims) {
  const groups = new Map();
  for (const c of claims) {
    if (!groups.has(c.p)) groups.set(c.p, []);
    groups.get(c.p).push(c);
  }
  const rank = (p) => (LEADING.includes(p) ? LEADING.indexOf(p) : LEADING.length);
  return [...groups.entries()].sort(([a], [b]) => rank(a) - rank(b) || a.localeCompare(b));
}
