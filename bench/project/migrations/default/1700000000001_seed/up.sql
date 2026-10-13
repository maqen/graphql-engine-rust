-- Deterministic seed data (no random()): every run produces identical rows,
-- so responses can be compared byte-for-byte across engines and runs.
-- Sizes: 1k artists, 3k albums, 30k tracks, 50 employees, 2k customers,
-- 20k invoices, 100k invoice lines.

INSERT INTO public.genre (genre_id, name)
SELECT g, 'Genre ' || g FROM generate_series(1, 25) AS g;

INSERT INTO public.media_type (media_type_id, name)
SELECT m, 'Media ' || m FROM generate_series(1, 5) AS m;

INSERT INTO public.artist (artist_id, name)
SELECT a, 'Artist ' || lpad(a::text, 4, '0') FROM generate_series(1, 1000) AS a;

INSERT INTO public.album (album_id, title, artist_id, released_on)
SELECT al,
       'Album ' || lpad(al::text, 4, '0'),
       ((al - 1) % 1000) + 1,
       DATE '1970-01-01' + (al * 7)
FROM generate_series(1, 3000) AS al;

INSERT INTO public.track (track_id, name, album_id, genre_id, media_type_id,
                          composer, milliseconds, bytes, unit_price)
SELECT t,
       'Track ' || lpad(t::text, 5, '0'),
       ((t - 1) % 3000) + 1,
       ((t - 1) % 25) + 1,
       ((t - 1) % 5) + 1,
       CASE WHEN t % 4 = 0 THEN NULL ELSE 'Composer ' || (t % 200) END,
       120000 + (t * 37) % 240000,
       1000000 + (t * 7919) % 9000000,
       CASE WHEN t % 3 = 0 THEN 1.99 ELSE 0.99 END
FROM generate_series(1, 30000) AS t;

INSERT INTO public.employee (employee_id, first_name, last_name, title, reports_to, email)
SELECT e,
       'First' || e,
       'Last' || e,
       CASE WHEN e = 1 THEN 'General Manager' WHEN e <= 5 THEN 'Sales Manager' ELSE 'Sales Agent' END,
       CASE WHEN e = 1 THEN NULL WHEN e <= 5 THEN 1 ELSE ((e - 1) % 4) + 2 END,
       'employee' || e || '@chinook.example'
FROM generate_series(1, 50) AS e;

INSERT INTO public.customer (customer_id, first_name, last_name, email, country,
                             support_rep_id, metadata)
SELECT c,
       'Cust' || c,
       'Omer' || c,
       'customer' || c || '@example.com',
       (ARRAY['USA', 'Canada', 'Sweden', 'Brazil', 'Germany', 'India', 'Japan', 'France'])[(c % 8) + 1],
       ((c - 1) % 45) + 6,
       jsonb_build_object('tier', CASE WHEN c % 10 = 0 THEN 'gold' ELSE 'basic' END,
                          'tags', jsonb_build_array('t' || (c % 3), 't' || (c % 5)))
FROM generate_series(1, 2000) AS c;

INSERT INTO public.invoice (invoice_id, customer_id, invoice_date, billing_country, total)
SELECT i,
       ((i - 1) % 2000) + 1,
       TIMESTAMPTZ '2020-01-01 00:00:00+00' + (i * INTERVAL '1 hour'),
       (ARRAY['USA', 'Canada', 'Sweden', 'Brazil', 'Germany', 'India', 'Japan', 'France'])[(((i - 1) % 2000) + 1) % 8 + 1],
       0
FROM generate_series(1, 20000) AS i;

INSERT INTO public.invoice_line (invoice_line_id, invoice_id, track_id, unit_price, quantity)
SELECT l,
       ((l - 1) / 5) + 1,
       ((l * 31) % 30000) + 1,
       CASE WHEN (((l * 31) % 30000) + 1) % 3 = 0 THEN 1.99 ELSE 0.99 END,
       (l % 3) + 1
FROM generate_series(1, 100000) AS l;

UPDATE public.invoice i
SET total = s.total
FROM (SELECT invoice_id, sum(unit_price * quantity) AS total
      FROM public.invoice_line GROUP BY invoice_id) s
WHERE s.invoice_id = i.invoice_id;

-- A few fixed playlists for read queries; ids >= 1000 are reserved for
-- benchmark mutations.
INSERT INTO public.playlist (playlist_id, name, owner_id, created_at)
SELECT p, 'Playlist ' || p, ((p - 1) % 20) + 1, TIMESTAMPTZ '2024-01-01 00:00:00+00'
FROM generate_series(1, 100) AS p;
SELECT setval('public.playlist_playlist_id_seq', 1000);

ANALYZE;
