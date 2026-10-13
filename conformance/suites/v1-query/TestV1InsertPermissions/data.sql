

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

ALTER TABLE public.resident DISABLE TRIGGER ALL;

INSERT INTO public.resident VALUES
	(1, 'Resident 1', 21),
	(2, 'Resident 2', 22);

ALTER TABLE public.resident ENABLE TRIGGER ALL;

ALTER TABLE public.address DISABLE TRIGGER ALL;

ALTER TABLE public.address ENABLE TRIGGER ALL;

ALTER TABLE public.author DISABLE TRIGGER ALL;

INSERT INTO public.author VALUES
	(1, 'Author 1', NULL, false),
	(2, 'Author 2', NULL, false);

ALTER TABLE public.author ENABLE TRIGGER ALL;

ALTER TABLE public.article DISABLE TRIGGER ALL;

INSERT INTO public.article VALUES
	(1, 'Article 1', 'Sample article content', 1, NULL, NULL),
	(2, 'Article 2', 'Sample article content', 1, NULL, NULL),
	(3, 'Article 3', 'Sample article content', 2, NULL, NULL);

ALTER TABLE public.article ENABLE TRIGGER ALL;

SELECT pg_catalog.setval('public.address_id_seq', 1, false);

SELECT pg_catalog.setval('public.article_id_seq', 3, true);

SELECT pg_catalog.setval('public.author_id_seq', 2, true);

SELECT pg_catalog.setval('public.resident_id_seq', 1, false);


