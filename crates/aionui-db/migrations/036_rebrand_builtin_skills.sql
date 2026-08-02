-- Preserve built-in skill identities across the CSBU WorkMate rebrand.
UPDATE skills
SET name = CASE name
    WHEN 'aionui-webui-setup' THEN 'csbu-workmate-webui-setup'
    WHEN 'aionui-webui-public' THEN 'csbu-workmate-webui-public'
    WHEN 'aionui-troubleshooting' THEN 'csbu-workmate-troubleshooting'
    WHEN 'aionui-config' THEN 'csbu-workmate-config'
    ELSE name
END
WHERE user_id IS NULL
  AND source = 'builtin'
  AND name IN (
    'aionui-webui-setup',
    'aionui-webui-public',
    'aionui-troubleshooting',
    'aionui-config'
  );
