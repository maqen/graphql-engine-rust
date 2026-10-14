DROP SCHEMA IF EXISTS public CASCADE;
DROP SCHEMA IF EXISTS other CASCADE;
CREATE SCHEMA public;

CREATE TABLE profile (
  id int PRIMARY KEY,
  name text NOT NULL
);
INSERT INTO profile VALUES (1, 'alice'), (2, 'bob');
