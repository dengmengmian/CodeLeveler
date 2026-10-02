import {test} from 'node:test';import assert from 'node:assert/strict';import {fileURLToPath} from 'node:url';import {mkdtempSync,writeFileSync,chmodSync,rmSync} from 'node:fs';import {tmpdir} from 'node:os';import path from 'node:path';import bridgeModule from '../src/bridge.cjs';
const fixture=fileURLToPath(new URL('./fixtures/bridge.cjs',import.meta.url));
// Bridge's production argv is fixed. Test peer is a small executable wrapper.
function peer(){const dir=mkdtempSync(path.join(tmpdir(),'leveler-bridge-test-'));const binary=path.join(dir,'peer');writeFileSync(binary,`#!/bin/sh\nexec '${process.execPath}' '${fixture}'\n`);chmodSync(binary,0o700);return {dir,binary};}
test('JSONL transport correlates responses, routes events and retains delivery uncertainty',async()=>{
 const {dir,binary}=peer();const frames=[];const bridge=new bridgeModule.Bridge(binary,{onEvent:frame=>frames.push(frame)});
 try{assert.deepEqual(await Promise.all([bridge.request('one',{n:1}),bridge.request('two',{n:2})]),[{n:1},{n:2}]);assert.equal(frames.filter(f=>f.event==='runtime').length,2);await assert.rejects(bridge.request('reject'),error=>error.kind==='outcome_unknown');}finally{await bridge.close();rmSync(dir,{recursive:true,force:true});}
});
test('bridge exit rejects pending call instead of reporting task success',async()=>{
 const {dir,binary}=peer();const bridge=new bridgeModule.Bridge(binary);
 try{await assert.rejects(bridge.request('exit'),/disconnected/);await assert.rejects(bridge.request('one'),/disconnected/);}finally{await bridge.close();rmSync(dir,{recursive:true,force:true});}
});
