DROP SCHEMA IF EXISTS ev CASCADE;
DROP SCHEMA IF EXISTS hdb_catalog CASCADE;
CREATE SCHEMA ev;
SET TIME ZONE 'UTC';

CREATE TABLE ev.item (
  id serial PRIMARY KEY,
  name text NOT NULL,
  price numeric,
  big bigint,
  info jsonb
);

CREATE TABLE ev.flaky (
  id serial PRIMARY KEY,
  name text NOT NULL
);

CREATE TABLE ev.broken (
  id serial PRIMARY KEY
);

-- The engine's transaction settings as text, for the tests (JSONB event payloads reorder keys).
CREATE TABLE ev.capture (
  id serial PRIMARY KEY,
  hasura_user text,
  hasura_trace text
);
CREATE FUNCTION ev.capture_settings() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO ev.capture (hasura_user, hasura_trace)
  VALUES (current_setting('hasura.user', 't'), current_setting('hasura.tracecontext', 't'));
  RETURN NEW;
END $$;
CREATE TRIGGER capture_settings AFTER INSERT ON ev.item
  FOR EACH ROW EXECUTE FUNCTION ev.capture_settings();
