// Entity (DESIGN §7.4), built on `context`: label, types, when with its source, where;
// claims grouped by predicate with full provenance; inbound claims, neighbors and
// temporal and spatial neighbors; a blob's image or text. A claim id (c_…) shows the
// claim itself and the claims about it (reviews, retractions).

import * as api from "../api.js";
import { byPredicate, context, idLink, loadActors, loadLabels, referencedIds, renderClaim } from "../claims.js";
import { h, idButton, shortId, tag } from "../dom.js";
import { WHEN_SOURCE, entityMeta, spaceAliases, titleLink, whenElement, whereText } from "../entities.js";
import { spacesDoc, viewDoc } from "../route.js";
import { emptyState, inlineError } from "../ui.js";

/** Per-section cap of the context query (its default is 20). */
const SECTION_LIMIT = 200;
/** DESIGN §7.4: a blob entity shows its text up to 1 MiB. */
const MAX_TEXT = 1 << 20;

export async function render(main, route, env) {
  const id = route.id;
  const heading = h("h1", { tabindex: "-1", class: "id-heading", title: id }, shortId(id));
  const body = h("div");
  main.append(heading, body);
  env.title(shortId(id));
  await env.load(body, id.startsWith("c_") ? "Unable to load this claim" : "Unable to load this entity", () =>
    id.startsWith("c_") ? claimPage(env, id, heading) : entityPage(env, id, heading),
  );
}

function setHeading(env, heading, label, id) {
  heading.textContent = label ?? shortId(id);
  heading.dir = label ? "auto" : "ltr";
  heading.classList.toggle("id-heading", !label);
  env.title(label ?? shortId(id));
}

function section(title, count, ...children) {
  return h(
    "section",
    { class: "section" },
    h("h2", { class: "section-head" }, title, count === null ? null : h("span", { class: "count" }, String(count))),
    children,
  );
}

function capNote(n) {
  return n >= SECTION_LIMIT ? h("p", { class: "note-text" }, `Showing the first ${SECTION_LIMIT}.`) : null;
}

async function entityPage(env, id, heading) {
  const c = env.controls;
  const ctx = context(c, env.registry);
  const res = await api.query({ op: "context", id, spaces: spacesDoc(c), view: viewDoc(c), limit: SECTION_LIMIT }, env.signal);
  const all = [...res.claims, ...res.inbound];
  await Promise.all([loadActors(ctx, all.map((cv) => cv.actor), env.signal), loadLabels(ctx, referencedIds(all), env.signal)]);
  const e = res.entity;
  setHeading(env, heading, e.label, id);

  const facts = h("dl", { class: "facts" });
  const fact = (dt, ...dd) => facts.append(h("dt", {}, dt), h("dd", {}, dd));
  fact("Id", idButton(id));
  if (e.types.length) fact("Types", h("span", { class: "tags" }, e.types.map(tag)));
  fact("When", e.when ? [whenElement(e.when), h("span", { class: "note-text" }, ` · ${WHEN_SOURCE[e.when.source]}`)] : h("span", { class: "note-text" }, "No date in this view: not on the timeline"));
  if (e.where) fact("Where", whereText(e.where));
  fact("Spaces", spaceAliases(env.registry, e.spaces));

  const claims = byPredicate(res.claims).map(([p, list]) =>
    h(
      "div",
      { class: "predicate-group" },
      h("h3", {}, p),
      h("ul", { class: "claims" }, list.map((cv) => h("li", {}, renderClaim(ctx, cv)))),
    ),
  );
  const primary = [
    id.startsWith("b_") ? preview(env, id, e.label, res.claims) : null,
    section(
      "Claims",
      res.claims.length,
      res.claims.length ? claims : h("p", { class: "note-text" }, "No claim about this id passes the view. Widen the view to see more."),
      capNote(res.claims.length),
    ),
  ];

  const aside = [
    section(
      "Links",
      res.neighbors.length,
      res.neighbors.length
        ? h(
            "ul",
            { class: "side-list" },
            res.neighbors.map((n) =>
              h(
                "li",
                {},
                titleLink(c, n.entity, ""),
                h("div", { class: "meta" }, tag(n.p), n.dir === "out" ? "this links to it" : "it links here", n.hop > 1 ? `${n.hop} hops` : null, h("span", {}, "via ", idLink(ctx, n.via))),
              ),
            ),
          )
        : h("p", { class: "note-text" }, "No links in this view."),
      capNote(res.neighbors.length),
    ),
    section(
      "Referenced by",
      res.inbound.length,
      res.inbound.length
        ? h("ul", { class: "claims" }, res.inbound.map((cv) => h("li", {}, renderClaim(ctx, cv, { show: "subject" }))))
        : h("p", { class: "note-text" }, "No claim in this view refers to this id."),
      capNote(res.inbound.length),
    ),
    section(
      "Around the same time",
      res.temporal.length,
      res.temporal.length ? h("ul", { class: "side-list" }, res.temporal.map((t) => h("li", {}, titleLink(c, t, ""), entityMeta(env, t)))) : h("p", { class: "note-text" }, e.when ? "Nothing else within a day of it." : "It has no date in this view."),
    ),
    res.spatial.length
      ? section("Nearby", res.spatial.length, h("ul", { class: "side-list" }, res.spatial.map((t) => h("li", {}, titleLink(c, t, ""), entityMeta(env, t)))))
      : null,
  ];

  return [h("div", { class: "entity-head" }, facts), h("div", { class: "entity-layout" }, h("div", {}, primary), h("aside", { "aria-label": "Related" }, aside))];
}

/**
 * A blob entity's bytes: an image or text inline when the server serves them so (the
 * accepted media type decides, DESIGN §7.4), otherwise a download button.
 */
function preview(env, id, label, claims) {
  const box = h("div", { class: "preview" }, h("p", { class: "loading" }, "Loading the file…"));
  const mediaTypes = claims.filter((cv) => cv.p === "knowa:mediaType" && typeof cv.o.text === "string").map((cv) => cv.o.text.toLowerCase());
  const size = claims.find((cv) => cv.p === "knowa:size" && Number.isInteger(cv.o.int))?.o.int ?? null;
  const name = claims.find((cv) => cv.p === "knowa:name" && typeof cv.o.text === "string")?.o.text ?? id;
  const inline = mediaTypes.some((m) => m.startsWith("image/") || m.startsWith("text/plain"));
  const tooBig = mediaTypes.some((m) => m.startsWith("text/")) && size !== null && size > MAX_TEXT;

  const download = (fetched, why) => {
    const btn = h("button", { type: "button", class: "btn" }, "Download file");
    btn.addEventListener("click", async () => {
      btn.disabled = true;
      try {
        const bytes = fetched ?? (await api.blob(id, spacesDoc(env.controls), env.signal)).bytes;
        const a = h("a", { href: env.objectUrl(bytes), download: name });
        a.click();
      } catch (err) {
        if (!(err instanceof api.ApiError)) throw err;
        box.append(inlineError(err));
      } finally {
        btn.disabled = false;
      }
    });
    return [h("p", { class: "note-text" }, why), btn];
  };

  const fill = async () => {
    if (!inline || tooBig) {
      box.replaceChildren(...download(null, tooBig ? "This text is larger than 1 MiB, so it is not shown here." : "This file is not an image or plain text, so it is not shown here."));
      return;
    }
    let got;
    try {
      got = await api.blob(id, spacesDoc(env.controls), env.signal);
    } catch (err) {
      if (!(err instanceof api.ApiError)) throw err;
      box.replaceChildren(
        err.code === "not_found"
          ? h("p", { class: "note-text" }, "The selected spaces do not store this file’s bytes (it was added as catalog only, or lives in another space).")
          : inlineError(err),
      );
      return;
    }
    if (got.attachment) {
      box.replaceChildren(...download(got.bytes, "No accepted media type lets this file show inline, so it downloads instead."));
    } else if (got.contentType.startsWith("image/")) {
      box.replaceChildren(h("img", { src: env.objectUrl(got.bytes), alt: label ? `Image: ${label}` : "Image content of this file" }));
    } else {
      box.replaceChildren(h("pre", {}, await got.bytes.text()));
    }
  };
  fill().catch((err) => {
    if (err?.name !== "AbortError") throw err;
  });
  return section("Content", null, box);
}

async function claimPage(env, id, heading) {
  const c = env.controls;
  const ctx = context(c, env.registry);
  const got = await api.query({ op: "get", spaces: spacesDoc(c), view: viewDoc(c), ids: [id] }, env.signal);
  const cv = got.items[0].claim;
  let about = { claims: [], inbound: [] };
  try {
    about = await api.query({ op: "context", id, spaces: spacesDoc(c), view: viewDoc(c), hops: 0, limit: SECTION_LIMIT }, env.signal);
  } catch (err) {
    // Nothing in the view describes or references this claim (no review, retraction or
    // reference to it): that is the common case, not a failure.
    if (!(err instanceof api.ApiError) || err.code !== "not_found") throw err;
  }
  const all = [cv, ...about.claims, ...about.inbound];
  await Promise.all([loadActors(ctx, all.map((x) => x.actor), env.signal), loadLabels(ctx, referencedIds(all), env.signal)]);
  heading.textContent = "Claim";
  heading.classList.remove("id-heading");
  env.title(`Claim ${shortId(id)}`);
  return [
    h("div", { class: "page-head" }, h("p", { class: "count" }, "Shown whatever the view, with its computed status.")),
    section("Statement", null, renderClaim(ctx, cv, { show: "subject" })),
    section(
      "Claims about this claim",
      about.claims.length,
      about.claims.length
        ? h("ul", { class: "claims" }, about.claims.map((x) => h("li", {}, renderClaim(ctx, x, { show: "predicate" }))))
        : emptyState("No review or retraction in this view", "Reviews and retractions of a claim are claims about it, and appear here."),
    ),
    about.inbound.length
      ? section("Referenced by", about.inbound.length, h("ul", { class: "claims" }, about.inbound.map((x) => h("li", {}, renderClaim(ctx, x, { show: "subject" })))))
      : null,
  ].filter(Boolean);
}
