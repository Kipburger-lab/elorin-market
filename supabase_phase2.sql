-- Elorin Market — Phase 2: buy manager schema.
--
-- Paste into Supabase → SQL Editor → New query → Run. Idempotent: safe to re-run.
--
-- Adds the buy rules the manager edits and the scanner consumes, plus a
-- junk-resistant price floor (p10) on item_stats.

-- ── Per-item buy rules ─────────────────────────────────────────────────────
create table if not exists public.watchlist (
  name       text primary key,
  buy        boolean not null default false,
  max_price  bigint,                        -- buy only at or below this price
  qty_limit  integer not null default 1,    -- per scanner session
  bought     integer not null default 0,    -- written by the scanner
  updated_at timestamptz not null default now()
);

-- ── Global buy settings (single row) ───────────────────────────────────────
create table if not exists public.buy_settings (
  id            int primary key default 1 check (id = 1),
  enabled       boolean not null default false,       -- master switch
  min_margin    bigint  not null default 1000000000,  -- profit (gp) that counts as a "big snipe"
  max_snipes    integer not null default 2,           -- per session; 0 = never
  snipes_used   integer not null default 0,
  session_start timestamptz,
  updated_at    timestamptz not null default now()
);
insert into public.buy_settings (id) values (1) on conflict (id) do nothing;

-- ── Access: the buy rules are private ──────────────────────────────────────
-- `public.offers` stays world-readable (the dashboard is the shop window), but
-- the watchlist and settings are readable ONLY by the signed-in owner — so
-- nobody anonymous can see what we buy or at what price, even with the public
-- key. The scanner reads them with the service key, which bypasses RLS.
alter table public.watchlist    enable row level security;
alter table public.buy_settings enable row level security;

drop policy if exists "watchlist read"  on public.watchlist;
drop policy if exists "watchlist write" on public.watchlist;
create policy "watchlist read"  on public.watchlist for select to authenticated using (true);
create policy "watchlist write" on public.watchlist for all to authenticated using (true) with check (true);

drop policy if exists "settings read"  on public.buy_settings;
drop policy if exists "settings write" on public.buy_settings;
create policy "settings read"  on public.buy_settings for select to authenticated using (true);
create policy "settings write" on public.buy_settings for all to authenticated using (true) with check (true);

revoke select on public.watchlist, public.buy_settings from anon;
grant  select on public.watchlist, public.buy_settings to authenticated;
grant insert, update, delete on public.watchlist, public.buy_settings to authenticated;

-- ── item_stats gains p10 (raw `low` is often a 1 gp bait listing) ───────────
-- A function's return type is fixed, so drop before recreating.
drop function if exists public.item_stats(bigint);
create function public.item_stats(since_ms bigint)
returns table (
  name        text,
  n           bigint,
  low         bigint,
  p10         bigint,
  median      double precision,
  avg         double precision,
  last_price  bigint,
  last_ts_ms  bigint,
  first_ts_ms bigint,
  icon        text
)
language sql stable as $$
  select
    name,
    count(*)::bigint,
    min(price),
    round(percentile_cont(0.1) within group (order by price))::bigint,
    percentile_cont(0.5) within group (order by price),
    avg(price)::double precision,
    (array_agg(price order by ts_ms desc))[1],
    max(ts_ms),
    min(ts_ms),
    (array_agg(icon order by ts_ms desc))[1]
  from public.offers
  where ts_ms >= since_ms
  group by name;
$$;

-- ── What the scanner asks for: the rules, joined to price context ──────────
-- rpc/buy_rules  {"since_ms": …}  → only rows ticked for buying.
drop function if exists public.buy_rules(bigint);
create function public.buy_rules(since_ms bigint)
returns table (
  name       text,
  max_price  bigint,
  qty_limit  integer,
  bought     integer,
  median     double precision,
  p10        bigint,
  last_price bigint,
  icon       text
)
language sql stable as $$
  with stats as (
    select
      name,
      percentile_cont(0.5) within group (order by price) as median,
      round(percentile_cont(0.1) within group (order by price))::bigint as p10,
      (array_agg(price order by ts_ms desc))[1] as last_price,
      (array_agg(icon order by ts_ms desc))[1] as icon
    from public.offers
    where ts_ms >= since_ms
    group by name
  )
  select w.name, w.max_price, w.qty_limit, w.bought, s.median, s.p10, s.last_price, s.icon
  from public.watchlist w
  join stats s on s.name = w.name
  where w.buy
  order by w.name;
$$;

grant execute on function public.item_stats(bigint) to anon, authenticated;
grant execute on function public.buy_rules(bigint)  to anon, authenticated;

-- ── Session start: fresh counters for each scanner run ────────────────────
-- The scanner calls this once at boot. A "session" is one run of
-- market_scanner.exe, so the per-item quantity caps and the big-snipe ration
-- start over each time you launch it.
create or replace function public.begin_session()
returns void
language sql as $$
  update public.buy_settings
     set snipes_used = 0, session_start = now(), updated_at = now()
   where id = 1;
  update public.watchlist set bought = 0 where buy;
$$;

grant execute on function public.begin_session() to anon, authenticated;

-- ── Scanner heartbeat: what the scanner is doing, and that it's alive ─────
-- The manager shows this, so "why didn't it buy?" and "is the loop still
-- running?" are answered on the page instead of in a log file.
alter table public.buy_settings add column if not exists scanner_dry_run boolean not null default true;
alter table public.buy_settings add column if not exists scanner_seen    timestamptz;

create or replace function public.scanner_heartbeat(p_dry_run boolean)
returns void
language sql as $$
  update public.buy_settings
     set scanner_dry_run = p_dry_run, scanner_seen = now()
   where id = 1;
$$;

grant execute on function public.scanner_heartbeat(boolean) to anon, authenticated;

-- ── One item's trend (manager row expander) ────────────────────────────────
-- rpc/item_series_for  {"p_name": "Ruby", "since_ms": …, "bucket": "day"}
drop function if exists public.item_series_for(text, bigint, text);
create function public.item_series_for(p_name text, since_ms bigint, bucket text default 'day')
returns table (
  b      timestamptz,
  n      bigint,
  low    bigint,
  high   bigint,
  median double precision
)
language sql stable as $$
  select
    date_trunc(bucket, to_timestamp(ts_ms / 1000.0)) as b,
    count(*)::bigint,
    min(price),
    max(price),
    percentile_cont(0.5) within group (order by price)
  from public.offers
  where name = p_name and ts_ms >= since_ms
  group by 1
  order by 1;
$$;

grant execute on function public.item_series_for(text, bigint, text) to anon, authenticated;
