// Real Runtime/ExecutionHost ownership across source switches and bridge replacement.
import { createRequire } from 'node:module';
import { mkdtemp, mkdir, writeFile, rm, realpath } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import assert from 'node:assert/strict';
import path from 'node:path';
const require=createRequire(import.meta.url);
const {Bridge}=require('../src/bridge.cjs');
const temporary=await mkdtemp('/tmp/leveler-terminal-routing-');
const home=path.join(temporary,'home'); await mkdir(home);
await writeFile(path.join(home,'config.toml'),'default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:9"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="fixture"\ncontext_window=131072\n');
const binary=process.env.LEVELER_BINARY??path.resolve(import.meta.dirname,'../../../target/debug/leveler');
const options={env:{...process.env,LEVELER_HOME:home,LEVELER_CONFIG_DIR:undefined}};
let bridge=new Bridge(binary,options);
const terminals=[];
try {
  for (const name of ['first','second']) {
    const workspace=path.join(temporary,name); await mkdir(workspace);
    await bridge.request('connect',{workspace});
    const created=await bridge.request('create',{goal:'terminal route'});
    const params={session_id:created.session.id,terminal_id:randomUUID()};
    terminals.push(params);
    const terminal=await bridge.request('create_terminal',{...params,cols:80,rows:24});
    assert.equal(terminal.cwd,await realpath(workspace)); assert.equal(terminal.status,'running');
  }
  // The currently selected source is second: first must keep its original owner.
  for(const params of terminals) assert.equal((await bridge.request('observe_terminal',{...params,cursor:0,max_bytes:65536})).status,'running');
  assert.equal((await bridge.request('snapshot',{session_id:terminals[1].session_id})).id,terminals[1].session_id);
  await assert.rejects(bridge.request('snapshot',{session_id:terminals[0].session_id}),/session is not selected/);
  bridge.close(); bridge=new Bridge(binary,options);
  // No connect/open/create in this bridge: recover from durable source identity.
  for(const params of terminals) {
    assert.equal((await bridge.request('observe_terminal',{...params,cursor:0,max_bytes:65536})).status,'running');
    const closed=await bridge.request('close_terminal',params);
    assert.ok(['exited','killed'].includes(closed.status));
    assert.equal((await bridge.request('observe_terminal',{...params,cursor:0,max_bytes:65536})).status,closed.status);
  }
  console.log('Terminal routing: two sources, selection switch, fresh bridge recovery, observed close passed.');
} finally {
  for(const params of terminals) await bridge.request('close_terminal',params).catch(()=>{});
  bridge.close();
  await rm(temporary,{recursive:true,force:true});
}
