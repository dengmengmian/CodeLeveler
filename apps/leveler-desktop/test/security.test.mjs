import {SNAPSHOT_VERSIONED_COMMANDS} from '../src/command-policy.gen.mjs';
import {test} from 'node:test';import assert from 'node:assert/strict';import security from '../src/security.cjs';
const envelope=command=>({command_id:'id',session_id:'s1',expected_version:SNAPSHOT_VERSIONED_COMMANDS.includes(command.type)?7:null,issued_at:new Date().toISOString(),command});
test('desktop IPC cannot shut down runtime or issue incomplete permission commands',()=>{
 for(const type of ['quit','shutdown_when_idle','force_retire','set_permission_profile','run_goal','add_attachment'])assert.throws(()=>security.validateEnvelope(envelope({type})));
});
test('desktop permission menu accepts only three exact current-session modes',()=>{
 for(const mode of ['full_access','assisted','request_approval'])assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'set_permission_profile',session_id:'s1',mode})));
 for(const mode of ['full','auto','restricted','auto_approve','FullAccess','',null,undefined,{},true])assert.throws(()=>security.validateEnvelope(envelope({type:'set_permission_profile',session_id:'s1',mode})));
 for(const session_id of ['s2','',null,undefined,' '])assert.throws(()=>security.validateEnvelope(envelope({type:'set_permission_profile',session_id,mode:'full_access'})));
 for(const extra of [{approval_policy:'auto_approve'},{network_scope:'all'},{resource_grants:[]},{sandbox:false}])assert.throws(()=>security.validateEnvelope(envelope({type:'set_permission_profile',session_id:'s1',mode:'full_access',...extra})));
});
test('desktop rejects cross-session submission but accepts scoped text and live approvals',()=>{
 assert.throws(()=>security.validateEnvelope(envelope({type:'submit_message',session_id:'s2',content:'hi'})));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'submit_message',session_id:'s1',content:'hi'})));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'approval_decision',request_id:'a1',decision:'approve_once'})));
});
test('desktop accepts current-session model, title, and archive commands using existing contracts',()=>{
 for(const command of [{type:'select_model',session_id:'s1',model:{provider:'openai',model:'gpt-4o'}},{type:'rename_session',session_id:'s1',name:'新名称'},{type:'archive_session',session_id:'s1'}])assert.doesNotThrow(()=>security.validateEnvelope(envelope(command)));
});
test('model, rename, archive commands reject wrong session, missing fields, and unknown authority',()=>{
 const commands=[{type:'select_model',session_id:'s1',model:{provider:'openai',model:'gpt-4o'}},{type:'rename_session',session_id:'s1',name:'新名称'},{type:'archive_session',session_id:'s1'}];
 for(const command of commands){
  for(const session_id of ['s2','',null,undefined])assert.throws(()=>security.validateEnvelope(envelope({...command,session_id})));
  assert.throws(()=>security.validateEnvelope(envelope({...command,permission:'full'})));
 }
 for(const model of [null,{}, {provider:'',model:'m'},{provider:'p',model:''},{provider:'p',model:'m',token:'secret'},{provider:' ',model:'m'},{provider:'p',model:'a'.repeat(257)},'p/m'])assert.throws(()=>security.validateEnvelope(envelope({type:'select_model',session_id:'s1',model})));
 for(const name of ['',null,undefined,'   ','a'.repeat(257)])assert.throws(()=>security.validateEnvelope(envelope({type:'rename_session',session_id:'s1',name})));
 for(const type of ['delete_session','set_default_model','set_permission_profile'])assert.throws(()=>security.validateEnvelope(envelope({type,session_id:'s1'})));
});
test('new session commands cannot target blank session identities',()=>{
 for(const type of ['select_model','rename_session','archive_session']){
  const command={type,session_id:'   ',...(type==='select_model'?{model:{provider:'p',model:'m'}}:type==='rename_session'?{name:'title'}:{})};
  assert.throws(()=>security.validateEnvelope({...envelope(command),session_id:'   '}));
 }
});
test('image-only input is supported while unknown submit authority stays rejected',()=>{
 const image={id:'image',kind:'image',name:'image.png',mime_type:'image/png',size_bytes:20,sha256:'a'.repeat(64),width:4,height:3};
 const command={type:'submit_message',session_id:'s1',content:'',attachments:[image]};
 assert.doesNotThrow(()=>security.validateEnvelope(envelope(command),()=>true));
 assert.throws(()=>security.validateEnvelope(envelope({...command,read_path:'/etc/passwd'}),()=>true));
});

test('diff query accepts only selected-session optional bounded query identity',()=>{
 for(const fields of [{},{query_id:'query-1'}])assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'request_diff',session_id:'s1',...fields})));
 for(const fields of [{session_id:'s2'},{session_id:''},{query_id:''},{query_id:' '},{query_id:null},{query_id:42},{query_id:'a'.repeat(257)},{path:'/private'},{include_all:true}])assert.throws(()=>security.validateEnvelope(envelope({type:'request_diff',session_id:'s1',...fields})));
});

test('existing settings reads require exact scoped correlated query fields',()=>{
 const simple=['list_agents','query_context'];
 const queries=[...simple.map(type=>({type,session_id:'s1',query_id:'q1'})),{type:'get_agent',session_id:'s1',query_id:'q1',name:'reviewer'},{type:'query_observability',session_id:'s1',query_id:'q1',center_seq:null,before:0,after:100}];
 for(const command of queries){
  assert.doesNotThrow(()=>security.validateEnvelope(envelope(command)));
  for(const fields of [{session_id:'s2'},{session_id:' '},{query_id:undefined},{query_id:null},{query_id:''},{query_id:' '},{query_id:'a'.repeat(257)},{path:'/private'},{mutate:true}])assert.throws(()=>security.validateEnvelope(envelope({...command,...fields})));
 }
 for(const name of ['default','explorer','worker','reviewer','agent-1','a'.repeat(64)])assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'get_agent',session_id:'s1',query_id:'q',name})));
 for(const name of ['',null,'Upper','../agent','a-','con','lpt1','a'.repeat(65)])assert.throws(()=>security.validateEnvelope(envelope({type:'get_agent',session_id:'s1',query_id:'q',name})));
 const command=queries.at(-1);
 for(const fields of [{center_seq:-1},{center_seq:0.5},{center_seq:Number.MAX_SAFE_INTEGER+1},{center_seq:'2'},{before:-1},{after:101},{before:0.5},{after:null},{limit:1}])assert.throws(()=>security.validateEnvelope(envelope({...command,...fields})));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({...command,center_seq:Number.MAX_SAFE_INTEGER,before:100,after:0})));
 for(const type of ['create_agent','update_agent','delete_agent','list_skills'])assert.throws(()=>security.validateEnvelope(envelope({type,session_id:'s1',query_id:'q'})));
});

test('existing memory list is a strict selected-session correlated read, not a mutation',()=>{
 for(const include_archived of [false,true])assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'list_memory',session_id:'s1',query_id:'q',include_archived})));
 const command={type:'list_memory',session_id:'s1',query_id:'q',include_archived:false};
 for(const fields of [{session_id:'other'},{query_id:null},{query_id:''},{include_archived:null},{include_archived:'true'},{include_archived:undefined},{path:'/private'},{forget:true}])assert.throws(()=>security.validateEnvelope(envelope({...command,...fields})));
 for(const type of ['accept_memory','reject_memory','forget_memory','remember_memory'])assert.throws(()=>security.validateEnvelope(envelope({...command,type})));
});

test('settings require a safe observed version while merge commands keep null',()=>{
 const rename={type:'rename_session',session_id:'s1',name:'observed'};
 for(const version of [null,undefined,-1,0.5,'7',Number.MAX_SAFE_INTEGER+1])assert.throws(()=>security.validateEnvelope({...envelope(rename),expected_version:version}));
 for(const version of [0,7])assert.doesNotThrow(()=>security.validateEnvelope({...envelope(rename),expected_version:version}));
 assert.throws(()=>security.validateEnvelope({...envelope({type:'submit_message',session_id:'s1',content:'hello'}),expected_version:7}));
});

test('existing task cancellation and continuation stay bounded to the selected session',()=>{
 const commands=[{type:'cancel_task',session_id:'s1'},{type:'resume_task',session_id:'s1',content:'继续'}];
 for(const command of commands){
  assert.doesNotThrow(()=>security.validateEnvelope(envelope(command)));
  for(const fields of [{session_id:'s2'},{session_id:''},{session_id:null},{permission:'full'}])assert.throws(()=>security.validateEnvelope(envelope({...command,...fields})));
 }
 for(const content of ['',null,42,'a'.repeat(100001)])assert.throws(()=>security.validateEnvelope(envelope({type:'resume_task',session_id:'s1',content})));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'resume_task',session_id:'s1',content:'a'.repeat(100000)})));
});
