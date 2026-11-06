-- M6.2: per-monitor incident sensitivity. Existing monitors keep the contract's 3/2.
ALTER TABLE monitors ADD COLUMN fail_threshold INTEGER NOT NULL DEFAULT 3;
ALTER TABLE monitors ADD COLUMN recover_threshold INTEGER NOT NULL DEFAULT 2;
