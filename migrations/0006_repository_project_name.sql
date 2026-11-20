-- Azure: nama project (bukan GUID di `project`), untuk label `project/repo`. GitHub: NULL.
-- Repo Azure lama NULL sampai di-backfill saat daftar remote repo dibuka atau webhook di-resync.
ALTER TABLE repositories ADD COLUMN project_name text NULL;
