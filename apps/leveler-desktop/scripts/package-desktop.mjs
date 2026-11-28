#!/usr/bin/env node
// Build the macOS Desktop RC bundle from the source tree.
//
// This is a packaging step, not a second product: it takes the existing
// Electron app, the release `leveler` binary, and the existing renderer/assets,
// then writes a self-contained .app, .zip and .dmg plus a manifest of hashes.
//
// Signing and notarization are deliberately separate: the script records the
// true signing state and refuses to claim a Developer ID signature. Without a
// real identity the artifact is an unsigned local RC, never a distribution
// build.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import {
  chmodSync, copyFileSync, cpSync, existsSync, mkdirSync, readFileSync, readdirSync,
  rmSync, writeFileSync,
} from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import packager from '@electron/packager';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '../../..');
const DESKTOP = path.join(ROOT, 'apps/leveler-desktop');
const args = process.argv.slice(2);
const option = name => {
  const index = args.indexOf(name);
  return index >= 0 ? args[index + 1] : undefined;
};
const sha = file => createHash('sha256').update(readFileSync(file)).digest('hex');
const run = (command, argv, options = {}) =>
  execFileSync(command, argv, { encoding: 'utf8', ...options }).trim();

if (process.platform !== 'darwin') {
  throw new Error('this packaging script builds the macOS RC only');
}
const cargoVersion = /^version\s*=\s*"([^"]+)"/m.exec(readFileSync(path.join(ROOT, 'Cargo.toml'), 'utf8'))?.[1];
const version = option('--version') ?? cargoVersion;
const binary = path.resolve(option('--binary') ?? path.join(ROOT, 'target/release/leveler'));
const out = path.resolve(option('--out') ?? path.join(ROOT, 'target/desktop-rc'));
if (!version) throw new Error('cannot resolve the product version');
if (!existsSync(binary)) throw new Error(`release runtime missing: ${binary}`);
const runtimeVersion = run(binary, ['--version']);
const runtimeHash = sha(binary);
const runtimeDirty = runtimeVersion.includes('UNTRUSTED');

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });

// ── Icon ────────────────────────────────────────────────────────────────────
const iconset = path.join(out, 'CodeLeveler.iconset');
mkdirSync(iconset);
const iconSource = name => path.join(ROOT, 'assets/brand', name);
const iconSizes = [
  ['icon_16x16.png', 16], ['icon_16x16@2x.png', 32],
  ['icon_32x32.png', 32], ['icon_32x32@2x.png', 64],
  ['icon_128x128.png', 128], ['icon_128x128@2x.png', 256],
  ['icon_256x256.png', 256], ['icon_256x256@2x.png', 512],
  ['icon_512x512.png', 512], ['icon_512x512@2x.png', 1024],
];
for (const [name, size] of iconSizes) {
  const source = size <= 512 ? iconSource(`codeleveler-app-icon-${size}.png`) : iconSource('codeleveler-app-icon-512.png');
  run('sips', ['-z', String(size), String(size), source, '--out', path.join(iconset, name)]);
}
const icon = path.join(out, 'CodeLeveler.icns');
run('iconutil', ['-c', 'icns', iconset, '-o', icon]);
rmSync(iconset, { recursive: true, force: true });

// ── Runtime bundled beside the app ──────────────────────────────────────────
const runtimeStage = path.join(out, 'runtime');
mkdirSync(runtimeStage);
const stagedRuntime = path.join(runtimeStage, 'leveler');
copyFileSync(binary, stagedRuntime);
chmodSync(stagedRuntime, 0o755);
if (existsSync(path.join(path.dirname(binary), 'leveler-confine.exe'))) {
  copyFileSync(path.join(path.dirname(binary), 'leveler-confine.exe'), path.join(runtimeStage, 'leveler-confine.exe'));
}

// ── Electron bundle ─────────────────────────────────────────────────────────
const [appPath] = await packager({
  dir: DESKTOP,
  out,
  name: 'CodeLeveler',
  appBundleId: 'com.codeleveler.desktop',
  appVersion: version,
  buildVersion: version,
  icon,
  platform: 'darwin',
  arch: process.arch === 'arm64' ? 'arm64' : 'x64',
  overwrite: true,
  prune: true,
  asar: false,
  extraResource: [runtimeStage],
  ignore: [
    /^\/scripts($|\/)/,
    /^\/test($|\/)/,
    /^\/README\.md$/,
    /^\/package-lock\.json$/,
    /^\/tsconfig\.json$/,
    /^\/eslint\.config\.mjs$/,
  ],
});
if (!appPath || !existsSync(appPath)) throw new Error('packager did not produce an .app');

// The renderer and protocol handler resolve this sibling from
// Resources/app/src/../../../packages, which is Contents/packages in a bundle.
const packagedPackages = path.join(appPath, 'Contents/packages');
mkdirSync(packagedPackages, { recursive: true });
cpSync(
  path.join(ROOT, 'packages/conversation-presentation'),
  path.join(packagedPackages, 'conversation-presentation'),
  { recursive: true },
);

const resources = path.join(appPath, 'Contents/Resources');
const bundledRuntime = path.join(resources, 'runtime/leveler');
const checks = {
  runtime_present: existsSync(bundledRuntime),
  runtime_version: existsSync(bundledRuntime) ? run(bundledRuntime, ['--version']) : null,
  runtime_matches_input: existsSync(bundledRuntime) ? sha(bundledRuntime) === runtimeHash : false,
  conversation_package: existsSync(path.join(packagedPackages, 'conversation-presentation/conversation.mjs')),
  marked: existsSync(path.join(resources, 'app/node_modules/marked/lib/marked.esm.js')),
  xterm: existsSync(path.join(resources, 'app/node_modules/@xterm/xterm/lib/xterm.mjs')),
  electron_framework: existsSync(path.join(appPath, 'Contents/Frameworks/Electron Framework.framework')),
  info_version: run('plutil', ['-extract', 'CFBundleShortVersionString', 'raw', path.join(appPath, 'Contents/Info.plist')]),
  info_bundle_id: run('plutil', ['-extract', 'CFBundleIdentifier', 'raw', path.join(appPath, 'Contents/Info.plist')]),
  info_executable: run('plutil', ['-extract', 'CFBundleExecutable', 'raw', path.join(appPath, 'Contents/Info.plist')]),
};
if (!checks.runtime_present || !checks.runtime_matches_input || !checks.conversation_package
  || !checks.marked || !checks.xterm || !checks.electron_framework) {
  throw new Error(`packaged bundle is incomplete: ${JSON.stringify(checks, null, 2)}`);
}
if (checks.info_version !== version) {
  throw new Error(`bundle version ${checks.info_version} != requested ${version}`);
}

// Signing state is reported, never guessed. `codesign -dv` fails for an
// unsigned bundle; a self-signed/ad-hoc identity is not a Developer ID.
let signing = { status: 'UNSIGNED', detail: 'no Developer ID identity available' };
try {
  const detail = run('codesign', ['-dv', '--verbose=4', appPath], { stdio: ['ignore', 'pipe', 'pipe'] });
  signing = { status: 'SIGNED_UNVERIFIED', detail };
} catch (error) {
  signing = { status: 'UNSIGNED', detail: String(error.stderr || error.message).trim() };
}
let gatekeeper = { status: 'NOT_ASSESSED' };
try {
  const detail = run('spctl', ['--assess', '--type', 'execute', '--verbose=4', appPath], { stdio: ['ignore', 'pipe', 'pipe'] });
  gatekeeper = { status: 'ACCEPTED', detail };
} catch (error) {
  gatekeeper = { status: 'REJECTED', detail: String(error.stderr || error.message).trim() };
}

// ── Distribution-shaped artifacts ───────────────────────────────────────────
const base = `CodeLeveler-${version}-macOS-${process.arch}`;
const zip = path.join(out, `${base}.zip`);
const dmg = path.join(out, `${base}.dmg`);
run('ditto', ['-c', '-k', '--sequesterRsrc', '--keepParent', appPath, zip]);
run('hdiutil', ['create', '-volname', `CodeLeveler ${version}`, '-srcfolder', appPath, '-ov', '-format', 'UDZO', dmg]);

const manifest = {
  product: 'CodeLeveler Desktop',
  version,
  source: {
    head: run('git', ['rev-parse', 'HEAD'], { cwd: ROOT }),
    dirty: run('git', ['status', '--porcelain'], { cwd: ROOT }).split('\n').filter(Boolean).length,
  },
  built_at: new Date().toISOString(),
  host: { os: os.version(), arch: process.arch, node: process.version },
  runtime: { path: binary, sha256: runtimeHash, version: runtimeVersion, dirty: runtimeDirty },
  checks,
  signing,
  gatekeeper,
  artifacts: {
    app: { path: appPath, sha256: null },
    zip: { path: zip, sha256: sha(zip) },
    dmg: { path: dmg, sha256: sha(dmg) },
  },
};
writeFileSync(path.join(out, 'manifest.json'), JSON.stringify(manifest, null, 2));
console.log(JSON.stringify(manifest, null, 2));
