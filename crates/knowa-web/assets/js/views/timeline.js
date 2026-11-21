// Timeline (DESIGN §7.4): dated entities, newest first, grouped by month. A visible
// toggle hides files (notType: knowa:File). Months are UTC, as `when` ranges are.

import * as api from "../api.js";
import { h, plural } from "../dom.js";
import { entityMeta, titleLink, whenElement } from "../entities.js";
import { spacesDoc, viewDoc, withParams } from "../route.js";
import { emptyState, inlineError } from "../ui.js";

const PAGE = 100;
const monthFmt = new Intl.DateTimeFormat(undefined, { month: "long", year: "numeric", timeZone: "UTC" });

/** The month an entity sits in: the start of its `when`, else its end. */
function period(e) {
  const t = e.when.from ?? e.when.to;
  if (t === null) return { key: "open", label: "Open-ended" };
  return { key: t.slice(0, 7), label: monthFmt.format(new Date(t)) };
}

export async function render(main, route, env) {
  const c = env.controls;
  const hideFiles = route.params.get("files") === "hide";
  env.title("Timeline");

  const toggle = h("input", { type: "checkbox", id: "hide-files", checked: hideFiles });
  toggle.addEventListener("change", () => {
    location.hash = withParams(route, c, { files: toggle.checked ? "hide" : null });
  });
  const results = h("div");
  main.append(
    h("h1", { tabindex: "-1" }, "Timeline"),
    h("div", { class: "toolbar" }, h("label", { class: "check", for: "hide-files" }, toggle, "Hide files")),
    results,
  );

  // `when: {}` keeps only entities that have a when (DESIGN §4.8).
  const doc = { op: "find", spaces: spacesDoc(c), view: viewDoc(c), target: "entities", where: { when: {} }, order: "-when", limit: PAGE };
  if (hideFiles) doc.where.notType = ["knowa:File"];

  await env.load(results, "Unable to load the timeline", async () => {
    const res = await api.query(doc, env.signal);
    if (res.items.length === 0) {
      return emptyState(
        hideFiles ? "Nothing but files has a date in this view" : "Nothing in this view has a date",
        "Entities get a date from a knowa:when claim, from a file’s modified time, or, for notes, from when they were written.",
      );
    }
    const periods = h("div");
    const footer = h("div", { class: "load-more" });
    const sections = new Map();
    const add = (page) => {
      for (const e of page.items) {
        const p = period(e);
        if (!sections.has(p.key)) {
          const count = h("span", { class: "count" });
          const list = h("ul", { class: "timeline-list" });
          periods.append(h("section", { class: "period" }, h("h2", {}, `${p.label} `, count), list));
          sections.set(p.key, { count, list, n: 0 });
        }
        const s = sections.get(p.key);
        const source = e.when.source === "claim" ? null : h("div", { class: "note-text" }, e.when.source === "file" ? "file time" : "recorded");
        s.list.append(h("li", {}, h("div", {}, whenElement(e.when), source), h("div", {}, titleLink(c, e), entityMeta(env, e, { when: false }))));
        s.n += 1;
        s.count.textContent = plural(s.n, { one: "# entity", other: "# entities" });
      }
      footer.replaceChildren();
      if (page.cursor) {
        const more = h("button", { type: "button", class: "btn" }, "Load older");
        more.addEventListener("click", async () => {
          more.disabled = true;
          try {
            add(await api.query({ ...doc, cursor: page.cursor }, env.signal));
          } catch (err) {
            if (!(err instanceof api.ApiError)) throw err;
            more.disabled = false;
            footer.append(inlineError(err));
          }
        });
        footer.append(more);
      } else if (spacesDoc(c)[0] === "*" && page.items.length === PAGE) {
        footer.append(h("p", { class: "note-text" }, `Showing the newest ${PAGE}: paging needs a single space.`));
      }
    };
    add(res);
    return [periods, footer];
  });
}
