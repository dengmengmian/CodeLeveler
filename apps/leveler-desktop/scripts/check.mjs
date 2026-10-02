import {spawnSync} from 'node:child_process';import {readdirSync} from 'node:fs';import {fileURLToPath} from 'node:url';import path from 'node:path';
const root=fileURLToPath(new URL('..',import.meta.url));
for(const directory of ['src','scripts','test'])for(const file of readdirSync(path.join(root,directory))){if(!/\.(mjs|cjs)$/.test(file))continue;const result=spawnSync(process.execPath,['--check',path.join(root,directory,file)],{stdio:'inherit'});if(result.status!==0)process.exit(result.status??1);}
console.log('Desktop JavaScript syntax check passed (native modules need no bundler).');
