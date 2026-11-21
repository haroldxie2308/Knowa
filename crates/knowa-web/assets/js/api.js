// The HTTP API of DESIGN §7.4. Every call carries the access token from the URL
// fragment `#k=…`, which is kept in sessionStorage so reloads and in-app links work
// after it is stripped from the address bar.

const TOKEN_KEY = "knowa.token";

/** An error object from the server (DESIGN §6.3), or a failure to reach it. */
export class ApiError extends Error {
  constructor(status, code, message, detail) {
    super(message);
    this.status = status;
    this.code = code;
    this.detail = detail;
  }
}

let token = null;

function storage() {
  // sessionStorage throws in some private modes; without it the token lives only in
  // this page's memory, which still serves every call until a reload.
  try {
    return window.sessionStorage;
  } catch {
    return null;
  }
}

/**
 * Takes the token from `#k=…` (first load) or sessionStorage (reloads). Returns false
 * when neither has one. Strips the token from the visible URL.
 */
export function loadToken() {
  const m = location.hash.match(/^#k=([a-z0-9]+)$/);
  if (m) {
    token = m[1];
    storage()?.setItem(TOKEN_KEY, token);
    history.replaceState(null, "", `${location.pathname}#/search`);
    return true;
  }
  token = storage()?.getItem(TOKEN_KEY) ?? null;
  return token !== null;
}

export function forgetToken() {
  token = null;
  storage()?.removeItem(TOKEN_KEY);
}

async function send(path, { method = "GET", body, signal } = {}) {
  const headers = { Authorization: `Bearer ${token}` };
  if (body !== undefined) headers["Content-Type"] = "application/json";
  let res;
  try {
    res = await fetch(path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body), signal });
  } catch (e) {
    if (e.name === "AbortError") throw e;
    throw new ApiError(0, "network", "Unable to reach knowa serve.", null);
  }
  if (!res.ok) {
    let err;
    try {
      err = (await res.json()).error;
    } catch {
      throw new ApiError(res.status, "internal", `The server answered ${res.status} without an error object.`, null);
    }
    throw new ApiError(res.status, err.code, err.message, err.detail ?? null);
  }
  return res;
}

export async function getJson(path, signal) {
  return (await send(path, { signal })).json();
}

export async function postJson(path, body, signal) {
  return (await send(path, { method: "POST", body, signal })).json();
}

export function query(doc, signal) {
  return postJson("/api/query", doc, signal);
}

/** The blob's bytes as a Blob, with the content type the server chose. */
export async function blob(id, spaces, signal) {
  const qs = spaces.length ? `?spaces=${encodeURIComponent(spaces.join(","))}` : "";
  const res = await send(`/api/blob/${encodeURIComponent(id)}${qs}`, { signal });
  return { bytes: await res.blob(), contentType: res.headers.get("Content-Type") ?? "", attachment: res.headers.has("Content-Disposition") };
}
