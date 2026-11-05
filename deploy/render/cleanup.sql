-- Run by start.sh at boot and every 10 minutes. The admin token of this demo is
-- public, so whatever strangers leave here has to expire by itself: their
-- deliveries, and replays of the seeded ones, go after 6 hours. Deliveries
-- listed in demo_keep are the seeds and stay. That list is filled by hand after
-- seeding. It is a table and not a marker in the row because anything in a row
-- can be sent by a stranger too.
set client_min_messages = warning;

create table if not exists demo_keep (
  delivery_id uuid primary key references deliveries (id) on delete cascade
);

delete from jobs
where replay
  and status in ('done', 'dead')
  and created_at < now() - interval '6 hours';

delete from deliveries d
where d.received_at < now() - interval '6 hours'
  and not exists (select 1 from demo_keep k where k.delivery_id = d.id)
  and not exists (
    select 1 from jobs j
    where j.delivery_id = d.id and j.status in ('pending', 'in_flight'));
