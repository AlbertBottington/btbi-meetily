# BTBI speaker attribution evaluation

Experimental, opt-in evaluation on the central processor. This is not an enabled
Meetily feature and does not name people automatically. It does not change the
Windows installer, recording callbacks, SharePoint permissions, upload queue,
production worker, or original transcripts. No service or schedule is installed.

## Why this first

Compare pyannote Community-1 against human-checked Teams references before trusting
names in BTBI transcripts. Teams is a comparison source, not ground truth: check
names and timing against audible speech. Keep enrollment/calibration meetings
separate from held-out evaluation meetings when automatic recognition is added.

Upstream already has related implementations:
- [issue 230](https://github.com/Zackriya-Solutions/meetily/issues/230)
- [PR 511](https://github.com/Zackriya-Solutions/meetily/pull/511): live Rust/ONNX
  embeddings and persistent profiles.
- [PR 538](https://github.com/Zackriya-Solutions/meetily/pull/538): wider desktop
  integration and profiles.
- [PR 833](https://github.com/Zackriya-Solutions/meetily/pull/833): offline Rust
  diarization, naming and playback.

This fork-only benchmark does not duplicate their desktop integration. Review
those approaches before selecting production recognition; do not port an entire
unreviewed PR or assume its reported accuracy applies to BTBI meetings.

## Data and access

Keep all recordings, name maps, reference labels and results in a private directory
**outside the repository**. Do not add private meeting content to PRs or CI.
This tool reads existing local inputs; it neither downloads meeting recordings nor
uploads anything. Use already-authorized copies without changing the no-local-
recording-staging design on Matt's or Patrick's PC. A real benchmark's temporary
processor storage and cleanup must follow the approved storage constraints.

Inference requires Python 3.11+, FFmpeg and the optional requirements below.
Accept the [Community-1 model conditions](https://huggingface.co/pyannote/speaker-diarization-community-1)
and download its complete offline bundle separately through an approved account.
The CLI accepts only a trusted local pipeline directory containing `config.yaml`;
it sets Hugging Face offline mode and disables pyannote and Hub telemetry before
import. Missing models fail; no token entry, hosted API fallback or silent download.
The trusted model configuration must be Community-1, not a hosted pipeline.
Toolkit license: MIT. Community-1 weights: CC-BY-4.0; retain notices/attribution.
Do not redistribute models in our installer through this benchmark.

```sh
python3 -m venv /private/path/speaker-eval-env
/private/path/speaker-eval-env/bin/pip install -r scripts/speaker-evaluation/requirements.txt
/private/path/speaker-eval-env/bin/python scripts/speaker-evaluation/evaluate.py infer \
  --audio /private/path/recording.wav \
  --model-dir /private/path/community-1 \
  --output /private/path/diarization.json
```

Inference uses CPU explicitly, records elapsed time (including model loading),
process peak RSS, real-time factor, library version, audio hash and model bundle
hashes. RSS is process-wide, not incremental model memory. The package version is
pinned; transitive dependencies are not locked. Retain `pip freeze` with each real
benchmark. Runtime/accuracy on the Mac mini have not yet been established.

## Names and transcript timing

Inspect speaker samples and create a **per-run** verified name map. Cluster labels
are arbitrary across runs; reusing SPEAKER_00 from another meeting is unsafe.
Copy `run_id` from the inference result:

```json
{"run_id":"COPY-EXACT-RUN-ID","speakers":{
  "SPEAKER_00":{"name":"Matt","confirmed":true},
  "SPEAKER_01":{"name":"Patrick","confirmed":true},
  "SPEAKER_02":{"name":"Client 1","confirmed":true}
}}
```

Use Client labels only after confirming that voice is external. Unmapped clusters
stay Unknown. A microphone source is supporting evidence, not automatic identity:
room bleed, shared devices and unrelated speech can be captured. This first PR
does not extract/store voiceprints or recognize a person across meetings.

The BTBI `transcripts.json` shape is `{"segments":[{"text":"...",
"audio_start_time":0.0,"audio_end_time":2.0}]}`. Times must be positive-duration
recording-relative intervals. Confirm the transcript and audio are the same
capture, and check alignment near the start, middle and end. The required offset
means **audio time = transcript time + offset**. It corrects a constant shift only;
pauses, missing chunks, sample-clock drift and dual-PC timelines need separate
reconciliation. Never use wall-clock offsets as a substitute for checking audio.
Do not align a compacted source WAV without reconstructing its source timing map.
Use the mixed recording baseline first; source tracks are a later evidence path.

```sh
python3 scripts/speaker-evaluation/evaluate.py align \
  --transcript /private/path/transcripts.json \
  --diarization /private/path/diarization.json \
  --names /private/path/verified-names.json --offset-seconds 0 \
  --output /private/path/attributed-evaluation.json
```

Original text, timing and other segment fields remain in the derivative. The tool
uses overlapping diarization, not exclusive turns, and only assigns a name when
one cluster covers the entire text segment. Speaker changes, overlap, gaps or
unconfirmed names remain Unknown with candidate clusters and a reason. This is
intentionally conservative; it does not split words by an invented timestamp.
Exclusive turns are preserved in the inference output for later alignment work.
Outputs use exclusive file creation, so existing artifacts cannot be overwritten.

## Reference scoring

Prepare human-verified reference turns and predicted named turns on the **same
audio timeline**, as JSON arrays of `{"start":0.0,"end":2.0,"speaker":"Matt"}`.
Use the same labels for external speakers in both. Hypothesis can include Unknown.
For scoring aligned transcript results, convert `audio_start_time/audio_end_time`
using the recorded offset; do not include transcript text. An absent predicted
turn is scored as unknown/missed coverage on reference speech, not a correct name.

```sh
python3 scripts/speaker-evaluation/evaluate.py score \
  --reference /private/path/reference-turns.json \
  --hypothesis /private/path/predicted-turns.json \
  --output /private/path/score.json
python3 -m unittest discover -s scripts/speaker-evaluation -v
```

Metrics partition non-overlapping reference speech into correct, wrong-name and
unknown seconds. Wrong-name rate uses all scored reference speech as denominator;
report it **together with named coverage**, because labeling nothing gives zero
wrong names. Reference overlap is separately counted, not scored for identity;
false-alarm time outside reference speech is reported. These are exact interval
metrics, not standard diarization error rate or word-level accuracy. Duplicate
same-speaker spans are unioned logically. No reference speech yields null rates.

## Pilot acceptance and remaining work

1. Use several approved meetings: Matt only, Patrick only, both recording the same
   meeting, a recurring client, interruptions/overlap, and pause/resume.
2. Verify capture completeness and clock alignment first. Retain corrections to
   Teams labels separately; do not tune on held-out evaluation meetings.
3. Run Community-1 and inspect errors, named coverage, overlap, elapsed time and
   memory. Report per meeting as well as overall; do not call synthetic tests an
   accuracy benchmark.
4. Only then select/calibrate an embedding model, enrollment samples, rejection
   threshold and runner-up margin. Prefer Unknown to a wrong personal name.
5. Implement dual-capture alignment/deduplication, source-track evidence and named
   text exports in a separate reviewed integration. Keep original artifacts and
   recording availability independent of attribution failure.

No real-audio inference, model accuracy, dual-PC reconciliation or production
activation is claimed by this PR. Standard-library tests cover alignment, unknown
fallback, overlap, fractional boundaries, malformed inputs and no-overwrite rules.
