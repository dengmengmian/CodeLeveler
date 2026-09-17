// schemas/*.schema.json → src/types/protocol.gen.ts
//
// Rust（leveler-client-protocol）是协议事实源：schemars 导出 JSON Schema
// 并由 schema_export.rs 测试守护；本脚本把 schema 翻译成 TypeScript，
// 让 TS 这一段也无法静默漂移。生成文件提交入库；`--check` 模式在
// typecheck/build 前比对，schema 变了而生成文件没跟上 → 直接红。
//
// 用法：
//   node scripts/gen-protocol.mjs          # 重新生成
//   node scripts/gen-protocol.mjs --check  # CI/typecheck 前置校验
//
// 只处理 schemars draft-07 实际产出的形态（tagged oneOf / definitions /
// ["T","null"] / anyOf-null / string enum / $ref / array / integer）。
// 遇到没见过的形态直接抛错——宁可红，不猜。

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, '..', '..', '..', '..');
const schemasDir = join(repoRoot, 'schemas');
const outPath = join(here, '..', 'src', 'types', 'protocol.gen.ts');

const SCHEMAS = [
  'client_command.schema.json',
  'runtime_event.schema.json',
  'ui_session_snapshot.schema.json',
];

/** definitions 跨 schema 去重：同名必须同形，否则事实源自身分叉了。 */
const definitions = new Map();

function fail(msg) {
  console.error(`gen-protocol: ${msg}`);
  process.exit(1);
}

function jsDoc(description, indent = '') {
  if (!description) return '';
  const oneLine = description.replace(/\s+/g, ' ').trim();
  return `${indent}/** ${oneLine.replace(/\*\//g, '*\\/')} */\n`;
}

function refName(ref) {
  const m = /^#\/definitions\/(\w+)$/.exec(ref);
  if (!m) fail(`unsupported $ref: ${ref}`);
  return m[1];
}

/** schema 节点 → TS 类型表达式。 */
function tsType(node, ctx) {
  if (node.$ref) return refName(node.$ref);
  if (node.allOf && node.allOf.length === 1) {
    return tsType({ ...node.allOf[0], description: node.description }, ctx);
  }
  if (node.anyOf) {
    return node.anyOf.map((n) => tsType(n, ctx)).join(' | ');
  }
  if (node.oneOf) {
    return node.oneOf.map((n) => tsType(n, ctx)).join(' | ');
  }
  const t = node.type;
  if (Array.isArray(t)) {
    // schemars 的 Option<T>：["T","null"]
    return t.map((one) => tsType({ ...node, type: one }, ctx)).join(' | ');
  }
  switch (t) {
    case 'null':
      return 'null';
    case 'string':
      if (node.enum) return node.enum.map((v) => `'${v}'`).join(' | ');
      return 'string';
    case 'boolean':
      return 'boolean';
    case 'integer':
    case 'number':
      return 'number';
    case 'array':
      if (!node.items) fail(`array without items in ${ctx}`);
      return `${tsType(node.items, ctx)}[]`;
    case 'object':
      return inlineObject(node, ctx);
    default:
      fail(`unsupported schema node in ${ctx}: ${JSON.stringify(node).slice(0, 120)}`);
  }
}

/** 内联 object（tagged union 的变体体）→ `{ a: T; b?: U }`。 */
function inlineObject(node, ctx) {
  const required = new Set(node.required ?? []);
  const props = node.properties ?? {};
  // `type` 判别字段排最前，其余按 schema 序（schemars 已排序）。
  const keys = Object.keys(props).sort((a, b) =>
    a === 'type' ? -1 : b === 'type' ? 1 : a.localeCompare(b),
  );
  const fields = keys.map((key) => {
    const opt = required.has(key) ? '' : '?';
    return `${key}${opt}: ${tsType(props[key], `${ctx}.${key}`)}`;
  });
  return `{ ${fields.join('; ')} }`;
}

/** 顶级/定义级声明 → `export type/interface`。 */
function emitDecl(name, node) {
  let out = jsDoc(node.description);
  if (node.oneOf) {
    const variants = node.oneOf.map((v) => {
      const doc = v.description ? `  ${jsDoc(v.description).trim()}\n` : '';
      return `${doc}  | ${tsType(v, name)}`;
    });
    out += `export type ${name} =\n${variants.join('\n')};\n`;
    return out;
  }
  if (node.type === 'object' && node.properties) {
    const required = new Set(node.required ?? []);
    const keys = Object.keys(node.properties).sort((a, b) => a.localeCompare(b));
    const fields = keys
      .map((key) => {
        const p = node.properties[key];
        const opt = required.has(key) ? '' : '?';
        return `${jsDoc(p.description, '  ')}  ${key}${opt}: ${tsType(p, `${name}.${key}`)};`;
      })
      .join('\n');
    out += `export interface ${name} {\n${fields}\n}\n`;
    return out;
  }
  out += `export type ${name} = ${tsType(node, name)};\n`;
  return out;
}

function collectDefinitions(schema, file) {
  for (const [name, node] of Object.entries(schema.definitions ?? {})) {
    const rendered = JSON.stringify(node);
    const seen = definitions.get(name);
    if (seen && seen.rendered !== rendered) {
      fail(`definition ${name} differs between schemas (${seen.file} vs ${file})`);
    }
    if (!seen) definitions.set(name, { node, rendered, file });
  }
}

const sections = [];
for (const file of SCHEMAS) {
  const schema = JSON.parse(readFileSync(join(schemasDir, file), 'utf8'));
  if (!schema.title) fail(`${file} has no title`);
  collectDefinitions(schema, file);
  // 顶级类型可能同时作为别的 schema 的 definition 出现（如 UiSessionSnapshot
  // 在 runtime_event 里）——definitions 版已包含同一形状，跳过避免重复声明。
  if (!definitions.has(schema.title)) sections.push(emitDecl(schema.title, schema));
}

const defDecls = [...definitions.keys()]
  .sort((a, b) => a.localeCompare(b))
  .map((name) => emitDecl(name, definitions.get(name).node));

const banner = `// 自动生成，禁止手改 —— npm run gen:protocol 重新生成。
// 事实源：Rust crates/leveler-client-protocol → schemas/*.schema.json
// （schema 由 \`UPDATE_SCHEMAS=1 cargo test -p leveler-client-protocol --features schema\` 守护）。
// web 网关自有帧（UpFrame/DownFrame/REST DTO）不在此文件，见 protocol.ts。

`;

const output = banner + [...defDecls, ...sections].join('\n');

if (process.argv.includes('--check')) {
  let committed = '';
  try {
    committed = readFileSync(outPath, 'utf8');
  } catch {
    fail(`${outPath} missing — run: npm run gen:protocol`);
  }
  if (committed !== output) {
    fail(
      'src/types/protocol.gen.ts is stale: the Rust protocol (schemas/) changed ' +
        'but the generated TS did not.\nRegenerate and commit: npm run gen:protocol',
    );
  }
  console.log('gen-protocol: protocol.gen.ts is in sync');
} else {
  writeFileSync(outPath, output);
  console.log(`gen-protocol: wrote ${outPath}`);
}
