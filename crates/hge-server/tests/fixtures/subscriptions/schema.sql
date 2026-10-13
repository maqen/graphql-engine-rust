DROP SCHEMA IF EXISTS public CASCADE;
CREATE SCHEMA public;
SET TIME ZONE 'UTC';

CREATE TABLE message (
  id serial PRIMARY KEY,
  author_id integer NOT NULL,
  body text NOT NULL,
  priority integer NOT NULL DEFAULT 0
);

INSERT INTO message (author_id, body, priority) VALUES
  (1, 'one', 1),
  (2, 'two', 2),
  (1, 'three', 2);
