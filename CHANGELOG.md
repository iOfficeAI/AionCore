# Changelog

## [0.1.32](https://github.com/iOfficeAI/AionCore/compare/v0.1.31...v0.1.32) (2026-06-18)


### Features

* **team:** centralize team MCP prompt governance ([#490](https://github.com/iOfficeAI/AionCore/issues/490)) ([5485a95](https://github.com/iOfficeAI/AionCore/commit/5485a95897c327dc2c8f4f1c44cfab7c6f628905))


### Bug Fixes

* **acp:** recover dead ACP connections ([#487](https://github.com/iOfficeAI/AionCore/issues/487)) ([8264873](https://github.com/iOfficeAI/AionCore/commit/8264873c3879a199201d3700a9f7a9a7b7ba1534))
* **conversation:** upsert streaming tool calls (AIO-30) ([#484](https://github.com/iOfficeAI/AionCore/issues/484)) ([a0b3737](https://github.com/iOfficeAI/AionCore/commit/a0b3737bf6a60c6f5483d4112dc4f4f733a9e6fa))


### Documentation

* **skills:** add cross-platform notes so Windows users translate shell examples ([#489](https://github.com/iOfficeAI/AionCore/issues/489)) ([e03b030](https://github.com/iOfficeAI/AionCore/commit/e03b0309c9fcd4914175d22787efa90e9599c8ec))

## [0.1.31](https://github.com/iOfficeAI/AionCore/compare/v0.1.30...v0.1.31) (2026-06-17)


### Features

* **assistant:** add built-in AionUi self-management assistant ([#474](https://github.com/iOfficeAI/AionCore/issues/474)) ([eea941e](https://github.com/iOfficeAI/AionCore/commit/eea941e344b9dd11338393078c63bddcc532137e))
* **assistant:** expand AionUi assistant into a butler with remote-access ([#481](https://github.com/iOfficeAI/AionCore/issues/481)) ([794c21a](https://github.com/iOfficeAI/AionCore/commit/794c21a589ef24de6f3fa03a628bb47e7958d6fe))
* enforce TeamRun ownership for agent turns ([#483](https://github.com/iOfficeAI/AionCore/issues/483)) ([4cc168a](https://github.com/iOfficeAI/AionCore/commit/4cc168a57c07879310d9e4fe8b8050735f35155a))
* **team:** support queued team_send_message semantics ([#479](https://github.com/iOfficeAI/AionCore/issues/479)) ([a57a079](https://github.com/iOfficeAI/AionCore/commit/a57a079136cbe8a5fafa0ff4d8660bbfa28a07c5))


### Bug Fixes

* **acp:** persist runtime model and mode into assistant preferences ([#482](https://github.com/iOfficeAI/AionCore/issues/482)) ([b9bcad9](https://github.com/iOfficeAI/AionCore/commit/b9bcad9d2deb94281a084c4b43a9f09c477444ed))
* harden ACP image path handling ([#477](https://github.com/iOfficeAI/AionCore/issues/477)) ([c79b5a8](https://github.com/iOfficeAI/AionCore/commit/c79b5a8a010fee82219579873a062bcca5c71fc2))
* **team:** retry handoff turns after runtime release ([#480](https://github.com/iOfficeAI/AionCore/issues/480)) ([77d252f](https://github.com/iOfficeAI/AionCore/commit/77d252fdd7e43c740043bc3f7963a06a1461fec8))

## [0.1.30](https://github.com/iOfficeAI/AionCore/compare/v0.1.29...v0.1.30) (2026-06-15)


### Features

* **acp:** use observed config options for preferences ([#468](https://github.com/iOfficeAI/AionCore/issues/468)) ([fd2d5c2](https://github.com/iOfficeAI/AionCore/commit/fd2d5c2db10e80dc478ee88c2d1f787e91015eb1))
* align team shared workspace resolution ([#475](https://github.com/iOfficeAI/AionCore/issues/475)) ([06b8e71](https://github.com/iOfficeAI/AionCore/commit/06b8e71572045ddac640bda38e2733dd9ad35f18))
* **team:** support slot-scoped team pause and wake flow ([#472](https://github.com/iOfficeAI/AionCore/issues/472)) ([398b20f](https://github.com/iOfficeAI/AionCore/commit/398b20f2279fc7b042ae65cbbe5658be953e6f31))


### Bug Fixes

* **agent:** send non-empty clientInfo in ACP initialize handshake ([#471](https://github.com/iOfficeAI/AionCore/issues/471)) ([5a8df22](https://github.com/iOfficeAI/AionCore/commit/5a8df22fd9db4b77ec0c7e9870aec78db6d7bec7))
* **agent:** wait for task shutdown during clear ([#446](https://github.com/iOfficeAI/AionCore/issues/446)) ([bea814e](https://github.com/iOfficeAI/AionCore/commit/bea814e08ddb96ccb5d09a8016e92d179a2f318a))
* **assistant:** remove star office helper remnants ([#470](https://github.com/iOfficeAI/AionCore/issues/470)) ([eec23d9](https://github.com/iOfficeAI/AionCore/commit/eec23d9fed25765c43ca9f5f50df91cd53d01888))
* **office:** fetch officecli installer from official mirror before GitHub ([#463](https://github.com/iOfficeAI/AionCore/issues/463)) ([08fbc6f](https://github.com/iOfficeAI/AionCore/commit/08fbc6f12d154d5419ae1b092a1a9352ee64250e))
* preserve assistant snapshot and skill wiring for cron ([#473](https://github.com/iOfficeAI/AionCore/issues/473)) ([2d47d8c](https://github.com/iOfficeAI/AionCore/commit/2d47d8cca71c4d0fdc3d1c2b93916c03b8c3b42c))
* **shell:** reveal file via FileManager1 D-Bus on Linux ([#466](https://github.com/iOfficeAI/AionCore/issues/466)) ([98c75ec](https://github.com/iOfficeAI/AionCore/commit/98c75ecc1bf20263f9bb682d8729d0924060f178))

## [0.1.29](https://github.com/iOfficeAI/AionCore/compare/v0.1.28...v0.1.29) (2026-06-12)


### Features

* converge team mode runtime architecture ([#464](https://github.com/iOfficeAI/AionCore/issues/464)) ([abeb9a1](https://github.com/iOfficeAI/AionCore/commit/abeb9a184a280a8da1f9089a90f7be2db3c94af4))
* **stt:** streaming transcription proxy over websocket ([#455](https://github.com/iOfficeAI/AionCore/issues/455)) ([1c19a8b](https://github.com/iOfficeAI/AionCore/commit/1c19a8b9a80be665d30310071c0c12bc95881c11))


### Bug Fixes

* **agent:** validate managed ACP platform binaries ([#462](https://github.com/iOfficeAI/AionCore/issues/462)) ([651c79f](https://github.com/iOfficeAI/AionCore/commit/651c79f0ec0e07009f637ebb2afa14de47c95ba3))
* **cron:** retry busy jobs from runtime state ([#459](https://github.com/iOfficeAI/AionCore/issues/459)) ([9918058](https://github.com/iOfficeAI/AionCore/commit/9918058788e07508ee61fc841e4c85cf757b8bb6))
* isolate ACP cancel turn completion ([#461](https://github.com/iOfficeAI/AionCore/issues/461)) ([ea01ee6](https://github.com/iOfficeAI/AionCore/commit/ea01ee6849d66dad698fee48f6374233d23985ae))
* **office:** probe star-office preferred_url host as given ([#456](https://github.com/iOfficeAI/AionCore/issues/456)) ([3c2149c](https://github.com/iOfficeAI/AionCore/commit/3c2149ca92aad8a0e19fae0d8083083500f60267))


### Code Refactoring

* **assistant:** finalize unified governance storage ([#449](https://github.com/iOfficeAI/AionCore/issues/449)) ([aba2d2a](https://github.com/iOfficeAI/AionCore/commit/aba2d2acc0a855152ae372c04b4249e956fc4cbf))


### Documentation

* clarify production logging guidance ([#460](https://github.com/iOfficeAI/AionCore/issues/460)) ([118ed03](https://github.com/iOfficeAI/AionCore/commit/118ed03b5393ec87edf8801ed7395d917c87855a))

## [0.1.28](https://github.com/iOfficeAI/AionCore/compare/v0.1.27...v0.1.28) (2026-06-11)


### Bug Fixes

* **auth:** allow same-origin framing on office preview proxy routes ([#454](https://github.com/iOfficeAI/AionCore/issues/454)) ([3543dbd](https://github.com/iOfficeAI/AionCore/commit/3543dbdc0b8ca46682b84383d2b6c4aee9bdbdd6))
* **file:** strip Windows verbatim prefix from /api/fs/browse paths ([#453](https://github.com/iOfficeAI/AionCore/issues/453)) ([f8c3f95](https://github.com/iOfficeAI/AionCore/commit/f8c3f950f9897c4e13ae7bb1dbb7816017b86480))
* **stt:** STT compatibility fixes for Groq Whisper and AionUI web frontend ([#400](https://github.com/iOfficeAI/AionCore/issues/400)) ([4c3fa09](https://github.com/iOfficeAI/AionCore/commit/4c3fa094087c8479d0c2975ec896ce46fb37abca))
* **stt:** treat blank base_url as unset and log malformed config ([#448](https://github.com/iOfficeAI/AionCore/issues/448)) ([f6b653b](https://github.com/iOfficeAI/AionCore/commit/f6b653bbdd5822dcd1b52790f0cf28db65115011))

## [0.1.27](https://github.com/iOfficeAI/AionCore/compare/v0.1.26...v0.1.27) (2026-06-10)


### Bug Fixes

* **ai-agent:** auto approve team mcp permissions ([#447](https://github.com/iOfficeAI/AionCore/issues/447)) ([096953e](https://github.com/iOfficeAI/AionCore/commit/096953e038aaa1f07333bbd6751ee927bf129e60))
* **ai-agent:** trim stderr buffer at UTF-8 char boundary ([#443](https://github.com/iOfficeAI/AionCore/issues/443)) ([7380c7c](https://github.com/iOfficeAI/AionCore/commit/7380c7cdd3c08a51de397e6af32f22361199b592))
* **office:** resolve officecli shim from node_modules/.bin after npm prefix install ([#440](https://github.com/iOfficeAI/AionCore/issues/440)) ([2fe76ee](https://github.com/iOfficeAI/AionCore/commit/2fe76eebbaab1d323b4f81acaff8187a0c00bac7))
* **office:** restore OfficeCLI installer resolution ([#444](https://github.com/iOfficeAI/AionCore/issues/444)) ([009e133](https://github.com/iOfficeAI/AionCore/commit/009e133e9e914556a579f27a3671fb5ff47333f7))

## [0.1.26](https://github.com/iOfficeAI/AionCore/compare/v0.1.25...v0.1.26) (2026-06-09)


### Bug Fixes

* **app:** use process synchronize access for parent watcher ([#438](https://github.com/iOfficeAI/AionCore/issues/438)) ([95571f1](https://github.com/iOfficeAI/AionCore/commit/95571f1c9fcd69df6ec15d8f595178c4869c15d0))

## [0.1.25](https://github.com/iOfficeAI/AionCore/compare/v0.1.24...v0.1.25) (2026-06-09)


### Features

* enforce agent runtime policy and turn-aware state ([#436](https://github.com/iOfficeAI/AionCore/issues/436)) ([b7099fe](https://github.com/iOfficeAI/AionCore/commit/b7099fee0cd2488326957dfe7811bf87e15aabb7))


### Bug Fixes

* **acp:** preserve confirmed model selection ([#437](https://github.com/iOfficeAI/AionCore/issues/437)) ([e16e11f](https://github.com/iOfficeAI/AionCore/commit/e16e11fa1e6ff9bd6449a8b1bc6bcfb5859fc865))
* **app:** stop backend when desktop exits ([#433](https://github.com/iOfficeAI/AionCore/issues/433)) ([d300235](https://github.com/iOfficeAI/AionCore/commit/d30023562bcb6d413f428fcb1dec412929a72ab3))

## [0.1.24](https://github.com/iOfficeAI/AionCore/compare/v0.1.23...v0.1.24) (2026-06-08)


### Bug Fixes

* **acp:** prefer config options catalogs ([#425](https://github.com/iOfficeAI/AionCore/issues/425)) ([9d89cc9](https://github.com/iOfficeAI/AionCore/commit/9d89cc9b46cc684a04c2cb9452ed9d792bd3a8de))
* expose managed resource preparation failure details ([#430](https://github.com/iOfficeAI/AionCore/issues/430)) ([e010024](https://github.com/iOfficeAI/AionCore/commit/e010024fce3975fe5f6930c24377bde9f636b55b))
* handle Hermes yolo fallback correctly ([#428](https://github.com/iOfficeAI/AionCore/issues/428)) ([e10d264](https://github.com/iOfficeAI/AionCore/commit/e10d26460ac6dabf292f8b1edd7eeb1c1ffd5cad))
* harden managed ACP bundle preparation and builtin CLI availability ([#426](https://github.com/iOfficeAI/AionCore/issues/426)) ([e0121f9](https://github.com/iOfficeAI/AionCore/commit/e0121f938e0160f48d2f57e9205f78bf31b92233))
* scope bundled ACP output under tool directories ([#431](https://github.com/iOfficeAI/AionCore/issues/431)) ([d079395](https://github.com/iOfficeAI/AionCore/commit/d079395b0679d5b9450497a4d088bc478c5cf45f))
* **shell:** support UNC paths in Windows terminal ([#411](https://github.com/iOfficeAI/AionCore/issues/411)) ([a041953](https://github.com/iOfficeAI/AionCore/commit/a04195329996b921cee811066994efb885b833e1))
* validate managed ACP packages via real entrypoints ([#429](https://github.com/iOfficeAI/AionCore/issues/429)) ([77221dd](https://github.com/iOfficeAI/AionCore/commit/77221ddddd883c56b954ca5cfea0f98038754efe))


### Code Refactoring

* **app:** organize CLI command boundaries ([#423](https://github.com/iOfficeAI/AionCore/issues/423)) ([cc84d52](https://github.com/iOfficeAI/AionCore/commit/cc84d523f1014978b0fb9c880842d4fd29330925))

## [0.1.23](https://github.com/iOfficeAI/AionCore/compare/v0.1.22...v0.1.23) (2026-06-07)


### Features

* **cli:** canonicalize CLI and bootstrap boundary errors ([#417](https://github.com/iOfficeAI/AionCore/issues/417)) ([9ddf82e](https://github.com/iOfficeAI/AionCore/commit/9ddf82e374f9f40fa5f7321fea54dca3a611f3c5))


### Bug Fixes

* **error:** canonicalize boundary errors ([#415](https://github.com/iOfficeAI/AionCore/issues/415)) ([84e04e1](https://github.com/iOfficeAI/AionCore/commit/84e04e122dad19eee712af29d3b5bd3f631a6fe1))
* **runtime:** report bundled resource installation failures ([#420](https://github.com/iOfficeAI/AionCore/issues/420)) ([bc4b7d9](https://github.com/iOfficeAI/AionCore/commit/bc4b7d9315727b1e4fe00cb54c1230828dd37cf1))
* **team:** inherit workspace for spawned agents ([#413](https://github.com/iOfficeAI/AionCore/issues/413)) ([82b31c5](https://github.com/iOfficeAI/AionCore/commit/82b31c5fbdb1e30a580865b4c441b1ac93ec5181))


### Code Refactoring

* centralize agent runtime session context building ([#419](https://github.com/iOfficeAI/AionCore/issues/419)) ([b21f833](https://github.com/iOfficeAI/AionCore/commit/b21f8334e2955c73be4acb2beb76c79133e2120a))
* centralize runtime turn lifecycle ([#421](https://github.com/iOfficeAI/AionCore/issues/421)) ([282c68c](https://github.com/iOfficeAI/AionCore/commit/282c68cb43a6c06862ee61c7441a9dd52a3008b7))

## [0.1.22](https://github.com/iOfficeAI/AionCore/compare/v0.1.21...v0.1.22) (2026-06-05)


### Bug Fixes

* **acp:** stabilize mode and model source of truth ([#409](https://github.com/iOfficeAI/AionCore/issues/409)) ([300bb1e](https://github.com/iOfficeAI/AionCore/commit/300bb1eba0c30207918dc9b23a2934f3542d2fe4))
* **conversation:** align workspace path availability handling ([#410](https://github.com/iOfficeAI/AionCore/issues/410)) ([30bc96b](https://github.com/iOfficeAI/AionCore/commit/30bc96b01632b2107279e81d641dfe020a3af873))
* **file:** lazy load browse roots ([#406](https://github.com/iOfficeAI/AionCore/issues/406)) ([668c562](https://github.com/iOfficeAI/AionCore/commit/668c5623ffd30243cb0ed72e670eecb578fb22cc))
* prepare managed acp tools locally without cdn ([#408](https://github.com/iOfficeAI/AionCore/issues/408)) ([2a48ae3](https://github.com/iOfficeAI/AionCore/commit/2a48ae34a498ff0e5bd37d0eedee81d2ea7d0154))


### Code Refactoring

* **error:** finish ApiError phase3 ([#398](https://github.com/iOfficeAI/AionCore/issues/398)) ([37523ab](https://github.com/iOfficeAI/AionCore/commit/37523ab628abdfaa29eaf5b0b713cb2251062146))

## [0.1.21](https://github.com/iOfficeAI/AionCore/compare/v0.1.20...v0.1.21) (2026-06-05)


### Features

* bundle managed node and ACP runtime resources ([#403](https://github.com/iOfficeAI/AionCore/issues/403)) ([6aafd57](https://github.com/iOfficeAI/AionCore/commit/6aafd572178ff4197b7a356db48fea8250d50318))

## [0.1.20](https://github.com/iOfficeAI/AionCore/compare/v0.1.19...v0.1.20) (2026-06-04)


### Features

* **acp,conversation:** elevate ACP protocol + assistant lineage logs to info ([#318](https://github.com/iOfficeAI/AionCore/issues/318)) ([fbcb299](https://github.com/iOfficeAI/AionCore/commit/fbcb29962da5ca4f52516663d592b57815875873))
* **acp:** warmup opens session eagerly + model-identity reminder via BehaviorPolicy ([#207](https://github.com/iOfficeAI/AionCore/issues/207)) ([09aa98b](https://github.com/iOfficeAI/AionCore/commit/09aa98b720bdefb8cb3b705ea835bc1d0a910cd8))
* add is_full_url flag for provider URL resolution ([#307](https://github.com/iOfficeAI/AionCore/issues/307)) ([3aa15da](https://github.com/iOfficeAI/AionCore/commit/3aa15da0c70a15da097e5bd839b83c4c0c720bf1))
* **agent:** classify structured agent send errors ([#356](https://github.com/iOfficeAI/AionCore/issues/356)) ([f52e8cd](https://github.com/iOfficeAI/AionCore/commit/f52e8cd93edb3e5edbee450ca41bef49e4cc9c48))
* **ai-agent:** add AcpSession aggregate root with reconcile pattern ([#151](https://github.com/iOfficeAI/AionCore/issues/151)) ([4b48b3a](https://github.com/iOfficeAI/AionCore/commit/4b48b3afe00cf831391e6ee45699a8aae3951e77))
* **ai-agent:** add cc-switch provider env injection for Claude ACP ([#291](https://github.com/iOfficeAI/AionCore/issues/291)) ([a7b93e7](https://github.com/iOfficeAI/AionCore/commit/a7b93e7dde78a7b254e26e2d2e25d7b9b885ad5b))
* **ai-agent:** add ReplaySuppressionGuard scaffold for session/load ([#186](https://github.com/iOfficeAI/AionCore/issues/186)) ([57f9273](https://github.com/iOfficeAI/AionCore/commit/57f9273e129ec6c8ed921d4075d3bdc8ccbafbda))
* **ai-agent:** add sort_order to agent catalog ([#129](https://github.com/iOfficeAI/AionCore/issues/129)) ([aea9354](https://github.com/iOfficeAI/AionCore/commit/aea93543b12f4756a3065fb4c86d6d4a8065a401))
* **ai-agent:** add stream broadcast channel to AcpAgentManager (W4-D25c-1) ([ab9710d](https://github.com/iOfficeAI/AionCore/commit/ab9710d5c26e053b63e4f82f9e1fad9cc6b7abe5))
* **ai-agent:** custom agent CRUD + two-step probe ([#211](https://github.com/iOfficeAI/AionCore/issues/211)) ([ebfd297](https://github.com/iOfficeAI/AionCore/commit/ebfd297c432615a8e2f038dd226549528d7ecfce))
* **ai-agent:** enable aionrs team capability via MCP injection ([18f25ec](https://github.com/iOfficeAI/AionCore/commit/18f25ec12fcd49bc239a4a52fd1783d3507e6da4))
* **ai-agent:** guard Guide MCP inject on solo team-capable backend (W5-D28c, guard only) ([9def1c1](https://github.com/iOfficeAI/AionCore/commit/9def1c17ad6d5408d5741b9aa274c1656febeffc))
* **ai-agent:** inject solo Team Guide prompt into first ACP message (W5-D28b) ([5a1ff97](https://github.com/iOfficeAI/AionCore/commit/5a1ff975cd857bd985fef839e23ef5a1ca1f8f84))
* **ai-agent:** inject solo Team Guide prompt into first ACP message (W5-D28b) ([3071eb4](https://github.com/iOfficeAI/AionCore/commit/3071eb4a10ba744b11971a76306ab54786cea7ca))
* **ai-agent:** log every CLI detection + add doctor subcommand ([#285](https://github.com/iOfficeAI/AionCore/issues/285)) ([5ef6d0a](https://github.com/iOfficeAI/AionCore/commit/5ef6d0a4d99345a502a9073dfdfa0d07cfa52a8c))
* **ai-agent:** populate yolo_id for all third-party agents + smart build cache ([#132](https://github.com/iOfficeAI/AionCore/issues/132)) ([0e1c72f](https://github.com/iOfficeAI/AionCore/commit/0e1c72fda97f5c0c75440bc903dbe0da71144dbb))
* **ai-agent:** suppress UI broadcast during session/load replay ([57f9273](https://github.com/iOfficeAI/AionCore/commit/57f9273e129ec6c8ed921d4075d3bdc8ccbafbda))
* **ai-agent:** W5-D28c Guide MCP guard (guard only, server wiring deferred) ([f7fad44](https://github.com/iOfficeAI/AionCore/commit/f7fad442cab26326889fbbea5eadfeb793a6fc76))
* **ai-agent:** wire AgentStreamChunk emits through ACP dispatch (W4-D25c-2) ([faee7ce](https://github.com/iOfficeAI/AionCore/commit/faee7ce43df16ea5207d3122cff689acaeba9533))
* **ai-agent:** wire AgentStreamChunk emits through ACP dispatch (W4-D25c-2) ([7cec7ae](https://github.com/iOfficeAI/AionCore/commit/7cec7ae583cceac47e639f3e9023410b3a045899))
* **ai-agent:** wire replay_suppression flag into AcpProtocol ([57f9273](https://github.com/iOfficeAI/AionCore/commit/57f9273e129ec6c8ed921d4075d3bdc8ccbafbda))
* **aionrs:** expose slash commands API ([c9d30ca](https://github.com/iOfficeAI/AionCore/commit/c9d30ca63b7840fd997048bb4ffbe1b4976eb63c))
* **aionrs:** expose slash commands via get_slash_commands() ([e6e120a](https://github.com/iOfficeAI/AionCore/commit/e6e120a883c522a045360325b325a81033c9d28d))
* **api-types:** add TeamMcpPhase enum + MCP status / teammate message payloads ([7f5767c](https://github.com/iOfficeAI/AionCore/commit/7f5767cf1dc71e6391413e62910a941838d18220))
* **api-types:** W5-D31a TeamMcpPhase enum + MCP/teammate payloads ([5daaeea](https://github.com/iOfficeAI/AionCore/commit/5daaeeaa7306bf97b69235883786fb8e3aac5a70))
* **app:** change database filename to aionui-backend.db ([e30f544](https://github.com/iOfficeAI/AionCore/commit/e30f5445cdcda4e2e9e3c83cd0a185bbd34bfc3e))
* **app:** copy legacy database before init on first run ([63b3b8c](https://github.com/iOfficeAI/AionCore/commit/63b3b8ca31cb130a8e1e84e7dc58d07652b4d523))
* **assets:** serve logo assets from backend ([1091b4b](https://github.com/iOfficeAI/AionCore/commit/1091b4b20dad060879d00a0799e7c43c69dd2d8c))
* **assets:** serve logo assets from backend ([78467e7](https://github.com/iOfficeAI/AionCore/commit/78467e790c02579c795882fcd1234f6db0cc84a5))
* **assistant:** allow overriding preset_agent_type on built-in assistants ([01358fa](https://github.com/iOfficeAI/AionCore/commit/01358fadd998b5cdda2e1b7df4cbb9fb426108da))
* **assistant:** allow overriding preset_agent_type on built-in assistants ([49baa5e](https://github.com/iOfficeAI/AionCore/commit/49baa5e7e3a82c833d50315ac256ba9812a0000a))
* **auth:** add local-only /api/webui/* routes + admin-seeded system user ([08fc161](https://github.com/iOfficeAI/AionCore/commit/08fc16168f0f5e721cb8223c7aac15b42b832b1c))
* **auth:** add local-only /api/webui/* routes + admin-seeded system user ([c8edae6](https://github.com/iOfficeAI/AionCore/commit/c8edae6269653f3bd48cd27a24ea8d5cf89b855e))
* **channel/dingtalk:** add structured logging for connection lifecycle ([8fca401](https://github.com/iOfficeAI/AionCore/commit/8fca4012b9a7c313ac9ad7669489c9752c710620))
* **channel/lark:** implement fragment reassembly and pong config parsing ([bf87993](https://github.com/iOfficeAI/AionCore/commit/bf87993adc61b9a07191a85376479f3ca7aac558))
* **channel/lark:** implement pbbp2 protobuf frame codec ([2e8625a](https://github.com/iOfficeAI/AionCore/commit/2e8625a34d73965ee0c7e5f8b37ad0ba7ccf7232))
* **channel/lark:** implement protobuf WebSocket protocol ([2ef399d](https://github.com/iOfficeAI/AionCore/commit/2ef399d1a7254ac85b66ef7fa3a890924f3575a5))
* **cli:** add --work-dir argument for conversation workspaces ([ed2d394](https://github.com/iOfficeAI/AionCore/commit/ed2d3942582245b243d7ab0e25175528a5db7d40))
* **cli:** add --work-dir argument for conversation workspaces ([fdfbbf5](https://github.com/iOfficeAI/AionCore/commit/fdfbbf5e36658f6aa4454f3cb5c38332a93f544b))
* **conversation:** build nested conversation response and extract preview_text in search ([74152f0](https://github.com/iOfficeAI/AionCore/commit/74152f0ab8a6d34df83f1513d75df07f0e7d604a))
* **conversation:** expose GET /api/conversations/active-count ([#243](https://github.com/iOfficeAI/AionCore/issues/243)) ([58a7b55](https://github.com/iOfficeAI/AionCore/commit/58a7b55f6cd808fe944ff0f1bfcb09c60b01e561))
* **conversation:** persist tool call events to messages table ([6ca5211](https://github.com/iOfficeAI/AionCore/commit/6ca52119c318192403c5ac103b36bcb4c7b633ae))
* **conversation:** persist tool call events to messages table ([70b59b8](https://github.com/iOfficeAI/AionCore/commit/70b59b8014585061ff004d814e8a6dcc6a35d9f4))
* **conversation:** return user_msg_id in 202 response and broadcast message.userCreated ([5bda6c3](https://github.com/iOfficeAI/AionCore/commit/5bda6c3e6ef5b8d35b5e86cbba5d923d1643d0ab))
* **conversation:** type-aware model routing + seed acp_session runtime ([#240](https://github.com/iOfficeAI/AionCore/issues/240)) ([6797d60](https://github.com/iOfficeAI/AionCore/commit/6797d600af65e289197965bf4d0dd886f58c2159))
* **db:** add maybe_copy_legacy_database for safe upgrade path ([d2ba388](https://github.com/iOfficeAI/AionCore/commit/d2ba388809fd3ec06e597f29b81f1c1381dbd544))
* **db:** safe database migration via copy-then-migrate ([762f8b2](https://github.com/iOfficeAI/AionCore/commit/762f8b291b94b4af8498bb76c4021cb39edd2762))
* **extension:** align backend discovery with existing AionUi contract ([23a45ff](https://github.com/iOfficeAI/AionCore/commit/23a45ff8bceaefe09aead9868c4a9e8dcd62b1c9))
* **extension:** align backend discovery with existing contract ([7837d5a](https://github.com/iOfficeAI/AionCore/commit/7837d5af19a2ec5695183ce1aca05702a8e1fc24))
* **extension:** bootstrap backend-served settings assets ([42849ef](https://github.com/iOfficeAI/AionCore/commit/42849ef50ea074dd73c919e090967cbb7366cff3))
* **file:** add GET /api/fs/browse shallow host-file picker ([#235](https://github.com/iOfficeAI/AionCore/issues/235)) ([1034d22](https://github.com/iOfficeAI/AionCore/commit/1034d22083df2b5c17fb430e610db8ea3f807347))
* **guide-server:** add team tool dispatch and resolve_team_context ([ce99666](https://github.com/iOfficeAI/AionCore/commit/ce99666d331522bb571d2ad15b1c4e9f9f917e36))
* **guide-stdio:** add team tool declarations for unified leader MCP ([4f39cf4](https://github.com/iOfficeAI/AionCore/commit/4f39cf455fd50de89be08189fff0ce9eb0db9dae))
* **logging:** integrate aionrs independent file logging ([da16d97](https://github.com/iOfficeAI/AionCore/commit/da16d97975202808c2b24ea884dff6f43c2de4d3))
* **logging:** integrate aionrs independent file logging ([dc950c8](https://github.com/iOfficeAI/AionCore/commit/dc950c8781b3f5fdc4aaa435c9f69e27b079ccb2))
* **mcp:** support session scoped MCP injection ([#363](https://github.com/iOfficeAI/AionCore/issues/363)) ([2974f47](https://github.com/iOfficeAI/AionCore/commit/2974f47346056ef5483fe3e9c39d58d63f714ae7))
* **runtime:** add subprocess Builder; migrate ai-agent and mcp spawns ([#195](https://github.com/iOfficeAI/AionCore/issues/195)) ([07f6d81](https://github.com/iOfficeAI/AionCore/commit/07f6d81e2bd4edc8471921c6cdde365282a99651))
* **runtime:** embed bun runtime with cross-platform extraction ([#187](https://github.com/iOfficeAI/AionCore/issues/187)) ([8c2d519](https://github.com/iOfficeAI/AionCore/commit/8c2d519fa791bd67b1d30a4420b9ba090cfd73af))
* **runtime:** enhance PATH at startup for all downstream spawn sites ([#193](https://github.com/iOfficeAI/AionCore/issues/193)) ([1e3e00e](https://github.com/iOfficeAI/AionCore/commit/1e3e00e54bc3995251899efd1ee8330106b2d7fc))
* **runtime:** full shell-style command in spawn logs ([#278](https://github.com/iOfficeAI/AionCore/issues/278)) ([dd51616](https://github.com/iOfficeAI/AionCore/commit/dd516165ae9e22fcb0573ae9d8d3aa094e54cff2))
* **service:** patch leader guide_mcp_config on backend restart ([0183062](https://github.com/iOfficeAI/AionCore/commit/0183062b32e0561379b270e2e843659e653ea468))
* **service:** skip leader in rebuild_agent_processes on team creation ([f7dfe23](https://github.com/iOfficeAI/AionCore/commit/f7dfe237cd4da4ad4db5cc52d4523b6be79b8d00))
* **team:** add codebuddy to TEAM_CAPABLE_BACKENDS and unify SPAWN_BACKEND_WHITELIST ([aa40593](https://github.com/iOfficeAI/AionCore/commit/aa40593b761b885c4d2cf3242b3abce4f5a6fee1))
* **team:** add guide::handle_aion_list_models handler (W5-D26c) ([3aba1e3](https://github.com/iOfficeAI/AionCore/commit/3aba1e3a37641d864ebf98a6bbc136cc3e4964eb))
* **team:** add GuideMcpServer skeleton (W5-D26a) ([e43e074](https://github.com/iOfficeAI/AionCore/commit/e43e074a74d908f09a1737aa8186b0d4b9d01122))
* **team:** add GuideMcpServer skeleton (W5-D26a) ([a718f6d](https://github.com/iOfficeAI/AionCore/commit/a718f6d944ce207e05701efce0a948e19107e2c6))
* **team:** add handle_inactivity_timeout watchdog recovery (W4-D22) ([b8a9822](https://github.com/iOfficeAI/AionCore/commit/b8a9822ebe584e2e614d3a65fdb8504e3e3dd825))
* **team:** add is_team_capable_backend pure fn + whitelist ([069934c](https://github.com/iOfficeAI/AionCore/commit/069934cac6d1a6530f358d67e5eb66b2a9ae8015))
* **team:** add parse_create_team_args for aion_create_team tool (W5-D26b-1) ([04ca985](https://github.com/iOfficeAI/AionCore/commit/04ca985c31d5789132c58698dad55c0dab438898))
* **team:** add resolve_full_auto_mode helper for dynamic permission mode ([dbf791d](https://github.com/iOfficeAI/AionCore/commit/dbf791d59546e6cc4ad492a3e9cc1decd220d8b3))
* **team:** add SpawnAgentRequest type and spawn_agent method skeleton ([9233b8e](https://github.com/iOfficeAI/AionCore/commit/9233b8eb61a148f875c1767e95e589a0fda21696))
* **team:** add structured logging to MCP server lifecycle ([86cb6b9](https://github.com/iOfficeAI/AionCore/commit/86cb6b91adb2a4652fcd1bd8d5afc9d6ba823dd1))
* **team:** align lead prompt with AionUi leadPrompt.ts ([a62104f](https://github.com/iOfficeAI/AionCore/commit/a62104fd4b4141a8bba087fbf4e92b3f3927380a))
* **team:** align lead prompt with AionUi leadPrompt.ts ([65e630e](https://github.com/iOfficeAI/AionCore/commit/65e630e1371ce4973db8d942ac2edb3e892981e4))
* **team:** broadcast team.mcpStatus on TCP bind (W5-D31b-1) ([48083de](https://github.com/iOfficeAI/AionCore/commit/48083dedca6d6c4a3494e756370b41f45fdbaec5))
* **team:** broadcast team.mcpStatus on TCP bind (W5-D31b-1) ([4b00dd9](https://github.com/iOfficeAI/AionCore/commit/4b00dd96dfa17acdac888cbd217863a2d679f2a3))
* **team:** clear scheduler state on remove_agent (W5-D30d-2) ([29039b0](https://github.com/iOfficeAI/AionCore/commit/29039b0a56b661a23834b570506b2d1cd108ec1e))
* **team:** clear scheduler state on remove_agent (W5-D30d-2) ([2cc6fd3](https://github.com/iOfficeAI/AionCore/commit/2cc6fd343e9c7beaf48a76566c9deb10238ec8d3))
* **team:** dynamic agent type list + fix warmup race skipping preset_context ([#208](https://github.com/iOfficeAI/AionCore/issues/208)) ([2384a23](https://github.com/iOfficeAI/AionCore/commit/2384a23281aa6ac375ef4ba1d94b6df5d05b408b))
* **team:** emit team.agent.shutdown on shutdown_approved (W5-D30a-2) ([869a3e4](https://github.com/iOfficeAI/AionCore/commit/869a3e4149119d9ed3a92e821b2311578d0fc9c9))
* **team:** emit team.agent.shutdown on shutdown_approved (W5-D30a-2) ([20c64cd](https://github.com/iOfficeAI/AionCore/commit/20c64cd52a8ba9850cf653a563a311c6e614ab9d))
* **team:** emit team.mcpStatus broadcasts from ensure_session (W5-D31b-2) ([d9333f7](https://github.com/iOfficeAI/AionCore/commit/d9333f767ac62a82f42eaccc783b083474a8bc15))
* **team:** emit team.mcpStatus broadcasts from ensure_session (W5-D31b-2) ([0db894e](https://github.com/iOfficeAI/AionCore/commit/0db894e4e3e3e44802c5ffe3610fe758879a7614))
* **team:** gate spawn_agent with caller role==Lead (W5-D29a-2) ([677504d](https://github.com/iOfficeAI/AionCore/commit/677504d720a385bed9d7779291889d66d18649bb))
* **team:** gate spawn_agent with caller role==Lead check (W5-D29a-2) ([a14441f](https://github.com/iOfficeAI/AionCore/commit/a14441fd3ce1fc1e2b9d6a0aa6827fa34394e75f))
* **team:** handle shutdown_rejected:&lt;reason&gt; in team_send_message (W5-D30b) ([ac1433d](https://github.com/iOfficeAI/AionCore/commit/ac1433d570972b04408725b71924ffbd184da243))
* **team:** handle shutdown_rejected:&lt;reason&gt; in team_send_message (W5-D30b) ([f373734](https://github.com/iOfficeAI/AionCore/commit/f373734c9b35ee43eb0378e7a207304d82e49abd))
* **team:** intercept shutdown_approved/rejected in team_send_message (W5-D30a-1) ([f91386b](https://github.com/iOfficeAI/AionCore/commit/f91386babe3999f752ea4d81edd368f23e0472d1))
* **team:** intercept shutdown_approved/rejected in team_send_message (W5-D30a-1) ([2025435](https://github.com/iOfficeAI/AionCore/commit/20254351a8c90d7f24cc1974bf279d4c5917ed6e))
* **team:** kill agent process on remove_agent (W5-D30d-1) ([ea7b0a8](https://github.com/iOfficeAI/AionCore/commit/ea7b0a805947cb911d616421bf510fc66189b6a6))
* **team:** kill agent process on remove_agent (W5-D30d-1) ([3db7911](https://github.com/iOfficeAI/AionCore/commit/3db791109b710750a52efb41ace646166a00dd8c))
* **team:** lock down leader-crash branch contract (W4-D20c) ([312bb07](https://github.com/iOfficeAI/AionCore/commit/312bb0722d4d31d45a9eba4cbf443d824eef30c2))
* **team:** normalize + uniqueness check in spawn_agent (W5-D29a-3) ([961db36](https://github.com/iOfficeAI/AionCore/commit/961db36f2c82df73e0707291107adab4884522f0))
* **team:** normalize + uniqueness-check agent name in spawn_agent (W5-D29a-3) ([1edd5df](https://github.com/iOfficeAI/AionCore/commit/1edd5df692ae566c15d2d3b90af0bf1c8c1538e7))
* **team:** parse_create_team_args pure fn (W5-D26b-1) ([7614010](https://github.com/iOfficeAI/AionCore/commit/7614010f60c95ed2dd8609c8bb347f551d393d83))
* **team:** reject shutdown_agent when target is team lead (W5-D30c) ([596df40](https://github.com/iOfficeAI/AionCore/commit/596df40903301c6cd3a0018f4296664c2e77667a))
* **team:** reject shutdown_agent when target is team lead (W5-D30c) ([21373e7](https://github.com/iOfficeAI/AionCore/commit/21373e767758155a7c3ad1c945e92d59d1472128))
* **team:** spawn_agent backend whitelist check (W5-D29a-4) ([cf81963](https://github.com/iOfficeAI/AionCore/commit/cf819631f555a133d794c5ea2cbbafbacf50f0c1))
* **team:** spawn_agent backend whitelist check (W5-D29a-4) ([a55349b](https://github.com/iOfficeAI/AionCore/commit/a55349be79391ea18cf8f29eb1a6ed85a0a58941))
* **team:** unified stdio MCP injection for guide + team tools ([3f8886b](https://github.com/iOfficeAI/AionCore/commit/3f8886b8045343cd9c86d01ed26792f4733d3869))
* **team:** use per-backend full-auto mode for all team agents ([bd82464](https://github.com/iOfficeAI/AionCore/commit/bd82464cdb62214ddb2f6f0a8487f49f123afaf4))
* **team:** W4-D18b-2 arm_wake_timeout watchdog spawn task ([56459da](https://github.com/iOfficeAI/AionCore/commit/56459daea72d17ff7e09bd14d42a122db5467768))
* **team:** W4-D18b-2 arm_wake_timeout watchdog spawn task ([61b5f16](https://github.com/iOfficeAI/AionCore/commit/61b5f16167d7eedc213dbfc92186b9cf3aaf81fa))
* **team:** W4-D22 add handle_inactivity_timeout watchdog recovery ([5871082](https://github.com/iOfficeAI/AionCore/commit/5871082b88ea3d29ac100db99bbabf2dc0422eb4))
* **team:** W5-D26c handle_aion_list_models handler ([62a986d](https://github.com/iOfficeAI/AionCore/commit/62a986d5764ab80a2dd43ab24524fb97082a7da3))
* **team:** wire exec_spawn_agent MCP dispatch into TeamSession::spawn_agent (W5-D29e) ([9298b85](https://github.com/iOfficeAI/AionCore/commit/9298b85ce17f4da7fc5cb6ea5772aa42d9816bea))
* **team:** wire exec_spawn_agent MCP dispatch into TeamSession::spawn_agent (W5-D29e) ([1bfbcbd](https://github.com/iOfficeAI/AionCore/commit/1bfbcbdb0481d3c70cb964f1dcf2bee0db1b2b88))
* **team:** wire spawn_agent end-to-end (W5-D29b) ([d3b1505](https://github.com/iOfficeAI/AionCore/commit/d3b1505af5190684bf8194a56ea614b8ab4944b8))
* **team:** wire spawn_agent end-to-end (W5-D29b) ([e815cc1](https://github.com/iOfficeAI/AionCore/commit/e815cc1380f13a750bb8109e0165d9ab91d68674))


### Bug Fixes

* **acp:** apply AvailableCommands event to session aggregate ([#270](https://github.com/iOfficeAI/AionCore/issues/270)) ([a46b561](https://github.com/iOfficeAI/AionCore/commit/a46b561b20421a59fd73e9629ef452c624781ef2))
* **acp:** load user MCP servers and emit empty-finish diagnostic (ELECTRON-1JG) ([#327](https://github.com/iOfficeAI/AionCore/issues/327)) ([2a6c2e9](https://github.com/iOfficeAI/AionCore/commit/2a6c2e943683a72eebaaa1d608be10fe5f795634))
* **acp:** re-apply preferred mode on session resume ([#139](https://github.com/iOfficeAI/AionCore/issues/139)) ([a9f3523](https://github.com/iOfficeAI/AionCore/commit/a9f352336c2060c8f07e42d84b0e5a3409a91ce5))
* **acp:** track close reason to avoid reporting user cancel as crash (ELECTRON-1K0) ([#328](https://github.com/iOfficeAI/AionCore/issues/328)) ([9506f9d](https://github.com/iOfficeAI/AionCore/commit/9506f9d1666e26b8659e3339dbfa8f13568f54ce))
* **agent:** add provider health check probe ([#358](https://github.com/iOfficeAI/AionCore/issues/358)) ([d3a8702](https://github.com/iOfficeAI/AionCore/commit/d3a8702c2c98a78085a24860bb20a15b1682dfda))
* **agent:** classify Bedrock 'model identifier is invalid' as model-not-found (AIO-12) ([#377](https://github.com/iOfficeAI/AionCore/issues/377)) ([07dc3ac](https://github.com/iOfficeAI/AionCore/commit/07dc3ac8b2fae8962e8a7e31a223875669e11ba1))
* **agent:** make codex sandbox sync non-fatal ([#370](https://github.com/iOfficeAI/AionCore/issues/370)) ([8916faa](https://github.com/iOfficeAI/AionCore/commit/8916faa9bc69ff1959aef2db83febb7c03f1441b))
* **agent:** preserve process-group cleanup after leader exit ([#369](https://github.com/iOfficeAI/AionCore/issues/369)) ([73d4fb4](https://github.com/iOfficeAI/AionCore/commit/73d4fb4f4e4647352ba3dcac07e4a6b277e46c7b))
* **agent:** tighten send_error classifier (AIO-87, AIO-89, AIO-90) ([#375](https://github.com/iOfficeAI/AionCore/issues/375)) ([d9a2f76](https://github.com/iOfficeAI/AionCore/commit/d9a2f763d14ec642c09f3aef5a2d8b716f4b0648))
* **ai-agent:** channel conversations hang with CodeBuddy due to missing yolo_id ([652cd46](https://github.com/iOfficeAI/AionCore/commit/652cd4643afb42db30a9ee5f0103ecf3374c9e2a))
* **ai-agent:** channel conversations hang with CodeBuddy due to missing yolo_id ([ffa80ff](https://github.com/iOfficeAI/AionCore/commit/ffa80ffe0e21652c7375f65dbfcb547f799f08cc))
* **ai-agent:** fall back to registry cache when session model info unavailable ([#185](https://github.com/iOfficeAI/AionCore/issues/185)) ([2d2accb](https://github.com/iOfficeAI/AionCore/commit/2d2accba018cf91b904dfb5d34a53081976b0da6))
* **ai-agent:** force-kill ACP processes on Windows (ELECTRON-1E9) ([#303](https://github.com/iOfficeAI/AionCore/issues/303)) ([e60fdd3](https://github.com/iOfficeAI/AionCore/commit/e60fdd31332512398715ed056a7f60eeee42a752))
* **ai-agent:** inject CLAUDE_CODE_EXECUTABLE + bun env for agent spawn ([589d316](https://github.com/iOfficeAI/AionCore/commit/589d316ec610d14cabfa69617543d3233021a4eb))
* **ai-agent:** make find_native_claude cross-platform (ELECTRON-1CG) ([#299](https://github.com/iOfficeAI/AionCore/issues/299)) ([fda9239](https://github.com/iOfficeAI/AionCore/commit/fda92398caa9384d8f0cdc11cf0a3616047448af))
* **ai-agent:** negotiate OpenClaw protocol v3..v4 ([#288](https://github.com/iOfficeAI/AionCore/issues/288)) ([dfeece0](https://github.com/iOfficeAI/AionCore/commit/dfeece0e6a465093090c0efdfa1f5aa93d9fa6e8))
* **ai-agent:** prevent stuck session after ACP cancel ([#313](https://github.com/iOfficeAI/AionCore/issues/313)) ([3a84bfe](https://github.com/iOfficeAI/AionCore/commit/3a84bfec1bfffd589d091efdd7b157ea1c3b2960))
* **ai-agent:** rebuild ACP session when CLI rejects stale sid (ELECTRON-1HQ) ([#320](https://github.com/iOfficeAI/AionCore/issues/320)) ([b4d8a75](https://github.com/iOfficeAI/AionCore/commit/b4d8a7505e78c48ed26af364b6e13ad4302b4727))
* **ai-agent:** restore session_id from DB on task rebuild (preserve context) ([32e1257](https://github.com/iOfficeAI/AionCore/commit/32e1257210ee308afbf08c662326a8a068a5576a))
* **ai-agent:** return 409 when remote WS not connected on cancel (ELECTRON-1CV) ([#302](https://github.com/iOfficeAI/AionCore/issues/302)) ([dc87f1c](https://github.com/iOfficeAI/AionCore/commit/dc87f1c37352be6cd820503ed4c38be4098d26ed))
* **ai-agent:** revert premature pub(crate) on AionrsResolvedConfig/new, delete broken team_smoke_e2e ([7cf47ab](https://github.com/iOfficeAI/AionCore/commit/7cf47ab0b7f27f15f5a794e6fc497b78ceae56c7))
* **ai-agent:** surface ACP startup crashes and accept work_dir paths (ELECTRON-1BT) ([#305](https://github.com/iOfficeAI/AionCore/issues/305)) ([7aa29a7](https://github.com/iOfficeAI/AionCore/commit/7aa29a78a2fa5013b9a4845217ba89d4b045822b))
* **ai-agent:** surface upstream ACP error messages without status prefix ([#268](https://github.com/iOfficeAI/AionCore/issues/268)) ([532f7e3](https://github.com/iOfficeAI/AionCore/commit/532f7e3bbee7e8389499f4d7bbda198c22363e13))
* **ai-agent:** use error level for apply_preferred_mode failure log ([f4057ce](https://github.com/iOfficeAI/AionCore/commit/f4057ce45483c67ec7efea6fa12ee8022697ccb3))
* **ai-agent:** use session/new+resume for Claude backend instead of session/load ([6e3aa91](https://github.com/iOfficeAI/AionCore/commit/6e3aa91636996caeb8111a95d6b382f84730198b))
* **aionrs:** abort engine.run() on cancel ([9eeb0a8](https://github.com/iOfficeAI/AionCore/commit/9eeb0a8620d10a3e2de74fa9d37907f3c8ab043a))
* **aionrs:** abort engine.run() on cancel instead of only emitting events ([74024c3](https://github.com/iOfficeAI/AionCore/commit/74024c3af6a8277588c4dd28e8453e1822789e15))
* **aionrs:** drop orphaned tool_call history on session resume (ELECTRON-1HV, ELECTRON-1J6) ([#330](https://github.com/iOfficeAI/AionCore/issues/330)) ([880722f](https://github.com/iOfficeAI/AionCore/commit/880722fd3b2f4e37fa5654cc5ed210cddbfd14b5))
* **aionrs:** merge preset_rules into system_prompt for preset assistants ([90fa3fd](https://github.com/iOfficeAI/AionCore/commit/90fa3fda7ee32bcabe6d0272053efbf299b3b64e))
* **aionrs:** merge preset_rules into system_prompt for preset assistants ([fec4e4f](https://github.com/iOfficeAI/AionCore/commit/fec4e4f97ef0c9655110783d1d205b3bba1b7276))
* **aionrs:** pass bedrock credentials from DB to aion-agent config ([#223](https://github.com/iOfficeAI/AionCore/issues/223)) ([9a10a57](https://github.com/iOfficeAI/AionCore/commit/9a10a57021ef8944889b27aa12ec2d35be15eb8f))
* **aionrs:** preserve tool call correlation across aborts ([#335](https://github.com/iOfficeAI/AionCore/issues/335)) ([d65c8ed](https://github.com/iOfficeAI/AionCore/commit/d65c8ed49be4a558aff99e907e359264d6729d1c))
* **aionrs:** reset runtime status on new turn to unblock StreamRelay ([2820ecf](https://github.com/iOfficeAI/AionCore/commit/2820ecf9a1a0f81b51f26c05c0d841da7a9b5e64))
* **aionrs:** second message stuck in processing state ([6181083](https://github.com/iOfficeAI/AionCore/commit/61810831ec89228188d8a7268f98f6078ff90442))
* **aionui-ai-agent:** classify aionrs API connection errors ([#389](https://github.com/iOfficeAI/AionCore/issues/389)) ([c3f16f7](https://github.com/iOfficeAI/AionCore/commit/c3f16f7453d061d0865cb7c61eca183a6d6e797f))
* **aionui-ai-agent:** strip HTML body from sanitized error detail (AIO-13) ([#380](https://github.com/iOfficeAI/AionCore/issues/380)) ([9fc5d8c](https://github.com/iOfficeAI/AionCore/commit/9fc5d8c088c644f771457bf50658ac7c6e98c1dc))
* align test request bodies with snake_case wire format ([187410f](https://github.com/iOfficeAI/AionCore/commit/187410fc551783b09729649513b486f78ea90240))
* **app:** bind backend before startup services ([#397](https://github.com/iOfficeAI/AionCore/issues/397)) ([1ae944c](https://github.com/iOfficeAI/AionCore/commit/1ae944cb4239e898f910118885df9dfa793aec54))
* **app:** update database_path test assertion to match new filename ([4bf0fc8](https://github.com/iOfficeAI/AionCore/commit/4bf0fc879f4d90018cada429efc067c65afd6b93))
* **assistant:** default agent_type to aionrs and resolve by provider (ELECTRON-1J1, ELECTRON-1KV) ([#325](https://github.com/iOfficeAI/AionCore/issues/325)) ([5c7fa04](https://github.com/iOfficeAI/AionCore/commit/5c7fa04bef47cf5bf2ea6badc66f723f0aafe1ec))
* **assistant:** pin user_data_dir to runtime --data-dir ([#274](https://github.com/iOfficeAI/AionCore/issues/274)) ([0d49022](https://github.com/iOfficeAI/AionCore/commit/0d49022f90d7950e00e0dfdb60e389116177182d))
* **auth:** return 401 for login attempts against empty password_hash ([#225](https://github.com/iOfficeAI/AionCore/issues/225)) ([669e8ba](https://github.com/iOfficeAI/AionCore/commit/669e8ba258157591b9e86136388dca4456fe62ba)), closes [#224](https://github.com/iOfficeAI/AionCore/issues/224)
* **auth:** widen rate limiter test window to prevent CI flakiness ([b5f1d05](https://github.com/iOfficeAI/AionCore/commit/b5f1d05e13f0608a68eb4dbf2250d1431f112c82))
* **backend-migration:** enforce preview sandbox boundaries ([679c1b9](https://github.com/iOfficeAI/AionCore/commit/679c1b90d5655a575cb77543f60ac4b44d3a3d90))
* **backend-migration:** enforce preview sandbox boundaries ([e5281f5](https://github.com/iOfficeAI/AionCore/commit/e5281f525cb7bcd16385abc18cfac7f4b4273eb1))
* **backend-migration:** handle symlinked skill directories ([3d261ef](https://github.com/iOfficeAI/AionCore/commit/3d261efbae1db395f956370f68a254dc72f93cf9))
* **backend-migration:** handle symlinked skill directories ([7b30072](https://github.com/iOfficeAI/AionCore/commit/7b300729001ef4b5b79f12df299ed96caeb980a9))
* channel reply stream cold start ([#366](https://github.com/iOfficeAI/AionCore/issues/366)) ([b848ddf](https://github.com/iOfficeAI/AionCore/commit/b848ddff8fe5a973c67ee3c67187c6248d8c7455))
* **channel/dingtalk:** add INPUTING state transition and skip AI Card for one-shot messages ([60ff0ae](https://github.com/iOfficeAI/AionCore/commit/60ff0ae15801bdffbe3f69568f3a00d54440010d))
* **channel/dingtalk:** fix ACK data and handle SYSTEM ping frames ([88f0916](https://github.com/iOfficeAI/AionCore/commit/88f0916d15c3f58a3929eba58afa89af926d4f1a))
* **channel/dingtalk:** fix AI Card streaming protocol ([494293f](https://github.com/iOfficeAI/AionCore/commit/494293f58466d117e4adb96e6c78ea3dab45349b))
* **channel/dingtalk:** fix chat_id encoding, card template, and message format ([5455638](https://github.com/iOfficeAI/AionCore/commit/545563881a5ac1cfc6c53e532e2d015665233ed6))
* **channel/dingtalk:** fix WebSocket Stream protocol and AI Card streaming ([9c9ee15](https://github.com/iOfficeAI/AionCore/commit/9c9ee15b0b52dd9649ce28151626880d13097233))
* **channel/dingtalk:** use body credentials for stream registration ([98b8e05](https://github.com/iOfficeAI/AionCore/commit/98b8e054ba844f1690def16544016db9616c7130))
* **channel/lark:** handle null fields in message event payload ([e912173](https://github.com/iOfficeAI/AionCore/commit/e912173f99211a3e766db06ce73d18b0336f37fb))
* **channel/lark:** handle null values in event JSON fields ([5a6c635](https://github.com/iOfficeAI/AionCore/commit/5a6c63564489e34eb4583b999c6efea6c86d5ba7))
* **channel/lark:** handle protobuf binary frames instead of text frames ([475c933](https://github.com/iOfficeAI/AionCore/commit/475c933e51a89dabc20e6771823bf184f53589f4))
* **channel/lark:** use correct auth and TLS for WS endpoint ([f6d1809](https://github.com/iOfficeAI/AionCore/commit/f6d18098cd446bd1697834264fba6317505494dd))
* **channel/lark:** use native-tls instead of rustls for WS connection ([453d0ae](https://github.com/iOfficeAI/AionCore/commit/453d0aef66950e857adc83482ce341e184acb47c))
* **channel:** enable lark and dingtalk plugins ([acc84b4](https://github.com/iOfficeAI/AionCore/commit/acc84b4afb510404212c37c4f11b2c85505e3551))
* **channel:** enable lark and dingtalk plugins in compiled binary ([833aacc](https://github.com/iOfficeAI/AionCore/commit/833aacc1d1fd9ecdcb1970cd80b711593a4f83a8))
* **channel:** pass model via extra for non-aionrs conversations ([#298](https://github.com/iOfficeAI/AionCore/issues/298)) ([eb65dfe](https://github.com/iOfficeAI/AionCore/commit/eb65dfed2a9f2ea3d9cb11699c276ba76690c03e))
* **channel:** resolve WebSocket TLS panic from ambiguous CryptoProvider ([46038bd](https://github.com/iOfficeAI/AionCore/commit/46038bd487d6156925ad25b4d6f7021ea0312b84))
* **channel:** resolve WebSocket TLS panic from ambiguous CryptoProvider ([8e7f9f0](https://github.com/iOfficeAI/AionCore/commit/8e7f9f02a49e3fe72f738b4249add47be6c7f4ba))
* **channel:** rewrite WeChat plugin to match iLink Bot protocol ([59344cd](https://github.com/iOfficeAI/AionCore/commit/59344cd7f615237ce1cc3142c2483fc8c6b64747))
* **channel:** rewrite WeChat plugin to match iLink Bot protocol ([05a5276](https://github.com/iOfficeAI/AionCore/commit/05a5276a03b7789cd9e25f0118f84b762f1dcd32))
* **ci:** add multi-platform matrix for clippy and test ([75893a5](https://github.com/iOfficeAI/AionCore/commit/75893a519de4840de7406cdd23b6067d445d56a9))
* **ci:** allow too_many_arguments on JobExecutor::new ([26918a0](https://github.com/iOfficeAI/AionCore/commit/26918a04b265a73298e216bda504b79bd47c852a))
* **ci:** auto-update Cargo.lock in release-please PR ([a3d6147](https://github.com/iOfficeAI/AionCore/commit/a3d614713cf0999f2471472dcfa6a8af4f9c0b8f))
* **ci:** auto-update Cargo.lock in release-please PR ([91f4495](https://github.com/iOfficeAI/AionCore/commit/91f44956ed24c8cb370d4ea71d9f62cd29e09fe7))
* **ci:** correct rsa dependency tree check logic ([2460714](https://github.com/iOfficeAI/AionCore/commit/24607148f570ab7b56558b1f277db5e61080bcb0))
* **ci:** ignore rsa false positive with compile-time guard ([301aa63](https://github.com/iOfficeAI/AionCore/commit/301aa635134e26040f83ab1b711ea244ef7ffa43))
* **ci:** only run security audit when dependencies change ([135b307](https://github.com/iOfficeAI/AionCore/commit/135b3073c337d5f8fb16f3be64b92b7b980b6633))
* **ci:** remove openssl-sys dependency via git2 default-features ([5753a5a](https://github.com/iOfficeAI/AionCore/commit/5753a5ab8b0821cfce85ba4e8700aa140080486c))
* **ci:** remove openssl-sys dependency via git2 default-features ([e921a9b](https://github.com/iOfficeAI/AionCore/commit/e921a9bffae1197ac05b02bfda9b4002e2bae2e6))
* **ci:** resolve clippy warnings in aionui-api-types and aionui-realtime ([7b8c1c8](https://github.com/iOfficeAI/AionCore/commit/7b8c1c82976284b149195ae67707a1d62bf01f0f))
* **ci:** resolve SSH permission and audit blocking issues ([f4f5cab](https://github.com/iOfficeAI/AionCore/commit/f4f5cabe29a210941897cdd1f79dffda81d45afa))
* **ci:** switch aionrs dependency from SSH to HTTPS URL ([05f0d1b](https://github.com/iOfficeAI/AionCore/commit/05f0d1bb51895cc68302ec89d0ae9193babc50ff))
* **ci:** switch TLS backend from native-tls to rustls ([9f42a3a](https://github.com/iOfficeAI/AionCore/commit/9f42a3a609c8204f9dc4bda2a43d2ba6e85f6e84))
* **ci:** switch TLS backend from native-tls to rustls ([aa21990](https://github.com/iOfficeAI/AionCore/commit/aa2199010359c1f9316c3059f04ab6e30b212116))
* classify missing MCP launcher runtimes ([#387](https://github.com/iOfficeAI/AionCore/issues/387)) ([fd8c20c](https://github.com/iOfficeAI/AionCore/commit/fd8c20cc0f6f36805cf5acc1fee3c708296d661a))
* **conversation:** align search response with frontend expectations ([3965cb8](https://github.com/iOfficeAI/AionCore/commit/3965cb8f0df15659335f5db2e3ce363488bad5e6))
* **conversation:** avoid thinking/text segment id collision ([#342](https://github.com/iOfficeAI/AionCore/issues/342)) ([7aae690](https://github.com/iOfficeAI/AionCore/commit/7aae690063be683101b7bacc8e916e9d2b990ede))
* **conversation:** compute thinking duration in send_thinking_done ([#183](https://github.com/iOfficeAI/AionCore/issues/183)) ([3fe82e1](https://github.com/iOfficeAI/AionCore/commit/3fe82e16494051b78c69e7dff04b91945302dfc5))
* **conversation:** kill agent process on conversation delete ([#267](https://github.com/iOfficeAI/AionCore/issues/267)) ([456ff32](https://github.com/iOfficeAI/AionCore/commit/456ff322845b96fd70583dcf1fc2fb12c2371030))
* **conversation:** send thinking_done before terminal event in stream relay ([290c395](https://github.com/iOfficeAI/AionCore/commit/290c3957d8f78eb8fb1ff1ba440d0f4bd5f7a5bf))
* **conversation:** send thinking_done before terminal event in stream relay ([dae304c](https://github.com/iOfficeAI/AionCore/commit/dae304cd97e070ebcda17e8b8c22b52c9b8075ba))
* **conversation:** unify provider/model resolution across send/cron paths (ELECTRON-1HX, ELECTRON-1HM) ([#326](https://github.com/iOfficeAI/AionCore/issues/326)) ([71e275a](https://github.com/iOfficeAI/AionCore/commit/71e275ae3295d88c9da5eacf9f959d4683b4043d))
* **conversation:** use merge strategy for tool call updates to preserve input fields ([ed7568a](https://github.com/iOfficeAI/AionCore/commit/ed7568abb4a62e43dee22b972544c5f8450ed90a))
* **cron:** add kill_and_wait to StubTaskManager in service_integration test ([07b9d7b](https://github.com/iOfficeAI/AionCore/commit/07b9d7b57edc4345185f3b36966fb04dbc8e39cb))
* **cron:** lazy-bind existing jobs and tighten orphan cleanup ([#249](https://github.com/iOfficeAI/AionCore/issues/249)) ([da9ba8f](https://github.com/iOfficeAI/AionCore/commit/da9ba8fe9ddc48abb3e52090874f1d54352becee))
* **cron:** validate aionrs agent_config and scope model to aionrs ([#242](https://github.com/iOfficeAI/AionCore/issues/242)) ([262758b](https://github.com/iOfficeAI/AionCore/commit/262758b8f13579d58f46140c2b4e2c50dedb4fe1))
* **db:** cast REAL timestamps to INTEGER in conversations table ([#275](https://github.com/iOfficeAI/AionCore/issues/275)) ([92e5fa9](https://github.com/iOfficeAI/AionCore/commit/92e5fa9f75065b85b5533476d0fbb836b0145b4e))
* **db:** normalize team and conversation JSON from camelCase to snake_case ([#168](https://github.com/iOfficeAI/AionCore/issues/168)) ([1523c8d](https://github.com/iOfficeAI/AionCore/commit/1523c8db639f700a1f4ee668105c16152482b4c8))
* **db:** prevent data loss in migration 002 table rebuilds ([563f145](https://github.com/iOfficeAI/AionCore/commit/563f145e1953499a16dc1660cb488e567b5254ac))
* **db:** prevent data loss in migration 002 table rebuilds ([abda366](https://github.com/iOfficeAI/AionCore/commit/abda3660855b272eb310bf3f5688bb9935c58345))
* **db:** serialize migrations with fs2 file lock to avoid concurrent race (ELECTRON-1KK) ([#329](https://github.com/iOfficeAI/AionCore/issues/329)) ([8550851](https://github.com/iOfficeAI/AionCore/commit/85508518b1df99b48d9ea09f474ed4d64437e8af))
* **deps:** eliminate rustls-webpki 0.101.7 vulnerability ([614741f](https://github.com/iOfficeAI/AionCore/commit/614741f14b2ac5fbef521bfbac79b0f17a135f1d))
* enforce workspace path whitespace errors across create and runtime ([#381](https://github.com/iOfficeAI/AionCore/issues/381)) ([9448a36](https://github.com/iOfficeAI/AionCore/commit/9448a36cec456648bd87a680e9dc84083038a63a))
* **extension:** fall back to directory copy when Windows symlink fails (Sentry I1) ([#331](https://github.com/iOfficeAI/AionCore/issues/331)) ([d65a0a1](https://github.com/iOfficeAI/AionCore/commit/d65a0a13449f0941a68adbeae950f094e2545bfe))
* **file:** raise paste-image body limit to 100MB ([#232](https://github.com/iOfficeAI/AionCore/issues/232)) ([6460b24](https://github.com/iOfficeAI/AionCore/commit/6460b24be0292c127aab8823fd710acfdb794269))
* **guide:** robust HTTP request reading with outer deadline ([a3b6cb9](https://github.com/iOfficeAI/AionCore/commit/a3b6cb9cbd5b4637b99e15b4d39a9d4647948ec5))
* **mcp:** clean up stdio test process trees ([#368](https://github.com/iOfficeAI/AionCore/issues/368)) ([3481956](https://github.com/iOfficeAI/AionCore/commit/3481956d4c7e2148302d9f31ecef5a88357c38e8))
* **mcp:** fix OpenAI schema rejection for no-arg team-guide tools ([#222](https://github.com/iOfficeAI/AionCore/issues/222)) ([9ef51b9](https://github.com/iOfficeAI/AionCore/commit/9ef51b9002863b42472ec78ba87159ad414623ca))
* move tracing imports inside #[cfg(unix)] to fix Windows clippy ([01a91f2](https://github.com/iOfficeAI/AionCore/commit/01a91f2a9140897becc63cd657d6edcb2f57eb78))
* **office:** stabilize flaky port_timeout_on_no_listener test ([30df119](https://github.com/iOfficeAI/AionCore/commit/30df119eec0ae5b125b2613d4573b6432ed42094))
* pin aionrs dependency to tag v0.1.18 instead of branch main ([40afe7d](https://github.com/iOfficeAI/AionCore/commit/40afe7d2076fe8f3dedc9d194734d50edc39577f))
* preserve cron timezone on legacy schedule updates ([#344](https://github.com/iOfficeAI/AionCore/issues/344)) ([6328b76](https://github.com/iOfficeAI/AionCore/commit/6328b7683133a6f74e87add6c11386ebbb0dad49))
* **prompt:** allow guide to use team tools after aion_create_team ([d1eda70](https://github.com/iOfficeAI/AionCore/commit/d1eda70a2def313eb5040b3e7e6dc1ce23f97135))
* **provider:** allow empty base_url in update for bedrock ([9fb472c](https://github.com/iOfficeAI/AionCore/commit/9fb472c1ec523fcf78cb2a9e19fafcf520fab357))
* **provider:** allow empty base_url in update request for bedrock providers ([5bca3f8](https://github.com/iOfficeAI/AionCore/commit/5bca3f8f4064ff9023431306310ae59187eeca39))
* **realtime:** forward id and read nested data in subscribe-show-open ([#323](https://github.com/iOfficeAI/AionCore/issues/323)) ([7dc222f](https://github.com/iOfficeAI/AionCore/commit/7dc222fd444e3869e7b44101fa709e4704ad0a7e))
* recover deleted conversation workspaces ([#379](https://github.com/iOfficeAI/AionCore/issues/379)) ([759afb8](https://github.com/iOfficeAI/AionCore/commit/759afb88ed404a055abd686c427e5805161b812b))
* remove unused import ErrorEventData in team scheduler tests ([cfbabee](https://github.com/iOfficeAI/AionCore/commit/cfbabeeeb008dc62ad845bc9ec14d745795d9928))
* resolve CI test failures for timestamp assertions ([e72ff55](https://github.com/iOfficeAI/AionCore/commit/e72ff55c5fcacd2c44f5a3062528e5bb9c43ced7))
* revert console_layer to match main (remove .with_ansi(false)) ([e1dfe73](https://github.com/iOfficeAI/AionCore/commit/e1dfe73db029685bac99f2f293cfab586db1f0b1))
* **runtime:** anchor bun cache and agent spawn env under AppConfig.data_dir ([#250](https://github.com/iOfficeAI/AionCore/issues/250)) ([75a107d](https://github.com/iOfficeAI/AionCore/commit/75a107df41c708c67f6494f9499f41132c3798fb))
* **runtime:** create node symlink in bundled bun directory (ELECTRON-1EY) ([#310](https://github.com/iOfficeAI/AionCore/issues/310)) ([c0ad26b](https://github.com/iOfficeAI/AionCore/commit/c0ad26bb74008609a8dac815758aabc2284a8066))
* **runtime:** include nvm node bins in startup path ([#261](https://github.com/iOfficeAI/AionCore/issues/261)) ([00c5762](https://github.com/iOfficeAI/AionCore/commit/00c57627592a567eb71fbc4edc564e2b579b86ee))
* **runtime:** make CLI detection work on Windows ([#276](https://github.com/iOfficeAI/AionCore/issues/276)) ([35bd121](https://github.com/iOfficeAI/AionCore/commit/35bd1217425a2e0d51f3e8f8e2f53ea37151c1eb))
* **runtime:** upgrade bundled bun from 1.1.38 to 1.3.13 ([#218](https://github.com/iOfficeAI/AionCore/issues/218)) ([bfc9ea6](https://github.com/iOfficeAI/AionCore/commit/bfc9ea6716be046e0b3639c4a1c1fe35a4d51a7b)), closes [#217](https://github.com/iOfficeAI/AionCore/issues/217)
* **runtime:** wait for extracted bun to be observable before returning ([#221](https://github.com/iOfficeAI/AionCore/issues/221)) ([7d3166e](https://github.com/iOfficeAI/AionCore/commit/7d3166ec6b2730b491000d936fca0fe72e375383)), closes [#220](https://github.com/iOfficeAI/AionCore/issues/220)
* split streamed message segments around tool boundaries ([#339](https://github.com/iOfficeAI/AionCore/issues/339)) ([476b1cc](https://github.com/iOfficeAI/AionCore/commit/476b1cc86f2adef8998477a666809dda50afca3e))
* **startup:** add backend readiness diagnostics ([#346](https://github.com/iOfficeAI/AionCore/issues/346)) ([ae8e01c](https://github.com/iOfficeAI/AionCore/commit/ae8e01c927118779bbad64da42a6b81aef27e9c9))
* **startup:** add startup phase diagnostics ([#388](https://github.com/iOfficeAI/AionCore/issues/388)) ([d24d027](https://github.com/iOfficeAI/AionCore/commit/d24d02726e03b852c8ee87caa872ed1605509143))
* **team-mcp:** use fixed server name to stay within 64-char tool limit (ELECTRON-1JY) ([#336](https://github.com/iOfficeAI/AionCore/issues/336)) ([eaa3aa0](https://github.com/iOfficeAI/AionCore/commit/eaa3aa098816191d8531ef0f1de12292e5e47cc5))
* **team:** add name dedup check to rename_agent ([d9be99f](https://github.com/iOfficeAI/AionCore/commit/d9be99f39983761b04d109af4267102783ea0d01))
* **team:** add name dedup check to rename_agent ([7e335e4](https://github.com/iOfficeAI/AionCore/commit/7e335e445d48f3bf290c16fb170527970849ff26))
* **team:** add name dedup check to service-layer rename_agent ([58ef06a](https://github.com/iOfficeAI/AionCore/commit/58ef06a07bc9ebb85f309262a18a2cbedf405ea3))
* **team:** add POST /api/teams/{id}/session-mode endpoint and harden wake race ([130ee22](https://github.com/iOfficeAI/AionCore/commit/130ee22e74b149bf00133299e520774454fc843d))
* **team:** add session-mode endpoint and harden wake race ([ba12c6f](https://github.com/iOfficeAI/AionCore/commit/ba12c6f2d5baca8651724b56bf7aaaab064c2d1a))
* **team:** auto-restore all team sessions on backend startup ([c4b764c](https://github.com/iOfficeAI/AionCore/commit/c4b764c059aba33d4620fcc561b5d225a719a59b))
* **team:** close remaining gaps in event loop refactor ([79d43dd](https://github.com/iOfficeAI/AionCore/commit/79d43ddf81eecad892a0c12a4e74fab08884ee81))
* **team:** default exec_members status to idle for cold-start agents ([7598cbb](https://github.com/iOfficeAI/AionCore/commit/7598cbb9d7b7527101213eaa50d9fbef39b20122))
* **team:** don't warmup agents on session restore (preserve history) ([51b783b](https://github.com/iOfficeAI/AionCore/commit/51b783be1cbc969b1f75bf74c2a49f32c839604c))
* **team:** fix aion_list_models empty models + missing aionrs ([#228](https://github.com/iOfficeAI/AionCore/issues/228)) ([e8ad8b3](https://github.com/iOfficeAI/AionCore/commit/e8ad8b3916bb6614f3a33218ba79407e1ac3c343))
* **team:** force-stop agents on team delete, fix DashMap deadlock ([b3d8cd1](https://github.com/iOfficeAI/AionCore/commit/b3d8cd17407de2cc53d0dc09b22b790812822241))
* **team:** force-stop agents on team delete, fix DashMap deadlock ([7b9bf2b](https://github.com/iOfficeAI/AionCore/commit/7b9bf2b551813d9d49671014a32e74803e4e0412))
* **team:** guard try_wake against duplicate StreamRelay when agent already running ([b5b0029](https://github.com/iOfficeAI/AionCore/commit/b5b0029ccf5ed7788799f0372ad9bce9210125dd))
* **team:** HTTP MCP handler was using auth token as slot_id ([fb56d58](https://github.com/iOfficeAI/AionCore/commit/fb56d58a1fe9ca826348edb8bae31c50149e19e6))
* **team:** implement drain_mailbox pattern to prevent message loss on warmup race ([fab1c8b](https://github.com/iOfficeAI/AionCore/commit/fab1c8b9d518098f371960a230d689e4466320df))
* **team:** make wake fire-and-forget (don't block MCP response) ([42a43be](https://github.com/iOfficeAI/AionCore/commit/42a43bec22895960e8f8c7ac6243c1d7a78b4bb9))
* **team:** mirror non-user mailbox rows into target agent conversations ([b42e1f9](https://github.com/iOfficeAI/AionCore/commit/b42e1f90d01092ef897a59a504fc9b8ae2d5e270))
* **team:** model routing + schema unification + lazy warm mode persistence ([#286](https://github.com/iOfficeAI/AionCore/issues/286)) ([199a392](https://github.com/iOfficeAI/AionCore/commit/199a392caca600ef215bb2ae71bfd82bda7bb744))
* **team:** move set_status(Working) to point-of-no-return in wake paths ([5415626](https://github.com/iOfficeAI/AionCore/commit/5415626dafef1eb962d411fa6e51c7ac4a655c75))
* **team:** move spawn_agent warmup to background task to unblock MCP response ([9f31504](https://github.com/iOfficeAI/AionCore/commit/9f31504a770731ff974e00f37e5e4c07f534dcca))
* **team:** pass workspace from CreateTeamRequest to agent conversations ([#273](https://github.com/iOfficeAI/AionCore/issues/273)) ([f4e3f32](https://github.com/iOfficeAI/AionCore/commit/f4e3f32e3a1a9f8fa34769205fa031b6037af00e))
* **team:** persist rename via service when MCP tool is used ([6af431c](https://github.com/iOfficeAI/AionCore/commit/6af431c87dfb8c9989b104377ab6cb2dbd05f89b))
* **team:** persist rename via service when MCP tool is used ([964c5ca](https://github.com/iOfficeAI/AionCore/commit/964c5cac85849408ee66c1ccfb94ca8584e03f9b))
* **team:** prevent guide MCP leak into team leaders ([#247](https://github.com/iOfficeAI/AionCore/issues/247)) ([71df5c6](https://github.com/iOfficeAI/AionCore/commit/71df5c64eab22d06ce0872d2226065bec1c026d7))
* **team:** properly track agent status + cold-start role prompt ([b7324bc](https://github.com/iOfficeAI/AionCore/commit/b7324bcb6ed9b80707f4e675f7c87c4298b35e8b))
* **team:** register finish_subscriber for spawned agents and unconditionally clear finalize dedup window ([c2bcb75](https://github.com/iOfficeAI/AionCore/commit/c2bcb75f6cb0099db3a3732d245a75c1f7e013fc))
* **team:** remove 30s heartbeat polling from agent event loop ([752be98](https://github.com/iOfficeAI/AionCore/commit/752be981a487c1281fee48bf0b21d4d9c1574bbf))
* **team:** remove await_agent_finish deadlock, simplify event loop ([d08d3e6](https://github.com/iOfficeAI/AionCore/commit/d08d3e62ec5f7b02d5df9ecea45042c372eafb5f))
* **team:** remove redundant 30s heartbeat polling from event loop ([88672eb](https://github.com/iOfficeAI/AionCore/commit/88672ebb59aa9eb25e3396ed312bd1d807df4e07))
* **team:** resolve agent name to slot_id in team MCP tools ([bfb54f5](https://github.com/iOfficeAI/AionCore/commit/bfb54f54437940dc00d408a0ffdee60db206188f))
* **team:** resolve clippy collapsible_if and correct server_count when skip_leader ([7c951fc](https://github.com/iOfficeAI/AionCore/commit/7c951fcc6ab84384ba46312ee07b0dd968a73684))
* **team:** resolve MCP bridge deadlock, auto-approve permissions, and conversation reuse ([bcc89b2](https://github.com/iOfficeAI/AionCore/commit/bcc89b20ab05630e324fbcb9497ad133d86f150f))
* **team:** resolve provider_id from providers table for aionrs spawn ([b02e590](https://github.com/iOfficeAI/AionCore/commit/b02e590d018d7236afb025660481a3375075d9cf))
* **team:** resolve provider_id from providers table for aionrs spawn ([86a9f41](https://github.com/iOfficeAI/AionCore/commit/86a9f41ab35705dd2352d3ff5c66b037807530a3))
* **team:** resolve provider_id from providers table for aionrs spawn ([c7efe54](https://github.com/iOfficeAI/AionCore/commit/c7efe54aa881d13d2e593850e0cd5ea74441a037))
* **team:** resolve team communication bugs — wake retry, SYSTEM NOTE stripping, MCP injection, and wake lock fixes ([37b84d5](https://github.com/iOfficeAI/AionCore/commit/37b84d514f25970138abc311f172e06ffde96741))
* **team:** set agent status to Working before wake (review fix) ([f776b2e](https://github.com/iOfficeAI/AionCore/commit/f776b2ebfff7fcea897f440e261dc41275c41976))
* **team:** update e2e_smoke test for TeamMcpServer::start new signature ([ba80c46](https://github.com/iOfficeAI/AionCore/commit/ba80c46f8fc3352818f587fbc78603d3a11d2065))
* **team:** wake target agent after team_send_message writes mailbox ([4122278](https://github.com/iOfficeAI/AionCore/commit/412227856186f6b96799a94fc33d2dd63d5876bf))
* **test:** add missing extra_mcp_servers field in test helper ([de4aa7a](https://github.com/iOfficeAI/AionCore/commit/de4aa7a2781b379aed766b08a204eae75f3d7b9f))
* **test:** repair broken E2E tests for message search and team session ([455a1b8](https://github.com/iOfficeAI/AionCore/commit/455a1b80e36eef0f74b46fc75e43c5f65724fc17))
* **test:** repair broken E2E tests for message search and team session ([bb4ec74](https://github.com/iOfficeAI/AionCore/commit/bb4ec746c44b474db57aa2db5392a881506c2727))
* **test:** update integration test mocks for D29b spawn_agent changes ([72fdd16](https://github.com/iOfficeAI/AionCore/commit/72fdd164a805460db7a56cfb014ab31f4574b30e))
* update aionrs dependency to latest (0.1.18) ([c7690d9](https://github.com/iOfficeAI/AionCore/commit/c7690d98c5dce8e8dff2d97a616d63d74d9b7fe9))
* update justfile URLs to HTTPS for aionrs ([e855d00](https://github.com/iOfficeAI/AionCore/commit/e855d00203cd1ec909ab519350aa38692acbbedd))
* update test to match AcpAgentManager::new 3-tuple return type ([9c06395](https://github.com/iOfficeAI/AionCore/commit/9c0639553962402b45ee2d760f215f3fe72c5aaf))
* use &gt;= in timestamp assertion to avoid CI timing flake ([312afca](https://github.com/iOfficeAI/AionCore/commit/312afcae65f738ca6ef163f8ba00a1ac5e83c022))
* use cargo check instead of cargo update in update-aionrs recipe ([5b7d52a](https://github.com/iOfficeAI/AionCore/commit/5b7d52a4492e308ffe8fe55c71e7323b1eee6ecf))


### Performance Improvements

* **ci:** run tests on Linux only, keep clippy on all platforms ([91e79ad](https://github.com/iOfficeAI/AionCore/commit/91e79adb2ac465fde5e906d7bc1fde1e87aa2d49))
* **team:** lazy warm — only start agent processes on first message ([#282](https://github.com/iOfficeAI/AionCore/issues/282)) ([6281f31](https://github.com/iOfficeAI/AionCore/commit/6281f31ac6a2656c1af51891589770f4583e00c2))


### Code Refactoring

* **acp:** replace first-message flag with PromptPipeline + hooks ([#262](https://github.com/iOfficeAI/AionCore/issues/262)) ([d1f3c95](https://github.com/iOfficeAI/AionCore/commit/d1f3c95eebea4053c45b56dcd973fe4e44f0fe6c))
* **ai-agent,conversation:** move session ops, tighten visibility, fix idle scanner + backfill ACP metadata ([#254](https://github.com/iOfficeAI/AionCore/issues/254)) ([299c5d3](https://github.com/iOfficeAI/AionCore/commit/299c5d30e7674d91136139886c9b02a99b932515))
* **ai-agent:** ACP aggregate state cleanup + observed-state persistence ([#199](https://github.com/iOfficeAI/AionCore/issues/199)) ([70790f8](https://github.com/iOfficeAI/AionCore/commit/70790f86370fe28d44e958e1b9bb6308c03878bf))
* **ai-agent:** ACP concurrency + single-flight task builds ([#138](https://github.com/iOfficeAI/AionCore/issues/138)) ([bfa2726](https://github.com/iOfficeAI/AionCore/commit/bfa2726b5b6165517e8cabfd1dadcc2194c3c572))
* **ai-agent:** ACP state management enhancements ([#201](https://github.com/iOfficeAI/AionCore/issues/201)) ([17eaf6d](https://github.com/iOfficeAI/AionCore/commit/17eaf6d66f41af0ffaf3a483b0068b63d73409b1))
* **ai-agent:** AcpSession single source of truth ([#7](https://github.com/iOfficeAI/AionCore/issues/7)b-3~5, [#7](https://github.com/iOfficeAI/AionCore/issues/7)c) ([#153](https://github.com/iOfficeAI/AionCore/issues/153)) ([a379072](https://github.com/iOfficeAI/AionCore/commit/a3790721abb4e56a18357e716d758a2136c07f3b))
* **ai-agent:** align manager/ layout to target.md §2 + service method renames ([#182](https://github.com/iOfficeAI/AionCore/issues/182)) ([59e88ef](https://github.com/iOfficeAI/AionCore/commit/59e88ef8a7f0f2d889dbd55339cf2e8103f3a90c))
* **ai-agent:** break event-tracker self-loop via notification mpsc (M3) ([#172](https://github.com/iOfficeAI/AionCore/issues/172)) ([9ccbcbf](https://github.com/iOfficeAI/AionCore/commit/9ccbcbf1e50fe38c0da5fbababaee6fc16491264))
* **ai-agent:** directory reorganization and pub-use surface cleanup ([#158](https://github.com/iOfficeAI/AionCore/issues/158)) ([67cdfd8](https://github.com/iOfficeAI/AionCore/commit/67cdfd89f4599e4da373595bbb0ab53a6f069a8a))
* **ai-agent:** drop trivial default-flag test + note Claude path in load_session ([57f9273](https://github.com/iOfficeAI/AionCore/commit/57f9273e129ec6c8ed921d4075d3bdc8ccbafbda))
* **ai-agent:** drop unused backend_binary_path + AcpAgentManager field rule ([#127](https://github.com/iOfficeAI/AionCore/issues/127)) ([84aba02](https://github.com/iOfficeAI/AionCore/commit/84aba020fe82c7fc1b072c8dd80e5dab104c9248))
* **ai-agent:** extract AcpSessionParams into factory/acp_assembler ([#150](https://github.com/iOfficeAI/AionCore/issues/150)) ([3c5fedf](https://github.com/iOfficeAI/AionCore/commit/3c5fedf5a48c28726147e4747c0c69de635ab0b7))
* **ai-agent:** introduce AgentRuntime value object + migrate managers (Stage 7, fixes m1) ([#175](https://github.com/iOfficeAI/AionCore/issues/175)) ([ad07f09](https://github.com/iOfficeAI/AionCore/commit/ad07f09cb56e42d2711281b26a5921dbaf06096f))
* **ai-agent:** introduce AgentService as sole business layer (Stage 2) ([#171](https://github.com/iOfficeAI/AionCore/issues/171)) ([0ca0ebc](https://github.com/iOfficeAI/AionCore/commit/0ca0ebcf1000de25fda058f6e740a50f7b4f3673))
* **ai-agent:** merge acp_service into acp_agent_service ([#141](https://github.com/iOfficeAI/AionCore/issues/141)) ([9da6076](https://github.com/iOfficeAI/AionCore/commit/9da6076896f3187fb675226327ca5b53c1da5a74))
* **ai-agent:** migrate permission producers to AcpPermission (B3) ([#174](https://github.com/iOfficeAI/AionCore/issues/174)) ([b7541b9](https://github.com/iOfficeAI/AionCore/commit/b7541b9766758f9cb62b1ff34e3d77053d4dccb7))
* **ai-agent:** move BuildExtra types to aionui-api-types ([#148](https://github.com/iOfficeAI/AionCore/issues/148)) ([a00131e](https://github.com/iOfficeAI/AionCore/commit/a00131e2e76bfee0219da1ad495edc7bca10cf8e))
* **ai-agent:** move connection_test to aionui-system ([#146](https://github.com/iOfficeAI/AionCore/issues/146)) ([87ae85a](https://github.com/iOfficeAI/AionCore/commit/87ae85a23445f7cd05aae5703db81346cc43c2cc))
* **ai-agent:** move middleware.rs to aionui-conversation ([#145](https://github.com/iOfficeAI/AionCore/issues/145)) ([bdbc2ba](https://github.com/iOfficeAI/AionCore/commit/bdbc2ba09b02690c51e167101143616819467141))
* **ai-agent:** move remote_agent files to manager/remote/ ([#147](https://github.com/iOfficeAI/AionCore/issues/147)) ([4861953](https://github.com/iOfficeAI/AionCore/commit/48619536915411581208449f7080f19d2286fddf))
* **ai-agent:** narrow capability module to pub(crate) (batch 3) ([c3039d9](https://github.com/iOfficeAI/AionCore/commit/c3039d9b7365a0e5e5641e91409e8ae0a42aa62b))
* **ai-agent:** narrow capability module visibility to pub(crate) (batch 3) ([28e87c3](https://github.com/iOfficeAI/AionCore/commit/28e87c30bef4071e05bce60305a693cde12e4c67))
* **ai-agent:** narrow protocol internals visibility (batch 1) ([f0e5c3c](https://github.com/iOfficeAI/AionCore/commit/f0e5c3c3fdbd2c6974b06d8144d8e9bb583a9cc3))
* **ai-agent:** narrow visibility batch 2 — types & translate internals ([75bf498](https://github.com/iOfficeAI/AionCore/commit/75bf498cf3a1c6eeeec96631bc7b9b6bd0f55ac3))
* **ai-agent:** narrow visibility of batch-2 internals to pub(crate) ([29090af](https://github.com/iOfficeAI/AionCore/commit/29090afe0c909c266515b716dc90b76bf3ad2470))
* **ai-agent:** narrow visibility of protocol internals to pub(crate) ([06c5a1a](https://github.com/iOfficeAI/AionCore/commit/06c5a1a559849f0a28785cdcc2fd8552a972fb4f))
* **ai-agent:** PR [#7](https://github.com/iOfficeAI/AionCore/issues/7)b-6 aggregate cleanup (value objects, domain events, CatalogForwarder) ([#154](https://github.com/iOfficeAI/AionCore/issues/154)) ([2f0a689](https://github.com/iOfficeAI/AionCore/commit/2f0a6896e1aa5616da0b3fb8af9412cab7ef65eb))
* **ai-agent:** PR [#8](https://github.com/iOfficeAI/AionCore/issues/8) IAgentTask trait + AgentInstance enum dispatch ([#157](https://github.com/iOfficeAI/AionCore/issues/157)) ([1fef472](https://github.com/iOfficeAI/AionCore/commit/1fef472411cf180d9121cbf8847faaa5e13b2876))
* **ai-agent:** remove dead api_client module ([#143](https://github.com/iOfficeAI/AionCore/issues/143)) ([7173eb5](https://github.com/iOfficeAI/AionCore/commit/7173eb5c7918dcdebbccaac22ea8f44cce27217d))
* **ai-agent:** remove redundant stream_tx pipeline ([ededeb7](https://github.com/iOfficeAI/AionCore/commit/ededeb7fb6ff23868532031b0cad3b402bab7a9e))
* **ai-agent:** remove redundant stream_tx pipeline, unify on event_tx ([09676ad](https://github.com/iOfficeAI/AionCore/commit/09676ad5fc798ce87dc3a29a2f6cd720aecc2b3e))
* **ai-agent:** rename IAgentTask::stop → cancel across all managers and tests ([#184](https://github.com/iOfficeAI/AionCore/issues/184)) ([6686eef](https://github.com/iOfficeAI/AionCore/commit/6686eef6102b3a8361a77d1e87525a2bcd836fd5))
* **ai-agent:** route set_mode through reconcile (M2) ([#173](https://github.com/iOfficeAI/AionCore/issues/173)) ([d8b2df2](https://github.com/iOfficeAI/AionCore/commit/d8b2df22f7af9fa4267bae327a2eef22df4c20b1))
* **ai-agent:** split acp/agent.rs into submodules (Stage 1) ([#169](https://github.com/iOfficeAI/AionCore/issues/169)) ([9ca0bbc](https://github.com/iOfficeAI/AionCore/commit/9ca0bbc978b1cbe4b42c41f991215913254dbe63))
* **ai-agent:** split AcpAgentManager construction into new+init phases ([#202](https://github.com/iOfficeAI/AionCore/issues/202)) ([dcb638a](https://github.com/iOfficeAI/AionCore/commit/dcb638a9e47baad289b8f3cf794e74801203ecc9))
* **ai-agent:** split cli_process into submodules (Stage 9.1, M5) ([#179](https://github.com/iOfficeAI/AionCore/issues/179)) ([00fe2e4](https://github.com/iOfficeAI/AionCore/commit/00fe2e49f726285800ca7bc4ad05a1b9e3da4bc2))
* **ai-agent:** split factory::build_agent per agent (Stage 8) ([#176](https://github.com/iOfficeAI/AionCore/issues/176)) ([6939fdc](https://github.com/iOfficeAI/AionCore/commit/6939fdcf5fa40426ba74002072b2f5a54cc10e0f))
* **ai-agent:** split openclaw/agent.rs into submodules (Stage 9.3, M5) ([#181](https://github.com/iOfficeAI/AionCore/issues/181)) ([e79f36a](https://github.com/iOfficeAI/AionCore/commit/e79f36acf047e49c7186032908f59733b101fe79))
* **ai-agent:** split skill_manager into submodules (Stage 9.2, M5) ([#180](https://github.com/iOfficeAI/AionCore/issues/180)) ([0fe499c](https://github.com/iOfficeAI/AionCore/commit/0fe499cfe6c734c6ce55ae36e2931c95c1ebaf3b))
* **ai-agent:** split stream_event.rs into protocol/events/ ([#149](https://github.com/iOfficeAI/AionCore/issues/149)) ([87c1d0b](https://github.com/iOfficeAI/AionCore/commit/87c1d0b8d363d7d441f773a9679684c57dc335e0))
* **ai-agent:** Stage 10 cleanup — drop realtime dep, deprecate dead event variants, make aionrs Drop explicit ([#177](https://github.com/iOfficeAI/AionCore/issues/177)) ([86c5a20](https://github.com/iOfficeAI/AionCore/commit/86c5a207561d3339735c73a5b5c2fce52ff93b58))
* **ai-agent:** tighten ReplaySuppressionGuard dead_code scope + test notes ([57f9273](https://github.com/iOfficeAI/AionCore/commit/57f9273e129ec6c8ed921d4075d3bdc8ccbafbda))
* **aionrs:** use Config::resolve() for consistent config loading ([e5dddd7](https://github.com/iOfficeAI/AionCore/commit/e5dddd73ad4962a29861504b598e0d9902dcd87f))
* **aionrs:** use Config::resolve() for consistent config loading ([31ecc67](https://github.com/iOfficeAI/AionCore/commit/31ecc678dd5bb4956cc94509b5fd7020f82686ef))
* **api-types:** restructure MessageSearchItem with nested conversation and preview_text ([4d5812f](https://github.com/iOfficeAI/AionCore/commit/4d5812f76171bf05e3cd2c24f121c9d5e998e7d8))
* **app:** extract CLI definitions to cli.rs ([#280](https://github.com/iOfficeAI/AionCore/issues/280)) ([5685d52](https://github.com/iOfficeAI/AionCore/commit/5685d5237b8f51c70e80895b1c654325c958196e))
* **app:** introduce commands/ module with layered bootstrap for subcommands ([#283](https://github.com/iOfficeAI/AionCore/issues/283)) ([1216597](https://github.com/iOfficeAI/AionCore/commit/12165971cfae61d85376c102ef9f9afc5a7c5bbf))
* **app:** replace argv sniffing with clap Subcommand for mcp-* helpers ([#277](https://github.com/iOfficeAI/AionCore/issues/277)) ([c3d137c](https://github.com/iOfficeAI/AionCore/commit/c3d137c9e5fdcb12e29d5ca7abd6a0585bbc6c8d))
* **app:** split monolithic lib.rs/main.rs into per-module files ([#284](https://github.com/iOfficeAI/AionCore/issues/284)) ([f3462cb](https://github.com/iOfficeAI/AionCore/commit/f3462cbb1d6d830a3a368a76b2d9ea6424f21b64))
* **channel/lark:** defer WS connection from initialize to start ([f3a8a5b](https://github.com/iOfficeAI/AionCore/commit/f3a8a5b246af2f1c7b26b49281977f32134ebda1))
* **conversation:** server-generate msg_id and unify stream relay ([#135](https://github.com/iOfficeAI/AionCore/issues/135)) ([eb04d61](https://github.com/iOfficeAI/AionCore/commit/eb04d6133c3c32a8e0f8e2c6c9a12f493e21f49a))
* **conversation:** simplify clone and improve test coverage ([#246](https://github.com/iOfficeAI/AionCore/issues/246)) ([91c322b](https://github.com/iOfficeAI/AionCore/commit/91c322bb5427c4cca739595b988561ceb40efa54))
* **db:** absorb 014_behavior_policy_supports_team into 001 seed data ([#234](https://github.com/iOfficeAI/AionCore/issues/234)) ([c8e584b](https://github.com/iOfficeAI/AionCore/commit/c8e584b55e5921cfe5524495508aff3aae37d3cc))
* **db:** consolidate migrations + fix aionrs session resume ([#233](https://github.com/iOfficeAI/AionCore/issues/233)) ([1ebb78a](https://github.com/iOfficeAI/AionCore/commit/1ebb78afa7177e9a8919d329cfe205b93c096804))
* **db:** expand MessageSearchRow to include full conversation fields ([3d07d46](https://github.com/iOfficeAI/AionCore/commit/3d07d466d8e1692aa3527544ee8b3598551b2c9c))
* **docs:** move team docs from docs/teams/ to crates/aionui-team/docs/ ([ac49870](https://github.com/iOfficeAI/AionCore/commit/ac49870cf863a780cc9f201b389668317d58e6bc))
* **error:** migrate phase2 service errors ([#395](https://github.com/iOfficeAI/AionCore/issues/395)) ([c6c42ee](https://github.com/iOfficeAI/AionCore/commit/c6c42eea051083c94eb822163149de6fef2387a3))
* four-layer architecture (connect / conv / biz) ([#349](https://github.com/iOfficeAI/AionCore/issues/349)) ([2a11285](https://github.com/iOfficeAI/AionCore/commit/2a11285e316ffc7f0076d385dad8e09a4af2de4b))
* **guide-server:** remove async send_message to leader, return actionable tool result ([06dc20f](https://github.com/iOfficeAI/AionCore/commit/06dc20ff087c79800199e9bc449b42865c135fdf))
* rename binary from aioncli to aioncore ([#293](https://github.com/iOfficeAI/AionCore/issues/293)) ([ae78cd1](https://github.com/iOfficeAI/AionCore/commit/ae78cd19f599fb3c8845ba5d3e208a75bf310368))
* rename binary from aionui-backend to aioncli ([#289](https://github.com/iOfficeAI/AionCore/issues/289)) ([30eeca3](https://github.com/iOfficeAI/AionCore/commit/30eeca37661441ba9474aa7dc51ca911abda0bfb))
* roll out aionui_runtime::Builder to office/extension/shell ([#196](https://github.com/iOfficeAI/AionCore/issues/196)) ([18f10c3](https://github.com/iOfficeAI/AionCore/commit/18f10c325b87567e3c8dbf13cde39c4787a4a501))
* **team:** pull-based event loop for agent lifecycle ([de891f6](https://github.com/iOfficeAI/AionCore/commit/de891f6038dd4037b2575b09f24d22aac899b087))
* **team:** replace push-based finish_subscribers with pull-based event loop ([3a05785](https://github.com/iOfficeAI/AionCore/commit/3a05785771632d3510d1830f49c47357d065122e))
* **team:** split scheduler.rs into submodule directory ([b0a84ad](https://github.com/iOfficeAI/AionCore/commit/b0a84ad06250155ca06f0afbc35cc245ce8d5a03))
* **team:** use let-chains syntax in wake_agent_in_session ([e6c4d22](https://github.com/iOfficeAI/AionCore/commit/e6c4d22833de82011958fdb816d8bfa839994342))


### Documentation

* add test scope requirements for happy path, bad path, security, and WebSocket events ([f00e9d4](https://github.com/iOfficeAI/AionCore/commit/f00e9d4c0e3fbc7f3bf5d7daf4000f716db42030))
* **assistants:** add word-form-creator to preset-id-whitelist ([#252](https://github.com/iOfficeAI/AionCore/issues/252)) ([343b15b](https://github.com/iOfficeAI/AionCore/commit/343b15bc5ab362c566ae0d8e2ed61921d58b9497))
* catch up with aionui-backend → AionCore rename ([#301](https://github.com/iOfficeAI/AionCore/issues/301)) ([40a7e83](https://github.com/iOfficeAI/AionCore/commit/40a7e83618bb62b145378e333e26b66dc0061c89))
* clean up AGENTS.md and sync ARCHITECTURE.md ([a6d1efa](https://github.com/iOfficeAI/AionCore/commit/a6d1efaf3d8167f1cb7987249938768410b9f1be))
* **team:** add phase2 bugfix status doc for handoff ([5aaf860](https://github.com/iOfficeAI/AionCore/commit/5aaf8601eee8b27bbfe8fc8d78563b06aa45c998))
* **teams:** clarify ensureSession timing (page-enter only, not on create) ([98783ce](https://github.com/iOfficeAI/AionCore/commit/98783cef6c0ab747d7c606b55ca341f06ad6bf40))
* **teams:** refresh frontend-guide + internals for Wave 4/5 progress ([52f5b75](https://github.com/iOfficeAI/AionCore/commit/52f5b7594ad1b10a60b94ecb2ea744e6523ee6fe))
* **teams:** update frontend-guide for Wave 5 completion ([03d0bca](https://github.com/iOfficeAI/AionCore/commit/03d0bca53784aaab23655c5c82707df41c241676))

## [0.1.19](https://github.com/iOfficeAI/AionCore/compare/v0.1.18...v0.1.19) (2026-06-02)


### Bug Fixes

* **aionui-ai-agent:** classify aionrs API connection errors ([#389](https://github.com/iOfficeAI/AionCore/issues/389)) ([c3f16f7](https://github.com/iOfficeAI/AionCore/commit/c3f16f7453d061d0865cb7c61eca183a6d6e797f))
* classify missing MCP launcher runtimes ([#387](https://github.com/iOfficeAI/AionCore/issues/387)) ([fd8c20c](https://github.com/iOfficeAI/AionCore/commit/fd8c20cc0f6f36805cf5acc1fee3c708296d661a))
* enforce workspace path whitespace errors across create and runtime ([#381](https://github.com/iOfficeAI/AionCore/issues/381)) ([9448a36](https://github.com/iOfficeAI/AionCore/commit/9448a36cec456648bd87a680e9dc84083038a63a))
* **startup:** add startup phase diagnostics ([#388](https://github.com/iOfficeAI/AionCore/issues/388)) ([d24d027](https://github.com/iOfficeAI/AionCore/commit/d24d02726e03b852c8ee87caa872ed1605509143))

## [0.1.18](https://github.com/iOfficeAI/AionCore/compare/v0.1.17...v0.1.18) (2026-06-01)


### Bug Fixes

* **agent:** classify Bedrock 'model identifier is invalid' as model-not-found (AIO-12) ([#377](https://github.com/iOfficeAI/AionCore/issues/377)) ([07dc3ac](https://github.com/iOfficeAI/AionCore/commit/07dc3ac8b2fae8962e8a7e31a223875669e11ba1))
* **agent:** preserve process-group cleanup after leader exit ([#369](https://github.com/iOfficeAI/AionCore/issues/369)) ([73d4fb4](https://github.com/iOfficeAI/AionCore/commit/73d4fb4f4e4647352ba3dcac07e4a6b277e46c7b))
* **agent:** tighten send_error classifier (AIO-87, AIO-89, AIO-90) ([#375](https://github.com/iOfficeAI/AionCore/issues/375)) ([d9a2f76](https://github.com/iOfficeAI/AionCore/commit/d9a2f763d14ec642c09f3aef5a2d8b716f4b0648))
* **aionui-ai-agent:** strip HTML body from sanitized error detail (AIO-13) ([#380](https://github.com/iOfficeAI/AionCore/issues/380)) ([9fc5d8c](https://github.com/iOfficeAI/AionCore/commit/9fc5d8c088c644f771457bf50658ac7c6e98c1dc))
* recover deleted conversation workspaces ([#379](https://github.com/iOfficeAI/AionCore/issues/379)) ([759afb8](https://github.com/iOfficeAI/AionCore/commit/759afb88ed404a055abd686c427e5805161b812b))

## [0.1.17](https://github.com/iOfficeAI/AionCore/compare/v0.1.16...v0.1.17) (2026-05-30)


### Bug Fixes

* **agent:** make codex sandbox sync non-fatal ([#370](https://github.com/iOfficeAI/AionCore/issues/370)) ([8916faa](https://github.com/iOfficeAI/AionCore/commit/8916faa9bc69ff1959aef2db83febb7c03f1441b))

## [0.1.16](https://github.com/iOfficeAI/AionCore/compare/v0.1.15...v0.1.16) (2026-05-29)


### Features

* **agent:** classify structured agent send errors ([#356](https://github.com/iOfficeAI/AionCore/issues/356)) ([f52e8cd](https://github.com/iOfficeAI/AionCore/commit/f52e8cd93edb3e5edbee450ca41bef49e4cc9c48))
* **mcp:** support session scoped MCP injection ([#363](https://github.com/iOfficeAI/AionCore/issues/363)) ([2974f47](https://github.com/iOfficeAI/AionCore/commit/2974f47346056ef5483fe3e9c39d58d63f714ae7))


### Bug Fixes

* channel reply stream cold start ([#366](https://github.com/iOfficeAI/AionCore/issues/366)) ([b848ddf](https://github.com/iOfficeAI/AionCore/commit/b848ddff8fe5a973c67ee3c67187c6248d8c7455))
* **mcp:** clean up stdio test process trees ([#368](https://github.com/iOfficeAI/AionCore/issues/368)) ([3481956](https://github.com/iOfficeAI/AionCore/commit/3481956d4c7e2148302d9f31ecef5a88357c38e8))

## [0.1.15](https://github.com/iOfficeAI/AionCore/compare/v0.1.14...v0.1.15) (2026-05-28)


### Bug Fixes

* **agent:** add provider health check probe ([#358](https://github.com/iOfficeAI/AionCore/issues/358)) ([d3a8702](https://github.com/iOfficeAI/AionCore/commit/d3a8702c2c98a78085a24860bb20a15b1682dfda))

## [0.1.14](https://github.com/iOfficeAI/AionCore/compare/v0.1.13...v0.1.14) (2026-05-27)


### Bug Fixes

* preserve cron timezone on legacy schedule updates ([#344](https://github.com/iOfficeAI/AionCore/issues/344)) ([6328b76](https://github.com/iOfficeAI/AionCore/commit/6328b7683133a6f74e87add6c11386ebbb0dad49))
* **startup:** add backend readiness diagnostics ([#346](https://github.com/iOfficeAI/AionCore/issues/346)) ([ae8e01c](https://github.com/iOfficeAI/AionCore/commit/ae8e01c927118779bbad64da42a6b81aef27e9c9))


### Code Refactoring

* four-layer architecture (connect / conv / biz) ([#349](https://github.com/iOfficeAI/AionCore/issues/349)) ([2a11285](https://github.com/iOfficeAI/AionCore/commit/2a11285e316ffc7f0076d385dad8e09a4af2de4b))

## [0.1.13](https://github.com/iOfficeAI/AionCore/compare/v0.1.12...v0.1.13) (2026-05-26)


### Bug Fixes

* **conversation:** avoid thinking/text segment id collision ([#342](https://github.com/iOfficeAI/AionCore/issues/342)) ([7aae690](https://github.com/iOfficeAI/AionCore/commit/7aae690063be683101b7bacc8e916e9d2b990ede))

## [0.1.12](https://github.com/iOfficeAI/AionCore/compare/v0.1.11...v0.1.12) (2026-05-26)


### Bug Fixes

* **aionrs:** drop orphaned tool_call history on session resume (ELECTRON-1HV, ELECTRON-1J6) ([#330](https://github.com/iOfficeAI/AionCore/issues/330)) ([880722f](https://github.com/iOfficeAI/AionCore/commit/880722fd3b2f4e37fa5654cc5ed210cddbfd14b5))
* **aionrs:** preserve tool call correlation across aborts ([#335](https://github.com/iOfficeAI/AionCore/issues/335)) ([d65c8ed](https://github.com/iOfficeAI/AionCore/commit/d65c8ed49be4a558aff99e907e359264d6729d1c))
* **conversation:** unify provider/model resolution across send/cron paths (ELECTRON-1HX, ELECTRON-1HM) ([#326](https://github.com/iOfficeAI/AionCore/issues/326)) ([71e275a](https://github.com/iOfficeAI/AionCore/commit/71e275ae3295d88c9da5eacf9f959d4683b4043d))
* split streamed message segments around tool boundaries ([#339](https://github.com/iOfficeAI/AionCore/issues/339)) ([476b1cc](https://github.com/iOfficeAI/AionCore/commit/476b1cc86f2adef8998477a666809dda50afca3e))
* startup materialization and migration races ([#333](https://github.com/iOfficeAI/AionCore/issues/333)) ([bd26ccc](https://github.com/iOfficeAI/AionCore/commit/bd26ccc9e8b08e7ea953f03b383cff6f67e2acba))
* **team-mcp:** use fixed server name to stay within 64-char tool limit (ELECTRON-1JY) ([#336](https://github.com/iOfficeAI/AionCore/issues/336)) ([eaa3aa0](https://github.com/iOfficeAI/AionCore/commit/eaa3aa098816191d8531ef0f1de12292e5e47cc5))
* Windows release CRT linkage ([#332](https://github.com/iOfficeAI/AionCore/issues/332)) ([fb445da](https://github.com/iOfficeAI/AionCore/commit/fb445da48defb54769253ee8623b784f178a3d2e))

## [0.1.11](https://github.com/iOfficeAI/AionCore/compare/v0.1.10...v0.1.11) (2026-05-25)


### Bug Fixes

* **acp:** load user MCP servers and emit empty-finish diagnostic (ELECTRON-1JG) ([#327](https://github.com/iOfficeAI/AionCore/issues/327)) ([2a6c2e9](https://github.com/iOfficeAI/AionCore/commit/2a6c2e943683a72eebaaa1d608be10fe5f795634))
* **acp:** track close reason to avoid reporting user cancel as crash (ELECTRON-1K0) ([#328](https://github.com/iOfficeAI/AionCore/issues/328)) ([9506f9d](https://github.com/iOfficeAI/AionCore/commit/9506f9d1666e26b8659e3339dbfa8f13568f54ce))
* **ai-agent:** rebuild ACP session when CLI rejects stale sid (ELECTRON-1HQ) ([#320](https://github.com/iOfficeAI/AionCore/issues/320)) ([b4d8a75](https://github.com/iOfficeAI/AionCore/commit/b4d8a7505e78c48ed26af364b6e13ad4302b4727))
* **assistant:** default agent_type to aionrs and resolve by provider (ELECTRON-1J1, ELECTRON-1KV) ([#325](https://github.com/iOfficeAI/AionCore/issues/325)) ([5c7fa04](https://github.com/iOfficeAI/AionCore/commit/5c7fa04bef47cf5bf2ea6badc66f723f0aafe1ec))
* **db:** serialize migrations with fs2 file lock to avoid concurrent race (ELECTRON-1KK) ([#329](https://github.com/iOfficeAI/AionCore/issues/329)) ([8550851](https://github.com/iOfficeAI/AionCore/commit/85508518b1df99b48d9ea09f474ed4d64437e8af))
* **extension:** fall back to directory copy when Windows symlink fails (Sentry I1) ([#331](https://github.com/iOfficeAI/AionCore/issues/331)) ([d65a0a1](https://github.com/iOfficeAI/AionCore/commit/d65a0a13449f0941a68adbeae950f094e2545bfe))
* **realtime:** forward id and read nested data in subscribe-show-open ([#323](https://github.com/iOfficeAI/AionCore/issues/323)) ([7dc222f](https://github.com/iOfficeAI/AionCore/commit/7dc222fd444e3869e7b44101fa709e4704ad0a7e))

## [0.1.10](https://github.com/iOfficeAI/AionCore/compare/v0.1.9...v0.1.10) (2026-05-24)


### Miscellaneous

* **deps:** bump aionrs from v0.1.25 to v0.1.26

## [0.1.9](https://github.com/iOfficeAI/AionCore/compare/v0.1.8...v0.1.9) (2026-05-22)


### Features

* **acp,conversation:** elevate ACP protocol + assistant lineage logs to info ([#318](https://github.com/iOfficeAI/AionCore/issues/318)) ([fbcb299](https://github.com/iOfficeAI/AionCore/commit/fbcb29962da5ca4f52516663d592b57815875873))

## [0.1.8](https://github.com/iOfficeAI/AionCore/compare/v0.1.7...v0.1.8) (2026-05-21)


### Features

* add is_full_url flag for provider URL resolution ([#307](https://github.com/iOfficeAI/AionCore/issues/307)) ([3aa15da](https://github.com/iOfficeAI/AionCore/commit/3aa15da0c70a15da097e5bd839b83c4c0c720bf1))


### Bug Fixes

* **ai-agent:** prevent stuck session after ACP cancel ([#313](https://github.com/iOfficeAI/AionCore/issues/313)) ([3a84bfe](https://github.com/iOfficeAI/AionCore/commit/3a84bfec1bfffd589d091efdd7b157ea1c3b2960))
* **runtime:** create node symlink in bundled bun directory (ELECTRON-1EY) ([#310](https://github.com/iOfficeAI/AionCore/issues/310)) ([c0ad26b](https://github.com/iOfficeAI/AionCore/commit/c0ad26bb74008609a8dac815758aabc2284a8066))

## [0.1.7](https://github.com/iOfficeAI/AionCore/compare/v0.1.6...v0.1.7) (2026-05-19)


### Bug Fixes

* **ai-agent:** surface ACP startup crashes and accept work_dir paths (ELECTRON-1BT) ([#305](https://github.com/iOfficeAI/AionCore/issues/305)) ([7aa29a7](https://github.com/iOfficeAI/AionCore/commit/7aa29a78a2fa5013b9a4845217ba89d4b045822b))

## [0.1.6](https://github.com/iOfficeAI/AionCore/compare/v0.1.5...v0.1.6) (2026-05-19)


### Bug Fixes

* **ai-agent:** force-kill ACP processes on Windows (ELECTRON-1E9) ([#303](https://github.com/iOfficeAI/AionCore/issues/303)) ([e60fdd3](https://github.com/iOfficeAI/AionCore/commit/e60fdd31332512398715ed056a7f60eeee42a752))
* **ai-agent:** make find_native_claude cross-platform (ELECTRON-1CG) ([#299](https://github.com/iOfficeAI/AionCore/issues/299)) ([fda9239](https://github.com/iOfficeAI/AionCore/commit/fda92398caa9384d8f0cdc11cf0a3616047448af))
* **ai-agent:** return 409 when remote WS not connected on cancel (ELECTRON-1CV) ([#302](https://github.com/iOfficeAI/AionCore/issues/302)) ([dc87f1c](https://github.com/iOfficeAI/AionCore/commit/dc87f1c37352be6cd820503ed4c38be4098d26ed))


### Documentation

* catch up with aionui-backend → AionCore rename ([#301](https://github.com/iOfficeAI/AionCore/issues/301)) ([40a7e83](https://github.com/iOfficeAI/AionCore/commit/40a7e83618bb62b145378e333e26b66dc0061c89))

## [0.1.5](https://github.com/iOfficeAI/AionCore/compare/v0.1.4...v0.1.5) (2026-05-19)


### Features

* **ai-agent:** add cc-switch provider env injection for Claude ACP ([#291](https://github.com/iOfficeAI/AionCore/issues/291)) ([a7b93e7](https://github.com/iOfficeAI/AionCore/commit/a7b93e7dde78a7b254e26e2d2e25d7b9b885ad5b))


### Bug Fixes

* **channel:** pass model via extra for non-aionrs conversations ([#298](https://github.com/iOfficeAI/AionCore/issues/298)) ([eb65dfe](https://github.com/iOfficeAI/AionCore/commit/eb65dfed2a9f2ea3d9cb11699c276ba76690c03e))


### Code Refactoring

* rename binary from aioncli to aioncore ([#293](https://github.com/iOfficeAI/AionCore/issues/293)) ([ae78cd1](https://github.com/iOfficeAI/AionCore/commit/ae78cd19f599fb3c8845ba5d3e208a75bf310368))

## [0.1.4](https://github.com/iOfficeAI/AionCLI/compare/v0.1.3...v0.1.4) (2026-05-16)


### Features

* **ai-agent:** log every CLI detection + add doctor subcommand ([#285](https://github.com/iOfficeAI/AionCLI/issues/285)) ([5ef6d0a](https://github.com/iOfficeAI/AionCLI/commit/5ef6d0a4d99345a502a9073dfdfa0d07cfa52a8c))
* **runtime:** full shell-style command in spawn logs ([#278](https://github.com/iOfficeAI/AionCLI/issues/278)) ([dd51616](https://github.com/iOfficeAI/AionCLI/commit/dd516165ae9e22fcb0573ae9d8d3aa094e54cff2))


### Bug Fixes

* **ai-agent:** negotiate OpenClaw protocol v3..v4 ([#288](https://github.com/iOfficeAI/AionCLI/issues/288)) ([dfeece0](https://github.com/iOfficeAI/AionCLI/commit/dfeece0e6a465093090c0efdfa1f5aa93d9fa6e8))
* **team:** model routing + schema unification + lazy warm mode persistence ([#286](https://github.com/iOfficeAI/AionCLI/issues/286)) ([199a392](https://github.com/iOfficeAI/AionCLI/commit/199a392caca600ef215bb2ae71bfd82bda7bb744))


### Performance Improvements

* **team:** lazy warm — only start agent processes on first message ([#282](https://github.com/iOfficeAI/AionCLI/issues/282)) ([6281f31](https://github.com/iOfficeAI/AionCLI/commit/6281f31ac6a2656c1af51891589770f4583e00c2))


### Code Refactoring

* **app:** extract CLI definitions to cli.rs ([#280](https://github.com/iOfficeAI/AionCLI/issues/280)) ([5685d52](https://github.com/iOfficeAI/AionCLI/commit/5685d5237b8f51c70e80895b1c654325c958196e))
* **app:** introduce commands/ module with layered bootstrap for subcommands ([#283](https://github.com/iOfficeAI/AionCLI/issues/283)) ([1216597](https://github.com/iOfficeAI/AionCLI/commit/12165971cfae61d85376c102ef9f9afc5a7c5bbf))
* **app:** replace argv sniffing with clap Subcommand for mcp-* helpers ([#277](https://github.com/iOfficeAI/AionCLI/issues/277)) ([c3d137c](https://github.com/iOfficeAI/AionCLI/commit/c3d137c9e5fdcb12e29d5ca7abd6a0585bbc6c8d))
* **app:** split monolithic lib.rs/main.rs into per-module files ([#284](https://github.com/iOfficeAI/AionCLI/issues/284)) ([f3462cb](https://github.com/iOfficeAI/AionCLI/commit/f3462cbb1d6d830a3a368a76b2d9ea6424f21b64))
* rename binary from aionui-backend to aioncli ([#289](https://github.com/iOfficeAI/AionCLI/issues/289)) ([30eeca3](https://github.com/iOfficeAI/AionCLI/commit/30eeca37661441ba9474aa7dc51ca911abda0bfb))

## [0.1.3](https://github.com/iOfficeAI/aionui-backend/compare/v0.1.2...v0.1.3) (2026-05-15)


### Bug Fixes

* **acp:** apply AvailableCommands event to session aggregate ([#270](https://github.com/iOfficeAI/aionui-backend/issues/270)) ([a46b561](https://github.com/iOfficeAI/aionui-backend/commit/a46b561b20421a59fd73e9629ef452c624781ef2))
* **assistant:** pin user_data_dir to runtime --data-dir ([#274](https://github.com/iOfficeAI/aionui-backend/issues/274)) ([0d49022](https://github.com/iOfficeAI/aionui-backend/commit/0d49022f90d7950e00e0dfdb60e389116177182d))
* **db:** cast REAL timestamps to INTEGER in conversations table ([#275](https://github.com/iOfficeAI/aionui-backend/issues/275)) ([92e5fa9](https://github.com/iOfficeAI/aionui-backend/commit/92e5fa9f75065b85b5533476d0fbb836b0145b4e))
* **runtime:** make CLI detection work on Windows ([#276](https://github.com/iOfficeAI/aionui-backend/issues/276)) ([35bd121](https://github.com/iOfficeAI/aionui-backend/commit/35bd1217425a2e0d51f3e8f8e2f53ea37151c1eb))
* **team:** pass workspace from CreateTeamRequest to agent conversations ([#273](https://github.com/iOfficeAI/aionui-backend/issues/273)) ([f4e3f32](https://github.com/iOfficeAI/aionui-backend/commit/f4e3f32e3a1a9f8fa34769205fa031b6037af00e))

## [0.1.2](https://github.com/iOfficeAI/aionui-backend/compare/v0.1.1...v0.1.2) (2026-05-14)


### Features

* **aionrs:** expose slash commands API ([c9d30ca](https://github.com/iOfficeAI/aionui-backend/commit/c9d30ca63b7840fd997048bb4ffbe1b4976eb63c))
* **aionrs:** expose slash commands via get_slash_commands() ([e6e120a](https://github.com/iOfficeAI/aionui-backend/commit/e6e120a883c522a045360325b325a81033c9d28d))
* **cli:** add --work-dir argument for conversation workspaces ([ed2d394](https://github.com/iOfficeAI/aionui-backend/commit/ed2d3942582245b243d7ab0e25175528a5db7d40))
* **cli:** add --work-dir argument for conversation workspaces ([fdfbbf5](https://github.com/iOfficeAI/aionui-backend/commit/fdfbbf5e36658f6aa4454f3cb5c38332a93f544b))


### Bug Fixes

* **ai-agent:** surface upstream ACP error messages without status prefix ([#268](https://github.com/iOfficeAI/aionui-backend/issues/268)) ([532f7e3](https://github.com/iOfficeAI/aionui-backend/commit/532f7e3bbee7e8389499f4d7bbda198c22363e13))
* **aionrs:** abort engine.run() on cancel ([9eeb0a8](https://github.com/iOfficeAI/aionui-backend/commit/9eeb0a8620d10a3e2de74fa9d37907f3c8ab043a))
* **aionrs:** abort engine.run() on cancel instead of only emitting events ([74024c3](https://github.com/iOfficeAI/aionui-backend/commit/74024c3af6a8277588c4dd28e8453e1822789e15))
* **ci:** allow too_many_arguments on JobExecutor::new ([26918a0](https://github.com/iOfficeAI/aionui-backend/commit/26918a04b265a73298e216bda504b79bd47c852a))
* **ci:** auto-update Cargo.lock in release-please PR ([a3d6147](https://github.com/iOfficeAI/aionui-backend/commit/a3d614713cf0999f2471472dcfa6a8af4f9c0b8f))
* **ci:** auto-update Cargo.lock in release-please PR ([91f4495](https://github.com/iOfficeAI/aionui-backend/commit/91f44956ed24c8cb370d4ea71d9f62cd29e09fe7))
* **ci:** resolve clippy warnings in aionui-api-types and aionui-realtime ([7b8c1c8](https://github.com/iOfficeAI/aionui-backend/commit/7b8c1c82976284b149195ae67707a1d62bf01f0f))
* **conversation:** kill agent process on conversation delete ([#267](https://github.com/iOfficeAI/aionui-backend/issues/267)) ([456ff32](https://github.com/iOfficeAI/aionui-backend/commit/456ff322845b96fd70583dcf1fc2fb12c2371030))
* **runtime:** include nvm node bins in startup path ([#261](https://github.com/iOfficeAI/aionui-backend/issues/261)) ([00c5762](https://github.com/iOfficeAI/aionui-backend/commit/00c57627592a567eb71fbc4edc564e2b579b86ee))


### Code Refactoring

* **acp:** replace first-message flag with PromptPipeline + hooks ([#262](https://github.com/iOfficeAI/aionui-backend/issues/262)) ([d1f3c95](https://github.com/iOfficeAI/aionui-backend/commit/d1f3c95eebea4053c45b56dcd973fe4e44f0fe6c))

## [0.1.1](https://github.com/iOfficeAI/aionui-backend/compare/v0.1.0...v0.1.1) (2026-05-13)


### Features

* **logging:** integrate aionrs independent file logging ([da16d97](https://github.com/iOfficeAI/aionui-backend/commit/da16d97975202808c2b24ea884dff6f43c2de4d3))
* **logging:** integrate aionrs independent file logging ([dc950c8](https://github.com/iOfficeAI/aionui-backend/commit/dc950c8781b3f5fdc4aaa435c9f69e27b079ccb2))


### Bug Fixes

* **office:** stabilize flaky port_timeout_on_no_listener test ([30df119](https://github.com/iOfficeAI/aionui-backend/commit/30df119eec0ae5b125b2613d4573b6432ed42094))
* revert console_layer to match main (remove .with_ansi(false)) ([e1dfe73](https://github.com/iOfficeAI/aionui-backend/commit/e1dfe73db029685bac99f2f293cfab586db1f0b1))
* **team:** remove 30s heartbeat polling from agent event loop ([752be98](https://github.com/iOfficeAI/aionui-backend/commit/752be981a487c1281fee48bf0b21d4d9c1574bbf))
* **team:** remove redundant 30s heartbeat polling from event loop ([88672eb](https://github.com/iOfficeAI/aionui-backend/commit/88672ebb59aa9eb25e3396ed312bd1d807df4e07))


### Code Refactoring

* **ai-agent,conversation:** move session ops, tighten visibility, fix idle scanner + backfill ACP metadata ([#254](https://github.com/iOfficeAI/aionui-backend/issues/254)) ([299c5d3](https://github.com/iOfficeAI/aionui-backend/commit/299c5d30e7674d91136139886c9b02a99b932515))


### Documentation

* **assistants:** add word-form-creator to preset-id-whitelist ([#252](https://github.com/iOfficeAI/aionui-backend/issues/252)) ([343b15b](https://github.com/iOfficeAI/aionui-backend/commit/343b15bc5ab362c566ae0d8e2ed61921d58b9497))
