-- Object tags (JSON map), on objects and on multipart uploads (applied at completion).
ALTER TABLE objects ADD COLUMN tags TEXT NULL;
ALTER TABLE multipart_uploads ADD COLUMN tags TEXT NULL;
