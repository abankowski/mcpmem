DROP TABLE IF EXISTS vector_embedding;
DROP TABLE IF EXISTS profile_vector;
DROP TABLE IF EXISTS index_job;
DELETE FROM taxonomy_vector WHERE subject_kind=2;
DELETE FROM taxonomy_job WHERE subject_kind=2;
