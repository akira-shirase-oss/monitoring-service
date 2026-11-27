-- M8 (D-025): per-monitor probe method and response assertions.
-- `method` is GET or opt-in HEAD — safe methods only, validated at the write
-- boundary; the CHECK keeps any other string unwriteable even by hand.
-- `expect_statuses` is the canonical "200-299,418" form (sorted, non-overlapping,
-- <= 8 ranges of 1..=599). `expect_body` is the literal byte marker searched in
-- the <= 64 KiB observed body prefix; NULL means no marker. Both columns carry
-- the contract's defaults so existing monitors behave exactly as before.
ALTER TABLE monitors ADD COLUMN method TEXT NOT NULL DEFAULT 'GET'
    CHECK (method IN ('GET', 'HEAD'));
ALTER TABLE monitors ADD COLUMN expect_statuses TEXT NOT NULL DEFAULT '200-299';
ALTER TABLE monitors ADD COLUMN expect_body BLOB;
