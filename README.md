# Elorin Market dashboard

Static page (no server, no build step) that reads the offers collected by
`market_scanner.exe` straight from Supabase, and shows:

- **Day / Week / Month** windows (24 h, 7 days, 30 days),
- **Best investments** — buy at the lowest price seen, resell at the median,
  after the 10% market fee: `margin% = (median × 0.9 − low) / low`,
- a sortable **items table** with search that filters and highlights matches,
- **item detail** (tap a row) — icon, stats, price trend, recent offers,
- **insights** — busiest/cheapest hour, activity by hour, price level by hour
  (normalised index), biggest risers and fallers.

## Configure

Everything lives in `config.js`:

| Key | Meaning |
|-----|---------|
| `supabaseUrl` | `https://<project-ref>.supabase.co` |
| `anonKey` | the **publishable** key (`sb_publishable_…`) — safe to publish |
| `feeRate` | market fee off the sale price (0.10 = 10%) |
| `minObservations` | minimum sightings before an item can rank as an investment |
| `topInvestments` | how many cards in the best-investments strip |
| `autoRefreshSeconds` | reload cadence (0 disables) |

The `sb_secret_…` key must never appear here — it stays in the scanner's local
`market.toml`.

## Deploy (free, GitHub Pages)

Settings → Pages → **Deploy from a branch** → `main` / `(root)` → Save.
The page appears at `https://<user>.github.io/elorin-market/`.

Open it on a phone and use "Add to Home Screen" for a full-screen app-like view.

## Update

Edit the files here and push:

```
git add -A && git commit -m "update dashboard" && git push
```

Schema and setup instructions live in the main project README (`supabase.sql`).
