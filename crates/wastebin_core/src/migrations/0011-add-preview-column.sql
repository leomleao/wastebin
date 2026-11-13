-- Label of a pad in the pad list, written by the editor on save.
ALTER TABLE entries ADD COLUMN preview TEXT;

-- Pad an uploaded asset belongs to. Assets are deleted with their pad.
ALTER TABLE entries ADD COLUMN owner INTEGER;
CREATE INDEX entries_owner ON entries (owner) WHERE owner IS NOT NULL;

-- Images and PDFs pasted through the form or the API by earlier builds were stored as binary
-- without being marked as assets. Encrypted and burn-after-reading pastes are left alone, and so
-- is SVG, which may just as well be source text.
UPDATE entries SET is_asset = 1
WHERE is_pad = 0
  AND nonce IS NULL
  AND IFNULL(burn_after_reading, 0) = 0
  AND lower(extension) IN ('png', 'jpg', 'jpeg', 'gif', 'webp', 'avif', 'bmp', 'ico', 'tiff', 'tif', 'pdf');
