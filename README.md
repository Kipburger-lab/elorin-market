# Elorin Market dashboard

Two static pages (no server, no build step) that read the offers collected by
`market_scanner.exe` straight from Supabase.

| Page | What it is |
|------|------------|
| `index.html` | **Dashboard** — public, read-only. Day/Week/Month windows, best investments after the 10% fee, search, per-item history, hourly insights. |
| `manager.html` | **Buy Manager** — private. Sets the rules the scanner obeys. |

## Configure

Everything lives in `config.js`:

| Key | Meaning |
|-----|---------|
| `supabaseUrl` | `https://<project-ref>.supabase.co` |
| `anonKey` | the **publishable** key (`sb_publishable_…`) — safe to publish |
| `feeRate` | market fee off the sale price (0.10 = 10%) |
| `minObservations` | minimum sightings before an item can rank as an investment |
| `topInvestments` | how many cards in the best-investments strip |
| `autoRefreshSeconds` | dashboard reload cadence (0 disables) |

The `sb_secret_…` key must never appear here — it stays in the scanner's local
`market.toml`.

## Buy Manager

Lists **every** item ever seen (new ones appear on their own, icon included) with
its price context, and writes the rules the scanner reads:

- **per item** — a `Buy` tick, a `Max price` threshold, and a per-session quantity
  cap. Once the scanner has bought that many, the row stops being eligible (and
  the manager flags it).
- **session rules** — a master switch (nothing is bought unless both it *and* the
  row are on), a **big-snipe profit floor** in absolute gp, and how many such
  snipes are allowed per session (`0` = never).
- A **SNIPE** badge marks rows whose estimated profit clears that floor — exactly
  the ones that consume the rationed slots.
- The log-scaled **spread bar** shows low / p10 / median / high with your
  threshold marked, so you can see where you're buying. `low` is often a 1 gp
  bait listing, which is why p10 is shown too.
- `1b` / `250m` / `20k` shorthand works in the price fields.

### It is private

These settings decide what the scanner spends money on, and the publishable key is
public, so the manager is gated twice over:

1. **The page** renders nothing but a sign-in card until you authenticate — and it
   is deliberately not linked from the public dashboard (`index.html` has no
   Manager link), so a visitor can't stumble onto it.
2. **The data** is unreadable anonymously: RLS on `watchlist` and `buy_settings`
   grants `select` to *authenticated* only, so even someone who knows the URL and
   the publishable key gets nothing from the API. The scanner reads them with the
   service key, which bypasses RLS.

Create the account in Supabase → Authentication → Users → **Add user** (tick
auto-confirm), then sign in. Signing out reloads the page back to the gate.

### Session counters

`bought` counts and the big-snipe tally reset per scanner run; **Reset** in the
manager zeroes them without restarting the scanner.

## Deploy (free, GitHub Pages)

Settings → Pages → **Deploy from a branch** → branch / `(root)` → Save.
Live at <https://kipburger-lab.github.io/elorin-market/>.

Note: Pages currently publishes the `gh-pages` branch, so a change pushed to
`main` also has to be merged into `gh-pages` (or switch the Pages source to
`main` and drop the extra branch).

Open it on a phone and use "Add to Home Screen" for a full-screen app-like view.
Bookmark `manager.html` directly — nothing links to it.

## Housekeeping

Raw rows grow ~1k/hour. Delete anything older than 90 days occasionally:

```sql
select public.prune_offers(90);
```
