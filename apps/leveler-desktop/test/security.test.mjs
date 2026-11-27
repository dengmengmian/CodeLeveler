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
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'set_permission_profile',session_id:'s1',mode:'assisted',contract:'v2'})));
 for(const contract of ['v1','V2','',null,2])assert.throws(()=>security.validateEnvelope(envelope({type:'set_permission_profile',session_id:'s1',mode:'assisted',contract})));
});
test('desktop rejects cross-session submission but accepts scoped text and live approvals',()=>{
 assert.throws(()=>security.validateEnvelope(envelope({type:'submit_message',session_id:'s2',content:'hi'})));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'submit_message',session_id:'s1',content:'hi'})));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'approval_decision',request_id:'a1',decision:'approve_once'})));
});
test('secret submissions validate captures, acknowledgement and session',()=>{
 const capture=(over={})=>({placeholder:'{{s}}',value:'sk-test-value',...over});
 const submit=(over={})=>({type:'submit_user_input',session_id:'s1',content:'use {{s}}',attachments:[],secret_captures:[capture()],plaintext_secret_ack:false,...over});
 assert.doesNotThrow(()=>security.validateEnvelope(envelope(submit())));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope(submit({content:'plain sk-test-value',secret_captures:[],plaintext_secret_ack:true}))));
 for(const bad of [
   submit({plaintext_secret_ack:true}),
   submit({content:'no placeholder'}),
   submit({content:'use {{s}} {{s}}'}),
   submit({secret_captures:[capture(),capture()]}),
   submit({secret_captures:[capture({placeholder:''})]}),
   submit({secret_captures:[capture({value:''})]}),
   submit({secret_captures:[capture({value:'a'.repeat(65537)})]}),
   submit({secret_captures:[capture({extra:1})]}),
   submit({session_id:'s2'}),
   submit({content:'a'.repeat(100001),secret_captures:[],plaintext_secret_ack:false}),
   submit({content:'',attachments:[],secret_captures:[],plaintext_secret_ack:false}),
 ])assert.throws(()=>security.validateEnvelope(envelope(bad)));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'steer_user_input',session_id:'s1',content:'use {{s}}',secret_captures:[capture()],plaintext_secret_ack:false})));
 assert.throws(()=>security.validateEnvelope(envelope({type:'steer_user_input',session_id:'s1',content:'',secret_captures:[],plaintext_secret_ack:false})));
 assert.throws(()=>security.validateEnvelope(envelope({type:'steer_user_input',session_id:'s2',content:'x',secret_captures:[],plaintext_secret_ack:false})));
 assert.throws(()=>security.validateEnvelope(envelope({type:'steer_user_input',session_id:'s1',content:'x',secret_captures:[],plaintext_secret_ack:false,attachments:[]})));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'capture_secret',session_id:'s1',request_id:'r1',value:'sk-test-value'})));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'capture_secret',session_id:'s1',request_id:'r1',value:'sk-test-value',key_hint:'API_KEY'})));
 for(const bad of [
   {type:'capture_secret',session_id:'s1',request_id:'r1',value:''},
   {type:'capture_secret',session_id:'s1',request_id:'r1',value:'a'.repeat(65537)},
   {type:'capture_secret',session_id:'s1',request_id:'',value:'x'},
   {type:'capture_secret',session_id:'s2',request_id:'r1',value:'x'},
   {type:'capture_secret',session_id:'s1',request_id:'r1',value:'x',extra:1},
 ])assert.throws(()=>security.validateEnvelope(envelope(bad)));
 const ref=`sec_${'a'.repeat(32)}`;
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'revoke_secret',session_id:'s1',secret_ref:ref})));
 for(const bad of [
   {type:'revoke_secret',session_id:'s1',secret_ref:'sec_short'},
   {type:'revoke_secret',session_id:'s1',secret_ref:`sec_${'A'.repeat(32)}`},
   {type:'revoke_secret',session_id:'s2',secret_ref:ref},
   {type:'revoke_secret',session_id:'s1',secret_ref:ref,extra:1},
 ])assert.throws(()=>security.validateEnvelope(envelope(bad)));
});
test('definition trust decisions are scoped, enumerated, and exact',()=>{
 for(const decision of ['allow_once','trust_current_version','deny'])assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'definition_trust_decision',request_id:'d1',decision,session_id:'s1'})));
 for(const session_id of ['s2','',null,undefined,' '])assert.throws(()=>security.validateEnvelope(envelope({type:'definition_trust_decision',request_id:'d1',decision:'allow_once',session_id})));
 for(const decision of ['allow','trust','deny_all','ALLOW_ONCE','',null,undefined,{}])assert.throws(()=>security.validateEnvelope(envelope({type:'definition_trust_decision',request_id:'d1',decision,session_id:'s1'})));
 for(const request_id of ['',null,undefined,'   ','a'.repeat(257)])assert.throws(()=>security.validateEnvelope(envelope({type:'definition_trust_decision',request_id,decision:'deny',session_id:'s1'})));
 assert.throws(()=>security.validateEnvelope(envelope({type:'definition_trust_decision',request_id:'d1',decision:'deny',session_id:'s1',content:'x'})));
});
test('desktop accepts current-session model, title, and archive commands using existing contracts',()=>{
 for(const command of [{type:'select_model',session_id:'s1',model:{provider:'openai',model:'gpt-4o'}},{type:'rename_session',session_id:'s1',name:'新名称'},{type:'archive_session',session_id:'s1'},{type:'unarchive_session',session_id:'s1'}])assert.doesNotThrow(()=>security.validateEnvelope(envelope(command)));
});
test('model, rename, archive commands reject wrong session, missing fields, and unknown authority',()=>{
 const commands=[{type:'select_model',session_id:'s1',model:{provider:'openai',model:'gpt-4o'}},{type:'rename_session',session_id:'s1',name:'新名称'},{type:'archive_session',session_id:'s1'},{type:'unarchive_session',session_id:'s1'}];
 for(const command of commands){
  for(const session_id of ['s2','',null,undefined])assert.throws(()=>security.validateEnvelope(envelope({...command,session_id})));
  assert.throws(()=>security.validateEnvelope(envelope({...command,permission:'full'})));
 }
 for(const model of [null,{}, {provider:'',model:'m'},{provider:'p',model:''},{provider:'p',model:'m',token:'secret'},{provider:' ',model:'m'},{provider:'p',model:'a'.repeat(257)},'p/m'])assert.throws(()=>security.validateEnvelope(envelope({type:'select_model',session_id:'s1',model})));
 for(const name of ['',null,undefined,'   ','a'.repeat(257)])assert.throws(()=>security.validateEnvelope(envelope({type:'rename_session',session_id:'s1',name})));
 for(const type of ['delete_session','set_default_model','set_permission_profile'])assert.throws(()=>security.validateEnvelope(envelope({type,session_id:'s1'})));
});
test('new session commands cannot target blank session identities',()=>{
 for(const type of ['select_model','rename_session','archive_session','unarchive_session']){
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

test('recovery, recap, side question and diagnostics stay bound to the selected session',()=>{
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'btw',session_id:'s1',question:'为什么？'})));
 for(const bad of [{type:'btw',session_id:'s2',question:'x'},{type:'btw',session_id:'s1',question:''},{type:'btw',session_id:'s1',question:'x',path:'/etc/passwd'}])assert.throws(()=>security.validateEnvelope(envelope(bad)));
 for(const type of ['cancel_btw','recap','acknowledge_crash_window'])assert.doesNotThrow(()=>security.validateEnvelope(envelope({type,session_id:'s1'})));
 for(const type of ['cancel_btw','recap','acknowledge_crash_window'])assert.throws(()=>security.validateEnvelope(envelope({type,session_id:'s2'})));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'restore_checkpoint',session_id:'s1',checkpoint_id:'chk-1'})));
 for(const bad of [{type:'restore_checkpoint',session_id:'s1',checkpoint_id:''},{type:'restore_checkpoint',session_id:'s2',checkpoint_id:'chk-1'},{type:'restore_checkpoint',session_id:'s1',checkpoint_id:'a'.repeat(257)},{type:'restore_checkpoint',session_id:'s1',checkpoint_id:'chk-1',path:'/tmp'}])assert.throws(()=>security.validateEnvelope(envelope(bad)));
 const query={type:'query_diagnostics',requester_session_id:'s1',query_id:'q1',locale:'zh'};
 assert.doesNotThrow(()=>security.validateEnvelope(envelope(query)));
 const {locale,...withoutLocale}=query;
 assert.doesNotThrow(()=>security.validateEnvelope(envelope(withoutLocale)));
 for(const bad of [{...query,requester_session_id:'s2'},{...query,query_id:''},{...query,locale:'fr'},{...query,path:'/tmp'}])assert.throws(()=>security.validateEnvelope(envelope(bad)));
});

test('tool output queries carry only a call id, a bounded part and a byte window',()=>{
 const base={type:'query_tool_output',session_id:'s1',query_id:'q1',call_id:'call_abc-123',offset:0,max_bytes:65536};
 assert.doesNotThrow(()=>security.validateEnvelope(envelope(base)));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({...base,index:0,offset:1024,max_bytes:262144})));
 for(const bad of [
   {...base,session_id:'s2'},
   {...base,call_id:''},
   {...base,call_id:'a'.repeat(257)},
   {...base,call_id:'../../etc/passwd'},
   {...base,call_id:'a b'},
   {...base,offset:-1},
   {...base,offset:1.5},
   {...base,offset:1073741825},
   {...base,max_bytes:0},
   {...base,max_bytes:262145},
   {...base,max_bytes:'65536'},
   {...base,index:-1},
   {...base,index:1025},
   {...base,index:1.5},
   {...base,path:'/etc/passwd'},
   {...base,query_id:null},
 ])assert.throws(()=>security.validateEnvelope(envelope(bad)));
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
 for(const type of ['create_agent','update_agent','delete_agent'])assert.throws(()=>security.validateEnvelope(envelope({type,session_id:'s1',query_id:'q'})));
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

test('background task cancellation is bounded to the selected session and exact task',()=>{
 const command={type:'cancel_background_task',session_id:'s1',task_id:'host-1'};
 assert.doesNotThrow(()=>security.validateEnvelope(envelope(command)));
 for(const fields of [{session_id:'s2'},{session_id:''},{session_id:' '},{task_id:''},{task_id:' '},{task_id:null},{task_id:42},{pid:1},{signal:'SIGKILL'},{expected_version:1}])assert.throws(()=>security.validateEnvelope(envelope({...command,...fields})));
});

test('fork accepts only the selected session and carries no versioned authority',()=>{
 const fork={type:'fork_session',session_id:'s1'};
 assert.doesNotThrow(()=>security.validateEnvelope(envelope(fork)));
 for(const fields of [{session_id:'s2'},{session_id:''},{session_id:' '},{session_id:null},{name:'copy'},{expected_version:0}])assert.throws(()=>security.validateEnvelope(envelope({...fork,...fields})));
});

test('compaction is a selected-session command with no versioned authority',()=>{
 const command={type:'compact_context',session_id:'s1'};
 assert.doesNotThrow(()=>security.validateEnvelope(envelope(command)));
 for(const fields of [{session_id:'s2'},{session_id:' '},{content:'x'},{expected_version:1}])assert.throws(()=>security.validateEnvelope(envelope({...command,...fields})));
});

test('scheduled automation is session-scoped, bounded and forwarded as a structured request',()=>{
 const valid={type:'manage_schedule',requester_session_id:'s1',query_id:'q1',request:{action:'list',filter:'all'}};
 assert.doesNotThrow(()=>security.validateEnvelope(envelope(valid)));
 for(const fields of [{requester_session_id:'s2'},{requester_session_id:''},{requester_session_id:' '},{query_id:''},{query_id:'a'.repeat(257)},{request:null},{request:[]},{request:'list'},{request:{}},{request:{action:'a'.repeat(33)}},{request:{action:'list',extra:'x'.repeat(200000)}},{path:'/private'},{expected_version:7}])assert.throws(()=>security.validateEnvelope(envelope({...valid,...fields})));
});

test('MCP management is session-scoped, exact and carries only environment names',()=>{
 const stdio={name:'echo',transport:'stdio',command:'/usr/bin/python3',args:['-u','/tmp/echo.py'],env:[{key:'API_KEY',source:'API_KEY'}],enabled:true};
 const http={name:'remote',transport:'streamable_http',url:'https://example.com/mcp',headers:[{key:'X-API-Key',source:'SERVICE_KEY'}],bearer_token_env:'SERVICE_TOKEN',enabled:false};
 const request=r=>({type:'manage_mcp',requester_session_id:'s1',query_id:'q1',request:r});
 for(const valid of [
   {action:'list'},{action:'reload'},{action:'reconnect',server:'echo'},{action:'remove',server:'echo'},
   {action:'set_enabled',server:'echo',enabled:false},{action:'add',spec:stdio},{action:'update',original_name:'echo',spec:http},
 ])assert.doesNotThrow(()=>security.validateEnvelope(envelope(request(valid))));
 for(const bad of [
   request({action:'delete'}),
   request({action:'list',extra:1}),
   request({action:'reconnect',server:'../escape'}),
   request({action:'set_enabled',server:'echo',enabled:'yes'}),
   request({action:'add',spec:{...stdio,url:'https://example.com/mcp'}}),
   request({action:'add',spec:{...stdio,command:'   '}}),
   request({action:'add',spec:{...stdio,env:[{key:'API_KEY',source:'sk-live-secret'}]}}), // shape-valid; the runtime refuses the value
   request({action:'add',spec:{...http,command:'npx'}}),
   request({action:'add',spec:{...http,url:'ftp://example.com/mcp'}}),
   request({action:'add',spec:{...http,url:'https://user:pass@example.com/mcp'}}),
   request({action:'add',spec:{...http,url:'https://example.com/mcp?token=abc'}}),
   request({action:'add',spec:{...stdio,token:'secret'}}),
   {type:'manage_mcp',requester_session_id:'s2',query_id:'q1',request:{action:'list'}},
   {...request({action:'list'}),path:'/private'},
   {...request({action:'list'}),expected_version:7},
 ])assert.throws(()=>security.validateEnvelope(envelope(bad)), undefined, JSON.stringify(bad));
});

test('desktop mode switching accepts only the three exact current-session axes',()=>{
 const command=(over={})=>({type:'set_product_axes',session_id:'s1',work_profile:'single',collaboration:'goal',...over});
 for(const collaboration of ['chat','plan','goal'])assert.doesNotThrow(()=>security.validateEnvelope(envelope(command({collaboration}))));
 for(const collaboration of ['Chat','Goal','auto','',null,undefined,'orchestrate',{}])assert.throws(()=>security.validateEnvelope(envelope(command({collaboration}))));
 for(const session_id of ['s2','',null,undefined,' '])assert.throws(()=>security.validateEnvelope(envelope(command({session_id}))));
 for(const work_profile of ['',null,undefined,'x'.repeat(65)])assert.throws(()=>security.validateEnvelope(envelope(command({work_profile}))));
 for(const extra of [{mode:'assisted'},{expected_version:1},{permission:'full'}])assert.throws(()=>security.validateEnvelope(envelope({...command(),...extra})));
});

test('Skill reads are session-bound, exact and bounded',()=>{
 const list=over=>({type:'list_skills',session_id:'s1',query_id:'q1',...over});
 assert.doesNotThrow(()=>security.validateEnvelope(envelope(list())));
 for(const over of [{session_id:'s2'},{session_id:''},{query_id:''},{query_id:' '},{query_id:'a'.repeat(257)},{extra:1},{name:'deploy'}])assert.throws(()=>security.validateEnvelope(envelope(list(over))),undefined,JSON.stringify(over));
 const get=over=>({type:'get_skill',session_id:'s1',name:'deploy',query_id:'q1',...over});
 for(const name of ['deploy','pdf2','my_skill','a.b-c_1'])assert.doesNotThrow(()=>security.validateEnvelope(envelope(get({name}))));
 for(const over of [{name:''},{name:null},{name:'.hidden'},{name:'../escape'},{name:'a/b'},{name:'a b'},{name:'a'.repeat(65)},{session_id:'s2'},{query_id:''},{extra:1}])assert.throws(()=>security.validateEnvelope(envelope(get(over))),undefined,JSON.stringify(over));
});

test('Review requests name one selected-session object and nothing else',()=>{
 const run=over=>({type:'run_review',session_id:'s1',focus:'检查并发',scope:{kind:'workspace'},...over});
 for(const scope of [
   {kind:'auto'},{kind:'current_task'},{kind:'workspace'},{kind:'project'},{kind:'session'},
   {kind:'commit',rev:'HEAD~1'},{kind:'branch',base:'origin/main'},{kind:'paths',paths:['src/lib.rs']},
 ])assert.doesNotThrow(()=>security.validateEnvelope(envelope(run({scope}))),undefined,JSON.stringify(scope));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope(run({model:{provider:'p',model:'m'}}))));
 // Another stored session is not an object this client can name.
 assert.throws(()=>security.validateEnvelope(envelope(run({scope:{kind:'session',session_id:'s2'}}))));
 for(const over of [
   {session_id:'s2'},{focus:null},{focus:'中'.repeat(3000)},
   {scope:{kind:'nope'}},{scope:null},{scope:{kind:'workspace',extra:1}},
   {scope:{kind:'commit',rev:'--upload-pack=x'}},{scope:{kind:'commit',rev:''}},
   {scope:{kind:'branch',base:'-x'}},
   {scope:{kind:'paths',paths:[]}},{scope:{kind:'paths',paths:['../etc/passwd']}},
   {scope:{kind:'paths',paths:['/etc/passwd']}},{scope:{kind:'paths',paths:['a','b','c','d','e','f','g','h','i','j','k','l','m','n','o','p','q','r','s','t','u','v','w','x','y','z','1','2','3','4','5','6','7','8','9','10','11','12','13','14','15','16','17','18','19','20','21','22','23','24','25']}},
   {model:{provider:'',model:'m'}},{model:{provider:'p',model:'',}},{model:{provider:'p',model:'m',extra:1}},
   {unexpected:true},
 ])assert.throws(()=>security.validateEnvelope(envelope(run(over))),undefined,JSON.stringify(over));
});

test('Review queries and cancellation are session-bound and id-shaped',()=>{
 const review='review-01234567-89ab-cdef-0123-456789abcdef';
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'cancel_review',session_id:'s1',review_id:review})));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'query_review_report',session_id:'s1',review_id:review,query_id:'q1'})));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'list_reviews',session_id:'s1',query_id:'q1'})));
 assert.doesNotThrow(()=>security.validateEnvelope(envelope({type:'query_review_scopes',session_id:'s1',query_id:'q1'})));
 for(const bad of [
   {type:'cancel_review',session_id:'s2',review_id:review},
   {type:'cancel_review',session_id:'s1',review_id:'attack'},
   {type:'cancel_review',session_id:'s1',review_id:''},
   {type:'cancel_review',session_id:'s1',review_id:`review-${'x'.repeat(200)}`},
   {type:'query_review_report',session_id:'s1',review_id:review,query_id:''},
   {type:'query_review_report',session_id:'s1',review_id:'review-',query_id:'q1'},
   {type:'query_review_report',session_id:'s2',review_id:review,query_id:'q1'},
   {type:'list_reviews',session_id:'s1'},
   {type:'list_reviews',session_id:'s2',query_id:'q1'},
   {type:'list_reviews',session_id:'s1',query_id:'q1',review_id:review},
   {type:'query_review_scopes',session_id:'s1',query_id:'q1',scope:{kind:'workspace'}},
 ])assert.throws(()=>security.validateEnvelope(envelope(bad)),undefined,JSON.stringify(bad));
});
