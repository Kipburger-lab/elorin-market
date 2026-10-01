-- Elorin Market — Supabase schema.
--
-- Paste this whole file into the Supabase SQL editor (Dashboard → SQL Editor →
-- New query → Run) once. It is idempotent, so re-running it is safe.
--
-- The scanner writes rows with the service_role key (kept on your machine); the
-- dashboard reads with the public anon key, which can only SELECT.

-- ── Raw offers ───────────────────────────────────────────────────────────────
create table if not exists public.offers (
  id       bigint generated always as identity primary key,
  ts_ms    bigint  not null,                 -- capture time (epoch ms, UTC)
  name     text    not null,
  seller   text,
  price    bigint  not null,
  quantity integer default 1,
  icon     text                               -- icon file name in the bucket
);

create index if not exists offers_ts_idx    on public.offers (ts_ms desc);
create index if not exists offers_name_idx  on public.offers (name, ts_ms desc);

-- ── Access ───────────────────────────────────────────────────────────────────
-- Anyone with the anon key may read; only the service_role (the scanner) writes.
alter table public.offers enable row level security;

drop policy if exists "public read" on public.offers;
create policy "public read" on public.offers for select using (true);

grant select on public.offers to anon, authenticated;

-- ── Day / Week / Month: per-item aggregates over a window ────────────────────
-- Called by the dashboard as: rpc/item_stats  {"since_ms": <epoch ms>}
--
-- A function's return type is fixed at creation, so if you later change the
-- column list below, drop it first:
--   drop function public.item_stats(bigint);
-- (same for item_series / hour_index / recent_offers / item_offers)
create or replace function public.item_stats(since_ms bigint)
returns table (
  name        text,
  n           bigint,
  low         bigint,
  high        bigint,
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
    max(price),
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

-- ── Trends: per-item, per-bucket series ─────────────────────────────────────
-- bucket is 'hour' or 'day'.   rpc/item_series  {"since_ms": …, "bucket": "day"}
create or replace function public.item_series(since_ms bigint, bucket text)
returns table (
  name   text,
  b      timestamptz,
  n      bigint,
  low    bigint,
  high   bigint,
  avg    double precision,
  median double precision
)
language sql stable as $$
  select
    name,
    date_trunc(bucket, to_timestamp(ts_ms / 1000.0)) as b,
    count(*)::bigint,
    min(price),
    max(price),
    avg(price)::double precision,
    percentile_cont(0.5) within group (order by price)
  from public.offers
  where ts_ms >= since_ms
  group by name, b;
$$;

-- ── Recent raw rows (live feed) ─────────────────────────────────────────────
-- rpc/recent_offers  {"lim": 200}
create or replace function public.recent_offers(lim int default 200)
returns setof public.offers
language sql stable as $$
  select * from public.offers order by ts_ms desc limit lim;
$$;

-- ── One item's offers (detail view) ─────────────────────────────────────────
-- rpc/item_offers  {"p_name": "Ruby", "lim": 60}
create or replace function public.item_offers(p_name text, lim int default 60)
returns setof public.offers
language sql stable as $$
  select * from public.offers where name = p_name order by ts_ms desc limit lim;
$$;

-- ── Insights: activity + price level by hour-of-day ────────────────────────
-- Each offer is normalised by its item's median over the window, so items of
-- wildly different value contribute equally: idx = 1.0 means that hour is
-- typical, below 1 means it is cheap (a good time to buy), above 1 is dear.
-- Hours are UTC — the dashboard shifts them to the viewer's local time.
-- rpc/hour_index  {"since_ms": …}
create or replace function public.hour_index(since_ms bigint)
returns table (hour int, n bigint, idx double precision)
language sql stable as $$
  with med as (
    select name, percentile_cont(0.5) within group (order by price) as m
    from public.offers
    where ts_ms >= since_ms
    group by name
  )
  select
    extract(hour from to_timestamp(o.ts_ms / 1000.0))::int as hour,
    count(*)::bigint,
    percentile_cont(0.5) within group (order by o.price / nullif(med.m, 0))
  from public.offers o
  join med on med.name = o.name
  where o.ts_ms >= since_ms
  group by 1;
$$;

grant execute on function public.item_stats(bigint)             to anon, authenticated;
grant execute on function public.item_series(bigint, text)      to anon, authenticated;
grant execute on function public.recent_offers(int)             to anon, authenticated;
grant execute on function public.item_offers(text, int)         to anon, authenticated;
grant execute on function public.hour_index(bigint)             to anon, authenticated;

-- ── Housekeeping: drop raw rows older than N days ──────────────────────────
-- Run manually, or schedule it with pg_cron:
--   select cron.schedule('prune-offers', '0 4 * * *', $$select public.prune_offers(90)$$);
create or replace function public.prune_offers(keep_days int default 90)
returns bigint
language plpgsql as $$
declare
  removed bigint;
begin
  delete from public.offers
   where ts_ms < (extract(epoch from now()) * 1000)::bigint - keep_days::bigint * 86400000;
  get diagnostics removed = row_count;
  return removed;
end;
$$;

-- ── Storage: public bucket for item icons ──────────────────────────────────
insert into storage.buckets (id, name, public)
values ('icons', 'icons', true)
on conflict (id) do update set public = true;

drop policy if exists "icons public read" on storage.objects;
create policy "icons public read" on storage.objects
  for select using (bucket_id = 'icons');
