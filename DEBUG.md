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
