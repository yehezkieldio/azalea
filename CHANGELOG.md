# Changelog

All notable changes to this project will be documented in this file.

**NOTE:** Changes are ordered by date, starting with the most latest to the oldest.

> This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) and uses [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) for commit messages.


## 0.2.0 (February 25, 2026)

### <!-- 10 -->🧪 Test
- [`8fb649b`](https://github.com/yehezkieldio/azalea-vb/commit/8fb649b3f69e278c0a5521f99dce79a68c1e6070) **media**: Add valid_host strategy and expand tweet url test
- [`22f0dfe`](https://github.com/yehezkieldio/azalea-vb/commit/22f0dfe2e1a2bdbcb41291c1895f954b89b384f6) **media**: Add tweet url parsing tests
- [`8f7683c`](https://github.com/yehezkieldio/azalea-vb/commit/8f7683cdee631ee25b68d5e382e9194dc666d547) **media**: Add twittpr domain to tweet url parsing
- [`e5fc854`](https://github.com/yehezkieldio/azalea-vb/commit/e5fc854d5a040a06aef794c60c527d8d3f692070) **media**: Add www url parsing tests and rename bare url tests

### <!-- 16 -->🤖 CI/CD
- [`6638d75`](https://github.com/yehezkieldio/azalea-vb/commit/6638d75cf66b0064955adeb1c38dc5930cc87900) **ci**: Decouple ci and release into seperate workflows
- [`8960334`](https://github.com/yehezkieldio/azalea-vb/commit/896033469a5ddb6a6a769e36edc99660065ff673) **deps**: Bump actions/checkout to v6 and attest-build-provenance to v3
- [`e2607ed`](https://github.com/yehezkieldio/azalea-vb/commit/e2607ed08d6c1ffcc82dadeb403693d03e4ebcf7) **ci**: Rename release job names in ci_release workflow

### <!-- 2 -->🧩 Dependencies Updates
- [`223ac57`](https://github.com/yehezkieldio/azalea-vb/commit/223ac57e3eb5b4c74983f8909bd93d7f4b44ec09) **deps**: Update rust crate anyhow to v1.0.102 ([#3](https://github.com/yehezkieldio/azalea-vb/issues/3))
- [`06dbd40`](https://github.com/yehezkieldio/azalea-vb/commit/06dbd40e77c28bfbc3fb89634d3ac065638cd46f) **deps**: Replace rustls-tls with rustls and bump lockfile
- [`0985485`](https://github.com/yehezkieldio/azalea-vb/commit/098548550242df202998dc4ac09c4d2559137ac7) **deps**: Update all non-major dependencies

### <!-- 3 -->🚀 New Features
- [`3a2a5bf`](https://github.com/yehezkieldio/azalea-vb/commit/3a2a5bf35cc1a6fd6e9606de1898eefe622f9547) **azalea**: Validate temp dir writable before starting worker
- [`cfd54fd`](https://github.com/yehezkieldio/azalea-vb/commit/cfd54fdeb04cf42e57a6cb6264c72a1cbb96a426) **main**: Add minimum tested ffmpeg and ffprobe version checks

### <!-- 4 -->🐛 Bug Fixes
- [`83cef15`](https://github.com/yehezkieldio/azalea-vb/commit/83cef151c829206cc9d5dc67fc5959c190d95a19) **pipeline**: Add content-length header check to reject downloads

### <!-- 5 -->📚 Documentation
- [`3b0c5e9`](https://github.com/yehezkieldio/azalea-vb/commit/3b0c5e91254f6cd79694d36ea291dd42696833a4) **readme**: Add ffmpeg and ffprobe links
- [`6a429c6`](https://github.com/yehezkieldio/azalea-vb/commit/6a429c6d1bd8b2b916614116f638063d4fb6e050) **readme**: Add release, CI, and docker badges
- [`403d25b`](https://github.com/yehezkieldio/azalea-vb/commit/403d25b1a338f9a71ee21a5039637e277355174c) **agents**: Add just fmt check to task list
- [`fbbb5e5`](https://github.com/yehezkieldio/azalea-vb/commit/fbbb5e505a5f1eb17a18f0e77822cce926882b70) **readme**: Correct requirs to requires

### <!-- 7 -->🚜 Refactor
- [`a3620c0`](https://github.com/yehezkieldio/azalea-vb/commit/a3620c085d116a22ecc63bfeee197e1c5ffad020) **media**: Extract cleanup to return removed paths
- [`388ab39`](https://github.com/yehezkieldio/azalea-vb/commit/388ab3930dce36efeb1c05844a66601a5f35b698) **azalea**: Normalize user-facing messages to lowercase
- [`cecb289`](https://github.com/yehezkieldio/azalea-vb/commit/cecb289dc9dc8300e08da16a478f4ce8762ac15d) **pipeline**: Normalize error messages to lowercase

## 0.1.0 (February 22, 2026)

### <!-- 10 -->🧪 Test
- [`a101376`](https://github.com/yehezkieldio/azalea-vb/commit/a101376cca5281aeaf8a85775f9907fc51ba9407) **pipeline**: Add unit tests for pipeline error messages and backoff
- [`969578b`](https://github.com/yehezkieldio/azalea-vb/commit/969578b2dbbae33a8ba5201aff5f9f7dda112838) **gateway**: Add session_info and temp file tests for resume
- [`2704396`](https://github.com/yehezkieldio/azalea-vb/commit/2704396455d8cb2739e5ef1998f0ea8b986d5478) **concurrency**: Add rate limiter tests for per-user limits
- [`427ee26`](https://github.com/yehezkieldio/azalea-vb/commit/427ee26657d00dd1bb529605b6854ca716857a7b) **azalea**: Add config and binary resolution tests
- [`0f9b542`](https://github.com/yehezkieldio/azalea-vb/commit/0f9b542a40e527d2c7bf596ca5dd72fb67bdd06e) **pipeline**: Add tests for pipeline modules
- [`dcc7e47`](https://github.com/yehezkieldio/azalea-vb/commit/dcc7e474cd6fc36ce019a4ebc00bc8f9df91304a) **storage**: Add tests for dedup cache and metrics persistence
- [`87aa4e6`](https://github.com/yehezkieldio/azalea-vb/commit/87aa4e6e46dd201d08585397a4503c6ae2129b2a) **media**: Add tests for tempfile and url parsing
- [`ddc3a5e`](https://github.com/yehezkieldio/azalea-vb/commit/ddc3a5ec4db0123f1f0eb9b678fa6cb77c33961f) **concurrency**: Add tests for permit clamping and configured values
- [`09e2d67`](https://github.com/yehezkieldio/azalea-vb/commit/09e2d673247e06761b67d220961dc4ed88f99b36) **config**: Add settings validation tests
- [`a55493c`](https://github.com/yehezkieldio/azalea-vb/commit/a55493c692bf84de8f6a6d599817d309379bb6c9) **core**: Add smoke tests for settings, tweet parsing, dedup cache

### <!-- 11 -->🛠️ Miscellaneous
- [`14817b6`](https://github.com/yehezkieldio/azalea-vb/commit/14817b6892470d5fe42b6a4759130a0cc54c24d5) **release**: 0.1.0
- [`6ce75e4`](https://github.com/yehezkieldio/azalea-vb/commit/6ce75e497e9539ad76d6a8decc09f889a81517e8) Add comments to Dockerfile and CI / Release workflow
- [`9b10d8e`](https://github.com/yehezkieldio/azalea-vb/commit/9b10d8e9dda2750db2f29ec667d1b3706817f105) **config**: Replace repo and sort order in cliff template
- [`4313dd0`](https://github.com/yehezkieldio/azalea-vb/commit/4313dd062b6757da9a72aaa739c412e4a9005a7c) **config**: Remove target-cpu=native rustflag from cargo config
- [`7dadd0f`](https://github.com/yehezkieldio/azalea-vb/commit/7dadd0f3633e93628b4b70819b051b11bfb6c4f3) **config**: Add azalea config schema and ignore .env and *.redb
- [`b4d93a9`](https://github.com/yehezkieldio/azalea-vb/commit/b4d93a98e88be172a10166fa55a0475d424a18fa) **config**: Add cliff changelog template

### <!-- 12 -->🔒 Security
- [`ebd8e64`](https://github.com/yehezkieldio/azalea-vb/commit/ebd8e64d4256812e195c703510ae139849d89e43) **pipeline**: Implement ssrf media url validation

### <!-- 16 -->🤖 CI/CD
- [`bc2412c`](https://github.com/yehezkieldio/azalea-vb/commit/bc2412c3947ec37a43df54ad189569e1d7ebba77) **ci**: Rename build-and-push job name in release workflow
- [`6871a92`](https://github.com/yehezkieldio/azalea-vb/commit/6871a925d83dd6d3aee461063f87ab33a46994ad) **ci**: Switch rust-toolchain action to stable
- [`63af6c1`](https://github.com/yehezkieldio/azalea-vb/commit/63af6c10e6cb39aa43541d96c32a52d5a39bacb1) **ci**: Add ci release workflow
- [`2995b09`](https://github.com/yehezkieldio/azalea-vb/commit/2995b09db4f1414110b8f403e01670a12a56ade1) **ci**: Add release workflow to build and push docker images
- [`9fb2225`](https://github.com/yehezkieldio/azalea-vb/commit/9fb2225d564a15d102019fb6da29f7939b8917ce) **ci**: Add setup-mold action to CI workflow
- [`151c059`](https://github.com/yehezkieldio/azalea-vb/commit/151c059c7b9c7bd5a132b107e2fd041e95d4e777) **ci**: Add paths-ignore for markdown, assets, and schema files
- [`0df7a12`](https://github.com/yehezkieldio/azalea-vb/commit/0df7a12978c3c1347e8c49f01871ab388171de50) **ci**: Add github actions workflow for clippy test fmt

### <!-- 2 -->🧩 Dependencies Updates
- [`7ef3bdb`](https://github.com/yehezkieldio/azalea-vb/commit/7ef3bdbb8e7c88ebd46ab7eeddddbb5d31016ccf) **deps**: Add proptest to workspace and dev-dependencies
- [`be1a50c`](https://github.com/yehezkieldio/azalea-vb/commit/be1a50ca3c1e189f7e51c42bea30cc7dd1a2cdbe) **deps**: Set lto to thin and codegen-units to 4

### <!-- 3 -->🚀 New Features
- [`a96fecb`](https://github.com/yehezkieldio/azalea-vb/commit/a96fecbf34250efe66ee1a5e5b6ef02b6cbc1d97) **azalea**: Add generate-config binary and config template
- [`cce5806`](https://github.com/yehezkieldio/azalea-vb/commit/cce580640a1faf918af9291ecf0d43c260d75927) **config**: Introduce env bindings registry for flat env keys
- [`57ef021`](https://github.com/yehezkieldio/azalea-vb/commit/57ef021b9117ccebbb9abe6a5f8e08af76790085) Add pipeline and worker diagnostics and warnings
- [`04121ca`](https://github.com/yehezkieldio/azalea-vb/commit/04121ca971c9b0895b5c437f1e5088b676ae15bb) Add hardware acceleration diagnostics and ffmpeg probes
- [`9a8a5ce`](https://github.com/yehezkieldio/azalea-vb/commit/9a8a5cefa0d1b83014ee11b347e3bf12195d5d05) **config**: Add parsing for single-underscore env paths
- [`e5addb4`](https://github.com/yehezkieldio/azalea-vb/commit/e5addb4b76aa6a6e1a535cde01d5e0198c59e172) **config**: Add figment config loading and token handling
- [`d390dc1`](https://github.com/yehezkieldio/azalea-vb/commit/d390dc1bd0b3e4485497e8e9b2f7642769547407) **discord**: Rename azalea commands to top-level chat-input commands
  - 💥 **BREAKING CHANGE:** This commit introduces a breaking change.
- [`d260e8e`](https://github.com/yehezkieldio/azalea-vb/commit/d260e8e7ad3960ff4189d6f9323afe3b8e514eae) **discord**: Add /azalea media command and enqueue jobs
- [`e4765a4`](https://github.com/yehezkieldio/azalea-vb/commit/e4765a40dd8000c67be494d1afd0efddff46b47e) **gateway**: Add re-exports for run restore and save
- [`066b82a`](https://github.com/yehezkieldio/azalea-vb/commit/066b82a3c952b94a9d2467647b956f167c8825c1) **main**: Add bot entrypoint with async bootstrap and pipeline worker
- [`78e37d9`](https://github.com/yehezkieldio/azalea-vb/commit/78e37d97c810618458a5a03e2744531162765e0c) **gateway**: Add gateway module with dispatch, event, resume
- [`85b00bd`](https://github.com/yehezkieldio/azalea-vb/commit/85b00bded16928c5526f65b2172e65c91686d7c1) **pipeline**: Add upload stage with retry and size checks
- [`c80dea9`](https://github.com/yehezkieldio/azalea-vb/commit/c80dea904280880e5f66e525f6067303ff92601b) **pipeline**: Add pipeline module with job and error types
- [`1fa88f3`](https://github.com/yehezkieldio/azalea-vb/commit/1fa88f32f65c59043efa867f516b214bb5992db0) **discord**: Add Discord slash commands and responder
- [`2278db2`](https://github.com/yehezkieldio/azalea-vb/commit/2278db280b8a64d9c0b92d67a1659de76661d8b3) **app**: Add application state and initialization
- [`31f110f`](https://github.com/yehezkieldio/azalea-vb/commit/31f110f9db5ca3cf5484f17e8ea36575120d114f) **azalea**: Add config and concurrency modules and schema generator
- [`6336a73`](https://github.com/yehezkieldio/azalea-vb/commit/6336a7341d6f996fb8054f0492d0ec290d150128) **pipeline**: Add run orchestration emitting progress updates
- [`49cb460`](https://github.com/yehezkieldio/azalea-vb/commit/49cb4606ebf1be4b63ff95fab59e19a293fe0aa1) **engine**: Add runtime wiring and engine type
- [`9837a3b`](https://github.com/yehezkieldio/azalea-vb/commit/9837a3b0ae5bee285a39b6924efdb6ce431af33a) **pipeline**: Add optimize stage with size-aware transcode ladder
- [`4d1ad22`](https://github.com/yehezkieldio/azalea-vb/commit/4d1ad2207c02adf33576f96d41f44fdd0e479502) **pipeline**: Add quality module with bitrate budgeting and ladder
- [`8d4db7d`](https://github.com/yehezkieldio/azalea-vb/commit/8d4db7d3614fd7ba0d47d95946c73db3382fa9a9) **pipeline**: Add media resolver chain with caching
- [`fae1c46`](https://github.com/yehezkieldio/azalea-vb/commit/fae1c461c2f0a7184ef4317e8a3f9ca99ae88d44) **ffmpeg**: Add ffmpeg argument builders and execution helpers
- [`c7c0f29`](https://github.com/yehezkieldio/azalea-vb/commit/c7c0f298f6f4a8ededdda1c7e4856f709780ebd6) **pipeline**: Add download stage with size limits and probing
- [`9ab662b`](https://github.com/yehezkieldio/azalea-vb/commit/9ab662b05f52314e6d80a50e661b793a9c6eab18) **pipeline**: Add process management helpers for subprocesses
- [`46f5116`](https://github.com/yehezkieldio/azalea-vb/commit/46f5116e3b1a8ad16a2ef62177e8f32b85b5346f) **pipeline**: Add disk space guard for pipeline stages
- [`feaeba3`](https://github.com/yehezkieldio/azalea-vb/commit/feaeba33c077278b7f574df4dccfff9b866b98aa) **pipeline**: Add pipeline error taxonomy and error type
- [`f2244cb`](https://github.com/yehezkieldio/azalea-vb/commit/f2244cb32d66372139813d149540cc0558280634) **pipeline**: Add core pipeline types
- [`6ab7c20`](https://github.com/yehezkieldio/azalea-vb/commit/6ab7c20c9d1bd65afe4e881d800f947b5f973595) **storage**: Add metrics storage with in-memory state and redb persistence
- [`920f143`](https://github.com/yehezkieldio/azalea-vb/commit/920f1431d93432d60143be9edfd13a1483b54d37) **storage**: Add dedup cache with optional persistence
- [`bc42bf7`](https://github.com/yehezkieldio/azalea-vb/commit/bc42bf73357b0c194145039e1dab1c75996157c5) **concurrency**: Add concurrency module and permits with semaphores
- [`63c9bf5`](https://github.com/yehezkieldio/azalea-vb/commit/63c9bf5823d7b4a1d93e639d961857c553a2036d) **media**: Add tempfile module with cleanup utilities
- [`4e81a93`](https://github.com/yehezkieldio/azalea-vb/commit/4e81a93a919623e66034dd1ebd41c1bf1215872b) **media**: Add tweet url parsing with regex and types
- [`35b5a8f`](https://github.com/yehezkieldio/azalea-vb/commit/35b5a8f5b10473887a50818de34fb5e638a37a82) **media**: Add media module
- [`e505fcf`](https://github.com/yehezkieldio/azalea-vb/commit/e505fcfb90c48ecc7e3092c2ab66a7c3338000c8) **core**: Add config for quality and transcode
- [`e5d1e19`](https://github.com/yehezkieldio/azalea-vb/commit/e5d1e19861312afdc99268d6b55916a4a8b12074) Initial workspace setup

### <!-- 5 -->📚 Documentation
- [`e71de38`](https://github.com/yehezkieldio/azalea-vb/commit/e71de38b82bff3beebd63a0e9deebf4e7d24da2b) **readme**: Clarify installation and logging instructions
- [`32d5ba4`](https://github.com/yehezkieldio/azalea-vb/commit/32d5ba45658ecdf906b17d2827d68e1dfcdbc7c7) **readme**: Clarify NVENC and FFmpeg hardware notes
- [`d255709`](https://github.com/yehezkieldio/azalea-vb/commit/d25570984f6da1ee72994382546d27a565f864b0) **readme**: Add legal notice and usage warning
- [`3d0fe79`](https://github.com/yehezkieldio/azalea-vb/commit/3d0fe794b9a77bee2536d7d8409ea8a5382d22f9) **core**: Add module docs for concurrency and media
- [`4bdb35d`](https://github.com/yehezkieldio/azalea-vb/commit/4bdb35d9c83f8686a6a619fd3641752a1b5202d4) **config**: Update config module docs and invariants
- [`bc7cd25`](https://github.com/yehezkieldio/azalea-vb/commit/bc7cd25490c42ed249676bec18ada17ba3bd32ca) **agents**: Add comment-hygiene guidelines to AGENTS.md
- [`dcafc4c`](https://github.com/yehezkieldio/azalea-vb/commit/dcafc4c3e60e7c3c933eec2a163250d374f170ce) **config**: Add config module docs and relocate import

### <!-- 8 -->🏗️ Build System
- [`86c302f`](https://github.com/yehezkieldio/azalea-vb/commit/86c302f8390eb9f8d91d0359bc3cb3ffc664a169) **justfile**: Add release version task
- [`32d9298`](https://github.com/yehezkieldio/azalea-vb/commit/32d92982c6a393c5d7dcb2e75aee8998e62c128c) **justfile**: Add run, generate-schema and generate-config tasks
- [`f7b010b`](https://github.com/yehezkieldio/azalea-vb/commit/f7b010b0fa76031eb9806b08ea4fb9827172da39) **docker**: Remove LIBVA_DRIVER_NAME env from Dockerfile
- [`9fb54f3`](https://github.com/yehezkieldio/azalea-vb/commit/9fb54f3eaeefec79659f5ef3a66cadee08706823) **docker**: Add dockerfile and .dockerignore

### <!-- 9 -->🎨 Code Styling
- [`095f017`](https://github.com/yehezkieldio/azalea-vb/commit/095f0171a0f259d229035ccc2947a1a729a404b9) Split long lines and wrap function arguments
- [`5e5dcc5`](https://github.com/yehezkieldio/azalea-vb/commit/5e5dcc5c806ebdfe4515b68021ef6abc88705583) **discord**: Reorder imports and wrap long command builder call


