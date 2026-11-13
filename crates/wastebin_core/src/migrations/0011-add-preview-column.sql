-- Label of a pad in the pad list, written by the editor on save.
ALTER TABLE entries ADD COLUMN preview TEXT;

-- Images and PDFs pasted through the form or the API by earlier builds were stored as binary
-- without being marked as assets. SVG is left alone: it may just as well be source text.
UPDATE entries SET is_asset = 1
WHERE is_pad = 0
  AND lower(extension) IN ('png', 'jpg', 'jpeg', 'gif', 'webp', 'avif', 'bmp', 'ico', 'tiff', 'tif', 'pdf');
