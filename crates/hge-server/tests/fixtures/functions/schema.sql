DROP SCHEMA IF EXISTS public CASCADE;
DROP SCHEMA IF EXISTS other CASCADE;
CREATE SCHEMA public;
SET TIME ZONE 'UTC';

CREATE TABLE author (
  id serial PRIMARY KEY,
  first_name text NOT NULL,
  last_name text,
  info jsonb
);

CREATE TABLE article (
  id serial PRIMARY KEY,
  title text NOT NULL,
  author_id integer NOT NULL REFERENCES author(id),
  is_published boolean NOT NULL DEFAULT false,
  rating integer
);

INSERT INTO author (first_name, last_name, info) VALUES
  ('Alice', 'Smith', '{"city": "Oslo"}'),
  ('Bob', NULL, NULL),
  ('Carol', 'Jones', '{"city": "Rome"}');

INSERT INTO article (title, author_id, is_published, rating) VALUES
  ('Rust tips', 1, true, 5),
  ('Draft', 1, false, NULL),
  ('Go tips', 2, true, 3),
  ('Rust again', 3, true, 4);

-- Scalar computed field.
CREATE FUNCTION author_full_name(a author) RETURNS text LANGUAGE sql STABLE AS $$
  SELECT a.first_name || coalesce(' ' || a.last_name, '')
$$;

-- Scalar computed field with an argument (with a default) and a json result.
CREATE FUNCTION author_greeting(a author, greeting text DEFAULT 'Hello') RETURNS text LANGUAGE sql STABLE AS $$
  SELECT greeting || ', ' || a.first_name
$$;

CREATE FUNCTION author_profile(a author) RETURNS jsonb LANGUAGE sql STABLE AS $$
  SELECT jsonb_build_object('name', a.first_name, 'info', a.info)
$$;

-- Session argument.
CREATE FUNCTION author_is_me(a author, hasura_session json) RETURNS boolean LANGUAGE sql STABLE AS $$
  SELECT a.id::text = (hasura_session ->> 'x-hasura-user-id')
$$;

-- Table computed field with a required argument, table argument not first.
CREATE FUNCTION author_articles_matching(search text, author_row author) RETURNS SETOF article LANGUAGE sql STABLE AS $$
  SELECT * FROM article WHERE author_id = author_row.id AND title ILIKE '%' || search || '%'
$$;

-- Table computed field without arguments.
CREATE FUNCTION author_published(a author) RETURNS SETOF article LANGUAGE sql STABLE AS $$
  SELECT * FROM article WHERE author_id = a.id AND is_published
$$;

-- Tracked functions.
CREATE FUNCTION search_articles(search text) RETURNS SETOF article LANGUAGE sql STABLE AS $$
  SELECT * FROM article WHERE title ILIKE '%' || search || '%'
$$;

CREATE FUNCTION top_rated(min_rating integer DEFAULT 4, published boolean DEFAULT true) RETURNS SETOF article LANGUAGE sql STABLE AS $$
  SELECT * FROM article WHERE rating >= min_rating AND is_published = published
$$;

CREATE FUNCTION my_articles(hasura_session json) RETURNS SETOF article LANGUAGE sql STABLE AS $$
  SELECT * FROM article WHERE author_id::text = (hasura_session ->> 'x-hasura-user-id')
$$;

CREATE FUNCTION first_article(text) RETURNS article LANGUAGE sql STABLE AS $$
  SELECT * FROM article WHERE title = $1 LIMIT 1
$$;

CREATE FUNCTION publish_article(article_id integer) RETURNS SETOF article LANGUAGE sql VOLATILE AS $$
  UPDATE article SET is_published = true WHERE id = article_id RETURNING *
$$;

-- Numeric computed fields for aggregates.
CREATE FUNCTION author_id2(a author) RETURNS integer LANGUAGE sql STABLE AS $$ SELECT a.id * 2 $$;
CREATE FUNCTION author_idn(a author) RETURNS numeric LANGUAGE sql STABLE AS $$ SELECT a.id / 3.0 $$;
CREATE FUNCTION author_idarg(a author, k integer DEFAULT 1) RETURNS integer LANGUAGE sql STABLE AS $$ SELECT a.id * k $$;
