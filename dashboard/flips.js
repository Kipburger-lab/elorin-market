"use strict";

// Elorin Market — realistic flips.
//
// The dashboard's "best investments" ranks by percent margin, which the market
// games badly: a single 1 gp bait listing makes `low` absurd, and one panic-buy
// at 10× makes `high` absurd, so the item that tops the list is often the one
// nobody could actually flip. This page refuses to look at either extreme.
//
//   buy  = p10      (a low you could plausibly fill at; ignores the 1 gp bait)
//   sell = median   (what the item habitually goes for; ignores the panic sale)
//
// and it keeps only items whose price has been *steady* — measured as the
// spread of their per-bucket medians, not their min/max — and whose volume has
// been steady too, so a one-off listing can't masquerade as a market.

const CFG = Object.assign({
  supabaseUrl: "",
  anonKey: "",
  iconsBucket: "icons",
  feeRate: 0.10,
}, window.ELORIN_CONFIG || {});

const WINDOWS = {
  week:  { label: "Week",  ms: 7 * 86400e3,  bucket: "day" },
  month: { label: "Month", ms: 30 * 86400e3, bucket: "day" },
};

const GOLD = "#ffcf5c", GREEN = "#6ee07a", RED = "#ff6b5e";

const state = {
  win: "week",
  search: "",
  sort: { key: "profit", dir: -1 },
  items: [],
  series: new Map(),
  since: 0,
  minObs: 12,
  maxCv: 0.35,
  minBuckets: 3,
  updatedAt: 0,
  statusHtml: "loading…",
};

const $ = id => document.getElementById(id);

function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, c =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function fmt(n) {
  if (n == null || isNaN(n)) return "–";
  return Math.round(Number(n)).toLocaleString("en-US");
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

function fmtPct(x) {
  if (x == null || isNaN(x)) return "–";
  return (x >= 0 ? "+" : "") + x.toFixed(1) + "%";
}

function dateTime(t) {
  if (!t) return "–";
  return new Date(t).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

function shortDate(t) {
  return new Date(t).toLocaleDateString([], { month: "short", day: "numeric" });
}

function iconUrl(file) {
  if (!file) return "";
  return `${CFG.supabaseUrl}/storage/v1/object/public/${CFG.iconsBucket}/${encodeURIComponent(file)}`;
}

function iconTag(file, size) {
  if (!file) return `<span style="display:inline-block;width:${size}px"></span>`;
  return `<img src="${esc(iconUrl(file))}" alt="" loading="lazy" style="width:${size}px;height:${size}px;object-fit:contain;image-rendering:pixelated" onerror="this.style.visibility='hidden'">`;
}

function hl(name) {
  const q = state.search.trim();
  if (!q) return esc(name);
  const i = name.toLowerCase().indexOf(q.toLowerCase());
  if (i < 0) return esc(name);
  return esc(name.slice(0, i)) + "<mark>" + esc(name.slice(i, i + q.length)) + "</mark>" + esc(name.slice(i + q.length));
}

// ── Supabase REST ─────────────────────────────────────────────────────────
async function rpcAt(path, body) {
  const res = await fetch(`${CFG.supabaseUrl}/rest/v1/rpc/${path}`, {
    method: "POST",
    headers: {
      apikey: CFG.anonKey,
      Authorization: `Bearer ${CFG.anonKey}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify(body || {}),
  });
  if (!res.ok) throw new Error(`${path} → ${res.status} ${(await res.text()).slice(0, 160)}`);
  return res.json();
}

function rpc(fn, body) {
  return rpcAt(fn, body);
}

/// Page past PostgREST's 1000-row cap (a single call silently drops the tail).
async function rpcPaged(fn, body, order, page = 1000) {
  const all = [];
  for (let offset = 0; offset <= 200000; offset += page) {
    const chunk = await rpcAt(`${fn}?select=*&order=${order}&limit=${page}&offset=${offset}`, body);
    if (!chunk || !chunk.length) break;
    all.push(...chunk);
    if (chunk.length < page) break;
  }
  return all;
}

// ── Stats ─────────────────────────────────────────────────────────────────
function mean(xs) {
  return xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : 0;
}

/// Coefficient of variation: the spread as a fraction of the level, so a 1m item
/// that wanders ±50k scores the same as a 1k item that wanders ±50.
function cv(xs) {
  if (xs.length < 2) return 0;
  const m = mean(xs);
  if (m <= 0) return 0;
  const variance = mean(xs.map(x => (x - m) * (x - m)));
  return Math.sqrt(variance) / m;
}

/// Turn one stats row plus its bucket series into a flip candidate.
function build(s) {
  const n = Number(s.n) || 0;
  const low = Number(s.low) || 0;
  const p10 = Number(s.p10) || 0;
  const median = Number(s.median) || 0;
  const high = Number(s.high) || 0;
  const avg = Number(s.avg) || 0;

  // p10 is the honest low; fall back to the raw low only if the schema predates it.
  const buy = p10 > 0 ? p10 : low;
  const sell = median;
  const net = sell * (1 - CFG.feeRate);
  const profit = net - buy;
  const pct = buy > 0 ? (profit / buy) * 100 : 0;

  const buckets = state.series.get(s.name) || [];
  const medians = buckets.map(b => b.v).filter(v => v > 0);
  const counts = buckets.map(b => b.n).filter(v => v > 0);

  return {
    name: String(s.name || ""),
    icon: s.icon || "",
    n, low, p10, median, high, avg,
    buy, sell, net, profit, pct,
    buckets: buckets.length,
    priceCv: cv(medians),   // steadiness of the price
    volCv: cv(counts),      // steadiness of the volume
    // Outlier tells, shown in the detail sheet so the numbers can be judged.
    bait: low > 0 && buy > 0 ? low / buy : 1,          // <<1 means 1 gp bait exists
    spike: median > 0 ? avg / median : 1,              // >>1 means high outliers
  };
}

function candidates() {
  const q = state.search.trim().toLowerCase();
  return state.items.filter(i =>
    (!q || i.name.toLowerCase().includes(q)) &&
    i.n >= state.minObs &&
    i.buckets >= state.minBuckets &&
    i.priceCv <= state.maxCv &&
    i.buy > 0 && i.sell > 0 && i.profit > 0
  );
}

function visible() {
  const list = candidates();
  const { key, dir } = state.sort;
  list.sort((a, b) => {
    const av = a[key], bv = b[key];
    if (typeof av === "string" || typeof bv === "string") {
      return String(av).localeCompare(String(bv)) * dir;
    }
    return (av - bv) * dir;
  });
  return list;
}

// ── Load ──────────────────────────────────────────────────────────────────
function paintStatus() {
  const age = state.updatedAt ? ` · updated ${dateTime(state.updatedAt)}` : "";
  $("status").innerHTML = state.statusHtml + `<span class="hint">${age}</span>`;
}

function setStatus(html) {
  state.statusHtml = html;
  paintStatus();
}

async function load() {
  if (!CFG.supabaseUrl || CFG.supabaseUrl.includes("YOUR-PROJECT")) {
    setStatus(`Set <b>supabaseUrl</b> and <b>anonKey</b> in config.js`);
    return;
  }
  const w = WINDOWS[state.win];
  state.since = Date.now() - w.ms;
  setStatus("loading…");
  try {
    const [stats, series] = await Promise.all([
      rpcPaged("item_stats", { since_ms: state.since }, "name"),
      rpcPaged("item_series", { since_ms: state.since, bucket: w.bucket }, "name,b"),
    ]);

    state.series = new Map();
    for (const s of series || []) {
      const t = Date.parse(s.b);
      if (!s.name || isNaN(t)) continue;
      const v = Number(s.median) || Number(s.avg) || 0;
      if (!state.series.has(s.name)) state.series.set(s.name, []);
      state.series.get(s.name).push({ t, n: Number(s.n) || 0, v });
    }
    for (const list of state.series.values()) list.sort((a, b) => a.t - b.t);

    state.items = (stats || []).map(build).filter(i => i.name);
    state.updatedAt = Date.now();
    setStatus(`${state.items.length} items · ${w.label.toLowerCase()} window`);
    render();
  } catch (e) {
    setStatus(`<span class="down">${esc(e.message)}</span>`);
  }
}

// ── Render ────────────────────────────────────────────────────────────────
function render() {
  const list = visible();
  const total = state.items.filter(i => i.n >= state.minObs).length;
  $("count").textContent = `${list.length} realistic flip${list.length === 1 ? "" : "s"} of ${total} with ≥${state.minObs} sightings`;
  renderNotes();

  document.querySelectorAll("#flips th").forEach(th => {
    const on = th.dataset.key === state.sort.key;
    th.classList.toggle("sorted", on);
    th.textContent = th.textContent.replace(/ [▲▼]$/, "") + (on ? (state.sort.dir < 0 ? " ▼" : " ▲") : "");
  });

  const body = $("flipsBody");
  if (!list.length) {
    body.innerHTML = `<tr><td colspan="9" class="empty">Nothing is steady enough yet — loosen the filters or wait for more sightings.</td></tr>`;
    return;
  }
  body.innerHTML = list.map(i => `
    <tr data-name="${esc(i.name)}">
      <td><div class="itemCell">${iconTag(i.icon, 20)}<span>${hl(i.name)}</span></div></td>
      <td class="num">${fmt(i.buy)}</td>
      <td class="num">${fmt(i.sell)}</td>
      <td class="num up">+${fmtCompact(i.profit)}</td>
      <td class="num ${i.pct >= 0 ? "up" : "down"}">${fmtPct(i.pct)}</td>
      <td class="num">${fmt(i.n)}</td>
      <td class="num">${fmt(i.buckets)}</td>
      <td class="num">${(i.priceCv * 100).toFixed(0)}%</td>
      <td class="num">${(i.volCv * 100).toFixed(0)}%</td>
    </tr>`).join("");
}

function renderNotes() {
  const w = WINDOWS[state.win];
  $("notes").innerHTML = `
    <b>Buy</b> uses the 10th percentile of prices, not the minimum — the minimum is
    usually a 1 gp bait listing you can never fill at. <b>Sell</b> uses the median,
    not the max, so one panic sale can't inflate the margin.<br>
    <b>Price σ</b> is how much each ${w.bucket}'s median wanders, as a % of the
    level; items above <b>${(state.maxCv * 100).toFixed(0)}%</b> are dropped as too
    jumpy to flip. <b>Volume σ</b> is the same for offers per ${w.bucket}.<br>
    Only items seen <b>≥${state.minObs}</b> times across <b>≥${state.minBuckets}</b>
    ${w.bucket}s are considered. Open a row for the outlier check (bait ratio and
    average-vs-median).`;
}

// ── Detail sheet ──────────────────────────────────────────────────────────
function closeDetail() {
  $("detail").classList.add("hidden");
  $("detail").setAttribute("aria-hidden", "true");
}

async function openDetail(name) {
  const it = state.items.find(i => i.name === name);
  const modal = $("detail"), sheet = $("detailSheet");
  modal.classList.remove("hidden");
  modal.setAttribute("aria-hidden", "false");

  const pts = (state.series.get(name) || []).map(b => ({ t: b.t, v: b.v }));
  const baitNote = it && it.bait < 0.6
    ? `<div class="hint">There is a bait listing at ${fmt(it.low)} — ${(it.bait * 100).toFixed(0)}% of the p10 we plan to buy at. Ignored on purpose.</div>`
    : "";
  const spikeNote = it && it.spike > 1.3
    ? `<div class="hint">Average runs ${it.spike.toFixed(2)}× the median — high outliers in the data, which the median ignores.</div>`
    : "";

  sheet.innerHTML = `
    <div class="sheetHead">
      ${iconTag(it ? it.icon : "", 44)}
      <div class="nm">${esc(name)}</div>
      <button class="ghost" id="detailClose">Close</button>
    </div>
    ${it ? `
      <div class="chips">
        <div class="chip">buy ≤ <b>${fmt(it.buy)}</b></div>
        <div class="chip">sell ~ <b>${fmt(it.sell)}</b></div>
        <div class="chip">profit <b class="up">+${fmtCompact(it.profit)}</b></div>
        <div class="chip">margin <b>${fmtPct(it.pct)}</b></div>
        <div class="chip">seen <b>${fmt(it.n)}</b></div>
        <div class="chip">price σ <b>${(it.priceCv * 100).toFixed(0)}%</b></div>
        <div class="chip">volume σ <b>${(it.volCv * 100).toFixed(0)}%</b></div>
      </div>
      <div class="box" style="margin-bottom:12px">
        <h3>Range seen this window</h3>
        <div class="hint">low ${fmt(it.low)} · p10 ${fmt(it.p10)} · median ${fmt(it.median)} · avg ${fmt(Math.round(it.avg))} · high ${fmt(it.high)}</div>
        ${baitNote}${spikeNote}
      </div>
      <div class="box" style="margin-bottom:12px">
        <h3>Median price per ${WINDOWS[state.win].bucket}</h3>
        ${lineChart(pts)}
      </div>` : `<div class="empty">No data for this item in the window.</div>`}
    <div class="box">
      <h3>Recent offers</h3>
      <div id="detailOffers" class="empty">loading…</div>
    </div>`;

  $("detailClose").onclick = closeDetail;

  try {
    const rows = await rpc("item_offers", { p_name: name, lim: 60 });
    const el = $("detailOffers");
    if (!el) return;
    el.className = "";
    el.innerHTML = !rows.length
      ? `<div class="empty">no offers in this window</div>`
      : `<div class="tableWrap"><table class="offers">
      <thead><tr><th>When</th><th>Seller</th><th class="num">Price</th><th class="num">Qty</th></tr></thead>
      <tbody>${rows.map(r => `<tr>
        <td>${dateTime(Number(r.ts_ms))}</td>
        <td>${esc(r.seller || "–")}</td>
        <td class="num">${fmt(r.price)}</td>
        <td class="num">${fmt(r.quantity)}</td>
      </tr>`).join("")}</tbody></table></div>`;
  } catch (e) {
    const el = $("detailOffers");
    if (el) el.innerHTML = `<span class="down">${esc(e.message)}</span>`;
  }
}

// ── Charts ────────────────────────────────────────────────────────────────
function lineChart(points) {
  const w = 600, h = 150, pad = { l: 56, r: 12, t: 12, b: 22 };
  if (!points.length) return `<div class="empty">no history in this window</div>`;
  const xs = points.map(p => p.t), ys = points.map(p => p.v);
  const x0 = Math.min(...xs), x1 = Math.max(...xs);
  let y0 = Math.min(...ys), y1 = Math.max(...ys);
  if (y0 === y1) { y0 -= 1; y1 += 1; }
  const sx = t => pad.l + (x1 === x0 ? 0.5 : (t - x0) / (x1 - x0)) * (w - pad.l - pad.r);
  const sy = v => (h - pad.b) - ((v - y0) / (y1 - y0)) * (h - pad.t - pad.b);
  const d = points.map((p, i) => `${i ? "L" : "M"}${sx(p.t).toFixed(1)},${sy(p.v).toFixed(1)}`).join(" ");
  const grid = [y1, (y0 + y1) / 2, y0].map(v => `
    <line x1="${pad.l}" y1="${sy(v).toFixed(1)}" x2="${w - pad.r}" y2="${sy(v).toFixed(1)}" stroke="#3a3226"/>
    <text x="${pad.l - 6}" y="${(sy(v) + 3).toFixed(1)}" text-anchor="end">${fmtCompact(v)}</text>`).join("");
  const labels = points.length > 1 ? `
    <text x="${pad.l}" y="${h - 6}">${shortDate(points[0].t)}</text>
    <text x="${w - pad.r}" y="${h - 6}" text-anchor="end">${shortDate(points[points.length - 1].t)}</text>` : "";
  return `<svg viewBox="0 0 ${w} ${h}">${grid}
    <path d="${d}" fill="none" stroke="${GOLD}" stroke-width="2" stroke-linejoin="round"/>
    ${labels}</svg>`;
}

// ── Events ────────────────────────────────────────────────────────────────
$("tabs").addEventListener("click", e => {
  const b = e.target.closest("button[data-window]");
  if (!b) return;
  state.win = b.dataset.window;
  document.querySelectorAll("#tabs button").forEach(x => x.classList.toggle("active", x === b));
  load();
});

$("search").addEventListener("input", e => { state.search = e.target.value; render(); });
$("reload").addEventListener("click", load);

$("minObs").addEventListener("change", e => { state.minObs = Math.max(1, Number(e.target.value) || 1); render(); });
$("minBuckets").addEventListener("change", e => { state.minBuckets = Math.max(2, Number(e.target.value) || 2); render(); });
$("maxCv").addEventListener("change", e => { state.maxCv = Math.max(1, Number(e.target.value) || 35) / 100; render(); });

document.querySelector("#flips thead").addEventListener("click", e => {
  const th = e.target.closest("th[data-key]");
  if (!th) return;
  const key = th.dataset.key;
  if (state.sort.key === key) state.sort.dir *= -1;
  else state.sort = { key, dir: key === "name" ? 1 : -1 };
  render();
});

document.addEventListener("click", e => {
  const row = e.target.closest("#flipsBody tr[data-name]");
  if (row) openDetail(row.dataset.name);
});

$("detail").addEventListener("click", e => { if (e.target === $("detail")) closeDetail(); });
document.addEventListener("keydown", e => { if (e.key === "Escape") closeDetail(); });

// ── Boot ──────────────────────────────────────────────────────────────────
load();
setInterval(paintStatus, 1000);
