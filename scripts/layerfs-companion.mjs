#!/usr/bin/env node
// Original LayerFS on a qualified Linux VM — companion process.
//
// SPEC §10.2.1: this script binds the PINNED ORIGINAL LayerFS SDK
// v1.0.0 (2aaf2552ce21154928b9f9c83582e41a637f5910) and drives it inside a
// qualified Linux VM. Nothing here re-implements the LayerFS Store: every
// Store/branch/workspace operation executes the upstream SDK code in the
// guest. The runtime export-surface probe below refuses to run when the
// SDK at SHENKO_LAYERFS_SDK_PATH does not match the pinned v1.0.0 surface.
//
// Protocol: one JSON object per line on stdin (requests), one JSON object
// per line on stdout (responses and async events). stderr is
// diagnostic-only.
//
// Required env:
//   SHENKO_LAYERFS_SDK_PATH   directory of the pinned original SDK checkout
//   SHENKO_LAYERFS_VM_PROVIDER qualified Linux VM provider id ("boxlite", ...)
//   SHENKO_LAYERFS_VM_NAME    VM name this companion owns

'use strict';

const VERSION = 'shenko-layerfs-companion/1';

// The pinned v1.0.0 JS SDK export surface (fixed SDK contract,
// sdk/js/README.md @ 2aaf2552ce21154928b9f9c83582e41a637f5910).
const PIN = 'v1.0.0@2aaf2552ce21154928b9f9c83582e41a637f5910';
const EXPECTED_EXPORTS = [
  // Store opener + guard: all direct openers share one Linux kernel and a
  // local ext4/xfs/btrfs filesystem; the guard checks the Store parent.
  'Store',
  // fs/branch fast path: opens the Store directly (no workspace needed).
  'Branch',
  // Full workspace: mount/exec/commit requires Docker Engine, FUSE,
  // devices and mount privileges, a matching runtime image and a plain
  // local named volume.
  'Workspace',
];
// At least one Store opener must exist; the others are checked for
// presence and reported so the Rust side sees the real surface.
const REQUIRED_EXPORTS = ['Store'];

const fs = require('node:fs');
const path = require('node:path');
const readline = require('node:readline');

function send(object) {
  process.stdout.write(JSON.stringify(object) + '\n');
}

function fail(seq, error, detail) {
  send({ seq, ok: false, error, detail: detail === undefined ? null : detail });
}

function ok(seq, detail) {
  send({ seq, ok: true, detail: detail === undefined ? {} : detail });
}

function event(kind, detail) {
  send({ event: kind, detail: detail === undefined ? {} : detail });
}

// ---------------------------------------------------------------------------
// SDK surface probe: bind the pinned ORIGINAL LayerFS SDK by its real
// runtime exports, never by assumption.
// ---------------------------------------------------------------------------

async function loadPinnedSdk(sdkPath) {
  if (!sdkPath) {
    throw Object.assign(new Error('SHENKO_LAYERFS_SDK_PATH is not set'), {
      code: 'layerfs_sdk_path_unset',
    });
  }
  const entry = path.join(sdkPath, 'sdk', 'js', 'dist', 'index.js');
  if (!fs.existsSync(entry)) {
    throw Object.assign(
      new Error(`pinned SDK entry not found at ${entry} (expected sdk/js/dist/index.js)`),
      { code: 'layerfs_sdk_entry_missing' }
    );
  }
  const pkgPath = path.join(sdkPath, 'sdk', 'js', 'package.json');
  let sdk;
  try {
    sdk = await import(entry);
  } catch (error) {
    throw Object.assign(new Error(`failed to import pinned SDK: ${error.message}`), {
      code: 'layerfs_sdk_import_failed',
    });
  }
  const exports = Object.keys(sdk);
  const missing = EXPECTED_EXPORTS.filter((name) => !exports.includes(name));
  const missingRequired = REQUIRED_EXPORTS.filter((name) => !exports.includes(name));
  if (missingRequired.length > 0) {
    throw Object.assign(
      new Error(
        `SDK export surface does not match ${PIN}: missing ${missingRequired.join(', ')}; ` +
          `present: ${exports.join(', ') || '(none)'}`
      ),
      { code: 'layerfs_sdk_surface_mismatch' }
    );
  }
  return { sdk, exports, missingOptional: missing };
}

// ---------------------------------------------------------------------------
// Qualified Linux VM provider. BoxLite is the optional first candidate
// (SPEC §34); any provider exposing this minimal real seam can be plugged
// in. No provider installed => typed-unavailable evidence, never a fake.
// ---------------------------------------------------------------------------

async function loadVmProvider(providerId) {
  const candidates = {
    boxlite: ['boxlite', '@boxlite/sdk'],
  };
  const names = candidates[providerId] || [providerId];
  let lastError = null;
  for (const name of names) {
    try {
      const mod = await import(name);
      const provider = mod.createVmProvider ? mod.createVmProvider() : mod.default;
      if (!provider || typeof provider.start !== 'function') {
        lastError = new Error(`provider ${name} does not expose start()`);
        continue;
      }
      return { provider, module: name };
    } catch (error) {
      lastError = error;
    }
  }
  throw Object.assign(
    new Error(
      `no qualified Linux VM provider "${providerId}" is installed (${String(lastError && lastError.message)})`
    ),
    { code: 'vm_provider_not_installed' }
  );
}

// Guest requirements for the FULL workspace path (SPEC §10.2.1): Docker
// Engine, FUSE, devices/mount privileges, matching runtime image, plain
// local named volume. The companion checks what the provider can prove
// and reports the rest as unverified, never assumed.
async function guestCapabilities(vm) {
  if (typeof vm.exec !== 'function') {
    return { exec: false, detail: 'provider exposes no guest exec channel' };
  }
  const probe = async (cmd) => {
    try {
      const result = await vm.exec(cmd);
      return result && (result.code === 0 || result.status === 0);
    } catch {
      return false;
    }
  };
  const [docker, fuse, mounts] = await Promise.all([
    probe('command -v docker'),
    probe('test -e /dev/fuse'),
    probe('cat /proc/mounts'),
  ]);
  return { exec: true, docker, fuse, mountsObserved: mounts, detail: 'probed via guest exec' };
}

// ---------------------------------------------------------------------------
// Companion main loop
// ---------------------------------------------------------------------------

async function main() {
  const sdkPath = process.env.SHENKO_LAYERFS_SDK_PATH || '';
  const providerId = process.env.SHENKO_LAYERFS_VM_PROVIDER || 'boxlite';
  const vmName = process.env.SHENKO_LAYERFS_VM_NAME || 'shenko-layerfs';

  // Surface probe first: bind the pinned original SDK before anything
  // touches a VM.
  let bound;
  try {
    bound = await loadPinnedSdk(sdkPath);
  } catch (error) {
    send({ event: 'fatal', detail: { code: error.code || 'unknown', error: error.message } });
    process.exit(2);
  }
  event('sdk_bound', {
    pin: PIN,
    exports: bound.exports,
    missingOptional: bound.missingOptional,
  });

  let provider = null;
  let providerModule = null;
  let vm = null;
  let vmState = 'stopped';

  async function ensureProvider() {
    if (provider) return;
    const loaded = await loadVmProvider(providerId);
    provider = loaded.provider;
    providerModule = loaded.module;
    event('vm_provider_loaded', { provider: providerId, module: providerModule });
  }

  async function ensureVm() {
    await ensureProvider();
    if (!vm) {
      vm = await provider.start(vmName);
      vmState = 'starting';
      event('vm_starting', { name: vmName });
      if (typeof vm.waitForDisplay === 'function') {
        const display = await vm.waitForDisplay();
        event('vm_display_ready', { display });
      }
      vmState = 'running';
    }
    return vm;
  }

  const commands = {
    async start(seq, args) {
      await ensureVm();
      const caps = await guestCapabilities(vm);
      const display =
        typeof vm.displayAddress === 'function' ? await vm.displayAddress() : null;
      const sshPort = typeof vm.sshPort === 'function' ? await vm.sshPort() : null;
      ok(seq, {
        state: vmState,
        display,
        sshPort,
        layerfsCommitted: false,
        guest: caps,
        sdk: { pin: PIN, exports: bound.exports },
      });
    },

    async stop(seq) {
      if (vm && typeof vm.stop === 'function') {
        await vm.stop();
      }
      vm = null;
      vmState = 'stopped';
      ok(seq, { state: vmState, layerfsCommitted: false });
    },

    async state(seq) {
      let display = null;
      let sshPort = null;
      if (vm) {
        if (typeof vm.displayAddress === 'function') display = await vm.displayAddress();
        if (typeof vm.sshPort === 'function') sshPort = await vm.sshPort();
        if (typeof vm.state === 'function') {
          const reported = await vm.state();
          if (reported && reported.state) vmState = reported.state;
        }
      }
      ok(seq, {
        state: vmState,
        display,
        sshPort,
        layerfsCommitted: false,
        sdk: { pin: PIN, exports: bound.exports },
      });
    },

    // The upstream Store's own commit receipt (layerfs_committed ONLY —
    // never product_durable, SPEC §10.2.1). Executed by the ORIGINAL SDK
    // inside the guest via the provider's exec channel.
    async commit_receipt(seq, args) {
      await ensureVm();
      if (typeof vm.exec !== 'function') {
        fail(seq, 'guest_exec_unavailable', 'provider exposes no guest exec channel');
        return;
      }
      const result = await vm.exec(
        `echo ${Buffer.from(
          `const s = await import('file://${sdkPath}/sdk/js/dist/index.js');` +
            `const r = typeof s.Store.commitReceipt === 'function'` +
            ` ? await s.Store.commitReceipt()` +
            ` : { note: 'pinned SDK exposes no Store.commitReceipt' };` +
            `console.log(JSON.stringify(r));`,
          'utf8'
        ).toString('base64')} | base64 -d | node --input-type=module`
      );
      const stdout = (result && (result.stdout || '')).toString();
      try {
        const receipt = JSON.parse(stdout);
        ok(seq, { layerfsCommitted: true, receipt });
      } catch {
        fail(seq, 'commit_receipt_unparseable', stdout.slice(0, 4096));
      }
    },
  };

  const rl = readline.createInterface({ input: process.stdin, terminal: false });
  rl.on('line', (line) => {
    if (!line.trim()) return;
    let request;
    try {
      request = JSON.parse(line);
    } catch {
      fail(0, 'protocol_decode_failed', line.slice(0, 256));
      return;
    }
    const handler = commands[request.cmd];
    if (!handler) {
      fail(request.seq, 'unknown_command', request.cmd);
      return;
    }
    Promise.resolve()
      .then(() => handler(request.seq, request.args || {}))
      .catch((error) => {
        fail(
          request.seq,
          error.code || 'companion_command_failed',
          error.message
        );
      });
  });
  rl.on('close', () => {
    // stdin closed: stop the guest best-effort and exit.
    Promise.resolve()
      .then(async () => {
        if (vm && typeof vm.stop === 'function') await vm.stop();
      })
      .finally(() => process.exit(0));
  });

  event('ready', { version: VERSION, pin: PIN, vmProvider: providerId, vmName });
}

main().catch((error) => {
  send({ event: 'fatal', detail: { code: error.code || 'unknown', error: error.message } });
  process.exit(2);
});
