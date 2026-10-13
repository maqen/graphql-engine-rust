DROP SCHEMA IF EXISTS public CASCADE;
DROP SCHEMA IF EXISTS other CASCADE;
CREATE SCHEMA public;
CREATE SCHEMA other;
SET TIME ZONE 'UTC';

CREATE TYPE mood AS ENUM ('happy', 'sad');

CREATE TABLE author (
  id serial PRIMARY KEY,
  name text NOT NULL CONSTRAINT author_name_key UNIQUE,
  rating numeric,
  big bigint,
  tags text[],
  info jsonb,
  mood mood,
  "createdAt" timestamptz NOT NULL DEFAULT '2020-01-01T00:00:00Z'
);

-- Mutations set "hasura.user" (the session variables) for triggers, like V2.
CREATE FUNCTION record_hasura_user() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.name = 'Who' THEN
    NEW.info := current_setting('hasura.user', true)::jsonb;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER record_hasura_user BEFORE INSERT ON author
  FOR EACH ROW EXECUTE FUNCTION record_hasura_user();

CREATE TABLE article (
  id serial PRIMARY KEY,
  title text NOT NULL,
  content text,
  author_id integer NOT NULL REFERENCES author(id),
  is_published boolean NOT NULL DEFAULT false,
  published_on date
);

CREATE TABLE other.thing (
  code text PRIMARY KEY,
  label text
);

INSERT INTO author (name, rating, big, tags, info, mood) VALUES
  ('Alice', 4.5, 9223372036854775807, '{a,b}', '{"city": "Oslo", "langs": ["rust", "go"]}', 'happy'),
  ('Bob', 3.2, 1, '{}', '{"city": "Lima"}', 'sad'),
  ('Carol', NULL, NULL, NULL, NULL, NULL);

INSERT INTO article (title, content, author_id, is_published, published_on) VALUES
  ('Rust in production', 'body 1', 1, true, '2021-05-01'),
  ('Draft notes', 'body 2', 1, false, NULL),
  ('Go concurrency', 'body 3', 2, true, '2020-01-15'),
  ('Lima travel', 'it''s "quoted"', 2, true, '2019-07-30');

INSERT INTO other.thing VALUES ('x', 'Ex'), ('y', NULL);
