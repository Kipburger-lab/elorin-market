# Elorin Bot

Local Win32 screen-reading tools for the Elorin client. No injection, no Java
agent, no client launching — purely window detection, GDI capture and GDI
overlays, the same techniques used by `Elorin/src/indicator.rs`.

Two binaries share a common library (`elorin_bot`):

| Binary | What it does |
|--------|--------------|
| `inventory_overlay.exe` | Calibration tool: draws a magenta box over each of the 28 inventory slots. |
| `market_scanner.exe`    | Reads the market "Most Recent Offers" prices (Open buttons + digit OCR). |

## Layout

```
src/lib.rs        shared: window, capture, search, overlay, hotkeys, market
src/bin/inventory_overlay/   the inventory calibration tool
src/bin/market_scanner.rs    the market price reader
```

Build & test:

```
cargo build --release
cargo test
```

After building, copy the exe(s) to the project root so they sit next to their
config (`copy /Y target\release\market_scanner.exe market_scanner.exe`).

---
---

# Inventory Slot Overlay (`inventory_overlay.exe`)

While the **Elorin client window is focused**, draws a **magenta rectangle over
each of the 28 inventory slots** (4 columns x 7 rows) as a transparent,
click-through, always-on-top overlay. Nudge the grid live until it is
pixel-perfect, then **save** the resolved slot coordinates.

## Run

Double-click `inventory_overlay.exe` (or `cargo run --release --bin inventory_overlay`).
Click into the Elorin client (inventory tab open). The magenta boxes appear only
while the client is focused; they hide when you alt-tab away. The game remains
fully playable underneath (the overlay is click-through).

## Controls (only while the client is focused; the keys never reach the game)

| Key | Action |
|-----|--------|
| `1`..`6` | Select the active parameter: `base_x`, `base_y`, `slot_w`, `slot_h`, `step_x`, `step_y` |
| `[` / `]` | Nudge the active parameter by 1 px (hold **Shift** for 5 px) |
| `S` | Save: write `inventory_slots.json` + rewrite `overlay.toml` |
| `R` | Reload `overlay.toml` (discard live tweaks) |
| `H` | Toggle the on-screen parameter HUD |
| `F10` | Quit |

Calibration tips: move the whole grid with `base_x`(1)/`base_y`(2); size slots
with `slot_w`(3)/`slot_h`(4); change spacing with `step_x`(5)/`step_y`(6).

## Output

`inventory_slots.json` (next to the exe) — client-relative slot boxes + centers:

```json
{
  "window_title": "Elorin",
  "grid": { "base_x": 567, "base_y": 241, "cols": 4, "rows": 7,
            "slot_w": 35, "slot_h": 31, "step_x": 42, "step_y": 36 },
  "slots": [ { "index": 0, "row": 0, "col": 0, "x1": 567, "y1": 241, "x2": 602,
               "y2": 272, "cx": 584, "cy": 256 }, ... ]
}
```

Config `overlay.toml`:

```toml
[window]
title_contains = "Elorin"

[grid]
base_x = 567
base_y = 241
cols = 4
rows = 7
slot_w = 35
slot_h = 31
step_x = 42
step_y = 36

[overlay]
show_hud = true
```

Slot `(col, row)`: `x1 = base_x + col*step_x`, `y1 = base_y + row*step_y`,
`x2 = x1 + slot_w`, `y2 = y1 + slot_h`.

---
---

# Market Scanner (`market_scanner.exe`)

Focus the Elorin client with the market **Most Recent Offers** list open. For each
row it finds the **Open** button, then reads three things:

- **price** — the digit glyphs to its left (comma separators are ignored; digits
  are concatenated left-to-right);
- **item name** — OCR of the orange text left of the price;
- **seller** — OCR of the bright shop name right of the price (used to tell a
  genuinely new offer from one that was already visible).

It also screenshots the **item icon** (left of the name) once per unique item name.

Two modes:
- **F8** — one-shot scan (writes a snapshot + debug dump + the captured frame).
- **F7** — refresh loop: click **Refresh**, poll the offers list every few ms until
  it changes (or 2 s elapse) → on a change, store only the rows that are **new vs
  the previous scan** (with the change timestamp + linked icon) → refresh again. If
  nothing changed, nothing is stored.

The price algorithm mirrors ABI TRADER: collect every digit-template match in the
band, cluster candidates by x into digit slots, keep the lowest-error candidate
per slot, then concatenate. Names/sellers go through the built-in Windows OCR.

## Controls (only while the client is focused)

| Key | Action |
|-----|--------|
| `F8` | Scan once: capture, read prices/names/sellers, write output |
| `F7` | Toggle the refresh loop (refresh → wait for change → store new rows) |
| `F9` | Toggle the debug boxes (digit glyphs) |
| `F10` | Quit |

The overlay draws: a magenta box around each detected **Open** button, a gray box
for the digit band, an orange box for the **item icon**, a cyan box for the
**name**, a green box for the **seller**, and the price + `name | seller` to the
right of each button. While the loop is running the overlay hides itself (so its
pixels can't be captured and disturb change detection).

## Output

- `data/market_prices.json` — snapshot of the latest scan (all rows).
- `data/market_history.jsonl` — one full snapshot per **F8** scan.
- `data/market_changes.jsonl` — **the dashboard data**: one line per genuinely new
  row found by the loop, with the change timestamp and linked icon:
  ```json
  { "ts_unix_ms": 1790706137951, "name": "Magic fang", "seller": "yellowman",
    "price": 150000000, "y": 107, "icon": "data/icons/Magic_fang.png" }
  ```
- `data/icons/<item name>.png` — one icon screenshot per unique item name.
- `data/market_debug.json`, `data/last_scan.png` — diagnostics.

Names/sellers use the **built-in Windows OCR** (`Windows.Media.Ocr`), local and
dependency-free. Each region is binarized (text → black on white), given a white
margin, and upscaled — which measurably improves accuracy on the client's tiny
bitmap font. Names are truncated by the client (e.g. `Gilded plateleg...`), so
treat them as prefix labels. Set `[name]`/`[seller] enable = false` to skip OCR.

## Configuration (`market.toml`)

```toml
[window]
title_contains = "Elorin"

[scan]
open_tolerance  = 20    # per-channel tolerance for Open.png
digit_tolerance = 35    # per-channel tolerance for the digit glyphs
digit_left      = 520   # band: px left of the Open button to start scanning
digit_gap       = 4     # gap between the band's right edge and the button
pad_y           = 3     # vertical padding around the button row
open_merge_px   = 6     # merge Open matches within this distance into one button
overlap_ratio   = 0.5   # glyphs overlapping by more than this are the same digit

[name]                 # item name (orange text)
enable = true
left = 305             # x1 = open.x - left   (92)
right = 195            # x2 = open.x - right  (202)
y_offset = 2           # y1 = open.y + y_offset
height = 18            # y2 = y1 + height
scale = 4              # upscale factor before OCR
pad = 6                # white margin around the text
rule = "orange"

[quantity]             # stack count (yellow number at the top of the icon)
enable = true          # no number shown => quantity 1
# Read by shape-matching the tiny digits against the price digit sprites
# (Windows OCR ignores glyphs this small and isolated). `rule` isolates the ink.
left = 372             # x1 = 25
right = 318            # x2 = 79
y_offset = -11         # y1 = open.y - 11 (digits sit at open.y-8 .. open.y-1)
height = 14
scale = 4
pad = 6
rule = "yellow"

[seller]               # shop name (bright text, right of the price)
enable = true
left = 95              # x1 = 302
right = 15             # x2 = 382
y_offset = 2
height = 15
scale = 4
pad = 6
rule = "bright"

[icon]                 # item icon (left of the name)
enable = true          # starts just inside the cell border (x=44 / open.y-10)
left = 352             # x1 = 45
right = 302            # x2 = 95
y_offset = -9          # y1 = open.y - 9
height = 31            # y2 = open.y + 22
dir = "data/icons"
trim = true            # tighten the saved crop to the sprite
remove_bg = true       # key out the panel background (transparent PNG)
bg_tolerance = 6       # match radius vs the panel colour palette

[watch]                # change-detection region (offers list only)
x1 = 92
y1 = 100
x2 = 390
y2 = 326

[refresh]
poll_ms = 5
timeout_ms = 2000
tolerance = 25         # per-channel tolerance for Refresh.png

[output]
file = "data/market_prices.json"
history = "data/market_history.jsonl"
changes = "data/market_changes.jsonl"
debug = "data/market_debug.json"   # every digit candidate per row
frame = "data/last_scan.png"       # captured frame (overlay-free)
save_frame = true

[hotkeys]
scan = "f8"
loop = "f7"
debug = "f9"
quit = "f10"

[cloud]                # push every collected offer to the online dashboard
enable = false         # see supabase.sql + the "Online Dashboard" section
url = "https://YOUR-PROJECT.supabase.co"
service_key = ""       # service_role key (write access) — stays on this machine
table = "offers"
icons_bucket = "icons"
flush_ms = 10000       # queue → cloud push cadence
batch_max = 200        # rows per request
```

**Change detection**: after clicking Refresh, only the `[watch]` rectangle is
captured (5 ms apart) and hashed — never the animated game world/chat — so the
signature stays stable until the offers list actually updates.

Calibrating: if a box is off, press `F8`, look at the overlay (magenta = Open,
orange = icon, cyan = name, green = seller, gray = digit band), and adjust the
matching region or tolerances in `market.toml`, then scan again. `data/last_scan.png`
(the frame the reader saw, overlay hidden) and `data/market_debug.json` (every digit
candidate with its position and error score) are there for diagnosis.

Sprites live in `Sprites/Utility/`: `Buttons/Open.png`, `Buttons/Refresh.png`,
`Market digits/0-9.png`.

## Notes

- Logs roll daily to `logs/` next to the exe (`elorin-bot.log`, `market_scanner.log`).
- **Close these tools before running the Elorin automation scripts.** Their
  overlays are captured by `CAPTUREBLT` screen grabs and would be picked up by the
  scripts' `find_magenta_centroid` target marker.

---

---

# Online Dashboard (free: Supabase + GitHub Pages)

The scanner can push every offer it collects to a free Supabase Postgres database;
`dashboard/` is a static web app (no server) that reads it with the public anon key
and works on a phone, laptop, tablet — anywhere.

```
Elorin Bot (this machine)            Supabase (free)                GitHub Pages (free)
  F7 loop ──queued rows──▶ POST /rest/v1/offers        ◀──reads──  dashboard/index.html
                                        item_stats() etc. (SQL)
  new icons ──▶ PUT /storage/v1/object/icons/…
```

## 1. Supabase (one-time, ~2 minutes)

1. Sign up at <https://supabase.com> (free) and create a project.
2. **SQL Editor → New query** → paste all of `supabase.sql` → **Run**.
   That creates the `offers` table, the aggregate functions (`item_stats`,
   `item_series`, `recent_offers`, `item_offers`, `hour_index`), read-only RLS,
   the `icons` Storage bucket and the `prune_offers()` housekeeping function.
3. **Project Settings → API**: copy the **Project URL**, the **anon public key**
   and the **service_role key**.

## 2. Point the scanner at it

In `market.toml`:

```toml
[cloud]
enable = true
url = "https://<your-project>.supabase.co"
service_key = "<service_role key>"
```

Restart `market_scanner.exe`. Rows are queued to `data/cloud_queue.jsonl` and
pushed in batches (default every 10 s); icons are uploaded once per item. The
scanner never blocks on the network, and anything collected while offline is
pushed as soon as the connection returns. `service_role` is a **write** key — keep
it on this machine only.

## 3. The dashboard

`dashboard/` (index.html, app.js, styles.css, config.js, README.md) is deployed to
the public repo **<https://github.com/Kipburger-lab/elorin-market>**; with
**Settings → Pages → Deploy from a branch → `main` / `(root)`** it is served at
**<https://kipburger-lab.github.io/elorin-market/>**.

Edit `config.js` first:

```js
window.ELORIN_CONFIG = {
  supabaseUrl: "https://<your-project>.supabase.co",
  anonKey: "<anon public key>",   // safe to publish: RLS allows SELECT only
  iconsBucket: "icons",
  feeRate: 0.10,                  // 10% market fee off the sale price
  minObservations: 4,             // ignore items seen fewer times than this
  topInvestments: 12,
  autoRefreshSeconds: 60,
};
```

The anon key is meant to be public — the RLS policy in `supabase.sql` only lets it
read. Adding the page to your phone's home screen gives it an app-like icon.

## What it shows

- **Day / Week / Month** toggle — last 24 h, 7 days, 30 days (aggregated in SQL,
  so the browser only receives small summaries).
- **Best investments** — for each item: buy at the **lowest** price seen, resell at
  the **median**, minus the 10% fee:
  `margin% = (median × 0.9 − low) / low`. Items with fewer than
  `minObservations` sightings are excluded so thin data can't top the list.
  *We only see sell offers, so the resale value is an estimate — treat it as a
  signal, not a guarantee.*
- **Items** table — last / low / median / high / sightings / margin, sortable by any
  column, with a search box that filters and highlights matches.
- **Item detail** (tap a row) — icon, stats, median-price trend chart for the
  window, and the item's recent offers (time, seller, price, quantity).
- **Insights** — busiest and cheapest hour of day, most-traded item, new listings,
  profitable-item count, an activity-by-hour chart, a **price-level-by-hour index**
  (each offer normalised by its item's median, so hour 20:00 at 0.77× means items
  are ~23% cheaper than usual then — the buy window), and the biggest risers/fallers
  between buckets. Hours are shown in your local time.

## Housekeeping

Raw rows grow ~1k/hour. Delete everything older than 90 days occasionally:

```sql
select public.prune_offers(90);
```

Or schedule it daily with `pg_cron`:

```sql
select cron.schedule('prune-offers', '0 4 * * *', $$select public.prune_offers(90)$$);
```

Free projects pause after ~a week of total inactivity; continuous scanning keeps it
awake, and resuming is one click.

---

---

# Phase 2 — Buy Manager (`manager.html`)

The dashboard is read-only; the **manager** is the controller. It lists every item
ever seen and writes the rules the scanner obeys.

## Setup (once)

1. Run **`supabase_phase2.sql`** in the SQL editor (same as before — idempotent).
   It adds `watchlist`, `buy_settings`, their RLS policies, the `p10` price floor
   on `item_stats`, and the `buy_rules` / `item_series_for` functions.
2. Create a login: **Authentication → Users → Add user**, set an email + password
   and tick auto-confirm. This is the account you sign into the manager with —
   writes are restricted to signed-in users because the anon key is public.

## What you control

Per item: a **Buy** tick, a **Max price** threshold, and a per-session **Qty**
cap. Globally: a master switch, the **big-snipe profit floor**, and how many big
snipes are allowed per session (`0` = never).

**Big snipe = absolute profit**, not a percentage. A 50M item sniped for 1000 is
a 99.99% "margin" worth 45M; a 25B item sniped for 250M is worth ~22B. Only the
second is worth a rationed slot, so the rule is `estimated profit ≥ your floor`
(profit is `median × 0.9 − price`, i.e. after the 10% fee). The manager badges the
rows that qualify.

The scanner will only ever buy an item that is **both** ticked *and* under the
master switch, at or below its max price, until its quantity cap is reached.

## Scanner side (`[buy]` in `market.toml`)

```toml
[buy]
enable = true
dry_run = true     # ← reports only; nothing is clicked while true
poll_ms = 5000     # refetch the rules this often
lookback_days = 30 # window for the median/p10 the profit estimate uses
```

With `dry_run = true` the scanner prints what it *would* do after each scan:

```
  BUY? [DRY RUN] Voting token @ 4,000,000 (max 4,000,000) · resale~6,379,483 · profit 2,379,483 (+59%)
  skip [DRY RUN] Osmumten's fang @ 250,000,000 · BIG SNIPE 2/2 · BLOCKED: snipe ration spent
```

Leave it on until the log matches what you'd do by hand; `dry_run = false` is the
switch that makes it click. `bought` counters and the snipe tally reset per
scanner run (**Reset** in the manager does it without restarting).

## Buying (phase 2b)

With `dry_run = false`, a qualifying offer triggers this, once per scan pass:

```
1. click Open on that row
2. wait for the seller's shop to replace the market panel
3. find the glowing item in the shop → right-click it
4. search ≤3s for "Buy X" (stack > 1) or "Buy 1" near it → click it
5. Buy X only: search ≤3s for "Enter amount"
                 → wait 1s → type min(stack, cap − bought) → Enter
6. record it (bought += units; consume a snipe if it was one; untick at the cap)
7. return to the market (only pressing the close key if it didn't come back itself)
```

**The item is found by its glow, not by its icon.** The game wraps the item you
searched for in a bright yellow marker, and that marker is identical for every
item. Comparing the icon itself cannot work in general: the shop renders it a few
pixels off from the market's copy (verified against real frames — same shape, 3 px
shorter), so a pixel-perfect match fails for *any* item, whatever its outline.
The raw icon comparison is kept only as a fallback.

Two guards learned from live failures:

- **The shop must replace the market before anything is matched.** Without that
  wait the icon search finds the item sitting in the market list itself — the one
  place it is guaranteed to be.
- **Buy 1 / Buy X must appear near the item** that was right-clicked (within
  `menu_radius`), so a coincidental match elsewhere is a non-event rather than a
  stray click.

### Debug frames

Every attempt writes PNGs to `data/buy/<timestamp>_<stage>.png`:

| Stage | Shows |
|-------|-------|
| `01_market` | the offers list and the row we're opening |
| `02_shop` | the seller's shop — where the glow must be found |
| `03_menu` | the right-click menu with Buy 1 / Buy X |
| `04_amount` | the "Enter amount" prompt |
| `05_after` | the aftermath |

A failed step saves the frame it timed out on, so a miss is diagnosable from the
images rather than guesswork. Set `debug_dir = ""` to switch it off. A test
(`finds_the_glowing_item_in_a_real_shop_frame`) replays the newest captured pair
and asserts the glow is found, so this path can't silently regress.

### First live run (deliberately small)

1. Tick **one cheap item**, stack of 1, `qty_limit = 1`, and set the master switch on.
2. `dry_run = false`, restart the scanner, press **F7**.
3. Read the `data/buy/*.png` frames together and confirm each step landed.
4. Check the manager: `bought` should read 1 and the item should have unticked
   itself at the cap.

Only widen to the real targets once that pass is clean.

`close_key` (default `escape`) leaves the shop; if the market doesn't come back
after three tries the scanner logs it and the loop retries on its own.
