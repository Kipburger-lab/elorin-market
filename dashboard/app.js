"use strict";

// ── Config ────────────────────────────────────────────────────────────────
const CFG = Object.assign({
  supabaseUrl: "",
  anonKey: "",
  iconsBucket: "icons",
  feeRate: 0.10,
  minObservations: 4,
  topInvestments: 12,
  autoRefreshSeconds: 60,
}, window.ELORIN_CONFIG || {});

const WINDOWS = {
  day:   { label: "Day",   ms: 24 * 3600e3,  bucket: "hour" },
  week:  { label: "Week",  ms: 7 * 86400e3,  bucket: "day" },
  month: { label: "Month", ms: 30 * 86400e3, bucket: "day" },
};

const GOLD = "#ffcf5c", GREEN = "#6ee07a", RED = "#ff6b5e", BLUE = "#6fc3ff";

const state = {
  win: "day",
  search: "",
  sort: { key: "pct", dir: -1 },
  items: [],
  series: [],
  hours: [],
  recent: [],
  since: 0,
  updatedAt: 0,
  statusHtml: "loading…",
  signature: "",
};

// ── Helpers ───────────────────────────────────────────────────────────────
const $ = id => document.getElementById(id);

function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, c =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function fmt(n) {
  if (n == null || isNaN(n)) return "–";
  return Number(n).toLocaleString("en-US");
}

function fmtPct(x) {
  if (x == null || isNaN(x)) return "–";
  return (x >= 0 ? "+" : "") + x.toFixed(1) + "%";
}

function fmtCompact(n) {
  n = Number(n) || 0;
  if (n >= 1e9) return (n / 1e9).toFixed(1) + "b";
  if (n >= 1e6) return (n / 1e6).toFixed(1) + "m";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + "k";
  return String(Math.round(n));
}

function timeAgo(t) {
  if (!t) return "–";
  const s = Math.max(0, (Date.now() - t) / 1000);
  if (s < 60) return Math.floor(s) + "s ago";
  if (s < 3600) return Math.floor(s / 60) + "m ago";
  if (s < 86400) return Math.floor(s / 3600) + "h ago";
  return Math.floor(s / 86400) + "d ago";
}

function dateTime(t) {
  if (!t) return "–";
  return new Date(t).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

function shortDate(t) {
  return new Date(t).toLocaleDateString([], { month: "short", day: "numeric" });
}

function shortHour(t) {
  return new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
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

function matches(name) {
  const q = state.search.trim().toLowerCase();
  return q && name.toLowerCase().includes(q);
}

// ── Supabase REST ─────────────────────────────────────────────────────────
async function rpc(fn, body) {
  const res = await fetch(`${CFG.supabaseUrl}/rest/v1/rpc/${fn}`, {
    method: "POST",
    headers: {
      apikey: CFG.anonKey,
      Authorization: `Bearer ${CFG.anonKey}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify(body || {}),
  });
  if (!res.ok) throw new Error(`${fn} → ${res.status} ${(await res.text()).slice(0, 160)}`);
  return res.json();
}

// ── Load ──────────────────────────────────────────────────────────────────
/// Repaint the status line; the age is re-rendered every second so it is
/// obvious the page is live.
function paintStatus() {
  const age = state.updatedAt ? ` · updated ${dateTime(state.updatedAt)}` : "";
  $("status").innerHTML = state.statusHtml + `<span class="hint">${age}</span>`;
}

function setStatus(html) {
  state.statusHtml = html;
  paintStatus();
}

/// Fetch everything for the current window. `quiet` skips the loading flash for
/// background refreshes, and identical data skips the DOM work entirely.
async function load(quiet) {
  if (!CFG.supabaseUrl || CFG.supabaseUrl.includes("YOUR-PROJECT")) {
    setStatus(`Set <b>supabaseUrl</b> and <b>anonKey</b> in config.js`);
    return;
  }
  const w = WINDOWS[state.win];
  state.since = Date.now() - w.ms;
  if (!quiet) setStatus("loading…");
  try {
    const [items, series, hours, recent] = await Promise.all([
      rpc("item_stats", { since_ms: state.since }),
      rpc("item_series", { since_ms: state.since, bucket: w.bucket }),
      rpc("hour_index", { since_ms: state.since }),
      rpc("recent_offers", { lim: 200 }),
    ]);
    state.items = (items || []).map(decorate).filter(i => i.name);
    state.series = (series || []).map(s => ({ name: s.name, t: Date.parse(s.b), v: Number(s.median) || Number(s.avg) || 0 }))
      .filter(s => s.name && !isNaN(s.t));
    state.hours = hours || [];
    state.recent = recent || [];
    state.updatedAt = Date.now();

    const offers = state.items.reduce((a, i) => a + i.n, 0);
    setStatus(`${state.items.length} items · ${fmt(offers)} offers · last ${w.label.toLowerCase()}`);

    // Only touch the DOM when something actually changed, so a 5 s refresh
    // cadence doesn't flicker or fight the user's scrolling.
    const sig = state.win + "|" + state.items.map(i => `${i.name}:${i.n}:${i.low}:${i.median}:${i.last}`).join(",")
      + "|" + state.hours.map(h => `${h.hour}:${h.n}:${h.idx}`).join(",");
    if (sig !== state.signature) {
      state.signature = sig;
      render();
    }
  } catch (e) {
    setStatus(`<span class="warn">${esc(e.message)}</span>`);
  }
}

function decorate(it) {
  const low = Number(it.low) || 0;
  const median = Number(it.median) || 0;
  const fee = median * CFG.feeRate;
  const net = median - fee;
  const profit = net - low;
  return {
    name: String(it.name || ""),
    n: Number(it.n) || 0,
    low, median, high: Number(it.high) || 0, avg: Number(it.avg) || 0,
    last: Number(it.last_price) || 0,
    lastTs: Number(it.last_ts_ms) || 0,
    firstTs: Number(it.first_ts_ms) || 0,
    icon: it.icon || "",
    fee, net, profit,
    pct: low > 0 ? (profit / low) * 100 : 0,
  };
}

// ── Render ────────────────────────────────────────────────────────────────
function render() {
  const y = window.scrollY;
  renderBest();
  renderItems();
  renderInsights();
  window.scrollTo(0, y);
}

function candidates() {
  return state.items
    .filter(i => i.n >= CFG.minObservations && i.low > 0 && i.median > 0 && i.profit > 0)
    .sort((a, b) => b.pct - a.pct);
}

function renderBest() {
  $("feeHint").textContent =
    `buy at the low · sell at the median · ${Math.round(CFG.feeRate * 100)}% fee · ≥${CFG.minObservations} sightings`;
  const list = candidates().slice(0, CFG.topInvestments);
  const el = $("best");
  if (!list.length) {
    el.innerHTML = `<div class="empty">Nothing has enough observations in this window yet. Run the scanner loop and check back.</div>`;
    return;
  }
  el.innerHTML = list.map(i => `
    <div class="card${matches(i.name) ? " hit" : ""}" data-name="${esc(i.name)}">
      ${iconTag(i.icon, 28)}
      <div class="grow">
        <div class="row1">
          <span class="nm">${hl(i.name)}</span>
          <span class="pct ${i.pct >= 0 ? "up" : "down"}">${fmtPct(i.pct)}</span>
        </div>
        <div class="sub">buy ≤ ${fmt(i.low)} · sell ~${fmt(i.median)} · fee ${fmt(Math.round(i.fee))} (${i.n} seen)</div>
      </div>
    </div>`).join("");
}

function visibleItems() {
  const q = state.search.trim().toLowerCase();
  const list = state.items.filter(i => !q || i.name.toLowerCase().includes(q));
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

function renderItems() {
  const list = visibleItems();
  $("itemsHint").textContent = state.search.trim()
    ? `${list.length} match${list.length === 1 ? "" : "es"}`
    : `${list.length} items · tap a row for history · click headers to sort`;

  document.querySelectorAll("#items th").forEach(th => {
    th.classList.toggle("sorted", th.dataset.key === state.sort.key);
    if (th.dataset.key === state.sort.key) {
      th.textContent = th.textContent.replace(/ [▲▼]$/, "") + (state.sort.dir < 0 ? " ▼" : " ▲");
    }
  });

  const body = $("itemsBody");
  if (!list.length) {
    body.innerHTML = `<tr><td colspan="7" class="empty">no items</td></tr>`;
    return;
  }
  body.innerHTML = list.map(i => `
    <tr data-name="${esc(i.name)}">
      <td><div class="itemCell">${iconTag(i.icon, 20)}<span>${hl(i.name)}</span></div></td>
      <td class="num">${fmt(i.last)}</td>
      <td class="num">${fmt(i.low)}</td>
      <td class="num">${fmt(i.median)}</td>
      <td class="num">${fmt(i.high)}</td>
      <td class="num">${fmt(i.n)}</td>
      <td class="num ${i.pct >= 0 ? "up" : "down"}">${fmtPct(i.pct)}</td>
    </tr>`).join("");
}

function renderInsights() {
  const el = $("insights");
  if (!state.items.length) {
    el.innerHTML = `<div class="empty">no data yet</div>`;
    return;
  }

  // Hours come back in UTC; shift them onto the viewer's clock.
  const shift = -new Date().getTimezoneOffset() / 60;
  const localHour = h => (((h + shift) % 24) + 24) % 24;
  const hourCounts = new Array(24).fill(0);
  const hourIndex = new Array(24).fill(0);
  for (const h of state.hours) {
    const lh = localHour(Number(h.hour));
    if (lh >= 0 && lh < 24) {
      hourCounts[lh] = Number(h.n) || 0;
      hourIndex[lh] = Number(h.idx) || 0;
    }
  }
  const busiest = hourCounts.indexOf(Math.max(...hourCounts));
  let cheapest = -1, cheapestIdx = Infinity;
  hourIndex.forEach((v, h) => { if (v > 0 && v < cheapestIdx) { cheapestIdx = v; cheapest = h; } });

  const movers = computeMovers();
  const risers = movers.slice(0, 5);
  const fallers = movers.slice(-5).reverse();

  const fresh = state.items.filter(i => i.firstTs >= state.since).sort((a, b) => b.firstTs - a.firstTs);
  const busiestItem = [...state.items].sort((a, b) => b.n - a.n)[0];
  const active = state.items.filter(i => i.n >= CFG.minObservations);
  const winners = active.filter(i => i.profit > 0).length;

  el.innerHTML = `
    <div class="chips">
      <div class="chip">busiest hour <b>${busiest >= 0 && hourCounts[busiest] ? busiest + ":00" : "–"}</b></div>
      <div class="chip">cheapest hour <b>${cheapest >= 0 ? cheapest + ":00" : "–"}</b>${cheapest >= 0 ? ` (${fmtPct((cheapestIdx - 1) * 100)} vs typical)` : ""}</div>
      <div class="chip">most traded <b>${busiestItem ? esc(busiestItem.name) + " (" + busiestItem.n + ")" : "–"}</b></div>
      <div class="chip">new listings <b>${fresh.length}</b></div>
      <div class="chip">profitable items <b>${winners}/${active.length}</b></div>
    </div>
    <div class="grid2">
      <div class="box">
        <h3>Activity by hour (offers seen)</h3>
        ${barChart(hourCounts, { color: BLUE })}
      </div>
      <div class="box">
        <h3>Price level by hour <span class="hint">— below 1.0× is cheaper than usual</span></h3>
        ${indexChart(hourIndex, 1)}
      </div>
      <div class="box">
        <h3>Biggest risers (median, first → last bucket)</h3>
        ${moverList(risers, true)}
      </div>
      <div class="box">
        <h3>Biggest fallers</h3>
        ${moverList(fallers, false)}
      </div>
    </div>`;
}

function computeMovers() {
  const byName = new Map();
  for (const s of state.series) {
    if (!byName.has(s.name)) byName.set(s.name, []);
    byName.get(s.name).push(s);
  }
  const out = [];
  for (const [name, pts] of byName) {
    if (pts.length < 2) continue;
    pts.sort((a, b) => a.t - b.t);
    const first = pts[0].v, last = pts[pts.length - 1].v;
    if (!first) continue;
    out.push({ name, first, last, pct: ((last - first) / first) * 100 });
  }
  return out.sort((a, b) => b.pct - a.pct);
}

function moverList(list, up) {
  if (!list.length) return `<div class="empty">not enough buckets yet</div>`;
  return `<div class="movers">` + list.map(m => `
    <div class="mover" data-name="${esc(m.name)}">
      <span>${esc(m.name)}</span>
      <span class="${up ? "up" : "down"}">${fmtPct(m.pct)}</span>
    </div>`).join("") + `</div>`;
}

// ── Charts ────────────────────────────────────────────────────────────────
function lineChart(points, opts) {
  const o = Object.assign({ w: 600, h: 150, color: GOLD, xFmt: shortDate }, opts || {});
  if (!points || !points.length) return `<div class="empty">no data</div>`;
  const pad = { l: 56, r: 12, t: 12, b: 22 };
  const xs = points.map(p => p.t), ys = points.map(p => p.v);
  const x0 = Math.min(...xs), x1 = Math.max(...xs);
  let y0 = Math.min(...ys), y1 = Math.max(...ys);
  if (y0 === y1) { y0 -= 1; y1 += 1; }
  const sx = t => pad.l + (x1 === x0 ? 0.5 : (t - x0) / (x1 - x0)) * (o.w - pad.l - pad.r);
  const sy = v => (o.h - pad.b) - ((v - y0) / (y1 - y0)) * (o.h - pad.t - pad.b);
  const d = points.map((p, i) => `${i ? "L" : "M"}${sx(p.t).toFixed(1)},${sy(p.v).toFixed(1)}`).join(" ");
  const area = `${d} L${sx(x1).toFixed(1)},${o.h - pad.b} L${sx(x0).toFixed(1)},${o.h - pad.b} Z`;

  const grid = [y1, (y0 + y1) / 2, y0].map(v => `
    <line x1="${pad.l}" y1="${sy(v).toFixed(1)}" x2="${o.w - pad.r}" y2="${sy(v).toFixed(1)}" stroke="#3a3226"/>
    <text x="${pad.l - 6}" y="${(sy(v) + 3).toFixed(1)}" text-anchor="end">${fmtCompact(v)}</text>`).join("");

  const labels = points.length > 1 ? `
    <text x="${pad.l}" y="${o.h - 6}">${o.xFmt(points[0].t)}</text>
    <text x="${o.w - pad.r}" y="${o.h - 6}" text-anchor="end">${o.xFmt(points[points.length - 1].t)}</text>` : "";

  const dots = points.length <= 40
    ? points.map(p => `<circle cx="${sx(p.t).toFixed(1)}" cy="${sy(p.v).toFixed(1)}" r="2.2" fill="${o.color}"/>`).join("")
    : "";

  return `<svg viewBox="0 0 ${o.w} ${o.h}">${grid}
    <path d="${area}" fill="${o.color}" opacity="0.10"/>
    <path d="${d}" fill="none" stroke="${o.color}" stroke-width="2" stroke-linejoin="round"/>
    ${dots}${labels}</svg>`;
}

function barChart(values, opts) {
  const o = Object.assign({ w: 600, h: 130, color: BLUE }, opts || {});
  const n = values.length;
  if (!n) return `<div class="empty">no data</div>`;
  const pad = { l: 48, r: 8, t: 10, b: 20 };
  const max = Math.max(1, ...values);
  const bw = (o.w - pad.l - pad.r) / n;
  const bars = values.map((v, i) => {
    const h = (v / max) * (o.h - pad.t - pad.b);
    return `<rect x="${(pad.l + i * bw + 1).toFixed(1)}" y="${(o.h - pad.b - h).toFixed(1)}" width="${Math.max(1, bw - 2).toFixed(1)}" height="${Math.max(0, h).toFixed(1)}" fill="${o.color}" opacity="0.8"/>`;
  }).join("");
  const labels = values.map((_, i) => i % 3 === 0
    ? `<text x="${(pad.l + i * bw + bw / 2).toFixed(1)}" y="${o.h - 6}" text-anchor="middle">${i}</text>` : "").join("");
  return `<svg viewBox="0 0 ${o.w} ${o.h}">${bars}${labels}
    <text x="${pad.l - 6}" y="${pad.t + 8}" text-anchor="end">${fmtCompact(max)}</text>
    <text x="${pad.l - 6}" y="${o.h - pad.b}" text-anchor="end">0</text></svg>`;
}

/// Bars around a centre line: values below `center` (cheap) go down in green,
/// above (dear) go up in red. Used for the hour-of-day price index.
function indexChart(values, center) {
  const n = values.length;
  if (!n) return `<div class="empty">no data</div>`;
  const w = 600, h = 140, pad = { l: 46, r: 8, t: 10, b: 20 };
  const dev = Math.max(0.02, ...values.map(v => Math.abs(v - center)));
  const y = v => (h - pad.b) - ((v - (center - dev)) / (2 * dev)) * (h - pad.t - pad.b);
  const bw = (w - pad.l - pad.r) / n;
  const bars = values.map((v, i) => {
    if (!v) return "";
    const top = Math.min(y(v), y(center));
    const bh = Math.max(0.5, Math.abs(y(v) - y(center)));
    const color = v < center ? GREEN : RED;
    return `<rect x="${(pad.l + i * bw + 1).toFixed(1)}" y="${top.toFixed(1)}" width="${Math.max(1, bw - 2).toFixed(1)}" height="${bh.toFixed(1)}" fill="${color}" opacity="0.85"><title>${i}:00 · ${v.toFixed(3)}×</title></rect>`;
  }).join("");
  const labels = values.map((_, i) => i % 3 === 0
    ? `<text x="${(pad.l + i * bw + bw / 2).toFixed(1)}" y="${h - 6}" text-anchor="middle">${i}</text>` : "").join("");
  return `<svg viewBox="0 0 ${w} ${h}">
    <line x1="${pad.l}" y1="${y(center).toFixed(1)}" x2="${w - pad.r}" y2="${y(center).toFixed(1)}" stroke="#5a4d3a"/>
    <text x="${pad.l - 6}" y="${(y(center) + 3).toFixed(1)}" text-anchor="end">1.0×</text>
    <text x="${pad.l - 6}" y="${(y(center - dev) + 3).toFixed(1)}" text-anchor="end">${(center - dev).toFixed(2)}×</text>
    <text x="${pad.l - 6}" y="${(y(center + dev) + 3).toFixed(1)}" text-anchor="end">${(center + dev).toFixed(2)}×</text>
    ${bars}${labels}</svg>`;
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

  const pts = state.series.filter(s => s.name === name).sort((a, b) => a.t - b.t);
  const bucket = WINDOWS[state.win].bucket;
  const xFmt = bucket === "hour" ? shortHour : shortDate;

  sheet.innerHTML = `
    <div class="sheetHead">
      ${iconTag(it ? it.icon : "", 44)}
      <div class="nm">${esc(name)}</div>
      <button class="ghost" id="detailClose">Close</button>
    </div>
    ${it ? `
      <div class="stats">
        <div class="stat"><div class="k">Last</div><div class="v">${fmt(it.last)}</div></div>
        <div class="stat"><div class="k">Low</div><div class="v">${fmt(it.low)}</div></div>
        <div class="stat"><div class="k">Median</div><div class="v">${fmt(it.median)}</div></div>
        <div class="stat"><div class="k">High</div><div class="v">${fmt(it.high)}</div></div>
        <div class="stat"><div class="k">Seen</div><div class="v">${fmt(it.n)}</div></div>
        <div class="stat"><div class="k">Margin</div><div class="v ${it.pct >= 0 ? "up" : "down"}">${fmtPct(it.pct)}</div></div>
      </div>
      <div class="box" style="margin-bottom:12px">
        <h3>Buy ≤ ${fmt(it.low)} · sell ~${fmt(it.median)} → net ${fmt(Math.round(it.net))} (fee ${fmt(Math.round(it.fee))})</h3>
        <div class="hint">First seen ${dateTime(it.firstTs)} · last seen ${dateTime(it.lastTs)}</div>
      </div>
      <div class="box" style="margin-bottom:12px">
        <h3>Median price per ${bucket} (this ${WINDOWS[state.win].label.toLowerCase()})</h3>
        ${lineChart(pts, { color: GOLD, xFmt })}
      </div>` : `<div class="empty">No observations of this item in the current window.</div>`}
    <div class="box">
      <h3>Recent offers</h3>
      <div id="detailOffers" class="empty">loading…</div>
    </div>`;

  $("detailClose").onclick = closeDetail;

  try {
    const rows = await rpc("item_offers", { p_name: name, lim: 60 });
    $("detailOffers").className = "";
    $("detailOffers").innerHTML = !rows.length
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
    $("detailOffers").innerHTML = `<span class="warn">${esc(e.message)}</span>`;
  }
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

document.querySelector("#items thead").addEventListener("click", e => {
  const th = e.target.closest("th[data-key]");
  if (!th) return;
  const key = th.dataset.key;
  if (state.sort.key === key) state.sort.dir *= -1;
  else state.sort = { key, dir: key === "name" ? 1 : -1 };
  document.querySelectorAll("#items th").forEach(x => { if (x.dataset.key !== key) x.textContent = x.textContent.replace(/ [▲▼]$/, ""); });
  renderItems();
});

document.addEventListener("click", e => {
  const row = e.target.closest("[data-name]");
  if (row) openDetail(row.dataset.name);
});

$("detail").addEventListener("click", e => { if (e.target === $("detail")) closeDetail(); });
document.addEventListener("keydown", e => { if (e.key === "Escape") closeDetail(); });

// ── Boot ──────────────────────────────────────────────────────────────────
load();
setInterval(paintStatus, 1000);
if (CFG.autoRefreshSeconds > 0) {
  setInterval(() => load(true), CFG.autoRefreshSeconds * 1000);
}
