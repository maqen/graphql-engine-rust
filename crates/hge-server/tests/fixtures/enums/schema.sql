DROP SCHEMA IF EXISTS public CASCADE;
DROP SCHEMA IF EXISTS other CASCADE;
CREATE SCHEMA public;
CREATE SCHEMA other;

-- An enum table with a comment column.
CREATE TABLE colors (value text PRIMARY KEY, comment text);
INSERT INTO colors VALUES ('red', 'Red colour'), ('blue', NULL), ('green', 'Green');

-- An enum table without one, tracked with a custom name.
CREATE TABLE shade (name text PRIMARY KEY);
INSERT INTO shade VALUES ('light'), ('dark');

-- An enum table outside `public`.
CREATE TABLE other.size (value text PRIMARY KEY);
INSERT INTO other.size VALUES ('small'), ('large');

CREATE TABLE post (
  id serial PRIMARY KEY,
  title text NOT NULL,
  color text NOT NULL REFERENCES colors (value),
  shade text REFERENCES shade (name),
  size text REFERENCES other.size (value),
  likes integer NOT NULL DEFAULT 0
);
INSERT INTO post (title, color, shade, size) VALUES
  ('first', 'red', 'dark', 'small'),
  ('second', 'blue', NULL, 'large'),
  ('third', 'red', 'light', NULL);

-- Not tracked at first; becomes a valid enum table only once `run_sql` adds its key.
CREATE TABLE weekday (name text NOT NULL);
INSERT INTO weekday VALUES ('monday'), ('friday');
CREATE TABLE shift (id integer PRIMARY KEY, day text NOT NULL);
