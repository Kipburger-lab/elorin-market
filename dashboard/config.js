// Supabase (Project Settings → API Keys).
// `anonKey` is the PUBLISHABLE key (sb_publishable_…) — designed to be public:
// RLS only lets it SELECT. Never put the sb_secret_ key in this file.
window.ELORIN_CONFIG = {
  supabaseUrl: "https://izcytzlfresnwyfzrzpq.supabase.co",
  anonKey: "sb_publishable_lf6o8e9e5fep97tUSA1s7w_YDkV7HQ2",

  // Item icons live in this public Storage bucket.
  iconsBucket: "icons",

  // Market fee taken off the sale price (0.10 = 10%).
  feeRate: 0.10,

  // Ignore items seen fewer times than this when ranking investments — a single
  // observation is not a trend.
  minObservations: 4,

  // How many items to show in the best-investments strip.
  topInvestments: 12,

  // Reload the data every N seconds (0 = off). Background refreshes are quiet —
  // identical data doesn't re-render, so this can be small.
  autoRefreshSeconds: 5,
};
