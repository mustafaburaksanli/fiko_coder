-- Eski kayitlarda NULL, degistirilen dosya kanitinin bilinmedigini ifade eder.
-- target_paths veya task_files rolunden geriye donuk kanit turetilmez.
ALTER TABLE fiko_memory.task_history
    ADD COLUMN modified_paths text[]
    CHECK (modified_paths IS NULL OR
           (cardinality(modified_paths) <= 32 AND
            octet_length(array_to_string(modified_paths, '')) <= 32768));
