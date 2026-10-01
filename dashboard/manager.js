"use strict";

// Elorin Buy Manager — the controller for the scanner's buy rules.
//
// Private: the page shows nothing but a sign-in card until authenticated, and
// the rules it edits are unreadable anonymously (RLS grants select on
// `watchlist` / `buy_settings` to authenticated users only).

const CFG = Object.assign({
  supabaseUrl: "",
  anonKey: "",
  iconsBucket: "icons",
  feeRate: 0.10,
}, window.ELORIN_CONFIG || {});

const AUTH_KEY = "elorin_manager_auth";
const PRESETS_KEY = "elorin_manager_presets";

const WINDOWS = {
  day:   { label: "Day",   ms: 24 * 3600e3,  bucket: "hour" },
  week:  { label: "Week",  ms: 7 * 86400e3,  bucket: "day" },
  month: { label: "Month", ms: 30 * 86400e3, bucket: "day" },
};

const PAGE = 150; // rows rendered at once (580 items on a phone)

const GOLD = "#ffcf5c", GREEN = "#6ee07a", RED = "#ff6b5e", BLUE = "#6fc3ff";

const state = {
  win: "month",
  search: "",
  filters: { on: false, profit: false, snipe: false },
  sort: "n",
  shown: PAGE,
  items: [],
  rules: new Map(),
  settings: null,
  auth: null,
  expanded: new Set(),
  trends: new Map(),
  timers: new Map(),
  savedAt: 0,
  since: 0,
};

const $ = id => document.getElementById(id);

function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, c =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function fmt(n) {
  if (n == null || isNaN(n)) return "–";
  return Number(n).toLocaleString("en-US");
}

function fmtCompact(n) {
  n = Number(n) || 0;
  const a = Math.abs(n);
  if (a >= 1e12) return (n / 1e12).toFixed(2) + "t";
  if (a >= 1e9) return (n / 1e9).toFixed(2) + "b";
  if (a >= 1e6) return (n / 1e6).toFixed(2) + "m";
  if (a >= 1e3) return (n / 1e3).toFixed(1) + "k";
  return String(Math.round(n));
}

function fmtSigned(n) {
  if (n == null || isNaN(n)) return "–";
  return (n >= 0 ? "+" : "−") + fmtCompact(Math.abs(n));
}

function timeAgo(t) {
  if (!t) return "–";
  const s = Math.max(0, (Date.now() - t) / 1000);
  if (s < 60) return Math.floor(s) + "s ago";
  if (s < 3600) return Math.floor(s / 60) + "m ago";
  if (s < 86400) return Math.floor(s / 3600) + "h ago";
  return Math.floor(s / 86400) + "d ago";
}

function shortDate(t) {
  return new Date(t).toLocaleDateString([], { month: "short", day: "numeric" });
}

function shortHour(t) {
  return new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

/// Accepts "250000000", "250m", "1.5b", "20k", "2,000,000".
function parseMoney(s) {
  if (s == null) return null;
  const t = String(s).replace(/[\s,_]/g, "").toLowerCase();
  if (!t) return null;
  const m = t.match(/^(\d+(?:\.\d+)?)([kmbt])?$/);
  if (!m) return null;
  const mult = { k: 1e3, m: 1e6, b: 1e9, t: 1e12 }[m[2]] || 1;
  const v = Math.round(Number(m[1]) * mult);
  return Number.isFinite(v) && v >= 0 ? v : null;
}

function iconUrl(file) {
  if (!file) return "";
  return `${CFG.supabaseUrl}/storage/v1/object/public/${CFG.iconsBucket}/${encodeURIComponent(file)}`;
}

// ── Auth ──────────────────────────────────────────────────────────────────
function loadAuth() {
  try {
    state.auth = JSON.parse(localStorage.getItem(AUTH_KEY) || "null");
  } catch {
    state.auth = null;
  }
}

function saveAuth(a) {
  state.auth = a;
  if (a) localStorage.setItem(AUTH_KEY, JSON.stringify(a));
  else localStorage.removeItem(AUTH_KEY);
}

async function signIn(email, password) {
  const res = await fetch(`${CFG.supabaseUrl}/auth/v1/token?grant_type=password`, {
    method: "POST",
    headers: { apikey: CFG.anonKey, "Content-Type": "application/json" },
    body: JSON.stringify({ email, password }),
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.msg || data.error_description || `sign in failed (${res.status})`);
  saveAuth({
    access_token: data.access_token,
    refresh_token: data.refresh_token,
    email: (data.user && data.user.email) || email,
    expires_at: Date.now() + (Number(data.expires_in) || 3600) * 1000,
  });
}

async function refreshAuth() {
  if (!state.auth || !state.auth.refresh_token) return false;
  try {
    const res = await fetch(`${CFG.supabaseUrl}/auth/v1/token?grant_type=refresh_token`, {
      method: "POST",
      headers: { apikey: CFG.anonKey, "Content-Type": "application/json" },
      body: JSON.stringify({ refresh_token: state.auth.refresh_token }),
    });
    if (!res.ok) throw new Error("refresh failed");
    const data = await res.json();
    saveAuth({
      access_token: data.access_token,
      refresh_token: data.refresh_token,
      email: (data.user && data.user.email) || state.auth.email,
      expires_at: Date.now() + (Number(data.expires_in) || 3600) * 1000,
    });
    return true;
  } catch {
    saveAuth(null);
    return false;
  }
}

async function token(force) {
  if (!state.auth) return null;
  if (force || state.auth.expires_at - Date.now() < 60000) {
    if (!(await refreshAuth())) return null;
  }
  return state.auth.access_token;
}

async function headers(write) {
  const h = { apikey: CFG.anonKey, "Content-Type": "application/json" };
  const t = state.auth ? await token() : null;
  if (t) h.Authorization = `Bearer ${t}`;
  else if (write) throw new Error("sign in first");
  else h.Authorization = `Bearer ${CFG.anonKey}`;
  return h;
}

async function rest(path, opts) {
  const o = opts || {};
  const res = await fetch(`${CFG.supabaseUrl}/rest/v1/${path}`, {
    method: o.method || "GET",
    headers: Object.assign(await headers(!!o.write), o.headers || {}),
    body: o.body,
  });
  if (!res.ok) {
    const txt = await res.text().catch(() => "");
    if (res.status === 401) saveAuth(null);
    throw new Error(`${o.method || "GET"} ${path} → ${res.status} ${txt.slice(0, 140)}`);
  }
  if (res.status === 204) return null;
  const txt = await res.text();
  return txt ? JSON.parse(txt) : null;
}

function rpc(fn, body) {
  return rest(`rpc/${fn}`, { method: "POST", body: JSON.stringify(body || {}) });
}

// ── Rules ─────────────────────────────────────────────────────────────────
function ruleOf(name) {
  const r = state.rules.get(name);
  return {
    name,
    buy: r ? !!r.buy : false,
    max_price: r && r.max_price != null ? Number(r.max_price) : null,
    qty_limit: r && r.qty_limit != null ? Number(r.qty_limit) : 1,
    bought: r && r.bought != null ? Number(r.bought) : 0,
  };
}

/// Effective numbers for one item: what we'd pay, what we'd make.
function derived(it) {
  const r = ruleOf(it.name);
  const minMargin = state.settings ? Number(state.settings.min_margin) : 1e9;
  const net = r.max_price != null ? r.max_price : null;
  const resale = it.median * (1 - CFG.feeRate);
  const profit = net != null ? resale - net : null;
  return {
    rule: r,
    profit,
    pct: net ? (profit / net) * 100 : null,
    snipe: profit != null && profit >= minMargin,
    capped: r.bought >= r.qty_limit,
  };
}

function setStatus(html) {
  $("status").innerHTML = html;
}

function markSaved(name, cls, text) {
  const el = document.querySelector(`tr[data-name="${cssEsc(name)}"] .state`);
  if (el) {
    el.className = "state " + (cls || "");
    el.textContent = text || "";
  }
}

function cssEsc(s) {
  return String(s).replace(/["\\]/g, "\\$&");
}

function debounceSave(name) {
  clearTimeout(state.timers.get(name));
  markSaved(name, "", "saving…");
  state.timers.set(name, setTimeout(() => saveRow(name), 600));
}

async function saveRow(name) {
  const r = ruleOf(name);
  try {
    await rest("watchlist?on_conflict=name", {
      method: "POST",
      write: true,
      headers: { Prefer: "resolution=merge-duplicates,return=minimal" },
      body: JSON.stringify([{
        name: r.name,
        buy: r.buy,
        max_price: r.max_price,
        qty_limit: r.qty_limit,
        bought: r.bought,
        updated_at: new Date().toISOString(),
      }]),
    });
    markSaved(name, "ok", "saved ✓");
    state.savedAt = Date.now();
  } catch (e) {
    markSaved(name, "err", "error");
    setStatus(`<span class="warnbox">${esc(e.message)}</span>`);
  }
}

function setRule(name, patch) {
  const cur = ruleOf(name);
  state.rules.set(name, Object.assign({}, cur, patch));
}

function debounceSettings() {
  clearTimeout(state.timers.get("__settings"));
  state.timers.set("__settings", setTimeout(saveSettings, 600));
}

async function saveSettings() {
  const s = state.settings;
  try {
    await rest("buy_settings?id=eq.1", {
      method: "PATCH",
      write: true,
      headers: { Prefer: "return=minimal" },
      body: JSON.stringify({
        enabled: !!s.enabled,
        min_margin: Number(s.min_margin) || 0,
        max_snipes: Number(s.max_snipes) || 0,
        updated_at: new Date().toISOString(),
      }),
    });
    state.savedAt = Date.now();
    setStatus(statusLine());
  } catch (e) {
    setStatus(`<span class="warnbox">${esc(e.message)}</span>`);
  }
}

function statusLine() {
  const s = state.settings;
  const done = [...state.rules.values()].filter(r => r.buy).length;
  const when = state.savedAt ? ` · saved ${timeAgo(state.savedAt)}` : "";
  return `${state.items.length} items · ${done} enabled · master ${s && s.enabled ? "ON" : "off"}${when}`;
}

// ── Presets ───────────────────────────────────────────────────────────────
function getPresets() {
  try {
    return JSON.parse(localStorage.getItem(PRESETS_KEY) || "{}") || {};
  } catch {
    return {};
  }
}

function setPresets(presets) {
  localStorage.setItem(PRESETS_KEY, JSON.stringify(presets));
}

function currentPreset() {
  return {
    enabled: !!state.settings?.enabled,
    min_margin: state.settings?.min_margin ?? 1e9,
    max_snipes: state.settings?.max_snipes ?? 2,
    rules: [...state.rules.values()].map(r => ({
      name: r.name,
      buy: !!r.buy,
      max_price: r.max_price != null ? Number(r.max_price) : null,
      qty_limit: r.qty_limit != null ? Number(r.qty_limit) : 1,
    })),
  };
}

function renderPresetSelect() {
  const sel = $("presetSelect");
  if (!sel) return;
  const presets = getPresets();
  const current = sel.value;
  sel.innerHTML = "";
  const def = document.createElement("option");
  def.value = "";
  def.textContent = "— choose a preset —";
  sel.appendChild(def);
  Object.keys(presets).forEach(n => {
    const opt = document.createElement("option");
    opt.value = n;
    opt.textContent = n;
    sel.appendChild(opt);
  });
  if (presets[current]) sel.value = current;
}

function showPresetStatus(msg, isErr) {
  const el = $("presetStatus");
  el.textContent = msg;
  el.className = isErr ? "warn" : "hint";
  setTimeout(() => { el.textContent = ""; el.className = "hint"; }, 3000);
}

function savePreset() {
  const name = $("presetName").value.trim();
  if (!name) { showPresetStatus("enter a preset name", true); return; }
  try {
    const presets = getPresets();
    presets[name] = currentPreset();
    setPresets(presets);
    renderPresetSelect();
    $("presetSelect").value = name;
    showPresetStatus("saved");
  } catch (e) {
    showPresetStatus("save failed: " + e.message, true);
    console.error("savePreset", e);
  }
}

function loadPreset() {
  const name = $("presetSelect").value;
  if (!name) { showPresetStatus("choose a preset first", true); return; }
  const presets = getPresets();
  const p = presets[name];
  if (!p) { showPresetStatus("preset not found", true); return; }

  if (p.settings) {
    state.settings.enabled = !!p.settings.enabled;
    state.settings.min_margin = p.settings.min_margin ?? 1e9;
    state.settings.max_snipes = p.settings.max_snipes ?? 2;
  } else {
    state.settings.enabled = !!p.enabled;
    state.settings.min_margin = p.min_margin ?? 1e9;
    state.settings.max_snipes = p.max_snipes ?? 2;
  }
  saveSettings();

  (p.rules || []).forEach(r => {
    state.rules.set(r.name, {
      name: r.name,
      buy: !!r.buy,
      max_price: r.max_price != null ? Number(r.max_price) : null,
      qty_limit: r.qty_limit != null ? Number(r.qty_limit) : 1,
      bought: 0,
    });
    saveRow(r.name);
  });

  renderRules();
  renderBanner();
  renderItems();
  showPresetStatus("loaded");
}

function deletePreset() {
  const name = $("presetSelect").value;
  if (!name) { showPresetStatus("choose a preset first", true); return; }
  const presets = getPresets();
  delete presets[name];
  setPresets(presets);
  renderPresetSelect();
  $("presetName").value = "";
  showPresetStatus("deleted");
}

// ── Load ──────────────────────────────────────────────────────────────────
async function load() {
  if (!CFG.supabaseUrl || CFG.supabaseUrl.includes("YOUR-PROJECT")) {
    setStatus(`Set <b>supabaseUrl</b> and <b>anonKey</b> in config.js`);
    return;
  }
  if (!state.auth) {
    showGate();
    return;
  }
  setStatus("loading…");
  const w = WINDOWS[state.win];
  state.since = Date.now() - w.ms;
  try {
    const [items, rules, settings] = await Promise.all([
      rpc("item_stats", { since_ms: state.since }),
      rest("watchlist?select=*"),
      rest("buy_settings?select=*&id=eq.1"),
    ]);
    state.items = (items || []).filter(i => i.name).map(i => ({
      name: i.name,
      n: Number(i.n) || 0,
      low: Number(i.low) || 0,
      p10: Number(i.p10) || 0,
      median: Number(i.median) || 0,
      high: Number(i.high) || 0,
      last: Number(i.last_price) || 0,
      lastTs: Number(i.last_ts_ms) || 0,
      firstTs: Number(i.first_ts_ms) || 0,
      icon: i.icon || "",
    }));
    state.rules = new Map((rules || []).map(r => [r.name, r]));
    state.settings = (settings || [])[0] || { id: 1, enabled: false, min_margin: 1e9, max_snipes: 2, snipes_used: 0 };
    setStatus(statusLine());
    showApp();
    renderRules();
    renderBanner();
    renderItems();
    refreshScannerState();
  } catch (e) {
    // A rejected token (or the schema not existing yet) sends us back to the
    // gate rather than showing a half-rendered page.
    if (!state.auth) {
      showGate();
      return;
    }
    setStatus(`<span class="warnbox">${esc(e.message)}</span>`);
    renderAuth();
    renderRules();
  }
}

// ── Gate: the app is invisible until signed in ────────────────────────────
function showGate() {
  $("app").hidden = true;
  $("gate").hidden = false;
  $("auth").innerHTML = "";
  $("status").textContent = "";
  $("body").innerHTML = "";
  state.items = [];
  state.rules = new Map();
  state.settings = null;
  const e = $("email");
  if (e) e.focus();
}

function showApp() {
  $("gate").hidden = true;
  $("app").hidden = false;
  renderAuth();
  renderBanner();
  renderPresetSelect();
}

function wireGate() {
  const go = async () => {
    $("gateErr").textContent = "";
    const btn = $("signin");
    btn.disabled = true;
    try {
      await signIn($("email").value.trim(), $("pw").value);
      showApp();
      await load();
    } catch (err) {
      $("gateErr").textContent = err.message;
      $("pw").value = "";
    } finally {
      btn.disabled = false;
    }
  };
  $("signin").onclick = go;
  $("pw").onkeydown = e => { if (e.key === "Enter") go(); };
  $("email").onkeydown = e => { if (e.key === "Enter") $("pw").focus(); };
}

// ── Render: scanner state banner ──────────────────────────────────────────
/// What the scanner is actually doing, from the heartbeat it publishes. This is
/// the answer to "why didn't it buy?" — mode, liveness and the master switch in
/// one line, instead of digging through a log file.
function renderBanner() {
  const el = $("banner");
  if (!el) return;
  const s = state.settings || {};

  if (!s.scanner_seen) {
    el.className = "banner";
    el.innerHTML = `Scanner: <b>never seen</b><span class="hint">Start market_scanner.exe. Until it checks in, nothing will be bought.</span>`;
    return;
  }

  const seen = Date.parse(s.scanner_seen);
  const quiet = Date.now() - seen > 120000;
  const dry = s.scanner_dry_run !== false;

  let cls, head, hint;
  if (quiet) {
    cls = "dead";
    head = `Scanner: not seen for ${timeAgo(seen)}`;
    hint = "The loop isn't running (or it can't reach Supabase) — nothing is being bought.";
  } else if (dry) {
    cls = "warn";
    head = `Scanner: DRY RUN · seen ${timeAgo(seen)}`;
    hint = "It reports what it would buy but never clicks. Set dry_run = false in market.toml (next to the exe), then restart it.";
  } else if (!s.enabled) {
    cls = "warn";
    head = `Scanner: LIVE · seen ${timeAgo(seen)}`;
    hint = "Armed, but the master switch below is OFF — nothing will be bought.";
  } else {
    cls = "ok";
    head = `Scanner: LIVE · seen ${timeAgo(seen)}`;
    hint = "Armed: ticked items are bought at or below their max price.";
  }
  el.className = "banner " + cls;
  el.innerHTML = `${esc(head)}<span class="hint">${esc(hint)}</span>`;
}

/// The scanner publishes scanner_* every ~20s. Refresh just those fields so the
/// banner notices a stopped or restarted scanner, without touching edits in
/// progress (enabled / min_margin / max_snipes stay as the form has them).
async function refreshScannerState() {
  if (!state.auth || !state.settings) return;
  try {
    const rows = await rest("buy_settings?select=scanner_dry_run,scanner_seen&id=eq.1");
    const r = (rows || [])[0];
    if (r) {
      state.settings.scanner_dry_run = r.scanner_dry_run;
      state.settings.scanner_seen = r.scanner_seen;
      renderBanner();
    }
  } catch {
    /* the banner keeps its last reading; the age still ticks up */
  }
}

// ── Render: auth bar ──────────────────────────────────────────────────────
function renderAuth() {
  const el = $("auth");
  if (!state.auth) {
    el.innerHTML = "";
    return;
  }
  el.innerHTML = `<span class="who">${esc(state.auth.email || "signed in")}</span>
    <button class="ghost" id="signout">Sign out</button>`;
  // Reload rather than re-render, so nothing from the session is left on screen.
  $("signout").onclick = () => { saveAuth(null); location.reload(); };
}

// ── Render: session rules ─────────────────────────────────────────────────
function renderRules() {
  const s = state.settings || {};
  const ro = state.auth ? "" : " disabled";
  $("rules").innerHTML = `
    <div class="rule">
      <label>Master switch</label>
      <div class="master">
        <input type="checkbox" id="enabled" ${s.enabled ? "checked" : ""}${ro}>
        <span class="big ${s.enabled ? "up" : "down"}">${s.enabled ? "BUYING" : "PAUSED"}</span>
      </div>
      <label>Off = nothing is bought at all</label>
    </div>
    <div class="rule">
      <label>Big snipe when profit ≥</label>
      <input type="text" id="min_margin" value="${fmt(s.min_margin || 0)}"${ro}>
      <label>absolute gp — "1b" or "250m" works. % alone is misleading.</label>
    </div>
    <div class="rule">
      <label>Big snipes allowed per session</label>
      <input type="number" id="max_snipes" min="0" step="1" value="${s.max_snipes != null ? s.max_snipes : 2}"${ro}>
      <label>0 = never take them · applies only to ticked items</label>
    </div>
    <div class="rule">
      <label>This session</label>
      <div class="row">
        <span class="big">${s.snipes_used || 0} / ${s.max_snipes != null ? s.max_snipes : 2}</span>
        <button class="ghost" id="reset"${ro}>Reset</button>
      </div>
      <label>snipes used since ${s.session_start ? timeAgo(Date.parse(s.session_start)) : "never started"}</label>
    </div>`;

  if (state.auth) {
    $("enabled").onchange = e => { state.settings.enabled = e.target.checked; renderRules(); renderBanner(); debounceSettings(); };
    $("min_margin").oninput = e => { const v = parseMoney(e.target.value); if (v != null) state.settings.min_margin = v; e.target.classList.toggle("dirty", v == null); };
    $("min_margin").onblur = e => { e.target.value = fmt(state.settings.min_margin); e.target.classList.remove("dirty"); renderItems(); };
    $("min_margin").onchange = () => { debounceSettings(); renderItems(); };
    $("max_snipes").onchange = e => { state.settings.max_snipes = Math.max(0, Number(e.target.value) || 0); debounceSettings(); renderItems(); };
    $("reset").onclick = resetSession;
  }

  $("rulesHint").innerHTML = state.auth
    ? `Edits save automatically. The scanner reads these every few seconds.`
    : `Read-only — sign in above to change anything. The scanner only obeys rules written by you.`;
}

/// Zero the per-session counters: snipes used and each item's bought count.
async function resetSession() {
  try {
    await rest("buy_settings?id=eq.1", {
      method: "PATCH",
      write: true,
      headers: { Prefer: "return=minimal" },
      body: JSON.stringify({ snipes_used: 0, updated_at: new Date().toISOString() }),
    });
    await rest("watchlist?buy=eq.true", {
      method: "PATCH",
      write: true,
      headers: { Prefer: "return=minimal" },
      body: JSON.stringify({ bought: 0, updated_at: new Date().toISOString() }),
    });
    for (const [k, v] of state.rules) state.rules.set(k, Object.assign({}, v, { bought: 0 }));
    state.settings.snipes_used = 0;
    renderRules();
    renderItems();
    setStatus("session counters reset");
  } catch (e) {
    setStatus(`<span class="warnbox">${esc(e.message)}</span>`);
  }
}

// ── Render: items ─────────────────────────────────────────────────────────
function visible() {
  const q = state.search.trim().toLowerCase();
  let list = state.items.filter(i => !q || i.name.toLowerCase().includes(q));
  if (state.filters.on) list = list.filter(i => ruleOf(i.name).buy);
  if (state.filters.profit) list = list.filter(i => { const d = derived(i); return d.profit != null && d.profit > 0; });
  if (state.filters.snipe) list = list.filter(i => derived(i).snipe);
  const key = state.sort;
  list.sort((a, b) => {
    if (key === "name") return a.name.localeCompare(b.name);
    if (key === "n" || key === "median") return b[key] - a[key];
    // profit: configured rows first, biggest profit on top
    const da = derived(a).profit, db = derived(b).profit;
    if (da == null && db == null) return b.n - a.n;
    if (da == null) return 1;
    if (db == null) return -1;
    return db - da;
  });
  return list;
}

function profitCell(d) {
  if (d.profit == null) return `<span class="qty-ok">set a max price</span>`;
  const cls = d.profit >= 0 ? "up" : "down";
  return `<span class="${cls}">${fmtSigned(d.profit)}</span> <span class="hint">${d.pct >= 0 ? "+" : ""}${d.pct.toFixed(0)}%</span>`;
}

/// Log-scaled bar showing where a threshold sits against the observed prices.
function spreadBar(it, maxPrice) {
  const marks = [it.low, it.p10, it.median, it.high].filter(v => v > 0);
  if (!marks.length) return "";
  let lo = Math.min(...marks);
  let hi = Math.max(...marks);
  if (maxPrice > 0) { lo = Math.min(lo, maxPrice); hi = Math.max(hi, maxPrice); }
  if (hi <= lo) hi = lo * 1.001 || lo + 1;
  const L = v => Math.log10(Math.max(v, 1));
  const span = L(hi) - L(lo) || 1;
  const x = v => 2 + ((L(v) - L(lo)) / span) * 196;
  const clamp = v => Math.max(2, Math.min(198, v));

  const inBand = it.p10 > 0 && it.median > 0
    ? `<rect x="${x(it.p10).toFixed(1)}" y="10" width="${Math.max(1, x(it.median) - x(it.p10)).toFixed(1)}" height="8" fill="${GREEN}" opacity="0.55"/>`
    : "";
  const tick = (v, color, h) => v > 0
    ? `<line x1="${x(v).toFixed(1)}" y1="${14 - h}" x2="${x(v).toFixed(1)}" y2="${14 + h}" stroke="${color}" stroke-width="1.5"/>` : "";
  const threshold = maxPrice > 0
    ? `<line x1="${clamp(x(maxPrice)).toFixed(1)}" y1="3" x2="${clamp(x(maxPrice)).toFixed(1)}" y2="25" stroke="${GOLD}" stroke-width="2"/>
       <text x="${clamp(x(maxPrice)).toFixed(1)}" y="8" text-anchor="middle" fill="${GOLD}">▲</text>`
    : "";

  return `<svg viewBox="0 0 200 30" style="width:200px;height:30px" title="low ${fmt(it.low)} · p10 ${fmt(it.p10)} · median ${fmt(it.median)} · high ${fmt(it.high)}">
    <rect x="2" y="10" width="196" height="8" rx="4" fill="#3a3226"/>
    ${inBand}
    ${tick(it.low, "#8b7f68", 8)}
    ${tick(it.p10, GREEN, 7)}
    ${tick(it.median, "#e8ddc8", 9)}
    ${tick(it.high, RED, 8)}
    ${threshold}
  </svg>`;
}

function renderItems() {
  const list = visible();
  const slice = list.slice(0, state.shown);
  $("count").textContent = `${list.length} shown · ${PAGE} at a time`;

  const ro = state.auth ? "" : " disabled";
  const body = $("body");
  if (!slice.length) {
    body.innerHTML = `<tr><td colspan="12" class="empty">nothing matches</td></tr>`;
    return;
  }

  body.innerHTML = slice.map(it => {
    const d = derived(it);
    const r = d.rule;
    const cap = r.qty_limit > 0 ? r.qty_limit : 0;
    const row = `
      <tr class="${r.buy ? "on" : "off"}" data-name="${esc(it.name)}">
        <td><input type="checkbox" data-k="buy" ${r.buy ? "checked" : ""}${ro}></td>
        <td><div class="itemCell">
          ${it.icon ? `<img src="${esc(iconUrl(it.icon))}" alt="" loading="lazy" onerror="this.style.visibility='hidden'">` : ""}
          <span>${esc(it.name)}</span>${d.snipe ? ` <span class="badge">SNIPE</span>` : ""}
        </div></td>
        <td class="num"><input type="text" data-k="max_price" value="${r.max_price != null ? fmt(r.max_price) : ""}" placeholder="—"${ro}></td>
        <td class="num"><input type="number" data-k="qty_limit" min="1" step="1" value="${r.qty_limit}"${ro}></td>
        <td class="num"><span class="${r.bought >= cap && cap > 0 ? "qty-done" : "qty-ok"}">${r.bought}/${cap}</span></td>
        <td class="num">${fmt(it.last)}</td>
        <td class="num">${fmt(it.p10)}</td>
        <td class="num">${fmt(it.median)}</td>
        <td class="num">${profitCell(d)}</td>
        <td>${spreadBar(it, r.max_price)}</td>
        <td class="num">${fmt(it.n)}</td>
        <td><span class="state"></span><button class="ghost" data-expand title="price history">▾</button></td>
      </tr>`;
    return state.expanded.has(it.name)
      ? row + `<tr class="trendRow" data-trend="${esc(it.name)}"><td colspan="12"><div class="trend">${state.trends.get(it.name) || `<div class="empty">loading trend…</div>`}</div></td></tr>`
      : row;
  }).join("");

  if (list.length > slice.length) {
    body.innerHTML += `<tr><td colspan="12" style="text-align:center">
      <button class="ghost" id="more">Show ${Math.min(PAGE, list.length - slice.length)} more</button></td></tr>`;
    $("more").onclick = () => { state.shown += PAGE; renderItems(); };
  }
}

// ── Trend (expanded row) ──────────────────────────────────────────────────
async function loadTrend(name) {
  const w = WINDOWS[state.win];
  try {
    const rows = await rpc("item_series_for", { p_name: name, since_ms: state.since, bucket: w.bucket });
    const pts = (rows || []).map(r => ({ t: Date.parse(r.b), v: Number(r.median) || 0, lo: Number(r.low) || 0, hi: Number(r.high) || 0 }))
      .filter(p => !isNaN(p.t));
    state.trends.set(name, lineChart(pts, w.bucket === "hour" ? shortHour : shortDate));
  } catch (e) {
    state.trends.set(name, `<div class="empty">${esc(e.message)}</div>`);
  }
  if (state.expanded.has(name)) renderItems();
}

function lineChart(pts, xFmt) {
  if (!pts.length) return `<div class="empty">no history in this window</div>`;
  const w = 760, h = 150, pad = { l: 60, r: 12, t: 10, b: 20 };
  const xs = pts.map(p => p.t), ys = pts.map(p => p.v);
  const x0 = Math.min(...xs), x1 = Math.max(...xs);
  let y0 = Math.min(...ys, ...pts.map(p => p.lo).filter(v => v > 0));
  let y1 = Math.max(...ys, ...pts.map(p => p.hi));
  if (y0 === y1) { y0 = y0 * 0.9 || 0; y1 = y1 * 1.1 || 1; }
  const sx = t => pad.l + (x1 === x0 ? 0.5 : (t - x0) / (x1 - x0)) * (w - pad.l - pad.r);
  const sy = v => (h - pad.b) - ((v - y0) / (y1 - y0)) * (h - pad.t - pad.b);
  const d = pts.map((p, i) => `${i ? "L" : "M"}${sx(p.t).toFixed(1)},${sy(p.v).toFixed(1)}`).join(" ");
  const band = pts.map((p, i) => `${i ? "L" : "M"}${sx(p.t).toFixed(1)},${sy(p.hi).toFixed(1)}`).join(" ")
    + " " + pts.slice().reverse().map(p => `L${sx(p.t).toFixed(1)},${sy(p.lo).toFixed(1)}`).join(" ") + " Z";
  const grid = [y1, (y0 + y1) / 2, y0].map(v => `
    <line x1="${pad.l}" y1="${sy(v).toFixed(1)}" x2="${w - pad.r}" y2="${sy(v).toFixed(1)}" stroke="#3a3226"/>
    <text x="${pad.l - 6}" y="${(sy(v) + 3).toFixed(1)}" text-anchor="end">${fmtCompact(v)}</text>`).join("");
  return `<svg viewBox="0 0 ${w} ${h}">${grid}
    <path d="${band}" fill="${BLUE}" opacity="0.12"/>
    <path d="${d}" fill="none" stroke="${GOLD}" stroke-width="2"/>
    <text x="${pad.l}" y="${h - 5}">${xFmt(pts[0].t)}</text>
    <text x="${w - pad.r}" y="${h - 5}" text-anchor="end">${xFmt(pts[pts.length - 1].t)}</text></svg>`;
}

// ── Events ────────────────────────────────────────────────────────────────
document.addEventListener("click", e => {
  const ex = e.target.closest("[data-expand]");
  if (!ex) return;
  const name = ex.closest("tr").dataset.name;
  if (state.expanded.has(name)) state.expanded.delete(name);
  else { state.expanded.add(name); loadTrend(name); }
  renderItems();
});

document.addEventListener("change", e => {
  const el = e.target;
  const tr = el.closest("tr[data-name]");
  if (!tr) return;
  const name = tr.dataset.name;
  const k = el.dataset.k;
  if (!k) return;

  if (!state.auth) { setStatus(`<span class="warnbox">sign in first — edits are disabled</span>`); renderItems(); return; }

  if (k === "buy") {
    setRule(name, { buy: el.checked });
    const it = state.items.find(i => i.name === name);
    const d = it ? derived(it) : null;
    if (d && d.capped) {
      setStatus(`<span class="warnbox">${esc(name)} already hit its cap (${d.rule.bought}/${d.rule.qty_limit})</span>`);
    }
  } else if (k === "max_price") {
    const v = parseMoney(el.value);
    if (el.value.trim() && v == null) { el.classList.add("dirty"); return; }
    el.classList.remove("dirty");
    setRule(name, { max_price: v });
  } else if (k === "qty_limit") {
    setRule(name, { qty_limit: Math.max(1, Number(el.value) || 1) });
  }
  debounceSave(name);
  renderItems();
});

document.addEventListener("input", e => {
  const el = e.target;
  if (el.dataset.k === "max_price") { el.classList.add("dirty"); }
});

$("search").addEventListener("input", e => { state.search = e.target.value; state.shown = PAGE; renderItems(); });
$("fOn").addEventListener("change", e => { state.filters.on = e.target.checked; state.shown = PAGE; renderItems(); });
$("fProfit").addEventListener("change", e => { state.filters.profit = e.target.checked; state.shown = PAGE; renderItems(); });
$("fSnipe").addEventListener("change", e => { state.filters.snipe = e.target.checked; state.shown = PAGE; renderItems(); });
$("sort").addEventListener("change", e => { state.sort = e.target.value; state.shown = PAGE; renderItems(); });
$("reload").addEventListener("click", load);
$("presetSave").addEventListener("click", savePreset);
$("presetLoad").addEventListener("click", loadPreset);
$("presetDelete").addEventListener("click", deletePreset);

// ── Boot ──────────────────────────────────────────────────────────────────
loadAuth();
wireGate();
if (state.auth) {
  showApp();
  load();
} else {
  showGate();
}
setInterval(() => { if (state.auth) token(); }, 10 * 60 * 1000);
// Keep the "seen Ns ago" honest, and notice a restarted scanner.
setInterval(renderBanner, 1000);
setInterval(refreshScannerState, 30000);
