# Changelog

All notable changes to this project will be documented in this file.

**NOTE:** Changes are ordered by date, starting with the most latest to the oldest.

> This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) and uses [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) for commit messages.


## 0.4.0 (March 27, 2026)

### <!-- 10 -->🧪 Test
- [`374eb07`](https://github.com/yehezkieldio/azalea/commit/374eb070adb485bdcc248917b0b1dd2056a586d4) **discord**: Convert stats_reports_queue_peak_depth to tokio test

### <!-- 16 -->🤖 CI/CD
- [`9567107`](https://github.com/yehezkieldio/azalea/commit/9567107d1f5742914cdfe75f754172f17c5d4c7a) **ci**: Add merge_group path to ci workflow

### <!-- 2 -->🧩 Dependencies Updates
- [`70fe56f`](https://github.com/yehezkieldio/azalea/commit/70fe56ff3e623923e5438a0f1d6921741388040a) **deps**: Update actions/attest-build-provenance action to v4 by [@yehezkieldio](https://github.com/yehezkieldio) ([#8](https://github.com/yehezkieldio/azalea/issues/8))
- [`211853a`](https://github.com/yehezkieldio/azalea/commit/211853a8baa616f0d70ea28b7c86a419180a366c) **deps**: Update actions/attest-build-provenance action to v4
- [`272fc7e`](https://github.com/yehezkieldio/azalea/commit/272fc7e9e71bf2d77908a957e28fc32de1dd1c35) **deps**: Update docker/build-push-action action to v7 by [@yehezkieldio](https://github.com/yehezkieldio) ([#9](https://github.com/yehezkieldio/azalea/issues/9))
- [`0c52718`](https://github.com/yehezkieldio/azalea/commit/0c5271863cf71125975738226b7eafaaa9d1f268) **deps**: Update docker/metadata-action action to v6 by [@yehezkieldio](https://github.com/yehezkieldio) ([#11](https://github.com/yehezkieldio/azalea/issues/11))
- [`534ca17`](https://github.com/yehezkieldio/azalea/commit/534ca17ae4e134b8b272a6307e80328553e4b904) **deps**: Update docker/login-action action to v4 by [@yehezkieldio](https://github.com/yehezkieldio) ([#10](https://github.com/yehezkieldio/azalea/issues/10))
- [`184b2aa`](https://github.com/yehezkieldio/azalea/commit/184b2aa34f90b0da87432308114b718bfe349909) **deps**: Update docker/login-action action to v4
- [`ccddb28`](https://github.com/yehezkieldio/azalea/commit/ccddb28d4cadf205469cfdaf66e315c83697797e) **deps**: Update docker/setup-buildx-action action to v4 by [@yehezkieldio](https://github.com/yehezkieldio) ([#12](https://github.com/yehezkieldio/azalea/issues/12))
- [`bea57d3`](https://github.com/yehezkieldio/azalea/commit/bea57d3e5f1600066471d87b5a0b9191556619d3) **deps**: Update docker/setup-buildx-action action to v4
- [`8ff5709`](https://github.com/yehezkieldio/azalea/commit/8ff57092da15d7c3534244672c74e8a9f0fe147d) **deps**: Update docker/metadata-action action to v6
- [`f28e421`](https://github.com/yehezkieldio/azalea/commit/f28e42195cd371c0cf1f1e0827e635fe7a7d842a) **deps**: Update docker/build-push-action action to v7
- [`2acb151`](https://github.com/yehezkieldio/azalea/commit/2acb1512d79b529ed757721d836ee333471ea3e2) **deps**: Update rust crate proptest to v1.11.0 by renovate[bot] ([#7](https://github.com/yehezkieldio/azalea/issues/7))

### <!-- 3 -->🚀 New Features
- [`c115f0c`](https://github.com/yehezkieldio/azalea/commit/c115f0cbd8f11eaea7abada256432baa61d956e6) **app**: Add queue peak depth tracking and reporting
- [`18cdf3a`](https://github.com/yehezkieldio/azalea/commit/18cdf3acae15798b1234011eaf72880777953bfd) **core**: Add http2 settings and download write buffer
- [`f29b3bd`](https://github.com/yehezkieldio/azalea/commit/f29b3bdad7325fc93119bf4b7cd96fdd01d71ad2) **startup**: Add startup config sanity warnings and logging
- [`f807232`](https://github.com/yehezkieldio/azalea/commit/f807232629e7f1346a71edbe0e061c74ba89ad5c) **shutdown**: Add unified shutdown signal handling
- [`5b60bde`](https://github.com/yehezkieldio/azalea/commit/5b60bdea8040797d02375c3a2bf5b0dd5edbebcb) **disk**: Add collective download reservation using atomic counter
- [`7ff426b`](https://github.com/yehezkieldio/azalea/commit/7ff426b4a57a810ee70f5a78a6a3890f5f66c9b4) **core**: Add transcode runtime with hardware fallback
- [`2cfbc57`](https://github.com/yehezkieldio/azalea/commit/2cfbc57ef42180fc4593fa9d5d68276941c45cf4) **storage**: Introduce rotation for corrupt redb stores
- [`7fab23f`](https://github.com/yehezkieldio/azalea/commit/7fab23f37f48c3b7faf7a7eb45812ec682b5d16c) **pipeline**: Add media probing and split-copy strategy plan

### <!-- 4 -->🐛 Bug Fixes
- [`cfd3a11`](https://github.com/yehezkieldio/azalea/commit/cfd3a1148231dae12cdde083d5dece2d389f1490) **storage**: Handle invalid data io error as corruption
- [`18b5b15`](https://github.com/yehezkieldio/azalea/commit/18b5b154a51eef3bc13bb94ccac239572553dfec) **main**: Add probe write and sync in temp dir check
- [`968f3fe`](https://github.com/yehezkieldio/azalea/commit/968f3fec652f0e1c45abd356c9c4ce12848f2fc2) **pipeline**: Replace re-read per retry with single attachment

### <!-- 7 -->🚜 Refactor
- [`1bfa131`](https://github.com/yehezkieldio/azalea/commit/1bfa13197a854b2a9631e2a48f64c78ae2891216) **tempfile**: Introduce StaleTempCleanup and return it
- [`6af14f0`](https://github.com/yehezkieldio/azalea/commit/6af14f0abdb4e64df6f454ff2798d2d819737d8f) **core**: Introduce observed_delta_secs and clarify dedup logging
- [`1a6dcb9`](https://github.com/yehezkieldio/azalea/commit/1a6dcb9bb6ab75bebeaf34c3d8942f2516ada6c1) **metrics**: Replace string error keys with ErrorCategory enum
- [`59a3eb8`](https://github.com/yehezkieldio/azalea/commit/59a3eb8ef3e898c96284586efcedbe5f3bc73111) **pipeline**: Replace format selection with ranked scoring
- [`61dfa0a`](https://github.com/yehezkieldio/azalea/commit/61dfa0a9f8faab953d093baec67e49fd055c7494) **ids**: Introduce strongly-typed Discord id wrappers
- [`db77866`](https://github.com/yehezkieldio/azalea/commit/db778661eb7058f417f51916ea0302c656e18ac7) **pipeline**: Extract stderr tail formatting into helper
- [`9522652`](https://github.com/yehezkieldio/azalea/commit/9522652a410c81f39552cc5888d877e33308bab4) **download**: Extract fetch_with_redirects_inner and add tests
- [`c84b412`](https://github.com/yehezkieldio/azalea/commit/c84b4127a6788099522a2cd84296e772509264d5) **storage**: Introduce flush-worker and restructure metrics snapshot
- [`5df04cc`](https://github.com/yehezkieldio/azalea/commit/5df04cc35648d65150c901ed9a747e8edd3186f7) **pipeline**: Replace prepared upload paths with prepared parts
- [`f924b91`](https://github.com/yehezkieldio/azalea/commit/f924b91c43b32c5fb2a67d1d5a26950009395cc5) **pipeline**: Extract run_json_subprocess and stderr tail reader

## 0.3.2 (March 23, 2026)

### <!-- 11 -->🛠️ Miscellaneous
- [`236b83a`](https://github.com/yehezkieldio/azalea/commit/236b83a1559f5714699ded91bb23b9145124d070) **release**: 0.3.2

### <!-- 5 -->📚 Documentation
- [`e61db50`](https://github.com/yehezkieldio/azalea/commit/e61db50cf9cc7092b1109424fddb6b2b2bc6788a) **legal**: Remove repository license section

### <!-- 8 -->🏗️ Build System
- [`125ed34`](https://github.com/yehezkieldio/azalea/commit/125ed34deb74c868b860abcd1185108e5d770ab9) **docker**: Guard ffmpeg extraction and add build args
- [`af12764`](https://github.com/yehezkieldio/azalea/commit/af1276423b9bc87ed68f5384b895dd9a96aecefd) **docker**: Add ffmpeg download validation and switch to latest tag

## 0.3.1 (March 22, 2026)

### <!-- 10 -->🧪 Test
- [`31e644d`](https://github.com/yehezkieldio/azalea/commit/31e644d594afe1290bf11d08b9335fe7442fe067) **pipeline**: Add negative cache tests for 404, timeout, 429
- [`401eac8`](https://github.com/yehezkieldio/azalea/commit/401eac804f12919a6d44e829252abbcb8953299b) **pipeline**: Add select_best_format tests
- [`43506f0`](https://github.com/yehezkieldio/azalea/commit/43506f03928b84234c9e49b8b3841abf259c8059) **pipeline**: Add proptest for bitrate monotonicity in upload budget
- [`f56b814`](https://github.com/yehezkieldio/azalea/commit/f56b8144e175c108bffa4ecabcb4f989c07a0575) **pipeline**: Add assertion that sanitized does not contain ..
- [`c58a7b3`](https://github.com/yehezkieldio/azalea/commit/c58a7b365d6de97d431d408a138722401a49132d) **media**: Add fuzz test for parse_tweet_urls to handle arbitrary bytes
- [`99b023b`](https://github.com/yehezkieldio/azalea/commit/99b023b91a4c9e45e82583526568af63b8bd5720) **pipeline**: Add is_allowed_extension and enforce sanitize checks
- [`60d7717`](https://github.com/yehezkieldio/azalea/commit/60d77177792b0ffb136e7aec8e544a676ce230e1) **pipeline**: Add exponential backoff saturation test

### <!-- 11 -->🛠️ Miscellaneous
- [`77d24ea`](https://github.com/yehezkieldio/azalea/commit/77d24eac716fc30035153218693715753eb3227c) **release**: 0.3.1

### <!-- 2 -->🧩 Dependencies Updates
- [`cb5f6e4`](https://github.com/yehezkieldio/azalea/commit/cb5f6e47522b9d9731e8151d7b5540771e4423df) **deps**: Update rust crate moka to v0.12.15 by renovate[bot] ([#6](https://github.com/yehezkieldio/azalea/issues/6))

### <!-- 3 -->🚀 New Features
- [`38e4897`](https://github.com/yehezkieldio/azalea/commit/38e48974f9099a1b5e19a92fde143e6fe547a4e8) **storage**: Add background flush notifier and task guard

### <!-- 4 -->🐛 Bug Fixes
- [`c3bb9e8`](https://github.com/yehezkieldio/azalea/commit/c3bb9e8010e972f292a42a3501ab4e611778cf93) **pipeline**: Resolve YtDlp parsing and select best format
- [`4665145`](https://github.com/yehezkieldio/azalea/commit/46651458f70fe6e70a45737d3bc145e308b3a954) **resolve**: Narrow negative cache for network and partial parse errors

### <!-- 7 -->🚜 Refactor
- [`9a276d5`](https://github.com/yehezkieldio/azalea/commit/9a276d53927aff1d594f2552054826a42aaaf4ba) **pipeline**: Instrument pipeline with tracing spans and logs
- [`13cd3bc`](https://github.com/yehezkieldio/azalea/commit/13cd3bc719e231ee249b66be6d59b073bceb25ae) **dedup**: Remove persistent lookup and defer hydration
- [`e024988`](https://github.com/yehezkieldio/azalea/commit/e024988a7c3b5b410d49b5fe4a6728b4488fc03e) **pipeline**: Replace url box<str> with cow<'static,str>
- [`819561a`](https://github.com/yehezkieldio/azalea/commit/819561a7940bb899ec4a18d10379b47d77dd36b2) **core**: Replace string with box<str> in pipeline and storage

### <!-- 99 -->🌀 Other
- [`70e7eba`](https://github.com/yehezkieldio/azalea/commit/70e7ebad06393902e6075f7ceafb7e56e32cca05) **gateway**: Introduce normalization to skip redundant saves

## 0.3.0 (March 7, 2026)

### <!-- 10 -->🧪 Test
- [`8329f81`](https://github.com/yehezkieldio/azalea/commit/8329f81cf9008fda3c60bdafce54f0bef3ef813a) **pipeline**: Add exponential backoff max-delay test
- [`d593ad4`](https://github.com/yehezkieldio/azalea/commit/d593ad4721269c9a48e42c25f70db18384e24556) **discord**: Add media command tests
- [`ffb54bb`](https://github.com/yehezkieldio/azalea/commit/ffb54bb98061f7ed8fcc34b0f2aed7d7147c480e) **smoke**: Add pass-through local fixture smoke test
- [`d2b8185`](https://github.com/yehezkieldio/azalea/commit/d2b81850e39d448ab5a5c3e96bf04c4c7351066b) **dedup**: Add reserve_inflight collision test for two callers
- [`090decc`](https://github.com/yehezkieldio/azalea/commit/090decc2dc03ecbbc2f6493f3758ec686e11c3db) **dedup**: Add prune test for expired persistent entries on lookup
- [`df021d8`](https://github.com/yehezkieldio/azalea/commit/df021d81503995aabb82e4c8e548d0977731f86a) **pipeline**: Add video bitrate boundaries and presets test

### <!-- 11 -->🛠️ Miscellaneous
- [`a32264e`](https://github.com/yehezkieldio/azalea/commit/a32264e4ab3f3e108171697df1b2d7b72cc4f7a7) **release**: 0.3.0
- [`828a1f3`](https://github.com/yehezkieldio/azalea/commit/828a1f30410c8a3d7c9d60706c860f5ce383073b) **changelog**: Regenerate changelog
- [`4441b56`](https://github.com/yehezkieldio/azalea/commit/4441b56e3d2b6bffe4795e6583b1fd3e7c6cc1b8) **config**: Replace repo and issue URL in cliff.toml

### <!-- 12 -->🔒 Security
- [`44f6f45`](https://github.com/yehezkieldio/azalea/commit/44f6f45f02c7f3fdb40fc582b153b9272f5db52d) **ssrf**: Enforce dns resolution and reject blocked ips

### <!-- 16 -->🤖 CI/CD
- [`f77ae94`](https://github.com/yehezkieldio/azalea/commit/f77ae9407fc357fefcfb710a5b2ced46f87a30e6) **ci**: Add tags-ignore to push workflow

### <!-- 2 -->🧩 Dependencies Updates
- [`f66b718`](https://github.com/yehezkieldio/azalea/commit/f66b7187d90db566d9c9fa1e5c50dacf1c65828d) **deps**: Update rust crate tokio to v1.50.0 by renovate[bot] ([#5](https://github.com/yehezkieldio/azalea/issues/5))
- [`9485cf3`](https://github.com/yehezkieldio/azalea/commit/9485cf392bd1e695dabdc8dd16c05aef45952b3d) **deps**: Update rust crate moka to v0.12.14 by renovate[bot] ([#4](https://github.com/yehezkieldio/azalea/issues/4))

### <!-- 3 -->🚀 New Features
- [`011a441`](https://github.com/yehezkieldio/azalea/commit/011a441340bad6fd4308fa8f661eac2e3e97e736) **pipeline**: Add jitter_millis_for_seed deterministic jitter calculation
- [`cd8aa20`](https://github.com/yehezkieldio/azalea/commit/cd8aa2055ac300506fddad67c97c8130b42583de) **pipeline**: Add uploading segment progress updates

### <!-- 4 -->🐛 Bug Fixes
- [`62682af`](https://github.com/yehezkieldio/azalea/commit/62682af801cd6480cd12d0c221654fb3c0a1b70e) **pipeline**: Guard preallocation and preallocate upload buffer
- [`8adc0ba`](https://github.com/yehezkieldio/azalea/commit/8adc0ba5427a99f000669ef2b7372e32b2901f53) **pipeline**: Replace audio bitrate mapping and clamp to 128-192

### <!-- 7 -->🚜 Refactor
- [`d742820`](https://github.com/yehezkieldio/azalea/commit/d742820e61a47bf363bee994415ccd7ccfd1133d) **gateway**: Extract shutdown_dispatch_tasks and add tests
- [`868ce0c`](https://github.com/yehezkieldio/azalea/commit/868ce0cb28214158ec13dd14e4a964d8de1cd023) **discord**: Extract error notification policy and add tests

### <!-- 8 -->🏗️ Build System
- [`776ee11`](https://github.com/yehezkieldio/azalea/commit/776ee1153d31ccf61b977fe7a2b5f729590c4d23) **justfile**: Add post-release git push instruction

## 0.2.3 (February 25, 2026)

### <!-- 11 -->🛠️ Miscellaneous
- [`554846d`](https://github.com/yehezkieldio/azalea/commit/554846d80b985fda33c722e2635c12f7c38cefc0) **release**: 0.2.3

### <!-- 16 -->🤖 CI/CD
- [`5f18557`](https://github.com/yehezkieldio/azalea/commit/5f185572c10d6a5016eb00940d6bc316b75bea7f) Fix release workflow env and restore tag trigger

## 0.2.2 (February 25, 2026)

### <!-- 11 -->🛠️ Miscellaneous
- [`90fe401`](https://github.com/yehezkieldio/azalea/commit/90fe401187b58be598f077c10da984b565d79959) **release**: 0.2.2

### <!-- 16 -->🤖 CI/CD
- [`b938987`](https://github.com/yehezkieldio/azalea/commit/b938987affbd1f2ae345860a7a6ac19817525146) **ci**: Remove tags-ignore v* from workflow

## 0.2.1 (February 25, 2026)

### <!-- 11 -->🛠️ Miscellaneous
- [`7f2a070`](https://github.com/yehezkieldio/azalea/commit/7f2a070834ef499e75d341a009a2443a0e679042) **release**: 0.2.1

### <!-- 16 -->🤖 CI/CD
- [`add747c`](https://github.com/yehezkieldio/azalea/commit/add747c7cbe06829c23fafc3216b5394310a78e2) **ci**: Add tags-ignore v* to ci workflow
- [`3067335`](https://github.com/yehezkieldio/azalea/commit/3067335a85bcf337561413224ff5ffe2a9324be7) **ci**: Remove tag filter from release workflow
- [`25786b3`](https://github.com/yehezkieldio/azalea/commit/25786b3a10f1a47ac4cfcb8bce1c22ffca02a246) **ci**: Add cleanup job to release workflow

### <!-- 8 -->🏗️ Build System
- [`5fd8352`](https://github.com/yehezkieldio/azalea/commit/5fd8352f3bf01ed36e6b929fc57b2934131f3d76) **docker**: Introduce runtime-base and restructure stages
- [`1004229`](https://github.com/yehezkieldio/azalea/commit/100422920678b0eff7e5d323002802e7a2a6c40d) **docker**: Remove stray indentation from dockerfile run
- [`8e3b8a3`](https://github.com/yehezkieldio/azalea/commit/8e3b8a3e4fb6190869d350ce7df471b9501cb607) **docker**: Pin yt-dlp and ffmpeg via build args

## 0.2.0 (February 25, 2026)

### <!-- 10 -->🧪 Test
- [`8fb649b`](https://github.com/yehezkieldio/azalea/commit/8fb649b3f69e278c0a5521f99dce79a68c1e6070) **media**: Add valid_host strategy and expand tweet url test
- [`22f0dfe`](https://github.com/yehezkieldio/azalea/commit/22f0dfe2e1a2bdbcb41291c1895f954b89b384f6) **media**: Add tweet url parsing tests
- [`8f7683c`](https://github.com/yehezkieldio/azalea/commit/8f7683cdee631ee25b68d5e382e9194dc666d547) **media**: Add twittpr domain to tweet url parsing
- [`e5fc854`](https://github.com/yehezkieldio/azalea/commit/e5fc854d5a040a06aef794c60c527d8d3f692070) **media**: Add www url parsing tests and rename bare url tests

### <!-- 11 -->🛠️ Miscellaneous
- [`e77659c`](https://github.com/yehezkieldio/azalea/commit/e77659c95f6870de27a95c7b8ddc8b8b2fcc84b9) **release**: 0.2.0

### <!-- 16 -->🤖 CI/CD
- [`6638d75`](https://github.com/yehezkieldio/azalea/commit/6638d75cf66b0064955adeb1c38dc5930cc87900) **ci**: Decouple ci and release into seperate workflows
- [`8960334`](https://github.com/yehezkieldio/azalea/commit/896033469a5ddb6a6a769e36edc99660065ff673) **deps**: Bump actions/checkout to v6 and attest-build-provenance to v3
- [`e2607ed`](https://github.com/yehezkieldio/azalea/commit/e2607ed08d6c1ffcc82dadeb403693d03e4ebcf7) **ci**: Rename release job names in ci_release workflow

### <!-- 2 -->🧩 Dependencies Updates
- [`223ac57`](https://github.com/yehezkieldio/azalea/commit/223ac57e3eb5b4c74983f8909bd93d7f4b44ec09) **deps**: Update rust crate anyhow to v1.0.102 by renovate[bot] ([#3](https://github.com/yehezkieldio/azalea/issues/3))
- [`06dbd40`](https://github.com/yehezkieldio/azalea/commit/06dbd40e77c28bfbc3fb89634d3ac065638cd46f) **deps**: Replace rustls-tls with rustls and bump lockfile
- [`0985485`](https://github.com/yehezkieldio/azalea/commit/098548550242df202998dc4ac09c4d2559137ac7) **deps**: Update all non-major dependencies

### <!-- 3 -->🚀 New Features
- [`3a2a5bf`](https://github.com/yehezkieldio/azalea/commit/3a2a5bf35cc1a6fd6e9606de1898eefe622f9547) **azalea**: Validate temp dir writable before starting worker
- [`cfd54fd`](https://github.com/yehezkieldio/azalea/commit/cfd54fdeb04cf42e57a6cb6264c72a1cbb96a426) **main**: Add minimum tested ffmpeg and ffprobe version checks

### <!-- 4 -->🐛 Bug Fixes
- [`83cef15`](https://github.com/yehezkieldio/azalea/commit/83cef151c829206cc9d5dc67fc5959c190d95a19) **pipeline**: Add content-length header check to reject downloads

### <!-- 5 -->📚 Documentation
- [`3b0c5e9`](https://github.com/yehezkieldio/azalea/commit/3b0c5e91254f6cd79694d36ea291dd42696833a4) **readme**: Add ffmpeg and ffprobe links
- [`6a429c6`](https://github.com/yehezkieldio/azalea/commit/6a429c6d1bd8b2b916614116f638063d4fb6e050) **readme**: Add release, CI, and docker badges
- [`403d25b`](https://github.com/yehezkieldio/azalea/commit/403d25b1a338f9a71ee21a5039637e277355174c) **agents**: Add just fmt check to task list
- [`fbbb5e5`](https://github.com/yehezkieldio/azalea/commit/fbbb5e505a5f1eb17a18f0e77822cce926882b70) **readme**: Correct requirs to requires

### <!-- 7 -->🚜 Refactor
- [`a3620c0`](https://github.com/yehezkieldio/azalea/commit/a3620c085d116a22ecc63bfeee197e1c5ffad020) **media**: Extract cleanup to return removed paths
- [`388ab39`](https://github.com/yehezkieldio/azalea/commit/388ab3930dce36efeb1c05844a66601a5f35b698) **azalea**: Normalize user-facing messages to lowercase
- [`cecb289`](https://github.com/yehezkieldio/azalea/commit/cecb289dc9dc8300e08da16a478f4ce8762ac15d) **pipeline**: Normalize error messages to lowercase

## 0.1.0 (February 22, 2026)

### <!-- 10 -->🧪 Test
- [`a101376`](https://github.com/yehezkieldio/azalea/commit/a101376cca5281aeaf8a85775f9907fc51ba9407) **pipeline**: Add unit tests for pipeline error messages and backoff
- [`969578b`](https://github.com/yehezkieldio/azalea/commit/969578b2dbbae33a8ba5201aff5f9f7dda112838) **gateway**: Add session_info and temp file tests for resume
- [`2704396`](https://github.com/yehezkieldio/azalea/commit/2704396455d8cb2739e5ef1998f0ea8b986d5478) **concurrency**: Add rate limiter tests for per-user limits
- [`427ee26`](https://github.com/yehezkieldio/azalea/commit/427ee26657d00dd1bb529605b6854ca716857a7b) **azalea**: Add config and binary resolution tests
- [`0f9b542`](https://github.com/yehezkieldio/azalea/commit/0f9b542a40e527d2c7bf596ca5dd72fb67bdd06e) **pipeline**: Add tests for pipeline modules
- [`dcc7e47`](https://github.com/yehezkieldio/azalea/commit/dcc7e474cd6fc36ce019a4ebc00bc8f9df91304a) **storage**: Add tests for dedup cache and metrics persistence
- [`87aa4e6`](https://github.com/yehezkieldio/azalea/commit/87aa4e6e46dd201d08585397a4503c6ae2129b2a) **media**: Add tests for tempfile and url parsing
- [`ddc3a5e`](https://github.com/yehezkieldio/azalea/commit/ddc3a5ec4db0123f1f0eb9b678fa6cb77c33961f) **concurrency**: Add tests for permit clamping and configured values
- [`09e2d67`](https://github.com/yehezkieldio/azalea/commit/09e2d673247e06761b67d220961dc4ed88f99b36) **config**: Add settings validation tests
- [`a55493c`](https://github.com/yehezkieldio/azalea/commit/a55493c692bf84de8f6a6d599817d309379bb6c9) **core**: Add smoke tests for settings, tweet parsing, dedup cache

### <!-- 11 -->🛠️ Miscellaneous
- [`14817b6`](https://github.com/yehezkieldio/azalea/commit/14817b6892470d5fe42b6a4759130a0cc54c24d5) **release**: 0.1.0
- [`6ce75e4`](https://github.com/yehezkieldio/azalea/commit/6ce75e497e9539ad76d6a8decc09f889a81517e8) Add comments to Dockerfile and CI / Release workflow
- [`9b10d8e`](https://github.com/yehezkieldio/azalea/commit/9b10d8e9dda2750db2f29ec667d1b3706817f105) **config**: Replace repo and sort order in cliff template
- [`4313dd0`](https://github.com/yehezkieldio/azalea/commit/4313dd062b6757da9a72aaa739c412e4a9005a7c) **config**: Remove target-cpu=native rustflag from cargo config
- [`7dadd0f`](https://github.com/yehezkieldio/azalea/commit/7dadd0f3633e93628b4b70819b051b11bfb6c4f3) **config**: Add azalea config schema and ignore .env and *.redb
- [`b4d93a9`](https://github.com/yehezkieldio/azalea/commit/b4d93a98e88be172a10166fa55a0475d424a18fa) **config**: Add cliff changelog template

### <!-- 12 -->🔒 Security
- [`ebd8e64`](https://github.com/yehezkieldio/azalea/commit/ebd8e64d4256812e195c703510ae139849d89e43) **pipeline**: Implement ssrf media url validation

### <!-- 16 -->🤖 CI/CD
- [`bc2412c`](https://github.com/yehezkieldio/azalea/commit/bc2412c3947ec37a43df54ad189569e1d7ebba77) **ci**: Rename build-and-push job name in release workflow
- [`6871a92`](https://github.com/yehezkieldio/azalea/commit/6871a925d83dd6d3aee461063f87ab33a46994ad) **ci**: Switch rust-toolchain action to stable
- [`63af6c1`](https://github.com/yehezkieldio/azalea/commit/63af6c10e6cb39aa43541d96c32a52d5a39bacb1) **ci**: Add ci release workflow
- [`2995b09`](https://github.com/yehezkieldio/azalea/commit/2995b09db4f1414110b8f403e01670a12a56ade1) **ci**: Add release workflow to build and push docker images
- [`9fb2225`](https://github.com/yehezkieldio/azalea/commit/9fb2225d564a15d102019fb6da29f7939b8917ce) **ci**: Add setup-mold action to CI workflow
- [`151c059`](https://github.com/yehezkieldio/azalea/commit/151c059c7b9c7bd5a132b107e2fd041e95d4e777) **ci**: Add paths-ignore for markdown, assets, and schema files
- [`0df7a12`](https://github.com/yehezkieldio/azalea/commit/0df7a12978c3c1347e8c49f01871ab388171de50) **ci**: Add github actions workflow for clippy test fmt

### <!-- 2 -->🧩 Dependencies Updates
- [`7ef3bdb`](https://github.com/yehezkieldio/azalea/commit/7ef3bdbb8e7c88ebd46ab7eeddddbb5d31016ccf) **deps**: Add proptest to workspace and dev-dependencies
- [`be1a50c`](https://github.com/yehezkieldio/azalea/commit/be1a50ca3c1e189f7e51c42bea30cc7dd1a2cdbe) **deps**: Set lto to thin and codegen-units to 4

### <!-- 3 -->🚀 New Features
- [`a96fecb`](https://github.com/yehezkieldio/azalea/commit/a96fecbf34250efe66ee1a5e5b6ef02b6cbc1d97) **azalea**: Add generate-config binary and config template
- [`cce5806`](https://github.com/yehezkieldio/azalea/commit/cce580640a1faf918af9291ecf0d43c260d75927) **config**: Introduce env bindings registry for flat env keys
- [`57ef021`](https://github.com/yehezkieldio/azalea/commit/57ef021b9117ccebbb9abe6a5f8e08af76790085) Add pipeline and worker diagnostics and warnings
- [`04121ca`](https://github.com/yehezkieldio/azalea/commit/04121ca971c9b0895b5c437f1e5088b676ae15bb) Add hardware acceleration diagnostics and ffmpeg probes
- [`9a8a5ce`](https://github.com/yehezkieldio/azalea/commit/9a8a5cefa0d1b83014ee11b347e3bf12195d5d05) **config**: Add parsing for single-underscore env paths
- [`e5addb4`](https://github.com/yehezkieldio/azalea/commit/e5addb4b76aa6a6e1a535cde01d5e0198c59e172) **config**: Add figment config loading and token handling
- [`d390dc1`](https://github.com/yehezkieldio/azalea/commit/d390dc1bd0b3e4485497e8e9b2f7642769547407) **discord**: Rename azalea commands to top-level chat-input commands
  - 💥 **BREAKING CHANGE:** This commit introduces a breaking change.
- [`d260e8e`](https://github.com/yehezkieldio/azalea/commit/d260e8e7ad3960ff4189d6f9323afe3b8e514eae) **discord**: Add /azalea media command and enqueue jobs
- [`e4765a4`](https://github.com/yehezkieldio/azalea/commit/e4765a40dd8000c67be494d1afd0efddff46b47e) **gateway**: Add re-exports for run restore and save
- [`066b82a`](https://github.com/yehezkieldio/azalea/commit/066b82a3c952b94a9d2467647b956f167c8825c1) **main**: Add bot entrypoint with async bootstrap and pipeline worker
- [`78e37d9`](https://github.com/yehezkieldio/azalea/commit/78e37d97c810618458a5a03e2744531162765e0c) **gateway**: Add gateway module with dispatch, event, resume
- [`85b00bd`](https://github.com/yehezkieldio/azalea/commit/85b00bded16928c5526f65b2172e65c91686d7c1) **pipeline**: Add upload stage with retry and size checks
- [`c80dea9`](https://github.com/yehezkieldio/azalea/commit/c80dea904280880e5f66e525f6067303ff92601b) **pipeline**: Add pipeline module with job and error types
- [`1fa88f3`](https://github.com/yehezkieldio/azalea/commit/1fa88f32f65c59043efa867f516b214bb5992db0) **discord**: Add Discord slash commands and responder
- [`2278db2`](https://github.com/yehezkieldio/azalea/commit/2278db280b8a64d9c0b92d67a1659de76661d8b3) **app**: Add application state and initialization
- [`31f110f`](https://github.com/yehezkieldio/azalea/commit/31f110f9db5ca3cf5484f17e8ea36575120d114f) **azalea**: Add config and concurrency modules and schema generator
- [`6336a73`](https://github.com/yehezkieldio/azalea/commit/6336a7341d6f996fb8054f0492d0ec290d150128) **pipeline**: Add run orchestration emitting progress updates
- [`49cb460`](https://github.com/yehezkieldio/azalea/commit/49cb4606ebf1be4b63ff95fab59e19a293fe0aa1) **engine**: Add runtime wiring and engine type
- [`9837a3b`](https://github.com/yehezkieldio/azalea/commit/9837a3b0ae5bee285a39b6924efdb6ce431af33a) **pipeline**: Add optimize stage with size-aware transcode ladder
- [`4d1ad22`](https://github.com/yehezkieldio/azalea/commit/4d1ad2207c02adf33576f96d41f44fdd0e479502) **pipeline**: Add quality module with bitrate budgeting and ladder
- [`8d4db7d`](https://github.com/yehezkieldio/azalea/commit/8d4db7d3614fd7ba0d47d95946c73db3382fa9a9) **pipeline**: Add media resolver chain with caching
- [`fae1c46`](https://github.com/yehezkieldio/azalea/commit/fae1c461c2f0a7184ef4317e8a3f9ca99ae88d44) **ffmpeg**: Add ffmpeg argument builders and execution helpers
- [`c7c0f29`](https://github.com/yehezkieldio/azalea/commit/c7c0f298f6f4a8ededdda1c7e4856f709780ebd6) **pipeline**: Add download stage with size limits and probing
- [`9ab662b`](https://github.com/yehezkieldio/azalea/commit/9ab662b05f52314e6d80a50e661b793a9c6eab18) **pipeline**: Add process management helpers for subprocesses
- [`46f5116`](https://github.com/yehezkieldio/azalea/commit/46f5116e3b1a8ad16a2ef62177e8f32b85b5346f) **pipeline**: Add disk space guard for pipeline stages
- [`feaeba3`](https://github.com/yehezkieldio/azalea/commit/feaeba33c077278b7f574df4dccfff9b866b98aa) **pipeline**: Add pipeline error taxonomy and error type
- [`f2244cb`](https://github.com/yehezkieldio/azalea/commit/f2244cb32d66372139813d149540cc0558280634) **pipeline**: Add core pipeline types
- [`6ab7c20`](https://github.com/yehezkieldio/azalea/commit/6ab7c20c9d1bd65afe4e881d800f947b5f973595) **storage**: Add metrics storage with in-memory state and redb persistence
- [`920f143`](https://github.com/yehezkieldio/azalea/commit/920f1431d93432d60143be9edfd13a1483b54d37) **storage**: Add dedup cache with optional persistence
- [`bc42bf7`](https://github.com/yehezkieldio/azalea/commit/bc42bf73357b0c194145039e1dab1c75996157c5) **concurrency**: Add concurrency module and permits with semaphores
- [`63c9bf5`](https://github.com/yehezkieldio/azalea/commit/63c9bf5823d7b4a1d93e639d961857c553a2036d) **media**: Add tempfile module with cleanup utilities
- [`4e81a93`](https://github.com/yehezkieldio/azalea/commit/4e81a93a919623e66034dd1ebd41c1bf1215872b) **media**: Add tweet url parsing with regex and types
- [`35b5a8f`](https://github.com/yehezkieldio/azalea/commit/35b5a8f5b10473887a50818de34fb5e638a37a82) **media**: Add media module
- [`e505fcf`](https://github.com/yehezkieldio/azalea/commit/e505fcfb90c48ecc7e3092c2ab66a7c3338000c8) **core**: Add config for quality and transcode
- [`e5d1e19`](https://github.com/yehezkieldio/azalea/commit/e5d1e19861312afdc99268d6b55916a4a8b12074) Initial workspace setup

### <!-- 5 -->📚 Documentation
- [`e71de38`](https://github.com/yehezkieldio/azalea/commit/e71de38b82bff3beebd63a0e9deebf4e7d24da2b) **readme**: Clarify installation and logging instructions
- [`32d5ba4`](https://github.com/yehezkieldio/azalea/commit/32d5ba45658ecdf906b17d2827d68e1dfcdbc7c7) **readme**: Clarify NVENC and FFmpeg hardware notes
- [`d255709`](https://github.com/yehezkieldio/azalea/commit/d25570984f6da1ee72994382546d27a565f864b0) **readme**: Add legal notice and usage warning
- [`3d0fe79`](https://github.com/yehezkieldio/azalea/commit/3d0fe794b9a77bee2536d7d8409ea8a5382d22f9) **core**: Add module docs for concurrency and media
- [`4bdb35d`](https://github.com/yehezkieldio/azalea/commit/4bdb35d9c83f8686a6a619fd3641752a1b5202d4) **config**: Update config module docs and invariants
- [`bc7cd25`](https://github.com/yehezkieldio/azalea/commit/bc7cd25490c42ed249676bec18ada17ba3bd32ca) **agents**: Add comment-hygiene guidelines to AGENTS.md
- [`dcafc4c`](https://github.com/yehezkieldio/azalea/commit/dcafc4c3e60e7c3c933eec2a163250d374f170ce) **config**: Add config module docs and relocate import

### <!-- 8 -->🏗️ Build System
- [`86c302f`](https://github.com/yehezkieldio/azalea/commit/86c302f8390eb9f8d91d0359bc3cb3ffc664a169) **justfile**: Add release version task
- [`32d9298`](https://github.com/yehezkieldio/azalea/commit/32d92982c6a393c5d7dcb2e75aee8998e62c128c) **justfile**: Add run, generate-schema and generate-config tasks
- [`f7b010b`](https://github.com/yehezkieldio/azalea/commit/f7b010b0fa76031eb9806b08ea4fb9827172da39) **docker**: Remove LIBVA_DRIVER_NAME env from Dockerfile
- [`9fb54f3`](https://github.com/yehezkieldio/azalea/commit/9fb54f3eaeefec79659f5ef3a66cadee08706823) **docker**: Add dockerfile and .dockerignore

### <!-- 9 -->🎨 Code Styling
- [`095f017`](https://github.com/yehezkieldio/azalea/commit/095f0171a0f259d229035ccc2947a1a729a404b9) Split long lines and wrap function arguments
- [`5e5dcc5`](https://github.com/yehezkieldio/azalea/commit/5e5dcc5c806ebdfe4515b68021ef6abc88705583) **discord**: Reorder imports and wrap long command builder call


