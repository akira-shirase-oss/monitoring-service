-- M6.3: four incident times kept apart — first_fail_at (when the streak began) and
-- last_fail_at (latest failing check while open), joined with opened_at/resolved_at.
-- NULL on pre-existing rows: unknown times are honest, never fabricated.
ALTER TABLE incidents ADD COLUMN first_fail_at TEXT;
ALTER TABLE incidents ADD COLUMN last_fail_at TEXT;
