## Observations

- Reproduction input is the pasted production log from 2026-05-05.
- The job downloaded a 25,286,835 byte 720x1280 MP4 with duration 139.029333s.
- Split-transcode planned 2 segments at 120s, with 278 kbps video and 128 kbps audio.
- QSV completed both segment transcodes successfully, then `ensure_split_segments_fit_limit` rejected segment 0 at 17,890,356 bytes.
- A 120s segment at the planned bitrate should be well under the 8,388,608 byte upload cap; the failure is after encode, not during planning.

## Hypotheses

### H1: QSV is not constrained to strict enough bitrate control for upload-sized segments
- Supports: planned bitrate is low, output is more than 2x the cap, and the failing run used `h264_qsv`.
- Conflicts: the args already include `-b:v`, `-maxrate`, and `-bufsize`.
- Test: inspect `ffmpeg -h encoder=h264_qsv` for bitrate-control options and add an args regression that QSV split/transcode includes them.

### H2: Segment duration is overestimated for the final segment
- Supports: segment 1 logs `duration_secs=120.0` even though only about 19s remain.
- Conflicts: the oversized segment is segment 0, which really covers the first 120s.
- Test: compare failing segment index and duration from the log.

### H3: Audio args are malformed in direct segment transcode
- Supports: an initial read looked suspicious around the audio bitrate flag.
- Conflicts: the current source emits one `-b:a`, and the size overshoot is video-dominated.
- Test: add an args regression that direct segment transcode emits one `-b:a`.

## Experiments

- H1 confirmed enough for production fix: local ffmpeg reports QSV supports `low_delay_brc`, described as strictly obeying average frame size, plus `bitrate_limit`.
- H2 rejected for this incident: the oversized output is segment 0, not the short tail segment.
- H3 rejected: the current source emits one audio bitrate flag.

## Root Cause

The QSV segment encoder was given bitrate targets but not the QSV bitrate-control flags needed for upload-size-sensitive output, so successful encodes could still overshoot Discord's per-file limit.

## Fix

- Add QSV bitrate-control flags to generated ffmpeg args.
- Add argument-level regression tests for QSV rate-control and direct segment audio bitrate flags.
- Add a split-transcode guard so QSV does not encode segments whose planned video bitrate is below `transcode.min_bitrate_kbps`; those low-bitrate split chunks use `libx264` instead.
- Preserve explicit software encode settings in `execute_with_hwacc_fallback`; runtime hardware backend selection must not override a split-local software decision.

## 2026-05-05 Serial Segment Playback Follow-Up

### Observations

- The pasted production log used `split_encoder="libx264"` and `parallel=false`, so the active path was serial split with software encoding.
- Parallel split already used direct `transcode_segment_args(...)` with explicit `-ss`, `-t`, timestamp reset, negative timestamp avoidance, and faststart.
- Serial split still used ffmpeg's segment muxer through `split_args(...)`, so it did not get the direct per-segment seek path from the earlier split-freeze fix.
- The user reported the first uploaded segment looked good while the second looked wonky, which points at the segment boundary rather than upload transport.

### Hypotheses

#### H1: Serial split is still using segment muxing instead of direct per-segment transcodes
- Supports: production log says `parallel=false`; source showed `split_serial(...)` calling `ffmpeg::split_args(...)`.
- Conflicts: `split_args(...)` did force keyframes and reset timestamps.
- Test: add a serial split test that records fake ffmpeg args and asserts separate `-ss` values with no `%03d` pattern.

#### H2: Low-bitrate software encoding is causing visual quality collapse on the short tail
- Supports: planned video bitrate was 278 kbps for 720x1280 content.
- Conflicts: the report singled out the second segment as structurally wonky, not just lower quality.
- Test: compare serial args first; if direct segment args still fail, inspect encoded packet timestamps and bitrate ladder.

#### H3: Discord upload batching corrupts the second attachment
- Supports: both parts were uploaded in one Discord request.
- Conflicts: prior upload metrics show the request succeeded cleanly, and the first attachment was fine.
- Test: only after local encoded files prove correct, upload the same two files separately and compare playback.

### Experiments

- H1 confirmed by source inspection: the serial path used ffmpeg segment muxing while only the parallel path used direct per-segment transcodes.
- Added a regression test that proves serial split now invokes ffmpeg once per output segment with `-ss 0.000`, `-ss 10.000`, and no `%03d` segment pattern.

### Root Cause

Serial split kept the old segment-muxer path, so low-concurrency runs could still produce boundary-sensitive Discord attachments even after the parallel direct-transcode fix.

### Fix

- Make `split_serial(...)` transcode each output segment directly from the original input, matching the parallel path's timestamp/keyframe hygiene.
- Delete the unused segment-muxer and copy-split ffmpeg helpers so production cannot fall back to the stale path.

## 2026-05-05 Long Split Oversize Follow-Up

### Observations

- The failing long video was 167.872s at 1280x720 and split into two direct software segments after QSV was disabled for the 278 kbps split budget.
- Both segment transcodes completed, then `ensure_split_segments_fit_limit(...)` rejected the output with `split segment exceeded upload limit`.
- The computed budget was 278 kbps video plus 128 kbps audio for a 120s segment, which should fit under 8 MiB.
- The software encoder args used budget caps but previously lacked `-b:v`, and direct segment args forced keyframes with `expr:gte(t,0)`.

### Hypotheses

#### H1: Software x264 is not using average bitrate mode
- Supports: the software path used CRF with peak caps; CRF is not an upload-size budget.
- Conflicts: not enough to explain a 2x overshoot after adding `-b:v` in isolation.
- Test: add an args regression that software transcode emits `-b:v` and no `-crf`.

#### H2: Direct segment keyframe expression forces every frame as a keyframe
- Supports: `expr:gte(t,0)` is true for every frame timestamp at or after zero, causing all-I-frame output.
- Conflicts: none after synthetic ffmpeg smoke reproduced the oversized bitrate.
- Test: encode 120s of 720p `testsrc2` at 278k/128k with `-force_key_frames 0` and verify output stays below 8 MiB.

#### H3: The planner's bitrate math is too optimistic
- Supports: the output exceeded the cap.
- Conflicts: after correcting keyframes, a 120s 720p synthetic motion sample produced a 6,221,661 byte output.
- Test: keep existing split budget tests and live smoke the encoder args.

### Experiments

- H1 partially confirmed: software output should be ABR for upload-budgeted transcodes, so `libx264` now receives `-b:v` and no `-crf`.
- H2 confirmed: a 20s synthetic smoke with `expr:gte(t,0)` produced about 2.0 MB, while `-force_key_frames 0` produced about 1.06 MB with the same bitrate settings.
- A full 120s synthetic 1280x720 smoke at 278k video and 128k audio produced 6,221,661 bytes, below the 8,388,608 byte cap.

### Root Cause

Direct segment transcodes forced every frame to be a keyframe, and software fallback used quality-oriented x264 settings instead of strict average bitrate control, so long motion-heavy segments could exceed Discord's upload cap.

### Fix

- Use ABR for software x264 budgeted transcodes by emitting `-b:v` and removing `-crf`.
- Change direct segment keyframe forcing from `expr:gte(t,0)` to `0`, which forces only the segment's first frame.

## 2026-05-05 Split Pixelation Follow-Up

### Observations

- The 167.872s 1280x720 run succeeded with two parts, but each part had only a 278 kbps video budget.
- The upload payload was 8,504,632 bytes across both files, so the size fix worked by compressing aggressively under the per-file cap.
- 278 kbps is below the 720p ladder floor; changing QSV/VAAPI/software cannot create more bits inside the same 120s segment.
- Discord accepts up to 10 attachments per upload request in this code path, and current batching already supports multiple parts.

### Hypotheses

#### H1: The split planner optimizes for fewest parts instead of preserving visible resolution
- Supports: it always used the max 120s segment duration before computing bitrate.
- Conflicts: fewer parts are faster to upload and less noisy in Discord.
- Test: for the reported 167.872s 720p input, compute a quality-aware split plan and verify it raises bitrate above the 720p floor.

#### H2: Hardware encoder mixing would improve the current two-part output
- Supports: QSV may be faster than software for supported bitrates.
- Conflicts: the logged budget is 278 kbps; encoder choice cannot make that enough for 720p.
- Test: inspect planner math before adding backend complexity.

#### H3: Downscaling would be better than more parts
- Supports: 278 kbps can be acceptable at lower resolution.
- Conflicts: the source is 720p and the uploader can batch more parts; preserving resolution is preferable while staying under attachment limits.
- Test: use more segments first; downscale remains the fallback when even 10 parts cannot preserve the source height.

### Experiments

- H1 confirmed: quality-aware planning chooses four roughly 42s parts for the reported 167.872s 720p input, raising the video budget above 1000 kbps.
- The existing parallel split threshold is 4, so this case should use parallel segment transcodes with the current transcode concurrency of 2.

### Root Cause

Split planning minimized attachment count by using 120s chunks, which starved 720p segments to 278 kbps and made successful uploads visibly pixelated.

### Fix

- Make `SplitTranscodePlan` resolution-aware and spend up to one Discord batch of attachments to preserve the source-height bitrate tier.
- Pass downloaded source height into split planning.
