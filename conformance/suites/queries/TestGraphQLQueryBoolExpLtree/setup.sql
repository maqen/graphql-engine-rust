

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


CREATE TABLE public.tree (
    path public.ltree
);

INSERT INTO public.tree VALUES
	('Tree'),
	('Tree.Science'),
	('Tree.Science.Astronomy'),
	('Tree.Science.Astronomy.Astrophysics'),
	('Tree.Science.Astronomy.Cosmology'),
	('Tree.Hobbies'),
	('Tree.Hobbies.Amateurs_Astronomy'),
	('Tree.Collections'),
	('Tree.Collections.Pictures'),
	('Tree.Collections.Pictures.Astronomy'),
	('Tree.Collections.Pictures.Astronomy.Stars'),
	('Tree.Collections.Pictures.Astronomy.Galaxies'),
	('Tree.Collections.Pictures.Astronomy.Astronauts');

CREATE INDEX path_gist_idx ON public.tree USING gist (path);

CREATE INDEX path_idx ON public.tree USING btree (path);


