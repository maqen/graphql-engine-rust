

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


CREATE TABLE public.automatic_comment_in_db (
    id integer NOT NULL,
    name text NOT NULL
);

COMMENT ON TABLE public.automatic_comment_in_db IS 'What a great comment in the DB';

COMMENT ON COLUMN public.automatic_comment_in_db.name IS 'The name column comment in the DB';

CREATE FUNCTION public.automatic_comment_in_db_upper_name(r public.automatic_comment_in_db) RETURNS text
    LANGUAGE sql STABLE
    AS $$
  SELECT upper(r.name)
$$;

COMMENT ON FUNCTION public.automatic_comment_in_db_upper_name(r public.automatic_comment_in_db) IS 'What a great comment on the function in the DB';

CREATE TABLE public.automatic_no_comment_in_db (
    id integer NOT NULL,
    name text NOT NULL
);

CREATE FUNCTION public.automatic_no_comment_in_db_upper_name(r public.automatic_no_comment_in_db) RETURNS text
    LANGUAGE sql STABLE
    AS $$
  SELECT upper(r.name)
$$;

CREATE TABLE public.explicit_comment_in_metadata (
    id integer NOT NULL,
    name text NOT NULL
);

COMMENT ON TABLE public.explicit_comment_in_metadata IS 'Fantastic comment, so good, so hidden';

COMMENT ON COLUMN public.explicit_comment_in_metadata.name IS 'The name column comment in the DB, but hidden!';

CREATE FUNCTION public.explicit_comment_in_metadata_upper_name(r public.explicit_comment_in_metadata) RETURNS text
    LANGUAGE sql STABLE
    AS $$
  SELECT upper(r.name)
$$;

COMMENT ON FUNCTION public.explicit_comment_in_metadata_upper_name(r public.explicit_comment_in_metadata) IS 'Fantastic comment on the function, so good, so hidden';

CREATE TABLE public.explicit_no_comment_in_metadata (
    id integer NOT NULL,
    name text NOT NULL
);

COMMENT ON TABLE public.explicit_no_comment_in_metadata IS 'This would be a great comment, but you can''t see it';

COMMENT ON COLUMN public.explicit_no_comment_in_metadata.name IS 'The name column comment in the DB, but you can''t see it';

CREATE FUNCTION public.explicit_no_comment_in_metadata_upper_name(r public.explicit_no_comment_in_metadata) RETURNS text
    LANGUAGE sql STABLE
    AS $$
  SELECT upper(r.name)
$$;

COMMENT ON FUNCTION public.explicit_no_comment_in_metadata_upper_name(r public.explicit_no_comment_in_metadata) IS 'This would be a great comment on the function, but you can''t see it';

CREATE SEQUENCE public.automatic_comment_in_db_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.automatic_comment_in_db_id_seq OWNED BY public.automatic_comment_in_db.id;

CREATE SEQUENCE public.automatic_no_comment_in_db_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.automatic_no_comment_in_db_id_seq OWNED BY public.automatic_no_comment_in_db.id;

CREATE SEQUENCE public.explicit_comment_in_metadata_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.explicit_comment_in_metadata_id_seq OWNED BY public.explicit_comment_in_metadata.id;

CREATE SEQUENCE public.explicit_no_comment_in_metadata_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.explicit_no_comment_in_metadata_id_seq OWNED BY public.explicit_no_comment_in_metadata.id;

ALTER TABLE ONLY public.automatic_comment_in_db ALTER COLUMN id SET DEFAULT nextval('public.automatic_comment_in_db_id_seq'::regclass);

ALTER TABLE ONLY public.automatic_no_comment_in_db ALTER COLUMN id SET DEFAULT nextval('public.automatic_no_comment_in_db_id_seq'::regclass);

ALTER TABLE ONLY public.explicit_comment_in_metadata ALTER COLUMN id SET DEFAULT nextval('public.explicit_comment_in_metadata_id_seq'::regclass);

ALTER TABLE ONLY public.explicit_no_comment_in_metadata ALTER COLUMN id SET DEFAULT nextval('public.explicit_no_comment_in_metadata_id_seq'::regclass);

SELECT pg_catalog.setval('public.automatic_comment_in_db_id_seq', 1, false);

SELECT pg_catalog.setval('public.automatic_no_comment_in_db_id_seq', 1, false);

SELECT pg_catalog.setval('public.explicit_comment_in_metadata_id_seq', 1, false);

SELECT pg_catalog.setval('public.explicit_no_comment_in_metadata_id_seq', 1, false);

ALTER TABLE ONLY public.automatic_comment_in_db
    ADD CONSTRAINT automatic_comment_in_db_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.automatic_no_comment_in_db
    ADD CONSTRAINT automatic_no_comment_in_db_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.explicit_comment_in_metadata
    ADD CONSTRAINT explicit_comment_in_metadata_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.explicit_no_comment_in_metadata
    ADD CONSTRAINT explicit_no_comment_in_metadata_pkey PRIMARY KEY (id);


