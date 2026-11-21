// Review (DESIGN §7.4): proposed claims grouped by author, then by via.session (or
// record day when a claim has none), then by subject. Accept all / Reject all act on
// exactly the claims a batch shows; each claim can also be reviewed alone. Rejecting
// asks first: a review is an append-only claim and cannot be taken back here.

import * as api from "../api.js";
import { actorKindText, context, idLink, loadActors, loadLabels, referencedIds, renderClaim } from "../claims.js";
import { announce, h, icon, idButton, plural, shortId } from "../dom.js";
import { KINDS, spacesDoc } from "../route.js";
import { confirmAction, emptyState, inlineError } from "../ui.js";

/** The api's page limit (DESIGN §6.1): the queue is read in one page. */
const LIMIT = 1000;
const dayFmt = new Intl.DateTimeFormat(undefined, { dateStyle: "long" });

function group(items, key) {
  const out = new Map();
  for (const it of items) {
    const k = key(it);
    if (!out.has(k)) out.set(k, []);
    out.get(k).push(it);
  }
  return out;
}

/** A claim's batch: its via.session, else the day (in the reader's time zone) it was recorded. */
function batchKey(cv) {
  if (cv.via?.session) return `session:${cv.via.session}`;
  const d = new Date(cv.at);
  return `day:${d.getFullYear()}-${d.getMonth() + 1}-${d.getDate()}`;
}

function batchTitle(key, first) {
  if (key.startsWith("session:")) return ["Session ", h("code", {}, key.slice("session:".length))];
  return `Recorded on ${dayFmt.format(new Date(first.at))}, no session`;
}

export async function render(main, route, env) {
  const c = env.controls;
  env.title("Review");
  const results = h("div");
  main.append(h("h1", { tabindex: "-1" }, "Review"), results);

  const view = { status: ["proposed"] };
  if (c.kinds.length < Object.keys(KINDS).length) view.actorKinds = c.kinds.map((k) => KINDS[k]);
  const doc = { op: "find", spaces: spacesDoc(c), view, target: "claims", order: "recorded", limit: LIMIT };

  await env.load(results, "Unable to load proposed claims", async () => {
    const res = await api.query(doc, env.signal);
    if (res.items.length === 0) {
      return emptyState(
        "Nothing to review",
        c.kinds.length < Object.keys(KINDS).length
          ? "No proposed claims by these author kinds. Widen the author kinds in the view to see others."
          : "Claims by agents and other actors without auto-accept land here as proposed, until an owner or member reviews them.",
      );
    }
    const ctx = context(c, env.registry);
    await Promise.all([loadActors(ctx, res.items.map((cv) => cv.actor), env.signal), loadLabels(ctx, referencedIds(res.items), env.signal)]);
    const queue = new Queue(env, ctx, res.items);
    return queue.render(res.items.length >= LIMIT || Boolean(res.cursor));
  });
}

/**
 * Which space a review of `cv` is written to, and whether that space takes writes. A
 * selection the registry does not list (the server's space given as a replica path)
 * is left to the server, which refuses a read-only one with its own error.
 */
function target(env, cv) {
  const c = env.controls;
  if (c.space === null) return { space: null, readonly: env.registry.find((r) => r.current)?.readonly ?? false };
  if (c.space !== "*") return { space: c.space, readonly: env.registry.find((r) => r.alias === c.space)?.readonly ?? false };
  const replicas = env.registry.filter((r) => r.space === cv.space);
  const writable = replicas.find((r) => !r.readonly);
  return writable ? { space: writable.alias, readonly: false } : { space: replicas[0]?.alias ?? null, readonly: true };
}

class Queue {
  constructor(env, ctx, claims) {
    this.env = env;
    this.ctx = ctx;
    this.items = new Map(claims.map((cv) => [cv.id, { cv, node: null }]));
    this.summary = h("p", { class: "count" });
    this.refreshers = [];
  }

  updateSummary() {
    for (const refresh of this.refreshers) refresh();
    const authors = new Set([...this.items.values()].map((i) => i.cv.actor)).size;
    this.summary.textContent = `${plural(this.items.size, { one: "# proposed claim", other: "# proposed claims" })} from ${plural(authors, { one: "# author", other: "# authors" })}`;
  }

  render(capped) {
    this.updateSummary();
    const out = [h("div", { class: "page-head" }, this.summary)];
    if (capped) out.push(h("p", { class: "notice" }, `Showing the oldest ${LIMIT} proposed claims. Review these to bring the rest into the queue.`));
    const byAuthor = group([...this.items.values()].map((i) => i.cv), (cv) => cv.actor);
    for (const [actor, claims] of byAuthor) out.push(this.authorSection(actor, claims));
    return out;
  }

  authorSection(actor, claims) {
    const a = this.ctx.actors.get(actor);
    const headingId = `author-${actor}`;
    return h(
      "section",
      { class: "author-group", "aria-labelledby": headingId },
      h(
        "div",
        { class: "author-head" },
        h("h2", { id: headingId, dir: "auto" }, a?.label ?? shortId(actor)),
        h("span", { class: "meta" }, actorKindText(this.ctx, actor), idButton(actor)),
      ),
      [...group(claims, batchKey)].map(([key, batch]) => this.batchSection(key, batch)),
    );
  }

  /** A batch's "all" buttons act on the claims it still shows, and say how many. */
  batchSection(key, claims) {
    const status = h("div", { "aria-live": "polite" });
    const readonly = claims.some((cv) => target(this.env, cv).readonly);
    const shown = () => claims.map((cv) => cv.id).filter((id) => this.items.has(id));
    const acceptText = h("span", {});
    const rejectText = h("span", {});
    const count = h("span", { class: "count" });
    const acceptAll = h("button", { type: "button", class: "btn btn-small", disabled: readonly }, icon("check"), acceptText);
    const rejectAll = h("button", { type: "button", class: "btn btn-small btn-danger", disabled: readonly }, icon("x"), rejectText);
    const refresh = () => {
      const n = shown().length;
      acceptText.textContent = `Accept all ${n}`;
      rejectText.textContent = `Reject all ${n}`;
      count.textContent = plural(n, { one: "# claim", other: "# claims" });
    };
    refresh();
    this.refreshers.push(refresh);
    acceptAll.addEventListener("click", () => this.decide("accept", shown(), status, acceptAll));
    rejectAll.addEventListener("click", () => this.decide("reject", shown(), status, rejectAll));
    return h(
      "section",
      { class: "batch" },
      h("div", { class: "batch-head" }, h("h3", {}, batchTitle(key, claims[0])), count, h("div", { class: "batch-actions" }, acceptAll, rejectAll)),
      readonly ? h("p", { class: "notice" }, "This space is registered read-only, so reviews cannot be written to it.") : null,
      status,
      [...group(claims, (cv) => cv.s)].map(([s, list]) =>
        h(
          "div",
          { class: "subject-group" },
          h("h4", {}, idLink(this.ctx, s)),
          h("ul", { class: "claims" }, list.map((cv) => this.claimItem(cv, readonly))),
        ),
      ),
    );
  }

  claimItem(cv, readonly) {
    const status = h("div", { "aria-live": "polite" });
    const accept = h("button", { type: "button", class: "btn btn-small", disabled: readonly, "aria-label": `Accept ${cv.p} claim ${shortId(cv.id)}` }, icon("check"), "Accept");
    const reject = h("button", { type: "button", class: "btn btn-small btn-danger", disabled: readonly, "aria-label": `Reject ${cv.p} claim ${shortId(cv.id)}` }, icon("x"), "Reject");
    accept.addEventListener("click", () => this.decide("accept", [cv.id], status, accept));
    reject.addEventListener("click", () => this.decide("reject", [cv.id], status, reject));
    const node = h("li", {}, renderClaim(this.ctx, cv, { show: "predicate", actions: [h("div", { class: "claim-actions" }, accept, reject), status] }));
    this.items.get(cv.id).node = node;
    return node;
  }

  /**
   * Writes one review per claim, to each claim's own space. Afterwards the claims are
   * read back: a review takes effect only when its author is an owner or member, so
   * a claim can stay proposed although its review was written.
   */
  async decide(decision, ids, status, trigger) {
    const n = ids.length;
    if (decision === "reject") {
      const ok = await confirmAction({
        title: n === 1 ? `Reject this ${this.items.get(ids[0]).cv.p} claim?` : `Reject ${n} claims?`,
        body: `Each claim gets a signed knowa:Reject review. Reviews are append-only: a later review can accept ${n === 1 ? "it" : "them"}, but ${n === 1 ? "it does" : "they do"} not return to proposed.`,
        confirmLabel: n === 1 ? "Reject claim" : `Reject ${n} claims`,
      });
      if (!ok) return;
    }
    const buttons = [...trigger.closest(".batch, li").querySelectorAll("button.btn")];
    const order = [...document.querySelectorAll("main .batch button.btn")];
    const later = order.slice(order.indexOf(trigger) + 1).filter((b) => !buttons.includes(b));
    for (const b of buttons) b.disabled = true;
    status.replaceChildren(h("p", { class: "inline-status" }, n === 1 ? "Writing the review…" : `Writing ${n} reviews…`));
    try {
      const bySpace = group(ids, (id) => target(this.env, this.items.get(id).cv).space);
      for (const [space, claims] of bySpace) {
        const body = { decision, claims };
        if (space !== null) body.space = space;
        await api.postJson("/api/review", body, this.env.signal);
      }
      const got = await api.query({ op: "get", spaces: spacesDoc(this.env.controls), ids }, this.env.signal);
      const still = got.items.filter((i) => i.claim.status === "proposed").map((i) => i.claim.id);
      for (const id of ids.filter((x) => !still.includes(x))) this.remove(id);
      const done = n - still.length;
      if (still.length) {
        status.replaceChildren(
          h("p", { class: "inline-error", role: "alert" }, `${plural(still.length, { one: "# claim is", other: "# claims are" })} still proposed: the review was written, but only reviews by an owner or member of the space take effect.`),
        );
        for (const b of buttons) b.disabled = false;
      }
      if (done) announce(`${plural(done, { one: "# claim", other: "# claims" })} ${decision === "accept" ? "accepted" : "rejected"}`);
      this.updateSummary();
      if (this.items.size === 0) {
        this.env.rerender();
      } else if (!trigger.isConnected) {
        // The reviewed claims are gone: continue with the next one in the queue.
        (later.find((b) => b.isConnected) ?? document.querySelector("main h1")).focus();
      }
    } catch (err) {
      if (err?.name === "AbortError") return;
      if (!(err instanceof api.ApiError)) throw err;
      status.replaceChildren(inlineError(err));
      for (const b of buttons) b.disabled = false;
    }
  }

  /** Drops a reviewed claim, and the subject group, batch and author left empty. */
  remove(id) {
    const { node } = this.items.get(id);
    this.items.delete(id);
    const containers = [node.closest(".subject-group"), node.closest(".batch"), node.closest(".author-group")];
    node.remove();
    for (const el of containers) {
      if (el.querySelector("li")) break;
      el.remove();
    }
  }
}
