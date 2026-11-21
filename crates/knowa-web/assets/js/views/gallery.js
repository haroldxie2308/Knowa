// Gallery (DESIGN §7.4): knowa:Photo entities as a grid, newest first. Images are
// fetched with the access token and shown from blob: URLs, only once a tile scrolls
// near the viewport, a few at a time.

import * as api from "../api.js";
import { h, plural } from "../dom.js";
import { whenText } from "../entities.js";
import { href, spacesDoc, viewDoc } from "../route.js";
import { emptyState, inlineError } from "../ui.js";

const PAGE = 60;
const CONCURRENT = 4;

export async function render(main, route, env) {
  const c = env.controls;
  env.title("Gallery");
  const results = h("div");
  main.append(h("h1", { tabindex: "-1" }, "Gallery"), results);

  const doc = { op: "find", spaces: spacesDoc(c), view: viewDoc(c), target: "entities", where: { type: ["knowa:Photo"] }, order: "-when", limit: PAGE };

  // Thumbnails load in viewport order, CONCURRENT at a time.
  const queue = [];
  let running = 0;
  const pump = () => {
    while (running < CONCURRENT && queue.length) {
      const job = queue.shift();
      running += 1;
      job().finally(() => {
        running -= 1;
        pump();
      });
    }
  };
  const observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        observer.unobserve(entry.target);
        queue.push(entry.target.loadImage);
      }
      pump();
    },
    { rootMargin: "400px" },
  );
  env.signal.addEventListener("abort", () => observer.disconnect());

  const tile = (e) => {
    const frame = h("div", { class: "tile-frame" }, e.id.startsWith("b_") ? "Loading…" : "No image bytes: not a file");
    if (e.id.startsWith("b_")) {
      frame.loadImage = async () => {
        let got;
        try {
          got = await api.blob(e.id, spacesDoc(c), env.signal);
        } catch (err) {
          if (err?.name === "AbortError") return;
          if (!(err instanceof api.ApiError)) throw err;
          frame.replaceChildren(err.code === "not_found" ? "The selected spaces do not store this file’s bytes" : "Unable to load the image. Reload the page to retry.");
          return;
        }
        frame.replaceChildren(got.contentType.startsWith("image/") ? h("img", { src: env.objectUrl(got.bytes), alt: "" }) : "No inline preview for this media type");
      };
      observer.observe(frame);
    }
    return h(
      "li",
      {},
      h(
        "a",
        { class: "tile", href: href(c, "entity", { id: e.id }) },
        frame,
        h("span", { class: "tile-label", title: e.label ?? e.id, dir: "auto" }, e.label ?? e.id),
        h("span", { class: "meta" }, e.when ? whenText(e.when) : "No date"),
      ),
    );
  };

  await env.load(results, "Unable to load photos", async () => {
    const res = await api.query(doc, env.signal);
    if (res.items.length === 0) {
      return emptyState(
        "No photos in this view",
        "Photos are images the media extractor types as knowa:Photo. Add some with knowa put <path>, or widen the view or the space.",
      );
    }
    const grid = h("ul", { class: "grid" });
    const count = h("p", { class: "count" });
    const footer = h("div", { class: "load-more" });
    let shown = 0;
    const add = (page) => {
      for (const e of page.items) grid.append(tile(e));
      shown += page.items.length;
      count.textContent = `${plural(shown, { one: "# photo", other: "# photos" })}${page.cursor ? " so far" : ""}, newest first`;
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
      }
    };
    add(res);
    return [h("div", { class: "page-head" }, count), grid, footer];
  });
}
