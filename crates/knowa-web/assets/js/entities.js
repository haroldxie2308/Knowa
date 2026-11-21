// How an entity summary (EntityView, DESIGN §6.2) reads in lists.

import { h, idButton, shortId, tag } from "./dom.js";
import { href } from "./route.js";

const dateTimeFmt = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });
const dayFmt = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeZone: "UTC" });
const monthFmt = new Intl.DateTimeFormat(undefined, { month: "short", year: "numeric", timeZone: "UTC" });
/** An instant: a datetime with `Z` or an offset. Without one the zone is unknown (EDTF). */
const INSTANT = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:\d{2})$/;
const DAY = /^\d{4}-\d{2}-\d{2}$/;
const MONTH = /^\d{4}-\d{2}$/;

/** One EDTF form in words, or null when it has none here (qualifiers, 19XX, local times). */
function edtfText(v) {
  if (INSTANT.test(v)) return dateTimeFmt.format(new Date(v));
  if (DAY.test(v)) return dayFmt.format(new Date(`${v}T00:00:00Z`));
  if (MONTH.test(v)) return monthFmt.format(new Date(`${v}-01T00:00:00Z`));
  return null;
}

/** What `when.source` means, as DESIGN §4.8 orders it. */
export const WHEN_SOURCE = {
  claim: "from a knowa:when claim",
  file: "file modified time; no knowa:when claim",
  recorded: "record time of this note",
};

/**
 * A `when` value for reading: instants in the reader's time zone, dates and months in
 * words, a range of two days as a range. Anything else is shown as written; the exact
 * EDTF is always in the tooltip.
 */
export function whenText(when) {
  const v = when.value;
  const [a, b] = v.split("/");
  if (b !== undefined && DAY.test(a) && DAY.test(b)) return dayFmt.formatRange(new Date(`${a}T00:00:00Z`), new Date(`${b}T00:00:00Z`));
  return edtfText(v) ?? v;
}

export function whenElement(when) {
  const range = `${when.from ?? "open"} – ${when.to ?? "open"}`;
  return h("time", { datetime: when.from ?? when.value, title: `${when.value} (${range})` }, whenText(when));
}

export function whereText(where) {
  return `${where.lat.toFixed(5)}, ${where.lon.toFixed(5)}${where.r ? ` ± ${where.r} m` : ""}`;
}

export function spaceAliases(registry, spaceIds) {
  return spaceIds.map((s) => registry.find((r) => r.space === s)?.alias ?? shortId(s)).join(", ");
}

/** The entity's label as a link, or its id in monospace when it has none. */
export function titleLink(controls, e, cls = "entity-title") {
  const target = href(controls, "entity", { id: e.id });
  if (e.label) return h("a", { href: target, class: cls, dir: "auto" }, e.label);
  return h("a", { href: target, class: `${cls} untitled`, title: e.id }, shortId(e.id));
}

/** Types, when (with a source note unless it is a knowa:when), where, spaces, id. */
export function entityMeta(env, e, { when = true } = {}) {
  return h(
    "div",
    { class: "meta" },
    e.types.length ? h("span", { class: "tags" }, e.types.map(tag)) : null,
    when && e.when ? h("span", {}, whenElement(e.when), e.when.source === "claim" ? "" : ` · ${e.when.source === "file" ? "file time" : "recorded"}`) : null,
    e.where ? h("span", {}, whereText(e.where)) : null,
    env.controls.space === "*" ? h("span", {}, `in ${spaceAliases(env.registry, e.spaces)}`) : null,
    idButton(e.id),
  );
}
