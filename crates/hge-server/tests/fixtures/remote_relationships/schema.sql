-- V2's server/tests-py/queries/remote_schemas/remote_relationships/setup.yaml
DROP SCHEMA IF EXISTS public CASCADE;
DROP SCHEMA IF EXISTS custom CASCADE;
DROP SCHEMA IF EXISTS other CASCADE;
CREATE SCHEMA public;
CREATE SCHEMA custom;

CREATE TABLE custom.profiles (
  id serial PRIMARY KEY,
  name text
);
INSERT INTO custom.profiles (name) VALUES ('alice'), ('bob'), ('alice');

CREATE TABLE authors (
  id serial PRIMARY KEY,
  name text
);

CREATE TABLE employees (
  id serial PRIMARY KEY,
  name text
);
INSERT INTO employees (name) VALUES ('alice'), (NULL), ('bob');

CREATE TABLE students (
  id serial PRIMARY KEY,
  name text NOT NULL,
  physics integer,
  maths integer
);
INSERT INTO students (name, physics, maths) VALUES ('alice', 45, 48), ('bob', 32, 40);

CREATE FUNCTION total_marks(student_row students) RETURNS integer AS $$
  SELECT student_row.physics + student_row.maths
$$ LANGUAGE sql STABLE;

CREATE FUNCTION total_marks_offset(student_row students, "offset" integer) RETURNS integer AS $$
  SELECT student_row.physics + student_row.maths - "offset"
$$ LANGUAGE sql STABLE;

CREATE FUNCTION total_marks_session(student_row students, hasura_session json) RETURNS integer AS $$
  SELECT student_row.physics + student_row.maths - (hasura_session ->> 'x-hasura-offset')::integer
$$ LANGUAGE sql STABLE;
