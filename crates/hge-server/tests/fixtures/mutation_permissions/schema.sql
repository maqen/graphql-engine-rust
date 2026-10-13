DROP SCHEMA IF EXISTS public CASCADE;
DROP SCHEMA IF EXISTS other CASCADE;
CREATE SCHEMA public;
SET TIME ZONE 'UTC';

CREATE TABLE author (
  id serial PRIMARY KEY,
  name text NOT NULL CONSTRAINT author_name_key UNIQUE,
  owner_id integer NOT NULL,
  rating integer NOT NULL DEFAULT 0,
  note text
);

CREATE TABLE article (
  id serial PRIMARY KEY,
  title text NOT NULL,
  author_id integer NOT NULL REFERENCES author(id)
);

INSERT INTO author (name, owner_id, rating) VALUES
  ('Alice', 1, 1),
  ('Bob', 2, 2),
  ('Bea', 2, 3);

INSERT INTO article (title, author_id) VALUES ('Alice 1', 1), ('Bob 1', 2);
