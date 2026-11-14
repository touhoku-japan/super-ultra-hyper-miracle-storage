-- Checksums (x-amz-checksum-*) as JSON: algorithm, type and value.
ALTER TABLE objects ADD COLUMN checksum TEXT;
ALTER TABLE multipart_uploads ADD COLUMN checksum TEXT;
ALTER TABLE multipart_parts ADD COLUMN checksum TEXT;
