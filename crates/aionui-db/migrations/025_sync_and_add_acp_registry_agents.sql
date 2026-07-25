-- Synchronize verified builtin launch contracts and add the 18 supported
-- npx/binary ACP Registry agents in one catalog migration.
-- No mutable Registry release URL/version is persisted.

UPDATE agent_metadata SET args='["--acp"]', updated_at=unixepoch('now','subsec')*1000
WHERE agent_source='builtin' AND agent_type='acp' AND backend='gemini';
UPDATE agent_metadata SET args='["--acp","--experimental-skills"]', yolo_id=NULL, updated_at=unixepoch('now','subsec')*1000
WHERE agent_source='builtin' AND agent_type='acp' AND backend='qwen';
UPDATE agent_metadata SET command='npx', args='["-y","--package","@tencent-ai/codebuddy-code","codebuddy","--acp"]',
 agent_source_info=json_remove(json_set(COALESCE(agent_source_info,'{}'),'$.binary_name','codebuddy','$.bridge_binary','npx'),'$.version'), updated_at=unixepoch('now','subsec')*1000
WHERE agent_source='builtin' AND agent_type='acp' AND backend='codebuddy';
UPDATE agent_metadata SET args='["acp-daemon"]', yolo_id=NULL, updated_at=unixepoch('now','subsec')*1000
WHERE agent_source='builtin' AND agent_type='acp' AND backend='droid';
UPDATE agent_metadata SET yolo_id=NULL, updated_at=unixepoch('now','subsec')*1000
WHERE agent_source='builtin' AND agent_type='acp' AND backend IN ('goose','auggie','kimi','copilot');
UPDATE agent_metadata SET command='cursor-agent', args='["acp"]',
 agent_source_info=json_remove(json_set(COALESCE(agent_source_info,'{}'),'$.binary_name','cursor-agent'),'$.bridge_binary','$.version','$.registry_id','$.distribution'),
 yolo_id=NULL, updated_at=unixepoch('now','subsec')*1000
WHERE agent_source='builtin' AND agent_type='acp' AND backend='cursor';
UPDATE agent_metadata SET command='npx', args='["-y","pi-acp"]',
 agent_source_info=json_remove(json_set(COALESCE(agent_source_info,'{}'),'$.binary_name','pi','$.bridge_binary','npx'),'$.version'),
 updated_at=unixepoch('now','subsec')*1000
WHERE agent_source='builtin' AND agent_type='acp' AND backend='pi';

-- Verified npx distributions use stable package identities. Autohand is an
-- adapter and therefore retains its external primary-CLI requirement.
INSERT INTO agent_metadata
 (id,name,backend,agent_type,agent_source,agent_source_info,enabled,command,args,env,native_skills_dirs,behavior_policy,yolo_id,sort_order,created_at,updated_at)
VALUES
 ('b3252207','Autohand Code','autohand','acp','builtin','{"binary_name":"autohand","bridge_binary":"npx"}',1,'npx','["-y","@autohandai/autohand-acp"]','[]',NULL,'{"supports_side_question":false,"supports_team":false,"team_capable_override":false}',NULL,3140,unixepoch('now','subsec')*1000,unixepoch('now','subsec')*1000),
 ('a11632d4','DeepAgents','deepagents','acp','builtin','{"binary_name":"deepagents","bridge_binary":"npx"}',1,'npx','["-y","deepagents-acp"]','[]','[".deepagents/skills","skills"]','{"supports_side_question":false,"supports_team":false,"team_capable_override":false}',NULL,3150,unixepoch('now','subsec')*1000,unixepoch('now','subsec')*1000),
 ('d14634b4','DimCode','dimcode','acp','builtin','{"binary_name":"dim","bridge_binary":"npx"}',1,'npx','["-y","dimcode","acp"]','[]',NULL,'{"supports_side_question":false,"supports_team":false,"team_capable_override":false}',NULL,3160,unixepoch('now','subsec')*1000,unixepoch('now','subsec')*1000),
 ('6a95eb4f','Dirac','dirac','acp','builtin','{"binary_name":"dirac","bridge_binary":"npx"}',1,'npx','["-y","dirac-cli","--acp"]','[]','[".dirac/skills"]','{"supports_side_question":false,"supports_team":false,"team_capable_override":false}','yolo',3170,unixepoch('now','subsec')*1000,unixepoch('now','subsec')*1000),
 ('5431c523','GLM Agent','glm-acp-agent','acp','builtin','{"binary_name":"glm-acp-agent","bridge_binary":"npx"}',1,'npx','["-y","glm-acp-agent"]','[]',NULL,'{"supports_side_question":false,"supports_team":false,"team_capable_override":false}','bypass_permissions',3180,unixepoch('now','subsec')*1000,unixepoch('now','subsec')*1000),
 ('145a4e5c','Grok Build','grok','acp','builtin','{"binary_name":"grok","bridge_binary":"npx"}',1,'npx','["-y","@xai-official/grok","agent","stdio"]','[]',NULL,'{"supports_side_question":false,"supports_team":false,"team_capable_override":false}',NULL,3190,unixepoch('now','subsec')*1000,unixepoch('now','subsec')*1000),
 ('54c5ccf0','Kilo','kilo','acp','builtin','{"binary_name":"kilo","bridge_binary":"npx"}',1,'npx','["-y","@kilocode/cli","acp"]','[]',NULL,'{"supports_side_question":false,"supports_team":false,"team_capable_override":false}',NULL,3200,unixepoch('now','subsec')*1000,unixepoch('now','subsec')*1000),
 ('19e05df6','Nova','nova','acp','builtin','{"binary_name":"nova","bridge_binary":"npx"}',1,'npx','["-y","@compass-ai/nova","acp"]','[]','[".compass/skills"]','{"supports_side_question":false,"supports_team":false,"team_capable_override":false}',NULL,3210,unixepoch('now','subsec')*1000,unixepoch('now','subsec')*1000),
 ('79767ac2','siGit Code','sigit','acp','builtin','{"binary_name":"sigit","bridge_binary":"npx"}',1,'npx','["-y","@smbcloud/sigit"]','[]',NULL,'{"supports_side_question":false,"supports_team":false,"team_capable_override":false}',NULL,3220,unixepoch('now','subsec')*1000,unixepoch('now','subsec')*1000)
ON CONFLICT(id) DO UPDATE SET name=excluded.name,description=NULL,backend=excluded.backend,agent_type=excluded.agent_type,agent_source=excluded.agent_source,agent_source_info=excluded.agent_source_info,enabled=excluded.enabled,command=excluded.command,args=excluded.args,env=excluded.env,native_skills_dirs=excluded.native_skills_dirs,behavior_policy=excluded.behavior_policy,yolo_id=excluded.yolo_id,sort_order=excluded.sort_order,updated_at=unixepoch('now','subsec')*1000;

-- Verified Registry binaries use their installed CLI entrypoints.
INSERT INTO agent_metadata
    (id, name, backend, agent_type, agent_source, agent_source_info, enabled,
     command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     sort_order, created_at, updated_at)
VALUES
    ('ca45e378', 'Amp', 'amp-acp', 'acp', 'builtin',
     '{"binary_name":"amp-acp"}',
     1, 'amp-acp', '[]', '[]', NULL,
     '{"supports_side_question":false,"supports_team":false,"team_capable_override":false}', 'bypass', 3230,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),
    ('d5fd9849', 'Cortex Code', 'cortex-code', 'acp', 'builtin',
     '{"binary_name":"cortex"}',
     1, 'cortex', '["acp","serve"]', '[]', '[".cortex/skills",".claude/skills"]',
     '{"supports_side_question":false,"supports_team":false,"team_capable_override":false}', 'bypass', 3240,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),
    ('e9bf0ab3', 'Corust Agent', 'corust-agent', 'acp', 'builtin',
     '{"binary_name":"corust-agent-acp"}',
     1, 'corust-agent-acp', '[]', '[]', '[".corust-agent/skills",".agents/skills"]',
     '{"supports_side_question":false,"supports_team":false,"team_capable_override":false}', NULL, 3250,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),
    ('5792d298', 'Devin', 'devin', 'acp', 'builtin',
     '{"binary_name":"devin"}',
     1, 'devin', '["acp"]', '[]', '[".devin/skills",".agents/skills",".windsurf/skills"]',
     '{"supports_side_question":false,"supports_team":false,"team_capable_override":false}', 'bypass', 3260,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),
    ('ca2e896a', 'Harn', 'harn', 'acp', 'builtin',
     '{"binary_name":"harn"}',
     1, 'harn', '["serve","acp"]', '[]', '[".harn/skills"]',
     '{"supports_side_question":false,"supports_team":false,"team_capable_override":false}', NULL, 3270,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),
    ('edd2858a', 'Junie', 'junie', 'acp', 'builtin',
     '{"binary_name":"junie"}',
     1, 'junie', '["--acp=true"]', '[]', NULL,
     '{"supports_side_question":false,"supports_team":false,"team_capable_override":false}', NULL, 3280,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),
    ('0d1e478d', 'Poolside', 'poolside', 'acp', 'builtin',
     '{"binary_name":"pool"}',
     1, 'pool', '["acp"]', '[]', '[".poolside/skills",".agents/skills"]',
     '{"supports_side_question":false,"supports_team":false,"team_capable_override":false}', NULL, 3290,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),
    ('b6ba32f7', 'Stakpak', 'stakpak', 'acp', 'builtin',
     '{"binary_name":"stakpak"}',
     1, 'stakpak', '["acp"]', '[]', '[".stakpak/skills"]',
     '{"supports_side_question":false,"supports_team":false,"team_capable_override":false}', NULL, 3300,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),
    ('2ab86949', 'VT Code', 'vtcode', 'acp', 'builtin',
     '{"binary_name":"vtcode"}',
     1, 'vtcode', '["acp"]', '[{"name":"VT_ACP_ENABLED","value":"1"},{"name":"VT_ACP_ZED_ENABLED","value":"1"}]', '[".vtcode/skills",".agents/skills"]',
     '{"supports_side_question":false,"supports_team":false,"team_capable_override":false}', NULL, 3310,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000)
ON CONFLICT(id) DO UPDATE SET
    name=excluded.name, backend=excluded.backend, agent_type=excluded.agent_type,
    agent_source=excluded.agent_source, agent_source_info=excluded.agent_source_info,
    enabled=excluded.enabled, command=excluded.command, args=excluded.args, env=excluded.env,
    native_skills_dirs=excluded.native_skills_dirs, behavior_policy=excluded.behavior_policy,
    yolo_id=excluded.yolo_id, sort_order=excluded.sort_order,
    updated_at=unixepoch('now','subsec')*1000;

UPDATE agent_metadata
SET icon = CASE backend
    WHEN 'autohand' THEN '/api/assets/logos/acp-registry/autohand.svg'
    WHEN 'deepagents' THEN '/api/assets/logos/acp-registry/deepagents.svg'
    WHEN 'dimcode' THEN '/api/assets/logos/acp-registry/dimcode.svg'
    WHEN 'dirac' THEN '/api/assets/logos/acp-registry/dirac.svg'
    WHEN 'glm-acp-agent' THEN '/api/assets/logos/acp-registry/glm-acp-agent.svg'
    WHEN 'grok' THEN '/api/assets/logos/acp-registry/grok.svg'
    WHEN 'kilo' THEN '/api/assets/logos/acp-registry/kilo.svg'
    WHEN 'nova' THEN '/api/assets/logos/acp-registry/nova.svg'
    WHEN 'sigit' THEN '/api/assets/logos/acp-registry/sigit.svg'
    WHEN 'amp-acp' THEN '/api/assets/logos/acp-registry/amp-acp.svg'
    WHEN 'cortex-code' THEN '/api/assets/logos/acp-registry/cortex-code.svg'
    WHEN 'corust-agent' THEN '/api/assets/logos/acp-registry/corust-agent.svg'
    WHEN 'devin' THEN '/api/assets/logos/acp-registry/devin.svg'
    WHEN 'harn' THEN '/api/assets/logos/acp-registry/harn.svg'
    WHEN 'junie' THEN '/api/assets/logos/acp-registry/junie.svg'
    WHEN 'poolside' THEN '/api/assets/logos/acp-registry/poolside.svg'
    WHEN 'stakpak' THEN '/api/assets/logos/acp-registry/stakpak.svg'
    WHEN 'vtcode' THEN '/api/assets/logos/acp-registry/vtcode.svg'
    ELSE icon
END,
updated_at = unixepoch('now','subsec')*1000
WHERE agent_source = 'builtin' AND agent_type = 'acp'
  AND backend IN ('autohand','deepagents','dimcode','dirac','glm-acp-agent','grok','kilo','nova','sigit',
                  'amp-acp','cortex-code','corust-agent','devin','harn','junie','poolside','stakpak','vtcode');
