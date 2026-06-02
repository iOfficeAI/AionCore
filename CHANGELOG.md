# Changelog

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
