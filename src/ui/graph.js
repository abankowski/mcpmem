"use strict";
/*
 * mcpmem knowledge-graph viewer.
 *
 * A dependency-free, Neo4j-Browser-style graph explorer rendered on a <canvas>:
 * force-directed layout, captioned circular nodes coloured by entity type,
 * curved multi-edges with relationship-type pills + arrowheads, a node
 * inspector, a live legend, paginated browse + full-text search, and — the
 * headline interaction — double-click a node to expand its relationships.
 *
 * Data endpoints (same server that serves /mcp; no MCP tools involved):
 *   GET /ui/workspaces?cursor         → accessible workspace pages
 *   GET /ui/graph?workspaceId&entityType&offset&limit → a graph page
 *   GET /ui/search?workspaceId&q&entityType&offset&limit → FTS matches
 *   GET /ui/node?workspaceId&name     → one node's observations
 *   GET /ui/expand?workspaceId&name&depth&direction → a node's neighbourhood
 * Graph routes return entities, relations, entityTypes, stats, and page details.
 */
(function () {
  const $ = (id) => document.getElementById(id);
  const cv = $("cv"), ctx = cv.getContext("2d");
  const SEP = "\u0000"; // key delimiter — a NUL byte never appears in real names

  // Neo4j Browser's default categorical label palette. Types are assigned a
  // colour in first-seen order and it sticks for the session.
  const PALETTE = [
    "#FFDF81", "#C990C0", "#F79767", "#57C7E3", "#F16667", "#D9C8AE",
    "#8DCC93", "#ECB5C9", "#4C8EDA", "#FFC454", "#DA7194", "#569480",
    "#848484", "#B2B2B2", "#B0C4DE", "#B58AA5",
  ];
  const colorOf = new Map();
  let paletteNext = 0;
  function colorForType(t) {
    if (!colorOf.has(t)) { colorOf.set(t, PALETTE[paletteNext % PALETTE.length]); paletteNext++; }
    return colorOf.get(t);
  }

  // ── Auth ──────────────────────────────────────────────────────────────────
  // Two ways this page's data requests authenticate, chosen by what the
  // server's 401 challenge advertises. When OAuth is on, the challenge names
  // the authorization server (`resource_metadata=…`), and this viewer is a
  // public PKCE client of it — exactly like the admin SPA: the browser goes
  // to `/oauth/authorize` and comes back with `?code=`, which is exchanged
  // here for an access token. When OAuth is off the challenge is a bare
  // `Bearer`, and the token box is the whole login.
  //
  // The static bearer token, for the deployments that configure one: URL hash
  // (`#token=…`, never sent to the server / not logged), else sessionStorage.
  // Kept client-side; forwarded as an Authorization header.
  function readHashToken() {
    const m = /[#&]token=([^&]+)/.exec(location.hash || "");
    return m ? decodeURIComponent(m[1]) : null;
  }
  const OAUTH_CLIENT_ID = "mcpmem-graph-ui";
  const OAUTH_TOKEN_KEY = "mcpmem_graph_access";
  const OAUTH_VERIFIER_KEY = "mcpmem_graph_verifier";
  const OAUTH_RETURN_KEY = "mcpmem_graph_attachment_return";
  // The client is seeded with "{public_url}/ui" and /oauth/authorize compares
  // redirect_uri byte-for-byte, so this derives the redirect from the page's
  // own path: a path-prefixed --public-url must not lose its prefix.
  const OAUTH_REDIRECT = location.origin + location.pathname.replace(/\/+$/, "");

  let token = readHashToken()
    || sessionStorage.getItem(OAUTH_TOKEN_KEY)
    || sessionStorage.getItem("mcpmem_token")
    || "";
  if (readHashToken()) {
    sessionStorage.setItem("mcpmem_token", token);
    history.replaceState(null, "", location.pathname + location.search);
  }

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
  async function pkceChallenge(verifier) {
    const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier));
    return b64url(new Uint8Array(digest));
  }
  function oauthAdvertised(res) {
    return (res.headers.get("WWW-Authenticate") || "").includes("resource_metadata");
  }
  async function beginOAuth(generation, isActive, scopes = "graph-read", flow = "graph") {
    const verifier = randomVerifier();
    sessionStorage.setItem(OAUTH_VERIFIER_KEY, verifier);
    const params = new URLSearchParams({
      response_type: "code",
      client_id: OAUTH_CLIENT_ID,
      redirect_uri: OAUTH_REDIRECT,
      scope: scopes,
      state: flow,
      code_challenge_method: "S256",
      code_challenge: await pkceChallenge(verifier),
    });
    if (!isCurrent(generation) || !isActive()) return;
    if (flow === "attachments" && selected) {
      sessionStorage.setItem(OAUTH_RETURN_KEY, JSON.stringify({
        workspaceId: workspace.id, entityName: selected.id, offset: browse.offset,
        limit: browse.limit, query: browse.query, entityType: browse.entityType,
      }));
    }
    location.href = "/oauth/authorize?" + params;
  }
  async function completeOAuth() {
    const params = new URLSearchParams(location.search);
    const code = params.get("code");
    const verifier = sessionStorage.getItem(OAUTH_VERIFIER_KEY);
    sessionStorage.removeItem(OAUTH_VERIFIER_KEY);
    // The code is single-use; keep it out of the address bar either way.
    history.replaceState(null, "", location.pathname);
    if (!code || !verifier) return false;
    const form = new URLSearchParams({
      grant_type: "authorization_code",
      code,
      redirect_uri: OAUTH_REDIRECT,
      client_id: OAUTH_CLIENT_ID,
      code_verifier: verifier,
    });
    let res;
    try {
      res = await fetch("/oauth/token", {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: form,
      });
    } catch {
      overlay("Sign-in failed", "The token exchange could not be completed. Reload to try again.", { err: true });
      return false;
    }
    if (!res.ok) {
      overlay("Sign-in failed", await res.text().catch(() => ""), { err: true });
      return false;
    }
    const body = await res.json();
    token = body.access_token;
    sessionStorage.setItem(OAUTH_TOKEN_KEY, token);
    return true;
  }

  // ── State ─────────────────────────────────────────────────────────────────
  const view = { x: 0, y: 0, k: 1 };
  const browse = { offset: 0, limit: 300, query: "", entityType: "" }; // paged browse/search cursor
  let page = { offset: 0, limit: 300, returned: 0, hasMore: false };
  let nodes = [], links = [], nodeById = new Map();
  let totalStats = null;
  let selected = null, hover = null, pinnedDrag = null;
  let alpha = 0, raf = null, busyReq = false;
  const workspace = { id: null, role: null, generation: 0 };
  let pendingInspector = null;
  const activeRequests = new Set();
  let workspaceListRequest = null;
  const isCurrent = (generation) => generation === workspace.generation;
  function beginRequest() {
    const controller = new AbortController();
    activeRequests.add(controller);
    return controller;
  }
  let dpr = Math.max(1, window.devicePixelRatio || 1);
  // Cap on nodes held in the canvas at once. Browse pages are already bounded
  // (≤1000, server-enforced); this bounds *expansion* so double-clicking a hub
  // can't push the force layout into tens of thousands of nodes.
  const MAX_RENDER_NODES = 3000;

  const toScreen = (wx, wy) => ({ x: (wx + view.x) * view.k, y: (wy + view.y) * view.k });
  const toWorld = (sx, sy) => ({ x: sx / view.k - view.x, y: sy / view.k - view.y });

  function resize() {
    dpr = Math.max(1, window.devicePixelRatio || 1);
    const r = cv.getBoundingClientRect();
    cv.width = Math.round(r.width * dpr);
    cv.height = Math.round(r.height * dpr);
    requestDraw();
  }
  window.addEventListener("resize", resize);

  // ── API & overlay ──────────────────────────────────────────────────────────
  function api(path, signal) {
    const headers = {};
    if (token) headers["Authorization"] = "Bearer " + token;
    return fetch(path, { headers, signal });
  }
  function overlay(title, msg, opts = {}) {
    $("ovTitle").textContent = title;
    $("ovMsg").textContent = msg || "";
    $("ovTokRow").style.display = opts.token ? "flex" : "none";
    $("overlay").classList.toggle("err", !!opts.err);
    $("overlay").classList.add("show");
    if (opts.token) $("ovToken").focus();
  }
  const hideOverlay = () => $("overlay").classList.remove("show");
  async function handleError(res, generation, isActive = () => true) {
    const current = () => isCurrent(generation) && isActive();
    if (res.ok || !current()) return !res.ok;
    if (res.status === 401) {
      // OAuth on: start PKCE login. A static bearer uses the token box.
      if (oauthAdvertised(res)) {
        await beginOAuth(generation, isActive);
        return true;
      }
      overlay("Authentication required", "This server requires a bearer token.", { token: true, err: true });
    } else {
      const message = await res.text().catch(() => "");
      if (!current()) return true;
      if (res.status === 403) {
        overlay("Graph reading disabled", message || "Start the server with --enable-graph-read (or --enable-all).", { err: true });
      } else {
        overlay("Error " + res.status, message, { err: true });
      }
    }
    return true;
  }

  function resetGraphView() {
    browse.offset = 0;
    browse.query = "";
    browse.entityType = "";
    page = { offset: 0, limit: browse.limit, returned: 0, hasMore: false };
    nodes = []; links = []; nodeById = new Map();
    totalStats = null;
    hover = null; pinnedDrag = null; dragging = false; dragMoved = false;
    alpha = 0; view.x = 0; view.y = 0; view.k = 1;
    colorOf.clear(); paletteNext = 0;
    clearTimeout(flashTimer);
    $("search").value = "";
    $("typeFilter").innerHTML = '<option value="">all labels</option>';
    $("tooltip").style.display = "none";
    $("legend").textContent = "";
    $("insBody").textContent = "";
    $("insName").textContent = "";
    selectNode(null);
    setBusy(false);
    if (!workspace.id) {
      $("stats").textContent = "—";
      $("pageLabel").textContent = "—";
    } else updateStats();
    requestDraw();
  }

  function selectWorkspace(id) {
    if (workspace.id === id) return;
    workspace.id = id;
    workspace.role = [...$("workspace").options].find((option) => option.value === id)?.dataset.role || null;
    workspace.generation++;
    for (const controller of activeRequests) controller.abort();
    activeRequests.clear();
    workspaceListRequest = null;
    resetGraphView();
    $("workspace").value = id || "";
    if (pendingInspector?.workspaceId === id) {
      browse.offset = Number.isSafeInteger(pendingInspector.offset) && pendingInspector.offset >= 0 ? pendingInspector.offset : 0;
      browse.limit = Number.isSafeInteger(pendingInspector.limit) && pendingInspector.limit > 0 ? Math.min(1000, pendingInspector.limit) : browse.limit;
      browse.query = pendingInspector.query || "";
      browse.entityType = pendingInspector.entityType || "";
      $("search").value = browse.query;
      $("typeFilter").value = browse.entityType;
    }
    if (id) load();
    else overlay("Select a workspace", "Choose a workspace to view its graph.");
  }

  function rejectWorkspace(generation) {
    if (!isCurrent(generation)) return;
    selectWorkspace(null);
    overlay("Workspace unavailable", "Access to this workspace ended. Choose another workspace.", { err: true });
    loadWorkspaces(false);
  }

  async function loadWorkspaces(initial) {
    if (workspaceListRequest) workspaceListRequest.abort();
    const generation = workspace.generation;
    const controller = beginRequest();
    workspaceListRequest = controller;
    const isActive = () => isCurrent(generation) && workspaceListRequest === controller;
    $("workspace").disabled = true;
    if (initial) {
      $("workspace").innerHTML = '<option value="">Loading workspaces…</option>';
      overlay("Loading workspaces…", "Fetching accessible workspaces.");
    }
    try {
      const workspaces = [];
      let cursor = null;
      do {
        const path = "/ui/workspaces" + (cursor === null ? "" : "?cursor=" + encodeURIComponent(cursor));
        const res = await api(path, controller.signal);
        if (!isActive()) return;
        if (await handleError(res, generation, isActive)) return;
        if (!isActive()) return;
        const data = await res.json();
        if (!isActive()) return;
        workspaces.push(...data.workspaces);
        cursor = data.nextCursor;
      } while (cursor !== null);

      const picker = $("workspace");
      picker.textContent = "";
      const prompt = document.createElement("option");
      prompt.value = "";
      prompt.textContent = "Select a workspace";
      picker.append(prompt);
      for (const entry of workspaces) {
        const option = document.createElement("option");
        option.value = entry.workspaceId;
        option.textContent = entry.name;
        option.dataset.role = entry.role;
        picker.append(option);
      }
      picker.disabled = false;
      if (initial) {
        const savedDefault = workspaces.find((entry) => entry.isDefault);
        const resume = pendingInspector && workspaces.find((entry) => entry.workspaceId === pendingInspector.workspaceId);
        if (resume) selectWorkspace(resume.workspaceId);
        else if (savedDefault) selectWorkspace(savedDefault.workspaceId);
        else overlay("Select a workspace", "No saved default. Choose a workspace to view its graph.");
      } else if (workspace.id && !workspaces.some((entry) => entry.workspaceId === workspace.id)) {
        selectWorkspace(null);
        overlay("Workspace unavailable", "Access to this workspace ended. Choose another workspace.", { err: true });
      } else picker.value = workspace.id || "";
    } catch (e) {
      if (isActive() && e.name !== "AbortError") {
        overlay("Workspace list failed", String(e), { err: true });
      }
    } finally {
      activeRequests.delete(controller);
      if (workspaceListRequest === controller) workspaceListRequest = null;
    }
  }

  // ── Paginated load (browse overview OR full-text search) ───────────────────
  function setBusy(b) {
    busyReq = b;
    const disabled = b || !workspace.id;
    for (const id of ["searchBtn", "overview", "typeFilter"]) $(id).disabled = disabled;
    if (!b) updatePager();
    else for (const id of ["prev", "next"]) $(id).disabled = true;
  }
  async function load() {
    if (!workspace.id || busyReq) return;
    const generation = workspace.generation;
    const controller = beginRequest();
    setBusy(true);
    overlay("Loading…", browse.query ? `Searching for “${browse.query}”…` : "Fetching the knowledge graph.");
    const p = new URLSearchParams({ workspaceId: workspace.id });
    if (browse.entityType) p.set("entityType", browse.entityType);
    p.set("offset", String(browse.offset));
    p.set("limit", String(browse.limit));
    const path = browse.query ? "/ui/search?q=" + encodeURIComponent(browse.query) + "&" + p : "/ui/graph?" + p;
    try {
      const res = await api(path, controller.signal);
      if (!isCurrent(generation)) return;
      if (res.status === 404) { rejectWorkspace(generation); return; }
      if (await handleError(res, generation)) return;
      if (!isCurrent(generation)) return;
      const data = await res.json();
      if (!isCurrent(generation)) return;
      setGraph(data);
    } catch (e) {
      if (isCurrent(generation) && e.name !== "AbortError") overlay("Connection failed", String(e), { err: true });
    } finally {
      activeRequests.delete(controller);
      if (isCurrent(generation)) setBusy(false);
    }
  }

  function runSearch() {
    browse.query = $("search").value.trim();
    browse.offset = 0;
    load();
  }
  function showOverview() {
    $("search").value = "";
    browse.query = "";
    browse.offset = 0;
    load();
  }
  function gotoPage(delta) {
    const next = browse.offset + delta * browse.limit;
    if (next < 0 || (delta > 0 && !page.hasMore)) return;
    browse.offset = Math.max(0, next);
    load();
  }

  // ── Expand (double-click traversal) ────────────────────────────────────────
  // A workspace failure arrives as JSON 404; an entity the caller asks
  // about but that no longer exists arrives as plain-text 404. Only the
  // first means the session lost its workspace. A deleted entity (another
  // writer removed it) is normal: degrade gracefully, keep the session.
  const isJsonError = (res) => (res.headers.get("Content-Type") || "").includes("application/json");
  async function expand(node) {
    if (!workspace.id || !node || node._loading) return;
    const generation = workspace.generation;
    const controller = beginRequest();
    node._loading = true; kick();
    const params = new URLSearchParams({ workspaceId: workspace.id, depth: "1", direction: "both", name: node.id });
    try {
      const res = await api("/ui/expand?" + params, controller.signal);
      if (!isCurrent(generation)) return;
      if (res.status === 404 && isJsonError(res)) { rejectWorkspace(generation); return; }
      if (!res.ok) {
        if (res.status === 401 || res.status === 403) await handleError(res, generation);
        if (!isCurrent(generation)) return;
        flash("expand failed (" + res.status + ")");
        return;
      }
      const data = await res.json();
      if (!isCurrent(generation)) return;
      node.expanded = true;
      const { added, capped } = mergeGraph(data, node);
      if (capped) flash(`node limit ${fmt(MAX_RENDER_NODES)} reached — dismiss or isolate to explore further`);
      else flash(added ? `+${added} node${added === 1 ? "" : "s"}` : "no new relationships");
    } catch (e) {
      if (isCurrent(generation) && e.name !== "AbortError") flash("expand failed: " + e);
    } finally {
      activeRequests.delete(controller);
      if (isCurrent(generation)) { node._loading = false; kick(); }
    }
  }

  // ── Graph (re)building ─────────────────────────────────────────────────────
  function makeNode(e, x, y) {
    const hasObs = Array.isArray(e.observations);
    return {
      id: e.name, type: e.entityType || "",
      // The browse/search list payloads omit observation *bodies* (they carry
      // only `obsCount`); the inspector lazy-loads bodies via /ui/node on select.
      // `obs === null` means "bodies not loaded yet"; `obsCount` is always known.
      obs: hasObs ? e.observations : null,
      obsCount: hasObs ? e.observations.length : (e.obsCount | 0),
      color: colorForType(e.entityType || ""), // cache: avoid a Map lookup per node per frame
      x, y, vx: 0, vy: 0, deg: 0, fixed: false, expanded: false, _loading: false,
      _lblR: -1, _lbl: "", // cached fitted label + the screen-radius bucket it was measured at
    };
  }
  function recomputeDegrees() {
    for (const n of nodes) n.deg = 0;
    for (const l of links) { l.source.deg++; l.target.deg++; }
    assignEdgeSlots();
  }
  // Fan parallel/reciprocal edges out as separate curved arcs.
  function assignEdgeSlots() {
    const groups = new Map();
    for (const l of links) {
      const key = l.source.id < l.target.id ? l.source.id + SEP + l.target.id : l.target.id + SEP + l.source.id;
      let g = groups.get(key); if (!g) { g = []; groups.set(key, g); }
      l._group = g; g.push(l);
    }
    for (const g of groups.values()) g.forEach((l, i) => { l._slot = i - (g.length - 1) / 2; });
  }

  // Even, overlap-free seed positions (sunflower/phyllotaxis spiral) for a fresh
  // page. Gives the layout real structure on the first frame so we don't need an
  // expensive synchronous pre-roll before fitting — the animation settles from a
  // sane start instead of from random noise.
  function spiralPos(i) {
    const a = i * 2.399963229728653; // golden angle
    const r = SPRING_LEN * 0.9 * Math.sqrt(i + 0.5);
    return { x: Math.cos(a) * r, y: Math.sin(a) * r };
  }
  function setGraph(data) {
    page = data.page || { offset: browse.offset, limit: browse.limit, returned: (data.entities || []).length, hasMore: false };
    const prev = new Map(nodes.map((n) => [n.id, n]));
    let seed = 0;
    nodes = (data.entities || []).map((e) => {
      const p = prev.get(e.name);
      const s = p ? null : spiralPos(seed++);
      const n = makeNode(e, p ? p.x : s.x, p ? p.y : s.y);
      if (p) n.fixed = p.fixed;
      return n;
    });
    nodeById = new Map(nodes.map((n) => [n.id, n]));
    links = (data.relations || [])
      .filter((r) => nodeById.has(r.from) && nodeById.has(r.to))
      .map((r) => ({ source: nodeById.get(r.from), target: nodeById.get(r.to), type: r.relationType || "" }));
    recomputeDegrees();

    for (const t of (data.entityTypes || [])) colorForType(t.type);
    fillTypeFilter(data.entityTypes || []);
    totalStats = data.stats || totalStats;

    selectNode(null);
    if (pendingInspector?.workspaceId === workspace.id) {
      const resume = pendingInspector;
      pendingInspector = null;
      const node = nodeById.get(resume.entityName);
      if (node) {
        selectNode(node);
        requestAttachmentConsent(false);
      }
    }
    updateStats(); updatePager();
    if (!nodes.length) {
      overlay(browse.query ? "No matches" : "Empty graph",
        browse.query ? `Nothing matched “${browse.query}”.` : "No entities to display for this filter.");
    } else hideOverlay();
    alpha = 1; fitView(true); kick();
  }

  // Merge an expansion result; returns the number of newly added nodes.
  function mergeGraph(data, origin) {
    let added = 0, capped = false;
    for (const e of (data.entities || [])) {
      if (nodeById.has(e.name)) continue;
      if (nodes.length >= MAX_RENDER_NODES) { capped = true; break; } // bound the layout
      const ang = Math.random() * Math.PI * 2, rad = 60 + Math.random() * 40;
      const n = makeNode(e, origin.x + Math.cos(ang) * rad, origin.y + Math.sin(ang) * rad);
      nodes.push(n); nodeById.set(n.id, n); added++;
    }
    const seen = new Set(links.map((l) => l.source.id + SEP + l.type + SEP + l.target.id));
    for (const r of (data.relations || [])) {
      const s = nodeById.get(r.from), t = nodeById.get(r.to);
      if (!s || !t) continue; // an endpoint was past the node cap — skip the edge
      const key = r.from + SEP + (r.relationType || "") + SEP + r.to;
      if (seen.has(key)) continue;
      seen.add(key);
      links.push({ source: s, target: t, type: r.relationType || "" });
    }
    recomputeDegrees(); buildLegend(); updateStats();
    if (selected === origin) selectNode(origin); // refresh inspector relation list
    alpha = Math.max(alpha, 0.45); kick(); // gentle reheat — keep existing layout calm
    return { added, capped };
  }

  function dismiss(node) {
    nodes = nodes.filter((n) => n !== node);
    links = links.filter((l) => l.source !== node && l.target !== node);
    nodeById.delete(node.id);
    if (selected === node) selectNode(null);
    recomputeDegrees(); buildLegend(); updateStats(); kick();
  }
  function isolate(node) {
    const keep = new Set([node]);
    for (const l of links) { if (l.source === node) keep.add(l.target); else if (l.target === node) keep.add(l.source); }
    nodes = nodes.filter((n) => keep.has(n));
    nodeById = new Map(nodes.map((n) => [n.id, n]));
    links = links.filter((l) => keep.has(l.source) && keep.has(l.target));
    recomputeDegrees(); buildLegend(); updateStats(); fitView(false); kick();
  }

  // ── Toolbar / legend / stats / pager ───────────────────────────────────────
  const fmt = (n) => (typeof n === "number" ? n.toLocaleString() : n);
  function updateStats() {
    const e = totalStats ? totalStats.entities : nodes.length;
    const r = totalStats ? totalStats.relations : links.length;
    $("stats").textContent = `${fmt(nodes.length)} / ${fmt(e)} nodes · ${fmt(links.length)} / ${fmt(r)} rels`;
    buildLegend();
  }
  function updatePager() {
    const from = page.returned ? page.offset + 1 : 0;
    const to = page.offset + page.returned;
    $("pageLabel").textContent = page.returned ? `${fmt(from)}–${fmt(to)}` : "0";
    $("prev").disabled = busyReq || !workspace.id || page.offset === 0;
    $("next").disabled = busyReq || !workspace.id || !page.hasMore;
  }
  function fillTypeFilter(types) {
    const sel = $("typeFilter"), cur = sel.value;
    sel.innerHTML = '<option value="">all labels</option>';
    for (const t of types) {
      const o = document.createElement("option");
      o.value = t.type; o.textContent = `${t.type} (${t.count})`;
      sel.appendChild(o);
    }
    sel.value = cur;
  }
  function buildLegend() {
    const counts = new Map();
    for (const n of nodes) counts.set(n.type, (counts.get(n.type) || 0) + 1);
    const el = $("legend"); el.innerHTML = "";
    [...counts.entries()].sort((a, b) => b[1] - a[1]).slice(0, 14).forEach(([type, c]) => {
      const d = document.createElement("span");
      d.className = "chip";
      d.innerHTML = `<span class="sw" style="background:${colorForType(type)}"></span>${escapeHtml(type || "—")} <span class="n">${c}</span>`;
      el.appendChild(d);
    });
  }
  let flashTimer = null;
  function flash(msg) {
    $("stats").textContent = msg;
    clearTimeout(flashTimer);
    flashTimer = setTimeout(updateStats, 1500);
  }

  // ── Force simulation ───────────────────────────────────────────────────────
  const REPULSION = 9000, SPRING = 0.02, SPRING_LEN = 130, GRAVITY = 0.012, DAMP = 0.9;
  const radius = (n) => 15 + Math.min(24, Math.sqrt(n.deg) * 4);
  const anyLoading = () => nodes.some((n) => n._loading);

  // ── Barnes-Hut quadtree: O(n log n) repulsion instead of O(n²) all-pairs, so
  //    large graphs (and hub expansions) stay at interactive frame rates.
  const THETA2 = 0.81; // (θ=0.9)² — cells smaller than θ·distance are lumped into one body
  function insert(q, n, depth) {
    q.cx = (q.cx * q.mass + n.x) / (q.mass + 1);
    q.cy = (q.cy * q.mass + n.y) / (q.mass + 1);
    q.mass++;
    if (q.mass === 1) { q.node = n; return; }
    if (q.size < 1e-3 || depth > 48) return; // coincident cluster — stop subdividing
    if (!q.quads) { q.quads = [null, null, null, null]; const old = q.node; q.node = null; if (old) place(q, old, depth); }
    place(q, n, depth);
  }
  function place(q, n, depth) {
    const half = q.size / 2;
    const i = (n.x >= q.x + half ? 1 : 0) + (n.y >= q.y + half ? 2 : 0);
    let c = q.quads[i];
    if (!c) c = q.quads[i] = { x: q.x + (i & 1 ? half : 0), y: q.y + (i & 2 ? half : 0), size: half, cx: 0, cy: 0, mass: 0, node: null, quads: null };
    insert(c, n, depth + 1);
  }
  function buildTree(ns) {
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (const n of ns) { if (n.x < minX) minX = n.x; if (n.y < minY) minY = n.y; if (n.x > maxX) maxX = n.x; if (n.y > maxY) maxY = n.y; }
    if (!isFinite(minX)) return null;
    const size = Math.max(maxX - minX, maxY - minY, 1);
    const root = { x: minX, y: minY, size, cx: 0, cy: 0, mass: 0, node: null, quads: null };
    for (const n of ns) insert(root, n, 0);
    return root;
  }
  function repulse(q, n, alpha) {
    if (!q || q.mass === 0) return;
    let dx = n.x - q.cx, dy = n.y - q.cy, d2 = dx * dx + dy * dy;
    if (q.quads && q.size * q.size >= THETA2 * d2) { // cell too close to approximate → recurse
      for (const c of q.quads) if (c) repulse(c, n, alpha);
      return;
    }
    if (q.node === n && q.mass === 1) return; // the node's own leaf
    if (d2 < 0.01) { dx = Math.random() - 0.5; dy = Math.random() - 0.5; d2 = 1; }
    const f = (REPULSION * q.mass / d2) * alpha, inv = 1 / Math.sqrt(d2);
    n.vx += dx * inv * f; n.vy += dy * inv * f;
  }

  function step() {
    if (alpha < 0.02) return false;
    const n = nodes.length;
    if (n > 1) {
      const tree = buildTree(nodes);
      for (let i = 0; i < n; i++) repulse(tree, nodes[i], alpha);
    }
    for (const l of links) {
      const dx = l.target.x - l.source.x, dy = l.target.y - l.source.y;
      const dist = Math.sqrt(dx * dx + dy * dy) || 1;
      const f = SPRING * (dist - SPRING_LEN) * alpha;
      const fx = (dx / dist) * f, fy = (dy / dist) * f;
      l.source.vx += fx; l.source.vy += fy; l.target.vx -= fx; l.target.vy -= fy;
    }
    for (const a of nodes) {
      a.vx -= a.x * GRAVITY * alpha; a.vy -= a.y * GRAVITY * alpha;
      if (a.fixed || a === pinnedDrag) { a.vx = 0; a.vy = 0; continue; }
      a.vx *= DAMP; a.vy *= DAMP; a.x += a.vx; a.y += a.vy;
    }
    alpha *= 0.985;
    return true;
  }
  function kick() { alpha = Math.max(alpha, 0.55); if (!raf) loop(); }
  function loop() {
    const busy = step();
    draw();
    raf = (busy || pinnedDrag || anyLoading()) ? requestAnimationFrame(loop) : null;
  }
  // Coalesce one-off redraws (hover, pan, zoom, selection) into a single rAF so
  // several events in one frame don't each paint. A no-op while the sim loop is
  // already running (it paints every frame).
  let drawPending = false;
  function requestDraw() {
    if (raf || drawPending) return;
    drawPending = true;
    requestAnimationFrame(() => { drawPending = false; draw(); });
  }

  // ── Rendering ──────────────────────────────────────────────────────────────
  const searchTerm = () => ($("search").value || "").trim().toLowerCase();
  const matches = (n, q) => q && (n.id.toLowerCase().includes(q) || n.type.toLowerCase().includes(q));
  function neighborsOf(node) {
    const s = new Set();
    for (const l of links) { if (l.source === node) s.add(l.target); else if (l.target === node) s.add(l.source); }
    return s;
  }
  function edgeControl(l) {
    const a = l.source, b = l.target;
    const mx = (a.x + b.x) / 2, my = (a.y + b.y) / 2;
    if (!l._slot) return { x: mx, y: my };
    const dx = b.x - a.x, dy = b.y - a.y, d = Math.hypot(dx, dy) || 1;
    const off = l._slot * 34;
    return { x: mx + (-dy / d) * off, y: my + (dx / d) * off };
  }

  function draw() {
    ctx.save();
    ctx.scale(dpr, dpr);
    const W = cv.width / dpr, H = cv.height / dpr;
    ctx.clearRect(0, 0, W, H);

    const q = searchTerm();
    const focus = selected || hover;
    const nbrs = focus ? neighborsOf(focus) : null;
    const showRelLabels = view.k > 0.75;
    // Viewport-cull margin — generous enough to cover node radius, curved edges
    // and relationship pills that bow just outside the strict box.
    const M = 80;

    for (const l of links) {
      const a = toScreen(l.source.x, l.source.y), b = toScreen(l.target.x, l.target.y);
      // Skip edges wholly off one side of the viewport (both endpoints outside).
      if ((a.x < -M && b.x < -M) || (a.x > W + M && b.x > W + M) ||
          (a.y < -M && b.y < -M) || (a.y > H + M && b.y > H + M)) continue;
      const ec = edgeControl(l), c = toScreen(ec.x, ec.y);
      const active = focus && (l.source === focus || l.target === focus);
      ctx.lineWidth = active ? 2 : 1.2;
      ctx.strokeStyle = active ? "rgba(1,139,255,.85)" : (focus ? "rgba(150,158,168,.22)" : "rgba(150,158,168,.55)");
      ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.quadraticCurveTo(c.x, c.y, b.x, b.y); ctx.stroke();
      drawArrow(c, b, radius(l.target) * view.k, active);
      if (l.type && (active || showRelLabels)) {
        const mx = 0.25 * a.x + 0.5 * c.x + 0.25 * b.x, my = 0.25 * a.y + 0.5 * c.y + 0.25 * b.y;
        drawRelLabel(mx, my, l.type, active);
      }
    }

    ctx.textAlign = "center"; ctx.textBaseline = "middle";
    for (const n of nodes) {
      const s = toScreen(n.x, n.y), r = radius(n) * view.k;
      if (s.x < -M || s.x > W + M || s.y < -M || s.y > H + M) continue; // off-screen
      const dim = focus && n !== focus && !(nbrs && nbrs.has(n));
      ctx.globalAlpha = dim ? 0.3 : 1;
      ctx.beginPath(); ctx.arc(s.x, s.y, r, 0, Math.PI * 2);
      ctx.fillStyle = n.color; ctx.fill();
      if (n === selected) { ctx.lineWidth = 3; ctx.strokeStyle = "rgba(1,139,255,.9)"; ctx.stroke(); }
      else if (q && matches(n, q)) { ctx.lineWidth = 3; ctx.strokeStyle = "#f0a020"; ctx.stroke(); }
      else { ctx.lineWidth = 1.5; ctx.strokeStyle = "rgba(0,0,0,.12)"; ctx.stroke(); }
      if (n.expanded) { ctx.lineWidth = 1.5; ctx.strokeStyle = "rgba(0,0,0,.28)"; ctx.beginPath(); ctx.arc(s.x, s.y, r + 3, 0, Math.PI * 2); ctx.stroke(); }
      if (n._loading) drawSpinner(s.x, s.y, r + 7);
      if (!dim && r > 13) {
        ctx.globalAlpha = 1;
        ctx.fillStyle = "#2a2c34";
        ctx.font = `${Math.max(9, Math.min(13, r * 0.5))}px -apple-system, system-ui, sans-serif`;
        // Cache the fitted (truncated) label per integer screen-radius. fit()'s
        // measureText is a canvas hotspot; the screen radius is constant frame to
        // frame while the layout settles (it only changes on zoom or degree),
        // so this reduces measureText from every-node-every-frame to near-zero.
        const rb = r | 0;
        if (n._lblR !== rb) { n._lbl = fit(ctx, n.id, r * 1.8); n._lblR = rb; }
        ctx.fillText(n._lbl, s.x, s.y);
      }
    }
    ctx.globalAlpha = 1;
    ctx.restore();
  }

  function fit(c, text, maxW) {
    if (c.measureText(text).width <= maxW) return text;
    let lo = 0, hi = text.length;
    while (lo < hi) { const mid = (lo + hi + 1) >> 1; if (c.measureText(text.slice(0, mid) + "…").width <= maxW) lo = mid; else hi = mid - 1; }
    return lo > 0 ? text.slice(0, lo) + "…" : "";
  }
  function drawArrow(from, to, targetR, active) {
    const dx = to.x - from.x, dy = to.y - from.y, d = Math.hypot(dx, dy) || 1;
    const ux = dx / d, uy = dy / d;
    const tipX = to.x - ux * (targetR + 1.5), tipY = to.y - uy * (targetR + 1.5), sz = active ? 9 : 7;
    ctx.fillStyle = active ? "rgba(1,139,255,.85)" : "rgba(150,158,168,.75)";
    ctx.beginPath();
    ctx.moveTo(tipX, tipY);
    ctx.lineTo(tipX - ux * sz - uy * sz * 0.5, tipY - uy * sz + ux * sz * 0.5);
    ctx.lineTo(tipX - ux * sz + uy * sz * 0.5, tipY - uy * sz - ux * sz * 0.5);
    ctx.closePath(); ctx.fill();
  }
  function drawRelLabel(x, y, text, active) {
    ctx.font = "10px -apple-system, system-ui, sans-serif";
    ctx.textAlign = "center"; ctx.textBaseline = "middle";
    const w = ctx.measureText(text).width + 10;
    ctx.fillStyle = active ? "rgba(1,139,255,.95)" : "rgba(255,255,255,.9)";
    roundRect(x - w / 2, y - 8, w, 16, 8); ctx.fill();
    if (!active) { ctx.strokeStyle = "rgba(150,158,168,.5)"; ctx.lineWidth = 1; roundRect(x - w / 2, y - 8, w, 16, 8); ctx.stroke(); }
    ctx.fillStyle = active ? "#fff" : "#5a616e";
    ctx.fillText(text, x, y);
  }
  function roundRect(x, y, w, h, r) {
    ctx.beginPath();
    ctx.moveTo(x + r, y);
    ctx.arcTo(x + w, y, x + w, y + h, r);
    ctx.arcTo(x + w, y + h, x, y + h, r);
    ctx.arcTo(x, y + h, x, y, r);
    ctx.arcTo(x, y, x + w, y, r);
    ctx.closePath();
  }
  let spinPhase = 0;
  function drawSpinner(x, y, r) {
    spinPhase += 0.3;
    ctx.strokeStyle = "rgba(1,139,255,.9)"; ctx.lineWidth = 2.5;
    ctx.beginPath(); ctx.arc(x, y, r, spinPhase, spinPhase + Math.PI * 1.4); ctx.stroke();
  }

  // ── Hit testing & interaction ──────────────────────────────────────────────
  function nodeAt(sx, sy) {
    for (let i = nodes.length - 1; i >= 0; i--) {
      const n = nodes[i], s = toScreen(n.x, n.y), r = radius(n) * view.k + 2;
      if ((sx - s.x) ** 2 + (sy - s.y) ** 2 <= r * r) return n;
    }
    return null;
  }
  let dragging = false, dragMoved = false, last = { x: 0, y: 0 };
  cv.addEventListener("mousedown", (e) => {
    const rect = cv.getBoundingClientRect();
    const n = nodeAt(e.clientX - rect.left, e.clientY - rect.top);
    dragging = true; dragMoved = false; last = { x: e.clientX, y: e.clientY };
    pinnedDrag = n || null; cv.classList.add("grabbing");
  });
  window.addEventListener("mousemove", (e) => {
    const rect = cv.getBoundingClientRect();
    const sx = e.clientX - rect.left, sy = e.clientY - rect.top;
    if (dragging) {
      const dx = e.clientX - last.x, dy = e.clientY - last.y;
      if (Math.abs(dx) + Math.abs(dy) > 2) dragMoved = true;
      last = { x: e.clientX, y: e.clientY };
      if (pinnedDrag) {
        const w = toWorld(sx, sy);
        pinnedDrag.x = w.x; pinnedDrag.y = w.y; pinnedDrag.vx = 0; pinnedDrag.vy = 0;
        if (!raf) loop(); // the sim loop paints each frame while a node is pinned
      } else { view.x += dx / view.k; view.y += dy / view.k; requestDraw(); }
      return;
    }
    const n = nodeAt(sx, sy);
    if (n !== hover) { hover = n; requestDraw(); }
    const tt = $("tooltip");
    if (n) {
      const oc = n.obsCount | 0;
      tt.style.display = "block";
      tt.style.left = Math.min(sx + 14, rect.width - 300) + "px";
      tt.style.top = (sy + 16) + "px";
      const o = oc ? `<div class="tt-sub">${fmt(oc)} observation${oc > 1 ? "s" : ""} · double-click to expand</div>` : `<div class="tt-sub">double-click to expand</div>`;
      tt.innerHTML = `<div class="tt-name">${escapeHtml(n.id)}</div><div class="tt-sub">${escapeHtml(n.type || "—")} · degree ${n.deg}</div>${o}`;
      cv.style.cursor = "pointer";
    } else { tt.style.display = "none"; cv.style.cursor = dragging ? "grabbing" : "grab"; }
  });
  window.addEventListener("mouseup", () => {
    if (dragging && pinnedDrag && !dragMoved) selectNode(pinnedDrag);
    else if (dragging && pinnedDrag && dragMoved) pinnedDrag.fixed = true;
    else if (dragging && !pinnedDrag && !dragMoved) selectNode(null);
    dragging = false; pinnedDrag = null; cv.classList.remove("grabbing");
  });
  cv.addEventListener("dblclick", (e) => {
    const rect = cv.getBoundingClientRect();
    const n = nodeAt(e.clientX - rect.left, e.clientY - rect.top);
    if (n) { expand(n); selectNode(n); }
  });
  cv.addEventListener("wheel", (e) => {
    e.preventDefault();
    const rect = cv.getBoundingClientRect();
    zoomAt(e.clientX - rect.left, e.clientY - rect.top, Math.exp(-e.deltaY * 0.0015));
  }, { passive: false });

  function zoomAt(sx, sy, factor) {
    const before = toWorld(sx, sy);
    view.k = Math.min(4, Math.max(0.05, view.k * factor));
    const after = toWorld(sx, sy);
    view.x += after.x - before.x; view.y += after.y - before.y;
    requestDraw();
  }
  const zoomCenter = (f) => zoomAt(cv.width / dpr / 2, cv.height / dpr / 2, f);

  // ── Inspector ──────────────────────────────────────────────────────────────
  const attachmentRequests = new Set();
  let attachmentGeneration = 0;
  let attachmentNode = null;
  let attachmentConsentGranted = false;
  let attachmentRows = [];
  let attachmentListRequest = null;
  let attachmentPageRequest = null;
  let attachmentPoll = null;
  let attachmentViewer = null;

  function attachmentContext() {
    return {
      node: selected, workspaceId: workspace.id,
      workspaceGeneration: workspace.generation, generation: attachmentGeneration,
    };
  }
  function currentAttachment(ctx) {
    return ctx && selected === ctx.node && workspace.id === ctx.workspaceId
      && isCurrent(ctx.workspaceGeneration) && attachmentGeneration === ctx.generation;
  }
  function attachmentRequest() {
    const controller = beginRequest();
    attachmentRequests.add(controller);
    return controller;
  }
  function finishAttachmentRequest(controller) {
    attachmentRequests.delete(controller);
    activeRequests.delete(controller);
  }
  function attachmentUrl(ctx, id, suffix = "") {
    return "/ui/attachments" + (id == null ? "" : "/" + encodeURIComponent(id) + suffix)
      + "?" + new URLSearchParams({ workspaceId: ctx.workspaceId });
  }
  function attachmentFetch(path, options = {}) {
    const headers = { ...(options.headers || {}) };
    if (token) headers.Authorization = "Bearer " + token;
    return fetch(path, { ...options, headers });
  }
  const canWriteAttachments = () => workspace.role === "owner" || workspace.role === "writer";
  function renderAttachmentPermissions() {
    $("attEnable").hidden = attachmentConsentGranted;
    $("attControls").hidden = !attachmentConsentGranted || !canWriteAttachments();
    $("attConsentNote").hidden = attachmentConsentGranted && canWriteAttachments();
    $("attConsentNote").textContent = attachmentConsentGranted
      ? "This workspace is read-only. You can view and download attachments."
      : "Attachment access needs separate consent.";
  }
  function attachmentMessage(text) {
    $("attMessage").textContent = text || "";
    $("attMessage").hidden = !text;
  }
  function cancelAttachmentPanel() {
    attachmentGeneration++;
    clearTimeout(attachmentPoll);
    attachmentPoll = null;
    for (const controller of attachmentRequests) controller.abort();
    attachmentRequests.clear();
    attachmentListRequest = null;
    attachmentPageRequest = null;
    attachmentNode = null;
    attachmentRows = [];
    attachmentViewer = null;
    $("attList").textContent = "";
    $("attViewer").hidden = true;
    $("attFile").value = "";
    $("attUpload").disabled = false;
    attachmentMessage("");
  }
  async function attachmentFailure(res, ctx, promptForConsent = false) {
    const challenge = res.headers.get("WWW-Authenticate") || "";
    const needsScope = res.status === 403 && challenge.includes('scope="attachments"');
    if (promptForConsent && (needsScope || res.status === 401) && oauthAdvertised(res)) {
      await beginOAuth(ctx.workspaceGeneration, () => currentAttachment(ctx),
        "graph-read attachments", "attachments");
      return;
    }
    const detail = await res.text().catch(() => "");
    if (!currentAttachment(ctx)) return;
    if (needsScope) {
      attachmentConsentGranted = false;
      attachmentRows = [];
      renderAttachmentPermissions();
      renderAttachmentRows(ctx);
      attachmentMessage("The attachments scope is required. Select Enable attachments to request consent.");
    } else if (res.status === 403) {
      attachmentMessage("Attachment access is denied in this workspace. " + detail);
    } else if (res.status === 401) {
      attachmentMessage("Sign in with an attachments-enabled bearer token.");
    } else if (res.status === 409) {
      attachmentMessage("This entity already has a file with that name. " + detail);
    } else {
      attachmentMessage("Attachment request failed (" + res.status + "). " + detail);
    }
  }
  function scheduleAttachmentPoll(ctx) {
    clearTimeout(attachmentPoll);
    attachmentPoll = null;
    if (!attachmentRows.some((row) => row.status === "uploaded" || row.status === "extracting")) return;
    attachmentPoll = setTimeout(() => {
      attachmentPoll = null;
      if (currentAttachment(ctx)) loadAttachmentList(ctx);
    }, 1500);
  }
  async function loadAttachmentList(ctx, promptForConsent = false) {
    if (!currentAttachment(ctx)) return;
    clearTimeout(attachmentPoll);
    attachmentPoll = null;
    if (attachmentListRequest) attachmentListRequest.abort();
    const controller = attachmentRequest();
    attachmentListRequest = controller;
    const current = () => currentAttachment(ctx) && attachmentListRequest === controller && !controller.signal.aborted;
    try {
      const params = new URLSearchParams({ workspaceId: ctx.workspaceId, entityName: ctx.node.id });
      const res = await attachmentFetch("/ui/attachments?" + params, { signal: controller.signal });
      if (!current()) return;
      if (!res.ok) {
        await attachmentFailure(res, ctx, promptForConsent);
        return;
      }
      const data = await res.json();
      if (!current()) return;
      attachmentConsentGranted = true;
      renderAttachmentPermissions();
      attachmentRows = data.attachments;
      renderAttachmentRows(ctx);
    } catch (e) {
      if (current() && e.name !== "AbortError") attachmentMessage("Attachment list failed: " + e.message);
    } finally {
      finishAttachmentRequest(controller);
      if (attachmentListRequest === controller) {
        attachmentListRequest = null;
        if (currentAttachment(ctx)) scheduleAttachmentPoll(ctx);
      }
    }
  }
  function requestAttachmentConsent(promptForConsent = true) {
    if (!selected || !workspace.id) return;
    attachmentNode = selected;
    $("attList").textContent = "Loading attachments…";
    loadAttachmentList(attachmentContext(), promptForConsent);
  }
  function attachmentButton(label, action) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = label;
    button.addEventListener("click", action);
    return button;
  }
  function renderAttachmentRows(ctx) {
    const list = $("attList");
    list.textContent = "";
    if (!attachmentRows.length) {
      list.textContent = "No attachments.";
      attachmentViewer = null;
      renderAttachmentViewer();
      return;
    }
    if (attachmentViewer && !attachmentRows.some((row) => row.attachmentId === attachmentViewer.id && row.status === "ready")) {
      attachmentViewer = null;
      renderAttachmentViewer();
    }
    for (const row of attachmentRows) {
      const item = document.createElement("div");
      item.className = "attach-row";
      const name = document.createElement("div");
      name.className = "attach-name";
      name.textContent = row.filename;
      const details = document.createElement("div");
      details.className = "attach-details";
      const status = document.createElement("span");
      status.className = "attach-badge" + (row.status === "ready" ? " ready" : row.status === "error" ? " error" : "");
      status.textContent = row.status;
      details.append(status);
      if (row.errorStage || row.lastError) {
        const error = document.createElement("span");
        error.className = "attach-badge error";
        error.textContent = [row.errorStage, row.lastError].filter(Boolean).join(": ");
        details.append(error);
      }
      const meta = document.createElement("span");
      meta.className = "meta";
      meta.textContent = `${fmt(row.sizeBytes)} bytes · ${fmt(row.pageCount)} pages`;
      details.append(meta);
      const actions = document.createElement("div");
      actions.className = "attach-row-actions";
      if (row.status === "ready" && row.pageCount > 0) {
        actions.append(attachmentButton("Read pages", () => openAttachmentViewer(ctx, row)));
      }
      actions.append(attachmentButton("Download", () => downloadAttachment(ctx, row)));
      if (canWriteAttachments()) {
        actions.append(attachmentButton("Delete", () => deleteAttachment(ctx, row)));
      }
      item.append(name, details, actions);
      list.append(item);
    }
  }
  async function uploadAttachment(ctx) {
    if (!currentAttachment(ctx)) return;
    if (!canWriteAttachments()) { attachmentMessage("This workspace is read-only. Attachment uploads need writer or owner access."); return; }
    const input = $("attFile");
    const file = input.files && input.files[0];
    if (!file) { attachmentMessage("Choose a file to upload."); return; }
    attachmentMessage("");
    const controller = attachmentRequest();
    const originalGeneration = attachmentGeneration;
    $("attUpload").disabled = true;
    try {
      const params = new URLSearchParams({ workspaceId: ctx.workspaceId, entityName: ctx.node.id, filename: file.name });
      const res = await attachmentFetch("/ui/attachments?" + params, {
        method: "POST", headers: { "Content-Type": file.type || "application/octet-stream" },
        body: file, signal: controller.signal,
      });
      if (!res.ok) {
        await attachmentFailure(res, ctx, true);
        return;
      }
      const uploaded = await res.json();
      if (!currentAttachment(ctx) || originalGeneration !== attachmentGeneration) return;
      input.value = "";
      attachmentMessage("Upload started for " + uploaded.filename + ".");
      await loadAttachmentList(ctx);
    } catch (e) {
      if (currentAttachment(ctx) && e.name !== "AbortError") attachmentMessage("Upload failed: " + e.message);
    } finally {
      finishAttachmentRequest(controller);
      $("attUpload").disabled = false;
    }
  }
  async function downloadAttachment(ctx, row) {
    if (!currentAttachment(ctx)) return;
    const controller = attachmentRequest();
    try {
      const res = await attachmentFetch(attachmentUrl(ctx, row.attachmentId, "/download"), { signal: controller.signal });
      if (!currentAttachment(ctx)) return;
      if (!res.ok) {
        await attachmentFailure(res, ctx, true);
        return;
      }
      const blob = await res.blob();
      if (!currentAttachment(ctx)) return;
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = row.filename;
      document.body.append(anchor);
      anchor.click();
      anchor.remove();
      setTimeout(() => URL.revokeObjectURL(url), 0);
    } catch (e) {
      if (currentAttachment(ctx) && e.name !== "AbortError") attachmentMessage("Download failed: " + e.message);
    } finally {
      finishAttachmentRequest(controller);
    }
  }
  async function deleteAttachment(ctx, row) {
    if (!currentAttachment(ctx)) return;
    if (!canWriteAttachments()) { attachmentMessage("This workspace is read-only. Attachment deletes need writer or owner access."); return; }
    if (!window.confirm("Delete " + row.filename + " from this entity?")) return;
    attachmentMessage("");
    const controller = attachmentRequest();
    try {
      const res = await attachmentFetch(attachmentUrl(ctx, row.attachmentId), {
        method: "DELETE", signal: controller.signal,
      });
      if (!res.ok) {
        await attachmentFailure(res, ctx, true);
        return;
      }
      if (currentAttachment(ctx)) await loadAttachmentList(ctx);
    } catch (e) {
      if (currentAttachment(ctx) && e.name !== "AbortError") attachmentMessage("Delete failed: " + e.message);
    } finally {
      finishAttachmentRequest(controller);
    }
  }
  function openAttachmentViewer(ctx, row) {
    if (!currentAttachment(ctx)) return;
    attachmentViewer = { id: row.attachmentId, filename: row.filename, page: 1, offset: 0, accum: "" };
    loadAttachmentPage(ctx);
  }
  async function loadAttachmentPage(ctx) {
    if (!currentAttachment(ctx) || !attachmentViewer) return;
    if (attachmentPageRequest) attachmentPageRequest.abort();
    const controller = attachmentRequest();
    attachmentPageRequest = controller;
    const viewer = attachmentViewer;
    $("attViewName").textContent = viewer.filename;
    $("attPageLabel").textContent = "Page " + viewer.page + "…";
    if (!viewer.accum) $("attPageText").textContent = "Loading…";
    $("attPrevPage").disabled = viewer.page <= 1;
    $("attNextPage").disabled = true;
    $("attMoreText").hidden = true;
    $("attViewer").hidden = false;
    try {
      const params = new URLSearchParams({ workspaceId: ctx.workspaceId, page: String(viewer.page), offset: String(viewer.offset), maxChars: "4096" });
      const res = await attachmentFetch("/ui/attachments/" + encodeURIComponent(viewer.id) + "/pages?" + params, {
        signal: controller.signal,
      });
      if (!currentAttachment(ctx) || attachmentViewer !== viewer) return;
      if (!res.ok) {
        attachmentPageRequest = null;
        finishAttachmentRequest(controller);
        await attachmentFailure(res, ctx, true);
        renderAttachmentViewer();
        return;
      }
      const data = await res.json();
      if (!currentAttachment(ctx) || attachmentViewer !== viewer) return;
      const accumulated = (viewer.offset === 0 ? "" : viewer.accum + "\n") + data.text;
      $("attPageText").textContent = data.eof ? accumulated : accumulated + "\n…";
      $("attPageLabel").textContent = "Page " + data.page;
      $("attNextPage").disabled = data.eof;
      if (!data.eof && data.nextOffset > data.offset) $("attMoreText").hidden = false;
      attachmentViewer = { ...viewer, offset: data.offset, nextOffset: data.nextOffset, accum: accumulated };
    } catch (e) {
      if (currentAttachment(ctx) && attachmentViewer === viewer && e.name !== "AbortError") {
        attachmentMessage("Page read failed: " + e.message);
        renderAttachmentViewer();
      }
      attachmentPageRequest = null;
      finishAttachmentRequest(controller);
    }
  }
  function renderAttachmentViewer() {
    const hasViewer = attachmentViewer && attachmentRows.some((row) => row.attachmentId === attachmentViewer.id && row.status === "ready");
    if (!hasViewer) {
      attachmentViewer = null;
      $("attViewer").hidden = true;
      return;
    }
    $("attPageText").textContent = attachmentViewer.offset ? "Page continues…" : "Loading…";
  }
  function selectNode(n) {
    if (selected !== n) cancelAttachmentPanel();
    selected = n;
    if (n && attachmentConsentGranted && attachmentNode !== n) {
      attachmentNode = n;
      loadAttachmentList(attachmentContext());
    }
    const ins = $("inspector");
    if (!n) { ins.classList.remove("show"); $("zoom").classList.remove("shift"); requestDraw(); return; }
    $("insName").textContent = n.id;
    $("insDot").style.background = n.color;
    const rels = [];
    for (const l of links) {
      if (l.source === n) rels.push({ dir: "out", other: l.target.id, type: l.type });
      else if (l.target === n) rels.push({ dir: "in", other: l.source.id, type: l.type });
    }
    const relHtml = rels.length ? rels.map((r) =>
      r.dir === "out"
        ? `<div class="rel"><span class="rt">${escapeHtml(r.type)}</span><span class="arrow">→</span><a data-goto="${escapeHtml(r.other)}">${escapeHtml(r.other)}</a></div>`
        : `<div class="rel dir-in"><a data-goto="${escapeHtml(r.other)}">${escapeHtml(r.other)}</a><span class="rt">${escapeHtml(r.type)}</span><span class="arrow">→</span></div>`
    ).join("") : `<div class="meta">No relations loaded — double-click to expand.</div>`;
    // Observation bodies are lazy-loaded (list payloads carry only obsCount).
    const oc = n.obsCount | 0;
    let obsHtml;
    if (n.obs === null && oc > 0) {
      obsHtml = `<div class="meta">Loading ${fmt(oc)} observation${oc === 1 ? "" : "s"}…</div>`;
      loadObservations(n);
    } else {
      const obs = n.obs || [];
      obsHtml = obs.length ? obs.map((o) => `<div class="obs">${escapeHtml(o.body)}</div>`).join("") : `<div class="meta">No observations.</div>`;
    }
    $("insBody").innerHTML =
      `<span class="pill" style="background:${n.color}">${escapeHtml(n.type || "—")}</span>` +
      `<div class="meta">degree ${n.deg} · ${fmt(oc)} observation${oc === 1 ? "" : "s"}</div>` +
      `<div class="sec">Observations</div>${obsHtml}` +
      `<div class="sec">Relationships (${rels.length})</div>${relHtml}`;
    ins.classList.add("show"); $("zoom").classList.add("shift");
    $("insBody").querySelectorAll("[data-goto]").forEach((a) =>
      a.addEventListener("click", () => { const t = nodeById.get(a.getAttribute("data-goto")); if (t) { centerOn(t); selectNode(t); } }));
    requestDraw();
  }
  // Lazy-fetch observation bodies for the inspected node; re-render if it's still
  // selected when they arrive. On failure, mark as loaded-empty so we don't retry.
  async function loadObservations(n) {
    if (!workspace.id) return;
    const generation = workspace.generation;
    const controller = beginRequest();
    const params = new URLSearchParams({ workspaceId: workspace.id, name: n.id });
    try {
      const res = await api("/ui/node?" + params, controller.signal);
      if (!isCurrent(generation)) return;
      if (res.status === 404 && isJsonError(res)) { rejectWorkspace(generation); return; }
      if (res.status === 404) { n.obs = []; n.obsCount = 0; /* entity vanished; keep the session */ }
      else if (!res.ok) {
        if (res.status === 401 || res.status === 403) await handleError(res, generation);
        if (!isCurrent(generation)) return;
        n.obs = [];
      } else {
        const data = await res.json();
        if (!isCurrent(generation)) return;
        n.obs = data.observations || [];
        n.obsCount = n.obs.length;
      }
    } catch (e) {
      if (!isCurrent(generation) || e.name === "AbortError") return;
      n.obs = [];
    } finally {
      activeRequests.delete(controller);
    }
    if (isCurrent(generation) && selected === n) selectNode(n);
  }
  function centerOn(n) {
    const W = cv.width / dpr, H = cv.height / dpr;
    view.x = W / (2 * view.k) - n.x; view.y = H / (2 * view.k) - n.y;
    requestDraw();
  }
  function fitView(reset) {
    if (!nodes.length) { view.x = 0; view.y = 0; view.k = 1; requestDraw(); return; }
    // A short Barnes-Hut warmup relaxes the (already spiral-seeded) layout enough
    // to fit sensibly, without the old 80-iteration synchronous O(n²) pre-roll
    // that froze the main thread on every load. The rAF loop finishes settling.
    if (reset) {
      const a = alpha; alpha = 1;
      const iters = nodes.length > 400 ? 12 : 30;
      for (let i = 0; i < iters; i++) step();
      alpha = a;
    }
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (const n of nodes) { minX = Math.min(minX, n.x); minY = Math.min(minY, n.y); maxX = Math.max(maxX, n.x); maxY = Math.max(maxY, n.y); }
    const W = cv.width / dpr, H = cv.height / dpr, pad = 110;
    const gw = Math.max(1, maxX - minX), gh = Math.max(1, maxY - minY);
    view.k = Math.min(2.2, Math.max(0.05, Math.min((W - pad) / gw, (H - pad) / gh)));
    view.x = W / (2 * view.k) - (minX + maxX) / 2;
    view.y = H / (2 * view.k) - (minY + maxY) / 2;
    requestDraw();
  }
  function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  }

  // ── Controls ────────────────────────────────────────────────────────────────
  $("workspace").addEventListener("change", () => selectWorkspace($("workspace").value || null));
  $("searchBtn").addEventListener("click", runSearch);
  $("overview").addEventListener("click", showOverview);
  $("typeFilter").addEventListener("change", () => { browse.entityType = $("typeFilter").value; browse.offset = 0; load(); });
  $("limit").addEventListener("change", () => { const v = parseInt($("limit").value, 10); if (v > 0) { browse.limit = Math.min(1000, v); browse.offset = 0; load(); } });
  $("search").addEventListener("input", requestDraw); // live-highlight loaded nodes as you type
  $("search").addEventListener("keydown", (e) => { if (e.key === "Enter") runSearch(); });
  $("prev").addEventListener("click", () => gotoPage(-1));
  $("next").addEventListener("click", () => gotoPage(1));
  $("zoomIn").addEventListener("click", () => zoomCenter(1.3));
  $("zoomOut").addEventListener("click", () => zoomCenter(1 / 1.3));
  $("zoomFit").addEventListener("click", () => fitView(false));
  $("insClose").addEventListener("click", () => selectNode(null));
  $("insExpand").addEventListener("click", () => selected && expand(selected));
  $("insIsolate").addEventListener("click", () => selected && isolate(selected));
  $("insDismiss").addEventListener("click", () => selected && dismiss(selected));
  $("attEnable").addEventListener("click", () => requestAttachmentConsent(true));
  $("attUpload").addEventListener("click", () => {
    const ctx = attachmentContext();
    if (!currentAttachment(ctx)) return;
    if (attachmentViewer) { attachmentViewer = null; $("attViewer").hidden = true; }
    uploadAttachment(ctx);
  });
  $("attViewClose").addEventListener("click", () => {
    const ctx = attachmentContext();
    if (!currentAttachment(ctx)) return;
    attachmentViewer = null;
    $("attViewer").hidden = true;
  });
  const attachmentPageStep = (delta) => {
    const ctx = attachmentContext();
    if (!currentAttachment(ctx) || !attachmentViewer) return;
    if (delta < 0 && attachmentViewer.page <= 1) return;
    attachmentViewer = { ...attachmentViewer, page: attachmentViewer.page + delta, offset: 0, accum: "" };
    loadAttachmentPage(ctx);
  };
  $("attPrevPage").addEventListener("click", () => attachmentPageStep(-1));
  $("attNextPage").addEventListener("click", () => attachmentPageStep(1));
  $("attMoreText").addEventListener("click", () => {
    const ctx = attachmentContext();
    if (!currentAttachment(ctx) || !attachmentViewer || typeof attachmentViewer.nextOffset !== "number") return;
    attachmentViewer = { ...attachmentViewer, offset: attachmentViewer.nextOffset };
    loadAttachmentPage(ctx);
  });
  $("ovGo").addEventListener("click", () => {
    token = $("ovToken").value.trim();
    if (!token) return;
    sessionStorage.setItem("mcpmem_token", token);
    sessionStorage.removeItem(OAUTH_TOKEN_KEY);
    selectWorkspace(null);
    loadWorkspaces(true);
  });
  $("ovToken").addEventListener("keydown", (e) => { if (e.key === "Enter") $("ovGo").click(); });
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { if (selected) selectNode(null); }
  });

  resize();
  (async function boot() {
    const callback = new URLSearchParams(location.search);
    if (callback.has("code")) {
      // Exchange the provider's code before the first workspace request.
      if (!await completeOAuth()) return;
      if (callback.get("state") === "attachments") {
        try {
          pendingInspector = JSON.parse(sessionStorage.getItem(OAUTH_RETURN_KEY));
        } catch {
          pendingInspector = null;
        }
        sessionStorage.removeItem(OAUTH_RETURN_KEY);
      }
    }
    setBusy(false);
    loadWorkspaces(true);
  })();
})();
