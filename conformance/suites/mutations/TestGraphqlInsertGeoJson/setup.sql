

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


CREATE TABLE public.area (
    id integer NOT NULL,
    name text,
    area public.geography(Polygon,4326)
);

CREATE SEQUENCE public.area_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.area_id_seq OWNED BY public.area.id;

CREATE TABLE public.compounds (
    user_id integer NOT NULL,
    name text,
    areas public.geography(MultiPolygon,4326)
);

CREATE TABLE public.drone_3d_location (
    drone_id integer NOT NULL,
    location public.geography(PointZ,4326)
);

CREATE TABLE public.geometry_collection (
    id integer NOT NULL,
    geometries public.geography(GeometryCollection,4326)
);

CREATE SEQUENCE public.geometry_collection_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.geometry_collection_id_seq OWNED BY public.geometry_collection.id;

CREATE TABLE public.landmark (
    id integer NOT NULL,
    name text,
    type text,
    location public.geography(Point,4326)
);

CREATE SEQUENCE public.landmark_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.landmark_id_seq OWNED BY public.landmark.id;

CREATE TABLE public.road (
    id integer NOT NULL,
    name text,
    path public.geography(LineString,4326)
);

CREATE SEQUENCE public.road_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.road_id_seq OWNED BY public.road.id;

CREATE TABLE public.route (
    id integer NOT NULL,
    name text,
    route public.geography(MultiLineString,4326)
);

CREATE SEQUENCE public.route_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.route_id_seq OWNED BY public.route.id;

CREATE TABLE public.service_locations (
    id integer NOT NULL,
    service text,
    locations public.geography(MultiPoint,4326)
);

CREATE SEQUENCE public.service_locations_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.service_locations_id_seq OWNED BY public.service_locations.id;

ALTER TABLE ONLY public.area ALTER COLUMN id SET DEFAULT nextval('public.area_id_seq'::regclass);

ALTER TABLE ONLY public.geometry_collection ALTER COLUMN id SET DEFAULT nextval('public.geometry_collection_id_seq'::regclass);

ALTER TABLE ONLY public.landmark ALTER COLUMN id SET DEFAULT nextval('public.landmark_id_seq'::regclass);

ALTER TABLE ONLY public.road ALTER COLUMN id SET DEFAULT nextval('public.road_id_seq'::regclass);

ALTER TABLE ONLY public.route ALTER COLUMN id SET DEFAULT nextval('public.route_id_seq'::regclass);

ALTER TABLE ONLY public.service_locations ALTER COLUMN id SET DEFAULT nextval('public.service_locations_id_seq'::regclass);

SELECT pg_catalog.setval('public.area_id_seq', 1, false);

SELECT pg_catalog.setval('public.geometry_collection_id_seq', 1, false);

SELECT pg_catalog.setval('public.landmark_id_seq', 1, false);

SELECT pg_catalog.setval('public.road_id_seq', 1, false);

SELECT pg_catalog.setval('public.route_id_seq', 1, false);

SELECT pg_catalog.setval('public.service_locations_id_seq', 1, false);

ALTER TABLE ONLY public.area
    ADD CONSTRAINT area_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.compounds
    ADD CONSTRAINT compounds_pkey PRIMARY KEY (user_id);

ALTER TABLE ONLY public.drone_3d_location
    ADD CONSTRAINT drone_3d_location_pkey PRIMARY KEY (drone_id);

ALTER TABLE ONLY public.geometry_collection
    ADD CONSTRAINT geometry_collection_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.landmark
    ADD CONSTRAINT landmark_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.road
    ADD CONSTRAINT road_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.route
    ADD CONSTRAINT route_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.service_locations
    ADD CONSTRAINT service_locations_pkey PRIMARY KEY (id);


