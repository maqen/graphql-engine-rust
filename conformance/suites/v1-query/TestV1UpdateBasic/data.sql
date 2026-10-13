

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
	(1, 'Author 1'),
	(2, 'Author 2');

ALTER TABLE public.author ENABLE TRIGGER ALL;

ALTER TABLE public.article DISABLE TRIGGER ALL;

ALTER TABLE public.article ENABLE TRIGGER ALL;

ALTER TABLE public.person DISABLE TRIGGER ALL;

INSERT INTO public.person VALUES
	(1, '{"name": {"last": "bar", "first": "foo"}, "address": "foobar"}');

ALTER TABLE public.person ENABLE TRIGGER ALL;

ALTER TABLE public.products DISABLE TRIGGER ALL;

INSERT INTO public.products VALUES
	(1, 'Product 1', 5),
	(2, 'Product 2', 15);

ALTER TABLE public.products ENABLE TRIGGER ALL;

SELECT pg_catalog.setval('public.article_id_seq', 1, false);

SELECT pg_catalog.setval('public.author_id_seq', 2, true);

SELECT pg_catalog.setval('public.person_id_seq', 1, true);

SELECT pg_catalog.setval('public.products_product_id_seq', 2, true);


