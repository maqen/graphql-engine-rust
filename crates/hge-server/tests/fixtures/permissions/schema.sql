DROP SCHEMA IF EXISTS public CASCADE;
DROP SCHEMA IF EXISTS other CASCADE;
CREATE SCHEMA public;
SET TIME ZONE 'UTC';

CREATE TABLE author (
  id integer PRIMARY KEY,
  name text NOT NULL,
  email text,
  is_active boolean NOT NULL DEFAULT true
);

CREATE TABLE article (
  id integer PRIMARY KEY,
  title text NOT NULL,
  author_id integer NOT NULL REFERENCES author(id),
  is_published boolean NOT NULL DEFAULT false,
  reviewer_id integer REFERENCES author(id),
  word_count integer NOT NULL,
  min_words integer NOT NULL,
  meta jsonb
);

CREATE TABLE allowed_reader (
  user_id integer PRIMARY KEY
);

INSERT INTO author (id, name, email, is_active) VALUES
  (1, 'Alice', 'alice@example.com', true),
  (2, 'Bob', 'bob@example.com', true),
  (3, 'Carol', NULL, false);

INSERT INTO article (id, title, author_id, is_published, reviewer_id, word_count, min_words, meta) VALUES
  (1, 'Alice published', 1, true, 2, 900, 500, '{"lang": "en", "draft": false}'),
  (2, 'Alice draft', 1, false, NULL, 100, 500, '{"lang": "en"}'),
  (3, 'Bob published', 2, true, 1, 400, 500, '{"lang": "no"}'),
  (4, 'Bob draft', 2, false, 2, 700, 300, NULL),
  (5, 'Carol published', 3, true, NULL, 50, 10, '{"draft": true}');

INSERT INTO allowed_reader (user_id) VALUES (7);
