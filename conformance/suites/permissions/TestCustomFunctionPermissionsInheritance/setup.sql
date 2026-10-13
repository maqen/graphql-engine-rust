

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

SET default_tablespace = '';


CREATE TABLE public."user" (
    id integer NOT NULL,
    name text NOT NULL,
    score integer,
    role_echo text DEFAULT ''::text
);

CREATE FUNCTION public.add_to_score(hasura_session json, search text, increment integer DEFAULT 1) RETURNS SETOF public."user"
    LANGUAGE sql
    AS $$
    UPDATE "user"
    SET score = score + increment
    WHERE name ilike ('%' || search || '%')
    RETURNING id,
              name,
              score,
              hasura_session->>'x-hasura-role' AS role_echo
  $$;

CREATE SEQUENCE public.user_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.user_id_seq OWNED BY public."user".id;

ALTER TABLE ONLY public."user" ALTER COLUMN id SET DEFAULT nextval('public.user_id_seq'::regclass);

INSERT INTO public."user" VALUES
	(1, 'user 1', 143, ''),
	(2, 'user 2', 132, '');

SELECT pg_catalog.setval('public.user_id_seq', 2, true);

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_pkey PRIMARY KEY (id);


