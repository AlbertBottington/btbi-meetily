# BTBI SharePoint pilot

Status: implementation under review, not a validated production release.

The current cloud-only design supersedes the local snapshot pilot described
below. See [cloud-only capture](cloud-only-capture.md) for its behavior and
remaining acceptance checks. No installed build is implied.

## Setup

Register a **single-tenant public-client** Microsoft Entra application for BTBI
Meetily. Enable public client/device-code flow. Delegated permissions:
`User.Read`, `Files.SelectedOperations.Selected`, `offline_access`. No client secret belongs in this
app. A tenant administrator may need to consent. Confirm that both Matt and
Patrick can write to the intended SharePoint folder with these permissions;
do not silently expand scopes if the tenant requires more.

Settings → SharePoint → One-time administrator setup accepts tenant ID,
application/client ID, document-library drive ID and destination-folder item ID.
Sign in with each recorder's own BTBI account. The app checks the destination is
a folder and uses the signed-in user ID. On Windows, refresh credentials are
stored in user-scoped DPAPI-encrypted files under the Tauri app-data directory
(`btbi/tokens/`), avoiding Credential Manager's password size limit. macOS keeps
using Keychain. Access tokens stay in memory. Authentication is Microsoft device-code flow, not a custom
password form. Tenant policy may block this flow; that requires an explicit
identity-design decision rather than embedding secrets or bypassing policy.

Enable Save recordings under Recordings. Upload pause stops transfers, not
capture or queueing. Changing accounts/destinations does not reroute old captures:
those require their original recorder and destination. Original recordings are
never automatically deleted in this pilot.

## Data and completion

- Existing mixed audio continues unchanged.
- Additional 48 kHz PCM16 WAV files preserve microphone and system sources.
  These consume approximately 691 MB/hour combined, plus mixed audio and the
  immutable upload snapshot. WAV has a bounded size; over-limit capture blocks
  automatic publication, rather than truncating silently.
- `source-index.jsonl` maps each source's sample offsets to capture timestamps.
  It preserves gaps and pauses; it does not assert sample-clock alignment or
  identify speakers. Missing input produces an empty valid WAV track and an
  explicit zero sample count in `sources-complete.json`.
- Only successful source/audio finalization and drained, error-free transcription
  produce `capture-ready.json`. Canonical transcript segments are collected in
  the backend; frontend render timing is not the upload source of truth.
- After SQLite saves the meeting, a durable snapshot is queued under its capture
  UUID. Startup scans saved opted-in completed meetings to recover missed queues.
- Queue files have SHA-256 checksums. Retries use the same capture folder and
  restore the server's confirmed byte offset. Existing remote files are verified
  by size and downloaded content hash before being accepted as duplicates.
- `manifest.json` is uploaded **last**. Central consumers must require this file
  and validate its declared file set/checksums before filing a meeting.
- Upload-session URLs are sensitive and remain local, never in shared manifests
  or UI status. Requests to those URLs do not carry Graph bearer tokens.

The app's separate identifier is `com.btbiconsulting.meetily`. It does not migrate
or delete the original Meetily installation's data. Upstream updater configuration
is removed. Pilot installers are unsigned; there is no automatic release/update
channel until BTBI signing is configured. Do not bypass managed-PC restrictions.

## Acceptance (must be exercised before rollout)

1. Matt signs in and records a meeting. Check separate tracks and final transcript
   include the final words; confirm one complete package in SharePoint.
2. Repeat with Patrick, then with both recording the same meeting. Expect two
   distinct capture packages; cross-capture reconciliation is deferred.
3. Disconnect internet mid-upload, quit/restart, and reconnect. Confirm resumption,
   no duplicate folders, and no completion manifest while partial.
4. Test revoked credentials, expired upload sessions, throttling, disk full,
   transcription errors/timeouts, microphone disconnect and changed account.
5. Check long-recording timestamps/pause gaps, original-file retention, Windows
   DPAPI-protected token storage, installer identity and disabled upstream updates.

Core tests are run independently with `cargo test --manifest-path
btbi-core/Cargo.toml`. Full app compilation, Windows installer and live recording/
SharePoint tests are separate acceptance claims. Mock/unit success is not live
upload proof. No speaker training, cross-capture merge, or email processing is
included.

## Implementation verification (2026-11-12)

- macOS core: 13 tests passed, including real local HTTP transfers with throttling,
  resume after restart, expired-session recreation, lost-commit reconciliation,
  conflict detection and no bearer token on preauthenticated transfer URLs.
  A separate regression rejects late transcript results from prior sessions.
  These are synthetic mock-server tests, not Microsoft tenant acceptance.
- Frontend TypeScript check and production static build passed.
- Source-only native Rust library and test-target checks passed with `DOCS_RS=1` (cidre native build skipped)
  and `TAURI_CONFIG` excluding bundled resources/sidecars. This verifies Rust types,
  not native linking, an executable, audio capture, or packaging. The normal Mac
  build cannot proceed without full Xcode.
- Initial Windows core run found a read-only file handle could not be flushed.
  Snapshots now open copied files for writing before flushing. Windows core retest passed at ed90ac4 (12 tests); the current head adds session
  isolation and needs a fresh Windows run. The installer pipeline remains pending; the native final-window regression test
  is included in that pipeline.
- Live tenant consent, account sign-in, SharePoint delivery, Windows native UI,
  actual devices, and pilot acceptance above remain unverified.

Protocol references: [resumable uploads](https://learn.microsoft.com/en-us/graph/api/driveitem-createuploadsession?view=graph-rest-1.0)
and [device authorization](https://learn.microsoft.com/en-us/entra/identity-platform/v2-oauth2-device-code).

## Windows sign-in storage repair

- Only ciphertext reaches the token file or its same-directory replacement file.
  File names and DPAPI entropy bind each token to tenant, client and recorder ID.
  Protection uses the current Windows user, not machine-wide DPAPI.
- Refresh rotation replaces the file atomically after flushing it; a failed write
  leaves the prior credential intact. A smaller legacy credential migrates only
  after protected storage succeeds. Corrupt protected files fail closed.
- Sign out deletes protected and legacy credentials, clears the profile and pauses
  uploads; it preserves destination settings and queued captures. Stop recording
  before signing out. Sign out before changing account or destination.
- Save setup shows confirmation. Authentication failures display a sign-in error.
- Windows regressions: oversized token round-trip, fresh-process read, rotation,
  blocked replacement, corrupt/cross-account ciphertext, migration and deletion.
  Run `cargo test --manifest-path btbi-core/Cargo.toml --features token-store --locked`
  on Windows. Cross-compilation alone does not prove DPAPI runtime behavior.
- Acceptance remains a real Windows sign-in, restart and SharePoint test upload.
  Do not treat the browser's Microsoft success page as app-side persistence proof.
