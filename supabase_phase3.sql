-- Elorin Market — Phase 3: server-side housekeeping + scanner dead-man's switch.
--
-- Paste into Supabase → SQL Editor → New query → Run. Idempotent: safe to re-run.
--
-- Two jobs, both scheduled with pg_cron:
--   1. alert_if_scanner_dead() — pings Discord when the scanner stops checking in.
--      A dead scanner cannot post anything itself, so the alert has to live here.
--   2. prune_offers(90)        — drops raw offers older than 90 days.
--
-- Two things are deliberately NOT in this file:
--   * the Discord webhook URL — it would be a secret in a public repo. You add
--     it yourself, once (see the `insert` at the bottom, commented).
--   * whether the extensions are on — enable them in Dashboard → Database →
--     Extensions, or let the `create extension` lines below do it.

create extension if not exists pg_cron;
create extension if not exists pg_net;

-- ── Where to alert, and when we last did ───────────────────────────────────
create table if not exists public.scanner_alerts (
  id            int primary key default 1 check (id = 1),
  webhook_url   text not null default '',   -- set this (Discord webhook)
  stale_minutes int  not null default 10,   -- silence that counts as "dead"
  last_alert_at timestamptz                 -- so we don't spam while it stays dead
);
insert into public.scanner_alerts (id) values (1) on conflict (id) do nothing;

alter table public.scanner_alerts enable row level security;
revoke select on public.scanner_alerts from anon;
grant select, insert, update on public.scanner_alerts to authenticated;

-- ── The check ──────────────────────────────────────────────────────────────
-- Reads the heartbeat the scanner publishes (scanner_seen, refreshed ~every
-- 20 s). Silent for longer than `stale_minutes` => post to Discord, at most
-- once an hour so a long outage doesn't flood the channel.
create or replace function public.alert_if_scanner_dead()
returns void
language plpgsql
security definer
set search_path = public, net
as $$
declare
  cfg   public.scanner_alerts;
  seen  timestamptz;
  quiet interval;
begin
  select * into cfg from public.scanner_alerts where id = 1;
  if cfg.webhook_url = '' then
    return;                                   -- not configured yet
  end if;
  if cfg.last_alert_at is not null and now() - cfg.last_alert_at < interval '1 hour' then
    return;                                   -- already shouted recently
  end if;

  select scanner_seen into seen from public.buy_settings where id = 1;
  quiet := now() - coalesce(seen, to_timestamp(0));
  if quiet < make_interval(mins => cfg.stale_minutes) then
    return;                                   -- alive
  end if;

  perform net.http_post(
    url     := cfg.webhook_url,
    body    := jsonb_build_object(
                 'username', 'Elorin Bot',
                 'content', format('Scanner silent for %s — market_scanner.exe may be down.',
                                   to_char(quiet, 'HH24:MI:SS'))
               ),
    headers := jsonb_build_object('Content-Type', 'application/json')
  );

  update public.scanner_alerts set last_alert_at = now() where id = 1;
end;
$$;

-- ── Schedule ───────────────────────────────────────────────────────────────
-- pg_cron takes whole minutes. The dead-man check every 15 min; the log cleanup
-- daily at 04:00 (UTC).
select cron.unschedule('elorin-deadman')  where exists (select 1 from cron.job where jobname = 'elorin-deadman');
select cron.unschedule('elorin-prune')    where exists (select 1 from cron.job where jobname = 'elorin-prune');

select cron.schedule('elorin-deadman', '*/15 * * * *', $$select public.alert_if_scanner_dead();$$);
select cron.schedule('elorin-prune',   '0 4 * * *',    $$select public.prune_offers(90);$$);

-- ── Realtime: publish inserts on `offers` ──────────────────────────────────
-- The dashboard subscribes to INSERTs so it updates live. Without this it joins
-- the channel but never receives anything (its polling timer still works as a
-- fallback). RLS is already public-read on `offers`, so the anon key can listen.
do $$
begin
  alter publication supabase_realtime add table public.offers;
exception
  when duplicate_object then null;   -- already published: fine
end $$;

-- ── Your one manual step ───────────────────────────────────────────────────
-- Replace the URL and run this once (and again whenever it changes):
--
--   update public.scanner_alerts
--      set webhook_url = 'https://discord.com/api/webhooks/…/…'
--    where id = 1;
--
-- Test it by faking a dead scanner:
--   update public.buy_settings set scanner_seen = now() - interval '1 hour' where id = 1;
--   select public.alert_if_scanner_dead();
-- (then the real heartbeat will refresh scanner_seen within ~20 s).
