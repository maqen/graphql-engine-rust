

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

SET SESSION AUTHORIZATION DEFAULT;

ALTER TABLE public.author DISABLE TRIGGER ALL;

INSERT INTO public.author VALUES
	(1, 'Author 1', '{}', '{"age": 23}', '0101000020E610000075029A081B9A5DC0F085C954C1F84040'),
	(2, 'Author 2', '{}', NULL, NULL);

ALTER TABLE public.author ENABLE TRIGGER ALL;

ALTER TABLE public.article DISABLE TRIGGER ALL;

ALTER TABLE public.article ENABLE TRIGGER ALL;

ALTER TABLE public.numerics DISABLE TRIGGER ALL;

INSERT INTO public.numerics VALUES
	(1, 12345, 12345, 12345, 12345.67, 12345.67, '$12,345.67', 12345.6700);

ALTER TABLE public.numerics ENABLE TRIGGER ALL;

ALTER TABLE public.person DISABLE TRIGGER ALL;

INSERT INTO public.person VALUES
	(1, '{"name": {"last": "bar", "first": "foo"}, "address": "foobar"}');

ALTER TABLE public.person ENABLE TRIGGER ALL;

ALTER TABLE public.spatial_ref_sys DISABLE TRIGGER ALL;

ALTER TABLE public.spatial_ref_sys ENABLE TRIGGER ALL;

SELECT pg_catalog.setval('public.article_id_seq', 1, false);

SELECT pg_catalog.setval('public.author_id_seq', 2, true);

SELECT pg_catalog.setval('public.numerics_id_seq', 1, true);

SELECT pg_catalog.setval('public.person_id_seq', 1, true);


