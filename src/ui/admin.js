"use strict";
/*
 * mcpmem principal administration.
 *
 * A dependency-free single page for the admin API: PKCE login through the
 * server's own authorization server with the reserved admin client, a
 * principals table (built-ins read-only, runtime rows editable), and the
 * approval waitlist with promote/dismiss. No build step.
 *
 * Data endpoints (same server that serves /mcp; no MCP tools involved):
 *   GET/POST    /ui/api/principals
 *   PATCH/DELETE /ui/api/principals/{id}
 *   GET         /ui/api/waitlist
 *   POST        /ui/api/waitlist/{id}/approve
 *   DELETE      /ui/api/waitlist/{id}
 * All bodies use camelCase keys: maskedByBuiltin, defaultNewPrincipalScopes,
 * firstSeenUs, lastSeenUs. A 401 starts the login flow; a 403 shows the
 * not-an-admin message.
 */

const CLIENT_ID = "mcpmem-admin-ui";
const TOKEN_KEY = "mcpmem_admin_access";
const VERIFIER_KEY = "mcpmem_admin_verifier";
const REDIRECT = location.origin + "/ui/admin/callback";

let accessToken = sessionStorage.getItem(TOKEN_KEY);
let defaultScopes = ["graph-read"];

function b64url(bytes) {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function randomVerifier() {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  return b64url(bytes);
}

async function challenge(verifier) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier));
  return b64url(new Uint8Array(digest));
}

async function beginAuth() {
  const verifier = randomVerifier();
  sessionStorage.setItem(VERIFIER_KEY, verifier);
  const params = new URLSearchParams({
    response_type: "code",
    client_id: CLIENT_ID,
    redirect_uri: REDIRECT,
    scope: "admin",
    state: "admin",
    code_challenge_method: "S256",
    code_challenge: await challenge(verifier),
  });
  location.href = "/oauth/authorize?" + params;
}

async function completeAuth() {
  const params = new URLSearchParams(location.search);
  const code = params.get("code");
  const verifier = sessionStorage.getItem(VERIFIER_KEY);
  sessionStorage.removeItem(VERIFIER_KEY);
  if (!code || !verifier) return;
  const form = new URLSearchParams({
    grant_type: "authorization_code",
    code,
    redirect_uri: REDIRECT,
    client_id: CLIENT_ID,
    code_verifier: verifier,
  });
  const res = await fetch("/oauth/token", {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: form,
  });
  if (!res.ok) {
    setStatus("Sign-in failed: " + (await res.text()));
    return;
  }
  const body = await res.json();
  accessToken = body.access_token;
  sessionStorage.setItem(TOKEN_KEY, accessToken);
  history.replaceState(null, "", "/ui/admin");
  load();
}

async function api(path, options = {}) {
  const headers = { ...(options.headers || {}) };
  if (accessToken) headers["Authorization"] = "Bearer " + accessToken;
  if (options.body) headers["Content-Type"] = "application/json";
  const res = await fetch(path, { ...options, headers });
  if (res.status === 401) {
    accessToken = null;
    sessionStorage.removeItem(TOKEN_KEY);
    await beginAuth();
    return null;
  }
  if (res.status === 403) {
    // A token that holds no admin scope: the tables are off-limits and the
    // message is the whole page. Drop the useless token so nothing retries
    // with it.
    accessToken = null;
    sessionStorage.removeItem(TOKEN_KEY);
    setStatus("Access failed: this login is not an admin.");
    return null;
  }
  if (!res.ok) {
    const text = await res.text();
    throw new Error(res.status + " " + text);
  }
  return res.status === 204 ? null : res.json();
}

async function load() {
  const data = await api("/ui/api/principals");
  if (!data) return;
  defaultScopes = data.defaultNewPrincipalScopes || [];
  renderPrincipals(data.principals);
  document.getElementById("add").hidden = false;
  const wait = await api("/ui/api/waitlist");
  if (wait) renderWaitlist(wait.entries);
  setStatus("");
}

function renderPrincipals(principals) {
  const tbody = document.getElementById("principal-rows");
  tbody.textContent = "";
  for (const p of principals) {
    const tr = document.createElement("tr");
    const name = document.createElement("td");
    name.textContent = p.name;
    if (p.builtin) name.appendChild(badge("built-in"));
    if (p.maskedByBuiltin) name.appendChild(badge("masked by built-in"));
    const identity = document.createElement("td");
    identity.textContent = p.iss + " — " + p.sub;
    identity.className = "mono";
    const scopes = document.createElement("td");
    scopes.textContent = p.scopes.join(", ");
    const actions = document.createElement("td");
    if (!p.builtin) {
      const edit = document.createElement("button");
      edit.textContent = "Edit";
      edit.onclick = () => openForm(p);
      const del = document.createElement("button");
      del.textContent = "Remove";
      del.onclick = () => removePrincipal(p);
      actions.append(edit, del);
    }
    tr.append(name, identity, scopes, actions);
    tbody.append(tr);
  }
}

function badge(text) {
  const span = document.createElement("span");
  span.className = "badge";
  span.textContent = text;
  return span;
}

function renderWaitlist(entries) {
  const tbody = document.getElementById("waitlist-rows");
  tbody.textContent = "";
  for (const e of entries) {
    const tr = document.createElement("tr");
    const name = document.createElement("td");
    name.textContent = e.name;
    const identity = document.createElement("td");
    identity.textContent = e.iss + " — " + e.sub;
    identity.className = "mono";
    const seen = document.createElement("td");
    seen.textContent = new Date(e.firstSeenUs / 1000).toLocaleString();
    const actions = document.createElement("td");
    const approve = document.createElement("button");
    approve.textContent = "Approve";
    approve.onclick = () => openApprove(e);
    const dismiss = document.createElement("button");
    dismiss.textContent = "Dismiss";
    dismiss.onclick = () => dismissEntry(e);
    actions.append(approve, dismiss);
    tr.append(name, identity, seen, actions);
    tbody.append(tr);
  }
}

async function removePrincipal(p) {
  if (!confirm("Remove " + p.name + "? Their token families will be revoked.")) return;
  try {
    await api("/ui/api/principals/" + encodeURIComponent(p.id), { method: "DELETE" });
    await load();
  } catch (e) {
    setStatus(e.message);
  }
}

function scopesPicker(selected) {
  const wrap = document.createElement("div");
  wrap.className = "scopes";
  const all = ["graph-read", "graph-write", "vectors", "code", "admin"];
  for (const slug of all) {
    const label = document.createElement("label");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.value = slug;
    box.checked = selected.includes(slug);
    label.append(box, " " + slug);
    wrap.append(label);
  }
  return wrap;
}

function openForm(p) {
  const dialog = document.getElementById("form");
  dialog.textContent = "";
  const h = document.createElement("h2");
  h.textContent = p ? "Edit " + p.name : "Add principal";
  dialog.append(h);

  const fields = [["name", "Name", p ? p.name : ""]];
  if (!p) fields.push(["iss", "Issuer (iss)", ""], ["sub", "Subject (sub)", ""]);
  fields.push(["label", "Label", p && p.label || ""]);
  for (const [key, label, value] of fields) {
    const row = document.createElement("label");
    row.textContent = label + ": ";
    const input = document.createElement("input");
    input.id = "f-" + key;
    input.value = value;
    row.append(input);
    dialog.append(row);
  }

  const scopes = scopesPicker(p ? p.scopes : defaultScopes);
  dialog.append(scopes);

  const save = document.createElement("button");
  save.textContent = "Save";
  save.onclick = async () => {
    const read = (k) => document.getElementById("f-" + k).value.trim();
    const picked = [...scopes.querySelectorAll("input:checked")].map((b) => b.value);
    const body = {
      name: read("name"),
      label: read("label") || null,
      scopes: picked,
    };
    try {
      if (p) {
        await api("/ui/api/principals/" + encodeURIComponent(p.id), {
          method: "PATCH",
          body: JSON.stringify({ name: body.name, label: body.label, scopes: body.scopes }),
        });
      } else {
        await api("/ui/api/principals", {
          method: "POST",
          body: JSON.stringify({ name: body.name, iss: read("iss"), sub: read("sub"), label: body.label, scopes: body.scopes }),
        });
      }
      dialog.close();
      await load();
    } catch (e) {
      setStatus(e.message);
    }
  };
  const cancel = document.createElement("button");
  cancel.textContent = "Cancel";
  cancel.onclick = () => dialog.close();
  dialog.append(save, cancel);
  dialog.showModal();
}

function openApprove(e) {
  const dialog = document.getElementById("form");
  dialog.textContent = "";
  const h = document.createElement("h2");
  h.textContent = "Approve " + e.name;
  dialog.append(h);
  const who = document.createElement("p");
  who.textContent = e.iss + " — " + e.sub;
  who.className = "mono";
  dialog.append(who);
  const scopes = scopesPicker(defaultScopes);
  dialog.append(scopes);
  const approve = document.createElement("button");
  approve.textContent = "Approve";
  approve.onclick = async () => {
    const picked = [...scopes.querySelectorAll("input:checked")].map((b) => b.value);
    try {
      await api("/ui/api/waitlist/" + encodeURIComponent(e.id) + "/approve", {
        method: "POST",
        body: JSON.stringify({ scopes: picked }),
      });
      dialog.close();
      await load();
    } catch (err) {
      setStatus(err.message);
    }
  };
  const cancel = document.createElement("button");
  cancel.textContent = "Cancel";
  cancel.onclick = () => dialog.close();
  dialog.append(approve, cancel);
  dialog.showModal();
}

async function dismissEntry(e) {
  try {
    await api("/ui/api/waitlist/" + encodeURIComponent(e.id), { method: "DELETE" });
    await load();
  } catch (err) {
    setStatus(err.message);
  }
}

function setStatus(text) {
  const el = document.getElementById("status");
  el.textContent = text;
  el.className = text.startsWith("Error") || text.includes("failed") ? "hint error" : "hint";
}

document.getElementById("add").onclick = () => openForm(null);

(async function boot() {
  if (new URLSearchParams(location.search).has("code")) {
    await completeAuth();
    return;
  }
  if (!accessToken) {
    await beginAuth();
    return;
  }
  load();
})();