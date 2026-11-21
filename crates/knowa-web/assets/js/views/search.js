// Search (DESIGN §7.4): free text, type and not-type filters, under the view controls.
// Returns entities. Without text it lists entities newest first.

import * as api from "../api.js";
import { h, plural } from "../dom.js";
import { entityMeta, titleLink } from "../entities.js";
import { href, spacesDoc, viewDoc, withParams } from "../route.js";
import { emptyState, inlineError } from "../ui.js";

const PAGE = 50;

const terms = (s) => s.split(/[\s,]+/).filter(Boolean);

export async function render(main, route, env) {
  const c = env.controls;
  const q = route.params.get("q") ?? "";
  const type = route.params.get("type") ?? "";
  const notType = route.params.get("notType") ?? "";
  env.title(q ? `Search: ${q}` : "Search");

  const text = h("input", { class: "input", id: "q", name: "q", type: "search", value: q, placeholder: "kyoto trip", autocomplete: "off" });
  const typeIn = h("input", { class: "input", id: "type", name: "type", value: type, placeholder: "knowa:Note", list: "known-types", autocomplete: "off", spellcheck: "false" });
  const notTypeIn = h("input", { class: "input", id: "notType", name: "notType", value: notType, placeholder: "knowa:File", list: "known-types", autocomplete: "off", spellcheck: "false" });
  const form = h(
    "form",
    { class: "search-form", role: "search" },
    h("div", { class: "field field-text" }, h("label", { for: "q" }, "Text"), text),
    h("div", { class: "field" }, h("label", { for: "type" }, "Type"), typeIn),
    h("div", { class: "field" }, h("label", { for: "notType" }, "Leave out type"), notTypeIn),
    h("button", { class: "btn btn-primary", type: "submit" }, "Search"),
    h("datalist", { id: "known-types" }, ["knowa:Note", "knowa:Photo", "knowa:File", "knowa:Person", "knowa:Place", "knowa:Event", "knowa:Insight"].map((t) => h("option", { value: t }))),
  );
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    location.hash = withParams(route, c, { q: text.value.trim(), type: typeIn.value.trim(), notType: notTypeIn.value.trim() });
  });

  const results = h("div");
  main.append(h("h1", { tabindex: "-1" }, "Search"), form, results);

  const doc = { op: "find", spaces: spacesDoc(c), view: viewDoc(c), target: "entities", where: {}, limit: PAGE };
  if (q) doc.where.text = q;
  if (terms(type).length) doc.where.type = terms(type);
  if (terms(notType).length) doc.where.notType = terms(notType);
  const filtered = Boolean(q || type || notType);

  await env.load(results, "Unable to search", async () => {
    const res = await api.query(doc, env.signal);
    if (res.items.length === 0) {
      return filtered
        ? emptyState(
            q ? `No entities match “${q}”` : "No entities match these types",
            "Try other words or types, or widen the view (statuses, author kinds) or the space.",
            h("a", { class: "btn", href: href(c, "search") }, "Clear search"),
          )
        : emptyState(
            "No entities in this view yet",
            "Add files with knowa put <path> or write a note with knowa note <text>. If you expected entities, widen the view or choose another space.",
          );
    }
    const list = h("ul", { class: "entity-list" });
    const heading = h("h2", { class: "sr-only" }, "Results");
    const count = h("p", { class: "count" });
    const footer = h("div", { class: "load-more" });
    let shown = 0;
    const add = (page) => {
      for (const e of page.items) list.append(h("li", {}, titleLink(c, e), entityMeta(env, e)));
      shown += page.items.length;
      const n = q ? plural(shown, { one: "# match", other: "# matches" }) : plural(shown, { one: "# entity", other: "# entities" });
      count.textContent = `${n}${page.cursor ? " so far" : ""}, ${q ? "best first" : "newest first"}`;
      footer.replaceChildren();
      if (page.cursor) {
        const more = h("button", { type: "button", class: "btn" }, "Load more");
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
        footer.append(h("p", { class: "note-text" }, `Showing the first ${PAGE}: paging needs a single space. Choose one, or narrow the search.`));
      }
    };
    add(res);
    return [h("div", { class: "page-head" }, heading, count), list, footer];
  });
}
