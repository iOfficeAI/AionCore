UPDATE assistant_definitions
SET rule_resource_type = CASE
        WHEN source IN ('user', 'generated') THEN 'user_file'
        ELSE 'none'
    END,
    rule_resource_ref = CASE
        WHEN source IN ('user', 'generated') THEN COALESCE(rule_resource_ref, assistant_id)
        ELSE NULL
    END
WHERE rule_resource_type = 'inline';

ALTER TABLE assistant_definitions DROP COLUMN rule_inline_content;
ALTER TABLE assistant_definitions DROP COLUMN source_version;
ALTER TABLE assistant_definitions DROP COLUMN source_hash;
