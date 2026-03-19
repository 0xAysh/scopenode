-- Migrate logs_bloom from raw BLOB to lowercase hex TEXT.
-- Hex is human-readable in DB browsers and still compact enough (512 chars vs 256 bytes).

ALTER TABLE headers RENAME COLUMN logs_bloom TO logs_bloom_old;
ALTER TABLE headers ADD COLUMN logs_bloom TEXT NOT NULL DEFAULT '';
UPDATE headers SET logs_bloom = lower(hex(logs_bloom_old));
ALTER TABLE headers DROP COLUMN logs_bloom_old;
