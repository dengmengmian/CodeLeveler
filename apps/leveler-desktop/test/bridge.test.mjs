import {test} from 'node:test';import assert from 'node:assert/strict';import {fileURLToPath} from 'node:url';import {mkdtempSync,writeFileSync,chmodSync,rmSync} from 'node:fs';import {tmpdir} from 'node:os';import path from 'node:path';import bridgeModule from '../src/bridge.cjs';
const fixture=fileURLToPath(new URL('./fixtures/bridge.cjs',import.meta.url));
const retryFixture=fileURLToPath(new URL('./fixtures/bridge-retry.cjs',import.meta.url));
// Bridge's production argv is fixed. Test peers are small executable wrappers.
function peer(file=fixture){const dir=mkdtempSync(path.join(tmpdir(),'leveler-bridge-test-'));const binary=path.join(dir,'peer');writeFileSync(binary,`#!/bin/sh\nexec '${process.execPath}' '${file}'\n`);chmodSync(binary,0o700);return {dir,binary};}
async function until(check,timeout=3000){const started=Date.now();for(;;){if(await check())return;if(Date.now()-started>timeout)throw new Error('condition not met in time');await new Promise(resolve=>setTimeout(resolve,20));}}
test('JSONL transport correlates responses, routes events and retains delivery uncertainty',async()=>{
 const {dir,binary}=peer();const frames=[];const bridge=new bridgeModule.Bridge(binary,{onEvent:frame=>frames.push(frame),retryDelaysMs:[]});
 try{assert.deepEqual(await Promise.all([bridge.request('one',{n:1}),bridge.request('two',{n:2})]),[{n:1},{n:2}]);assert.equal(frames.filter(f=>f.event==='runtime').length,2);await assert.rejects(bridge.request('reject'),error=>error.kind==='outcome_unknown');}finally{await bridge.close();rmSync(dir,{recursive:true,force:true});}
});
test('a lost bridge rejects the in-flight request as uncertain, never as success',async()=>{
 const {dir,binary}=peer();const bridge=new bridgeModule.Bridge(binary,{retryDelaysMs:[10,20]});
 try{const lost=bridge.request('exit');await assert.rejects(lost,error=>error.kind==='outcome_unknown');}finally{await bridge.close();rmSync(dir,{recursive:true,force:true});}
});
test('the bridge respawns after the child exits and serves later requests',async()=>{
 const {dir,binary}=peer(retryFixture);const states=[];const bridge=new bridgeModule.Bridge(binary,{onState:state=>states.push(state),retryDelaysMs:[10,20]});
 try{
   const first=bridge.request('exit');await assert.rejects(first,error=>error.kind==='outcome_unknown');
   await until(()=>states.some(state=>state.state==='reconnecting'));
   let result=null;await until(async()=>{try{result=await bridge.request('one',{n:1});return true;}catch{return false;}});
   assert.equal(result.params.n,1);
   assert.equal(states.filter(state=>state.state==='reconnecting').length>=1,true);
 }finally{await bridge.close();rmSync(dir,{recursive:true,force:true});}
});
test('a request made while the child is gone is definitely not sent',async()=>{
 const {dir,binary}=peer(retryFixture);const bridge=new bridgeModule.Bridge(binary,{retryDelaysMs:[100]});
 try{
   await assert.rejects(bridge.request('exit'),error=>error.kind==='outcome_unknown');
   await assert.rejects(bridge.request('one'),error=>error.kind==='not_sent');
 }finally{await bridge.close();rmSync(dir,{recursive:true,force:true});}
});
test('closing stops respawn and rejects later calls as closed',async()=>{
 const {dir,binary}=peer(retryFixture);const states=[];const bridge=new bridgeModule.Bridge(binary,{onState:state=>states.push(state),retryDelaysMs:[10]});
 try{
   await bridge.close();
   await assert.rejects(bridge.request('one'),error=>error.kind==='closed');
   await new Promise(resolve=>setTimeout(resolve,60));
   assert.equal(states.some(state=>state.state==='reconnecting'&&state.attempt>0),false);
 }finally{rmSync(dir,{recursive:true,force:true});}
});
