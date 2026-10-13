

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

ALTER TABLE public.area DISABLE TRIGGER ALL;

ALTER TABLE public.area ENABLE TRIGGER ALL;

ALTER TABLE public.compounds DISABLE TRIGGER ALL;

ALTER TABLE public.compounds ENABLE TRIGGER ALL;

ALTER TABLE public.drone_3d_location DISABLE TRIGGER ALL;

ALTER TABLE public.drone_3d_location ENABLE TRIGGER ALL;

ALTER TABLE public.geometry_collection DISABLE TRIGGER ALL;

ALTER TABLE public.geometry_collection ENABLE TRIGGER ALL;

ALTER TABLE public.landmark DISABLE TRIGGER ALL;

ALTER TABLE public.landmark ENABLE TRIGGER ALL;

ALTER TABLE public.road DISABLE TRIGGER ALL;

ALTER TABLE public.road ENABLE TRIGGER ALL;

ALTER TABLE public.route DISABLE TRIGGER ALL;

ALTER TABLE public.route ENABLE TRIGGER ALL;

ALTER TABLE public.service_locations DISABLE TRIGGER ALL;

ALTER TABLE public.service_locations ENABLE TRIGGER ALL;

ALTER TABLE public.spatial_ref_sys DISABLE TRIGGER ALL;

ALTER TABLE public.spatial_ref_sys ENABLE TRIGGER ALL;

SELECT pg_catalog.setval('public.area_id_seq', 1, false);

SELECT pg_catalog.setval('public.geometry_collection_id_seq', 1, false);

SELECT pg_catalog.setval('public.landmark_id_seq', 1, false);

SELECT pg_catalog.setval('public.road_id_seq', 1, false);

SELECT pg_catalog.setval('public.route_id_seq', 1, false);

SELECT pg_catalog.setval('public.service_locations_id_seq', 1, false);


