-- Live pads are the only entries the pad API may read, modify or list.
ALTER TABLE entries ADD COLUMN is_pad BOOLEAN NOT NULL DEFAULT 0;

-- Pads created before this column existed: plain, untitled entries written since the pad
-- feature added `updated_at`. Pastes created through the form or the API before that have no
-- `updated_at`; encrypted, burn-after-reading and asset entries are never pads.
UPDATE entries SET is_pad = 1
WHERE updated_at IS NOT NULL
  AND uid IS NULL
  AND title IS NULL
  AND nonce IS NULL
  AND IFNULL(burn_after_reading, 0) = 0
  AND is_asset = 0;

CREATE INDEX entries_pad_updated_at ON entries (is_pad, updated_at);
