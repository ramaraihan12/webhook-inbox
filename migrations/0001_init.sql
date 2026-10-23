-- One row per accepted or rejected webhook.
create table deliveries (
  id uuid primary key default gen_random_uuid(),
  source text not null,
  received_at timestamptz not null default now(),
  -- The provider's own event id, when the source is configured to find one.
  event_id text,
  verification text not null check (verification in ('verified', 'rejected')),
  reject_reason text,
  -- [[name, value], ...] in the order received, secrets removed.
  headers jsonb not null,
  body bytea not null
);

-- A repeated provider event id for the same source is a duplicate.
create unique index deliveries_dedupe on deliveries (source, event_id) where event_id is not null;
create index deliveries_received on deliveries (received_at desc);
create index deliveries_source_received on deliveries (source, received_at desc);
create index deliveries_rejected on deliveries (received_at desc) where verification = 'rejected';

-- One row per (delivery, target) that has to be forwarded.
create table jobs (
  id uuid primary key default gen_random_uuid(),
  delivery_id uuid not null references deliveries (id) on delete cascade,
  target text not null,
  url text not null,
  status text not null default 'pending' check (status in ('pending', 'in_flight', 'done', 'dead')),
  attempts int not null default 0,
  next_attempt_at timestamptz not null default now(),
  -- Set while a worker holds the job. An expired lease can be claimed again.
  locked_until timestamptz,
  -- True for jobs created by the Replay button.
  replay boolean not null default false,
  last_status int,
  last_error text,
  created_at timestamptz not null default now(),
  finished_at timestamptz
);

create index jobs_due on jobs (next_attempt_at) where status in ('pending', 'in_flight');
create index jobs_delivery on jobs (delivery_id);
create index jobs_dead on jobs (finished_at desc) where status = 'dead';

-- One row per HTTP try. (job_id, attempt) is unique, so a try can only be recorded once.
create table attempts (
  id bigserial primary key,
  job_id uuid not null references jobs (id) on delete cascade,
  delivery_id uuid not null references deliveries (id) on delete cascade,
  attempt int not null,
  started_at timestamptz not null,
  duration_ms int not null,
  http_status int,
  error text,
  outcome text not null check (outcome in ('ok', 'retry', 'dead')),
  -- Delay before the next try, when there is one.
  retry_in_ms bigint,
  unique (job_id, attempt)
);

create index attempts_delivery on attempts (delivery_id, started_at);
