# Cloud-only capture implementation

Requested by Matt, Telegram 11285–11292, 2026-11-16. Work item:
`work_mwdym8lc_e05dbf`. This is development code, not an installed pilot.

## Acceptance target

Record and stop on Windows without an application-created recording staging
folder. Upload bounded audio segments to Company Docs/Comms/Needs Filing,
assemble one playable recording per meeting, verify its bytes and the complete
transcript, then clean up only that capture's verified segments. Never delete
original Meetily recordings. Teams runs alongside during pilot acceptance.
Matt accepts loss during extended outages; that must appear as an incomplete
capture, never success. Application RAM use does not guarantee Windows won't
page memory or include it in crash dumps.

## Implemented core

- `cloud_audio`: mono 48kHz PCM16 WAV segmentation at 30 seconds, exact tail
  preservation, checksums, ordered-input validation and WAV size-limit checks.
- `cloud_transfer`: immutable segment uploads, downloaded-byte verification,
  and two-pass remote assembly. First pass validates segments and hashes the
  intended recording. Second pass downloads one segment at a time and feeds
  320KiB-aligned requests into a final SharePoint upload session whose total
  length is now known. A final download/hash check verifies the recording.
- No filesystem calls in these two modules. Processing memory is bounded by
  one segment plus a transfer block, not meeting duration. The native capture
  caller uses the bounded queue and failure state described below.
- Existing remote content is accepted only if its bytes match; retries after
  final commit do not create another recording. Conditional segment cleanup runs
  only after final artifact verification.

## Native integration and processor

The BTBI recorder requires SharePoint sign-in and Save recordings enabled.
Its mixed-audio saver bypasses the local folder/checkpoint path. The source
pipeline feeds separate microphone and system segmenters. Timing indexes upload
with each segment rather than accumulating for the entire meeting. Twelve pending segments are allowed in RAM; overflow or upload failure
marks the capture incomplete. Uploads retry twice, with bounded delays. A
completion manifest is published only after audio drains and transcription
completes. Original Meetily files and old upload queues are not deleted.
Normal application settings, OS-protected credentials, and Meetily's SQLite
transcript history still exist locally; this change removes audio staging, not
all application persistence or operating-system paging/crash dumps.

Matt selected the Mac mini for assembly (Telegram audio after 11292). The worker
reads only the configured Needs Filing folder, preserves separate source tracks,
writes a mixed `recording.wav`, source WAVs, transcripts and a completion receipt.
It verifies final files before deleting source segments using conditional ETags.
A cleanup receipt allows discovery to skip finished captures. A partial capture
without its ready manifest is retained and never reported as complete.

Worker commands take a non-secret configuration path: `configure` reads a
Destination JSON object on stdin; `signin` uses Microsoft device-code flow and
the OS keychain; `once` processes a bounded folder scan; `run` scans every 60s.
The processor has not been installed as a service or signed in. Do not run
multiple worker instances. Its configured account and folder-specific write
grant must be verified before activation; do not broaden access to make a test
pass. Recorder and worker request Files.SelectedOperations.Selected, User.Read
and offline_access. Other folders must be denied in live acceptance.

## Remaining acceptance / release gates

- Pass Windows CI and obtain human review of the final PR head. Cargo 1.100
  changed its build-output layout; the exact-version ort-sys CI patch resolves
  the profile directory from the build ancestor rather than a fixed depth.
  The pipeline tests both layouts before native tests and packaging. The CPU
  portability guard also discovers nested CMake caches without weakening flag
  checks. Both Windows core and native subsystem tests passed at `1cfdc42`;
  installer packaging still required the portability-discovery fix.
- Install and sign in on Matt's PC and the Mac worker. Register the processor
  through a reviewed service setup; no new service or schedule is active yet.
- Record live microphone/system audio, confirm the final words, timeline and
  source tracks, verify the single recording in SharePoint, and verify segment
  cleanup and no local audio/checkpoint files. Test a short network interruption,
  buffer exhaustion, revoked access, and process shutdown/restart.
- Source-only Rust checks do not prove native linking or hardware behavior.
  macOS native builds require full Xcode; no Windows installer for this head
  has been produced. Local original recordings remain untouched.

## Verification

Core tests include exact sample preservation over segment boundaries; malformed,
missing/reordered/corrupt inputs; queue rejection propagation; and an HTTP mock
that exercises real request bodies for segmented upload and multi-block assembly,
retry-after-commit idempotency, corruption rejection and no bearer token leakage
onto preauthenticated transfer URLs. These are not live Microsoft tests.
