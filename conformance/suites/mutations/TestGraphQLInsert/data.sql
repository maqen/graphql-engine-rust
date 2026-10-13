

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

ALTER TABLE public.author ENABLE TRIGGER ALL;

ALTER TABLE public.article DISABLE TRIGGER ALL;

ALTER TABLE public.article ENABLE TRIGGER ALL;

ALTER TABLE public.orders DISABLE TRIGGER ALL;

ALTER TABLE public.orders ENABLE TRIGGER ALL;

ALTER TABLE public.person DISABLE TRIGGER ALL;

ALTER TABLE public.person ENABLE TRIGGER ALL;

ALTER TABLE public.test_types DISABLE TRIGGER ALL;

ALTER TABLE public.test_types ENABLE TRIGGER ALL;

SELECT pg_catalog.setval('public.article_id_seq', 1, false);

SELECT pg_catalog.setval('public.author_id_seq', 1, false);

SELECT pg_catalog.setval('public.orders_id_seq', 1, false);

SELECT pg_catalog.setval('public.person_id_seq', 1, false);

SELECT pg_catalog.setval('public.test_types_c10_bigserial_seq', 1, false);

SELECT pg_catalog.setval('public.test_types_c8_smallserial_seq', 1, false);

SELECT pg_catalog.setval('public.test_types_c9_serial_seq', 1, false);


