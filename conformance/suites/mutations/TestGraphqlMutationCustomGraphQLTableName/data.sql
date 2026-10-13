

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

ALTER TABLE public."author details" DISABLE TRIGGER ALL;

INSERT INTO public."author details" VALUES
	(1, 'Author 1', NULL),
	(2, 'Author 2', NULL);

ALTER TABLE public."author details" ENABLE TRIGGER ALL;

ALTER TABLE public.article DISABLE TRIGGER ALL;

INSERT INTO public.article VALUES
	(1, 'Article 1', 'Content for Article 1', 1),
	(2, 'Article 2', 'Content for Article 2', 1),
	(3, 'Article 3', 'Content for Article 3', 2);

ALTER TABLE public.article ENABLE TRIGGER ALL;

SELECT pg_catalog.setval('public.article_id_seq', 3, true);

SELECT pg_catalog.setval('public."author details_id_seq"', 2, true);


