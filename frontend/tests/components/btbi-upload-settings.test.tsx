import { afterAll, afterEach, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import React from 'react';
const originalCore = { ...await import('@tauri-apps/api/core') };
afterAll(() => mock.module('@tauri-apps/api/core', () => originalCore));
const destination = { tenant_id: 'tenant', client_id: 'client', drive_id: 'drive', folder_id: 'folder' };
let status: any;
let failure = '';
const invoke = mock(async (name: string) => {
  if (name === 'btbi_status') return structuredClone(status);
  if (failure) throw new Error(failure);
  if (name === 'btbi_configure') status.notice = 'Setup saved.';
  if (name === 'btbi_signout') {
    status.settings.profile = null;
    status.settings.enabled = false;
    status.notice = 'Signed out. Automatic upload paused.';
  }
});
mock.module('@tauri-apps/api/core', () => ({ invoke }));
const { BtbiUploadSettings } = await import('../../src/components/BtbiUploadSettings');
let renderer: ReactTestRenderer | undefined;
afterEach(() => { act(() => renderer?.unmount()); renderer = undefined; failure = ''; invoke.mockClear(); });
async function render(signedIn = false) {
  status = { settings: { destination, enabled: signedIn, profile: signedIn ? { displayName: 'Test User' } : null }, entries: [], signing_in: false, notice: '' };
  await act(async () => { renderer = create(<BtbiUploadSettings />); });
}
function button(label: string) { return renderer!.root.findAllByType('button').find(b => b.children.join('') === label)!; }
function visible() { return JSON.stringify(renderer!.toJSON()); }
test('Save setup refreshes status and shows confirmation without waiting for poll', async () => {
  await render();
  await act(async () => { await button('Save setup').props.onClick(); });
  expect(visible()).toContain('Setup saved.');
  expect(invoke.mock.calls.some(c => c[0] === 'btbi_configure')).toBe(true);
});
test('Sign out clears displayed profile and pauses uploads', async () => {
  await render(true);
  expect(visible()).toContain('Signed in as Test User');
  await act(async () => { await button('Sign out').props.onClick(); });
  expect(visible()).toContain('Not signed in');
  expect(visible()).toContain('Automatic upload paused');
  expect(visible()).not.toContain('Signed in as Test User');
});
test('Command failures show an alert, never a success confirmation', async () => {
  await render(); failure = 'Could not save setup';
  await act(async () => { await button('Save setup').props.onClick(); });
  expect(renderer!.root.findByProps({ role: 'alert' }).children.join('')).toContain('Could not save setup');
  expect(visible()).not.toContain('Setup saved.');
});
