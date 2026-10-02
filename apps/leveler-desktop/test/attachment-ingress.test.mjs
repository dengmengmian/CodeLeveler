import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtemp,writeFile,truncate,rm,symlink} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {AttachmentIngress,readPickedUpload,MAX_UPLOAD_BYTES} from '../src/attachment-ingress.cjs';
import security from '../src/security.cjs';
const ref={id:'att',kind:'text_file',name:'note.txt',mime_type:'text/plain',size_bytes:5,sha256:'a'.repeat(64),width:null,height:null};
const frame=data=>({event:'runtime',session_id:'s1',data});
const resultFrame=(f,data,commandId=f.ingress.pending?.envelope.command_id)=>frame({...data,command_id:commandId});
function fixture(pickFile=async()=>'/chosen/note.txt',deliver=async()=>({})){const requests=[];const ingress=new AttachmentIngress({schedule:callback=>Promise.resolve().then(callback),cancelSchedule:()=>{},pickFile,readFile:async()=>({name:'note.txt',data_base64:'aGVsbG8='}),deliver:async envelope=>{requests.push(envelope);return deliver(envelope);}});ingress.activate({sourceId:'source',sessionId:'s1',vision:false});return {ingress,requests};}
function deadlineFixture(){
 const timers=new Map();const requests=[];let picks=0,serial=0;
 const ingress=new AttachmentIngress({pickFile:async()=>{picks++;return '/chosen/note.txt';},readFile:async()=>({name:'note.txt',data_base64:'aGVsbG8='}),deliver:async envelope=>{requests.push(envelope);},schedule:callback=>{const id=++serial;timers.set(id,callback);return id;},cancelSchedule:id=>timers.delete(id),deadlineMs:100});
 ingress.activate({sourceId:'source',sessionId:'s1',vision:false});
 return {ingress,timers,requests,picks:()=>picks,expire(){const [id,callback]=timers.entries().next().value;timers.delete(id);callback();}};
}
async function dispatched(f){for(let i=0;i<8;i++)await Promise.resolve();assert.equal(f.requests.length>0,true);}
test('missing result including legacy event reaches an owned deadline and retries the frozen import',async()=>{
 const f=deadlineFixture();const choosing=f.ingress.choose('s1');await dispatched(f);
 assert.equal(f.timers.size,1);await assert.rejects(f.ingress.choose('s1'));
 f.ingress.observe(frame({type:'attachment_added',attachment:ref}));assert.equal(f.timers.size,1);
 f.expire();const unknown=await choosing;assert.equal(unknown.status,'unknown');assert.equal(unknown.error.kind,'outcome_unknown');
 const retry=f.ingress.choose('s1');await dispatched(f);assert.equal(f.picks(),1);assert.deepEqual(f.requests[0],f.requests[1]);
 f.ingress.observe(resultFrame(f,{type:'attachment_added',attachment:ref}));assert.equal((await retry).status,'ready');assert.equal(f.timers.size,0);
});
test('session switch cleans deadline and late failure can settle a timed-out matching import',async()=>{
 const f=deadlineFixture();const choosing=f.ingress.choose('s1');await dispatched(f);f.ingress.activate({sourceId:'source',sessionId:'s2',vision:false});
 assert.equal(f.timers.size,0);assert.equal((await choosing).status,'failed');assert.equal(f.ingress.pending,null);
 f.ingress.activate({sourceId:'source',sessionId:'s1',vision:false});const next=f.ingress.choose('s1');await dispatched(f);f.expire();assert.equal((await next).status,'unknown');
 f.ingress.observe(resultFrame(f,{type:'attachment_processing_failed',error:'actual import failed'}));assert.equal(f.ingress.pending,null);assert.equal(f.timers.size,0);
});
test('discard is unknown-only and exact-owner; abandoning pending bytes makes the next add a new picker',async()=>{
 const f=deadlineFixture();const choosing=f.ingress.choose('s1');await dispatched(f);const id=f.requests[0].command_id;
 assert.throws(()=>f.ingress.discard('s1',id));assert.equal(f.timers.size,1);f.expire();await choosing;
 for(const [session,importId] of [['s2',id],['s1','foreign-id']])assert.throws(()=>f.ingress.discard(session,importId));
 assert.deepEqual(f.ingress.discard('s1',id),{ok:true});assert.equal(f.ingress.pending,null);
 const next=f.ingress.choose('s1');await dispatched(f);assert.equal(f.picks(),2);assert.notEqual(f.requests[1].command_id,id);
 f.ingress.observe(resultFrame(f,{type:'attachment_added',attachment:ref},id));assert.equal(f.ingress.authorize('s1',ref),false);
 f.expire();await next;
});
test('native upload ingress keeps bytes private and imports only bounded chosen immutable data',async()=>{
 const dir=await mkdtemp(path.join(tmpdir(),'leveler-upload-'));try{
 const file=path.join(dir,'note.txt');await writeFile(file,'hello');const result=await readPickedUpload(file);assert.equal(result.name,'note.txt');assert.equal(result.data_base64,'aGVsbG8=');assert.equal(result.path,undefined);
 const tooBig=path.join(dir,'huge');await writeFile(tooBig,'');await truncate(tooBig,MAX_UPLOAD_BYTES+1);await assert.rejects(readPickedUpload(tooBig));await assert.rejects(readPickedUpload(dir));
 const link=path.join(dir,'link');await symlink(file,link);await assert.rejects(readPickedUpload(link));
 }finally{await rm(dir,{recursive:true,force:true});}
});
test('cancel/stale/native-unselected session does not import; ordinary files do not require vision',async()=>{
 const cancelled=fixture(async()=>null);assert.equal((await cancelled.ingress.choose('s1')).status,'cancelled');assert.equal(cancelled.requests.length,0);
 const f=fixture();await assert.rejects(f.ingress.choose('other'));await f.ingress.choose('s1');assert.equal(f.requests[0].command.type,'add_attachment_data');assert.equal(f.requests[0].command.session_id,'s1');assert.equal(f.requests[0].command.path,undefined);
 await f.ingress.choose('s1');assert.deepEqual(f.requests[0],f.requests[1]);
 let resolve;const stale=fixture(()=>new Promise(r=>{resolve=r;}));const choosing=stale.ingress.choose('s1');stale.ingress.activate({sourceId:'other-source',sessionId:'s2',vision:true});resolve('/chosen/note.txt');await assert.rejects(choosing);assert.equal(stale.requests.length,0);
});
test('upload ACK is processing, only Runtime refs authorize submits, and refs cannot cross session/source',async()=>{
 const f=fixture();const queued=await f.ingress.choose('s1');assert.equal(queued.status,'unknown');assert.equal(f.ingress.authorize('s1',ref),false);
 f.ingress.observe(resultFrame(f,{type:'attachment_added',attachment:ref}));assert.equal(f.ingress.authorize('s1',ref),true);assert.equal(f.ingress.authorize('other',ref),false);assert.equal(f.ingress.authorize('s1',{...ref,sha256:'b'.repeat(64)}),false);
 const envelope={command_id:'id',session_id:'s1',issued_at:new Date().toISOString(),expected_version:null,command:{type:'submit_message',session_id:'s1',content:'hello',attachments:[ref]}};
 assert.throws(()=>security.validateEnvelope(envelope));
 const image={...ref,kind:'image',mime_type:'image/png',width:4,height:3};
 await f.ingress.choose('s1');f.ingress.observe(resultFrame(f,{type:'attachment_added',attachment:image}));
 assert.doesNotThrow(()=>security.validateEnvelope({...envelope,command:{...envelope.command,attachments:[image]}},(id,value)=>f.ingress.authorize(id,value)));
 assert.throws(()=>security.validateEnvelope({...envelope,command:{...envelope.command,attachments:[{...image,extra:true}]}},()=>true));
 f.ingress.activate({sourceId:'other-source',sessionId:'s1',vision:true});assert.equal(f.ingress.authorize('s1',ref),false);
});
test('uncertain upload retries frozen envelope rather than importing with new identity',async()=>{
 let calls=0;const f=fixture(async()=>'/chosen/note.txt',async()=>{if(calls++===0){const error=new Error('lost ACK');error.kind='outcome_unknown';throw error;}return {};});
 const unknown=await f.ingress.choose('s1');assert.equal(unknown.status,'unknown');assert.equal(unknown.error.kind,'outcome_unknown');await f.ingress.choose('s1');assert.equal(f.requests.length,2);assert.deepEqual(f.requests[0],f.requests[1]);
 f.ingress.observe(resultFrame(f,{type:'attachment_processing_failed',error:'decode failed'}));await f.ingress.choose('s1');assert.notEqual(f.requests[2].command_id,f.requests[0].command_id);
});
test('generic files upload truthfully but cannot silently disappear from model message input',async()=>{
 const f=fixture();await f.ingress.choose('s1');f.ingress.observe(resultFrame(f,{type:'attachment_added',attachment:ref}));
 const envelope={command_id:'id',session_id:'s1',issued_at:new Date().toISOString(),expected_version:null,command:{type:'submit_message',session_id:'s1',content:'read file',attachments:[ref]}};
 assert.throws(()=>security.validateEnvelope(envelope,(id,value)=>f.ingress.authorize(id,value)),/普通文件/);
});
test('late same-name upload success cannot complete a newer import after switching away and back',async()=>{
 const f=fixture();let reads=0;f.ingress.readFile=async()=>({name:'x.png',data_base64:++reads===1?'b2xk':'bmV3'});const old=await f.ingress.choose('s1');
 f.ingress.activate({sourceId:'source',sessionId:'s2',vision:false});
 f.ingress.activate({sourceId:'source',sessionId:'s1',vision:false});
 const newer=await f.ingress.choose('s1');assert.notEqual(newer.import_id,old.import_id);assert.notEqual(f.requests[0].command.data_base64,f.requests[1].command.data_base64);
 const oldRef={...ref,id:'old-import',name:'x.png',kind:'image',mime_type:'image/png',width:1,height:1,sha256:'b'.repeat(64)};
 f.ingress.observe(resultFrame(f,{type:'attachment_added',attachment:oldRef},old.import_id));
 assert.equal(f.ingress.authorize('s1',oldRef),false,'late old ref must not authorize the newer selected upload');
 assert.equal(f.ingress.pending?.envelope.command_id,newer.import_id);
 assert.equal(f.ingress.pending?.status,'unknown');
});
test('late upload failure cannot fail a newer import after switching away and back',async()=>{
 const f=fixture();const old=await f.ingress.choose('s1');
 f.ingress.activate({sourceId:'source',sessionId:'s2',vision:false});
 f.ingress.activate({sourceId:'source',sessionId:'s1',vision:false});
 const newer=await f.ingress.choose('s1');
 f.ingress.observe(resultFrame(f,{type:'attachment_processing_failed',error:'old decode failed'},old.import_id));
 assert.equal(f.ingress.pending?.envelope.command_id,newer.import_id,'late old failure must leave newer import pending');
 assert.equal(f.ingress.pending?.status,'unknown');
});
test('uncorrelated and wrong-id upload events never authorize a ref or consume pending import',async()=>{
 const f=fixture();const queued=await f.ingress.choose('s1');
 for(const commandId of [undefined,'foreign-command']){
  f.ingress.observe(frame({type:'attachment_added',attachment:ref,command_id:commandId}));
  f.ingress.observe(frame({type:'attachment_processing_failed',error:'foreign failure',command_id:commandId}));
  assert.equal(f.ingress.authorize('s1',ref),false);assert.equal(f.ingress.pending?.envelope.command_id,queued.import_id);
 }
 f.ingress.observe({...resultFrame(f,{type:'attachment_added',attachment:ref}),session_id:'other'});assert.equal(f.ingress.authorize('s1',ref),false);
 f.ingress.observe(resultFrame(f,{type:'attachment_added',attachment:ref}));assert.equal(f.ingress.authorize('s1',ref),true);
});
test('matching successful import before acknowledgement stays ready without filename correlation',async()=>{
 let f;f=fixture(async()=>'/chosen/note.txt',async envelope=>{f.ingress.observe(resultFrame(f,{type:'attachment_added',attachment:{...ref,name:'runtime-canonical.txt'}},envelope.command_id));return {};});
 const ready=await f.ingress.choose('s1');assert.equal(ready.status,'ready');assert.equal(ready.attachment.name,'runtime-canonical.txt');assert.equal(f.ingress.authorize('s1',ready.attachment),true);
});
