// schemas/client_command.schema.json -> src/command-policy.gen.{mjs,cjs}
//
// Rust (leveler-client-protocol) is the protocol source of truth: schemars
// exports JSON Schema and schema_export.rs guards it. The desktop consumes the
// snapshot-versioned command list, so it is generated from that schema rather
// than copied by hand. Run with --check to fail when the committed files are
// stale.
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const schemaPath = path.join(root, '../../schemas/client_command.schema.json');
const schema = JSON.parse(readFileSync(schemaPath, 'utf8'));
const commands = schema['x-snapshot-versioned-commands'];
if (!Array.isArray(commands) || !commands.every(tag => typeof tag === 'string')) {
  console.error('gen-command-policy: client command schema lacks snapshot version policy');
  process.exit(1);
}

const literal = JSON.stringify(commands);
const banner = '// Generated from Rust command schema; run npm run gen:protocol in apps/leveler-desktop.\n';
const outputs = [
  [path.join(root, 'src/command-policy.gen.mjs'), `${banner}export const SNAPSHOT_VERSIONED_COMMANDS = ${literal};\n`],
  [path.join(root, 'src/command-policy.gen.cjs'), `${banner}module.exports.SNAPSHOT_VERSIONED_COMMANDS = ${literal};\n`],
];

let stale = false;
for (const [file, content] of outputs) {
  if (process.argv.includes('--check')) {
    if (readFileSync(file, 'utf8') !== content) {
      console.error(`gen-command-policy: ${file} is stale; run npm run gen:protocol`);
      stale = true;
    }
  } else {
    writeFileSync(file, content);
  }
}
if (stale) process.exit(1);
