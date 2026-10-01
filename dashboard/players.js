"use strict";

const CFG = Object.assign({
  supabaseUrl: "",
  anonKey: "",
  iconsBucket: "icons",
}, window.ELORIN_CONFIG || {});

const state = {
  sellers: [],
  search: "",
  updatedAt: 0,
  statusHtml: "loading…",
};

const $ = id => document.getElementById(id);

function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>'"]/g, c =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function fmt(n) {
  if (n == null || isNaN(n)) return "–";
  return Number(n).toLocaleString("en-US");
}

function dateTime(t) {
  if (!t) return "–";
  return new Date(t).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

function iconUrl(file) {
  if (!file) return "";
  return `${CFG.supabaseUrl}/storage/v1/object/public/${CFG.iconsBucket}/${encodeURIComponent(file)}`;
}

function iconTag(file, size) {
  if (!file) return `<span style="display:inline-block;width:${size}px"></span>`;
  return `<img src="${esc(iconUrl(file))}" alt="" loading="lazy" style="width:${size}px;height:${size}px;object-fit:contain;image-rendering:pixelated" onerror="this.style.visibility='hidden'">`;
}

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
  setStatus("loading…");
  try {
    const sellers = await rpc("seller_list", { p_search: state.search.trim(), lim: 200 });
    state.sellers = sellers || [];
    state.updatedAt = Date.now();
    setStatus(`${state.sellers.length} players`);
    render();
  } catch (e) {
    setStatus(`<span class="warn">${esc(e.message)}</span>`);
  }
}

function render() {
  const list = state.sellers;
  $("playersHint").textContent = list.length
    ? `${list.length} player${list.length === 1 ? "" : "s"}`
    : "no players found";

  const body = $("playersBody");
  if (!list.length) {
    body.innerHTML = `<tr><td colspan="3" class="empty">no players</td></tr>`;
    return;
  }
  body.innerHTML = list.map(s => `
    <tr data-seller="${esc(s.seller || "Unknown")}">
      <td>${esc(s.seller || "Unknown")}</td>
      <td class="num">${fmt(s.n)}</td>
      <td>${dateTime(Number(s.last_ts_ms))}</td>
    </tr>`).join("");
}

async function openSeller(seller) {
  const modal = $("detail"), sheet = $("detailSheet");
  modal.classList.remove("hidden");
  modal.setAttribute("aria-hidden", "false");
  sheet.innerHTML = `
    <div class="sheetHead">
      <div class="nm">${esc(seller)}</div>
      <button class="ghost" id="detailClose">Close</button>
    </div>
    <div class="box">
      <h3>All offers</h3>
      <div id="sellerOffers" class="empty">loading…</div>
    </div>`;
  $("detailClose").onclick = closeDetail;

  try {
    const rows = await rpc("seller_offers", { p_seller: seller, lim: 200 });
    const container = $("sellerOffers");
    container.className = "";
    container.innerHTML = !rows.length
      ? `<div class="empty">no offers</div>`
      : `<div class="tableWrap"><table class="offers">
          <thead><tr><th>When</th><th>Item</th><th class="num">Price</th><th class="num">Qty</th></tr></thead>
          <tbody>${rows.map(r => `<tr>
            <td>${dateTime(Number(r.ts_ms))}</td>
            <td><div class="itemCell">${iconTag(r.icon, 20)}<span>${esc(r.name)}</span></div></td>
            <td class="num">${fmt(r.price)}</td>
            <td class="num">${fmt(r.quantity)}</td>
          </tr>`).join("")}</tbody></table></div>`;
  } catch (e) {
    $("sellerOffers").innerHTML = `<span class="warn">${esc(e.message)}</span>`;
  }
}

function closeDetail() {
  $("detail").classList.add("hidden");
  $("detail").setAttribute("aria-hidden", "true");
}

$("search").addEventListener("input", e => {
  state.search = e.target.value;
  load();
});
$("reload").addEventListener("click", load);

$("players").addEventListener("click", e => {
  const row = e.target.closest("[data-seller]");
  if (row) openSeller(row.dataset.seller);
});

$("detail").addEventListener("click", e => { if (e.target === $("detail")) closeDetail(); });
document.addEventListener("keydown", e => { if (e.key === "Escape") closeDetail(); });

load();
setInterval(paintStatus, 1000);
