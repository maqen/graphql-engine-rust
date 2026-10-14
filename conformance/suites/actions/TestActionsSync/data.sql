

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

ALTER TABLE public.article DISABLE TRIGGER ALL;

INSERT INTO public.article VALUES
	(1, 'foo', 1),
	(2, 'bar', 1),
	(3, 'bar', 1),
	(4, 'baz', 2);

ALTER TABLE public.article ENABLE TRIGGER ALL;

ALTER TABLE public."user" DISABLE TRIGGER ALL;

ALTER TABLE public."user" ENABLE TRIGGER ALL;

SELECT pg_catalog.setval('public.article_id_seq', 4, true);

SELECT pg_catalog.setval('public.user_id_seq', 1, false);


