-- Cron and one-off scheduled event tables, from V2's initialise.sql (metadata catalog v48).
CREATE TABLE hdb_catalog.hdb_cron_events
(
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  trigger_name TEXT NOT NULL,
  scheduled_time TIMESTAMPTZ NOT NULL,
  status TEXT NOT NULL DEFAULT 'scheduled',
  tries INTEGER NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ DEFAULT NOW(),
  next_retry_at TIMESTAMPTZ,

  CONSTRAINT valid_status CHECK (status IN ('scheduled','locked','delivered','error','dead'))
);

CREATE INDEX hdb_cron_event_status ON hdb_catalog.hdb_cron_events (status);

CREATE UNIQUE INDEX hdb_cron_events_unique_scheduled ON
hdb_catalog.hdb_cron_events (trigger_name, scheduled_time)
where status = 'scheduled';

CREATE TABLE hdb_catalog.hdb_cron_event_invocation_logs
(
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  event_id TEXT,
  status INTEGER,
  request JSON,
  response JSON,
  created_at TIMESTAMPTZ DEFAULT NOW(),

  FOREIGN KEY (event_id) REFERENCES hdb_catalog.hdb_cron_events (id)
    ON UPDATE CASCADE ON DELETE CASCADE
);

CREATE INDEX hdb_cron_event_invocation_event_id ON hdb_catalog.hdb_cron_event_invocation_logs (event_id);

CREATE TABLE hdb_catalog.hdb_scheduled_events
(
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  webhook_conf JSON NOT NULL,
  scheduled_time TIMESTAMPTZ NOT NULL,
  retry_conf JSON,
  payload JSON,
  header_conf JSON,
  status TEXT NOT NULL DEFAULT 'scheduled',
  tries INTEGER NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ DEFAULT NOW(),
  next_retry_at TIMESTAMPTZ,
  comment TEXT,
  CONSTRAINT valid_status CHECK (status IN ('scheduled','locked','delivered','error','dead'))
);

CREATE INDEX hdb_scheduled_event_status ON hdb_catalog.hdb_scheduled_events (status);

CREATE TABLE hdb_catalog.hdb_scheduled_event_invocation_logs
(
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  event_id TEXT,
  status INTEGER,
  request JSON,
  response JSON,
  created_at TIMESTAMPTZ DEFAULT NOW(),

  FOREIGN KEY (event_id) REFERENCES hdb_catalog.hdb_scheduled_events (id)
     ON DELETE CASCADE ON UPDATE CASCADE
);

