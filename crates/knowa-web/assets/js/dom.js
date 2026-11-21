// DOM helpers. Everything is built with createElement, never innerHTML: claim text is
// untrusted, and the page's CSP allows no inline styles or scripts.

/** h("a", { href, class }, child, …). Children may be nodes, strings, arrays or null. */
export function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === null || v === undefined || v === false) continue;
    if (k === "class") el.className = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "dataset") Object.assign(el.dataset, v);
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  append(el, children);
  return el;
}

export function append(el, children) {
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : String(c));
  }
  return el;
}

const SVG_NS = "http://www.w3.org/2000/svg";
const ICONS = {
  check: ["M3.5 8.5l3 3 6-7"],
  x: ["M4 4l8 8", "M12 4l-8 8"],
  copy: ["M5.5 5.5h7v7h-7z", "M3.5 10.5v-7h7"],
};

/** A 16×16 line icon in currentColor; decorative, so hidden from assistive tech. */
export function icon(name) {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  // Matches the 500-weight labels the icons sit beside.
  svg.setAttribute("stroke-width", "1.75");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  for (const d of ICONS[name]) {
    const p = document.createElementNS(SVG_NS, "path");
    p.setAttribute("d", d);
    svg.append(p);
  }
  return svg;
}

const numberFmt = new Intl.NumberFormat();
const pluralRules = new Intl.PluralRules();

/** `forms` maps plural categories to messages with `#` for the number. */
export function plural(n, forms) {
  const form = forms[pluralRules.select(n)] ?? forms.other;
  return form.replace("#", numberFmt.format(n));
}

const dateTimeFmt = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });

/** A record time (`at`, RFC 3339 UTC) in the reader's time zone, exact value on hover. */
export function recordTime(at) {
  return h("time", { datetime: at, title: at }, dateTimeFmt.format(new Date(at)));
}

/** Middle-truncated, so ids that share a prefix stay distinguishable. */
export function shortId(id) {
  return id.length > 18 ? `${id.slice(0, 10)}…${id.slice(-5)}` : id;
}

let announcer = null;

/** Polite announcement through the page's one live region. */
export function announce(message) {
  announcer ??= document.getElementById("announce");
  announcer.textContent = "";
  // A new text node in a cleared region is announced even when the text repeats.
  requestAnimationFrame(() => {
    announcer.textContent = message;
  });
}

/**
 * An id in monospace. Long ids are truncated; the full value is in the tooltip, shown
 * in place on keyboard focus, and copied on click.
 */
export function idButton(id) {
  const label = h("span", { class: "id-text" }, shortId(id));
  const btn = h("button", { type: "button", class: "id", "data-full": id, title: `${id}\nClick to copy`, "aria-label": `Copy id ${id}` }, label, icon("copy"));
  btn.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(id);
    } catch {
      // The browser refused clipboard access: show the full id selected instead.
      label.textContent = id;
      getSelection()?.selectAllChildren(label);
      announce("Unable to copy automatically. The full id is selected; copy it with your keyboard.");
      return;
    }
    label.textContent = "Copied";
    announce("Id copied");
    setTimeout(() => {
      label.textContent = shortId(id);
    }, 1500);
  });
  return btn;
}

/** A kernel or vocabulary term in monospace. */
export function tag(term) {
  return h("span", { class: "tag" }, term);
}
