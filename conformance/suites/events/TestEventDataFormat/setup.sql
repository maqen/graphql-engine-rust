

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

CREATE SCHEMA hge_tests;

CREATE EXTENSION IF NOT EXISTS citext WITH SCHEMA public;

COMMENT ON EXTENSION citext IS 'data type for case-insensitive character strings';

CREATE EXTENSION IF NOT EXISTS ltree WITH SCHEMA public;

COMMENT ON EXTENSION ltree IS 'data type for hierarchical tree-like structures';

CREATE EXTENSION IF NOT EXISTS pgcrypto WITH SCHEMA public;

COMMENT ON EXTENSION pgcrypto IS 'cryptographic functions';

CREATE EXTENSION IF NOT EXISTS postgis WITH SCHEMA public;

COMMENT ON EXTENSION postgis IS 'PostGIS geometry and geography spatial types and functions';

SET default_tablespace = '';


CREATE TABLE hge_tests.test_bigint (
    id bigint,
    name text
);

CREATE TABLE hge_tests.test_geojson (
    id integer NOT NULL,
    location public.geography(Point,4326)
);

CREATE SEQUENCE hge_tests.test_geojson_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE hge_tests.test_geojson_id_seq OWNED BY hge_tests.test_geojson.id;

ALTER TABLE ONLY hge_tests.test_geojson ALTER COLUMN id SET DEFAULT nextval('hge_tests.test_geojson_id_seq'::regclass);

INSERT INTO hge_tests.test_geojson VALUES
	(1, '0101000020E6100000C3F5285C8FE245C052B81E85EBD14640');

SELECT pg_catalog.setval('hge_tests.test_geojson_id_seq', 1, true);

ALTER TABLE ONLY hge_tests.test_geojson
    ADD CONSTRAINT test_geojson_pkey PRIMARY KEY (id);


