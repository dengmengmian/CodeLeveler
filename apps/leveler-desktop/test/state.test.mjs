import { test } from 'node:test';
import assert from 'node:assert/strict';
import { projectSnapshot, applyEvent, commandEnvelope } from '../src/state.mjs';
const snapshot = { id:'session-1', messages:[{id:'u1',role:'user',text:'hello'}], task_status:'running', pending_interactions:[], active_tools:[] };
test('snapshot restores transcript and only live waiter approvals', () => {
  const state = projectSnapshot({...snapshot,pending_interactions:[{type:'approval',request:{id:'a1',summary:'run'}}]});
  assert.equal(state.messages[0].text,'hello');
  assert.deepEqual(state.approvals.map(a=>a.id),['a1']);
  const restored = projectSnapshot(snapshot, [{event:{type:'approval_requested',request:{id:'stale'}}}]);
  assert.deepEqual(restored.approvals,[]);
});
test('assistant stream does not invent completion or duplicate canonical messages', () => {
  let state = projectSnapshot(snapshot);
  state = applyEvent(state,{type:'assistant_message_started',message_id:'a1'});
  state = applyEvent(state,{type:'assistant_text_delta',message_id:'a1',delta:'Hello'});
  state = applyEvent(state,{type:'assistant_message_completed',message_id:'a1'});
  assert.equal(state.messages.at(-1).text,'Hello');
  assert.equal(state.status,'running');
  state = projectSnapshot({...snapshot,messages:[...snapshot.messages,{id:'a1',role:'assistant',text:'Hello'}],task_status:'answered'});
  assert.equal(state.messages.length,2);
  assert.equal(state.status,'answered');
});
test('tool evidence survives history replay but past approval cannot be clicked', () => {
  const state = projectSnapshot(snapshot,[{event:{type:'tool_call_started',id:'t1',name:'read_file',arguments:'{}'}},{event:{type:'tool_call_completed',id:'t1',ok:true,preview:'file contents'}}]);
  assert.equal(state.tools[0].preview,'file contents');
  assert.equal(state.tools[0].status,'success');
});
test('runtime resolves waiter and envelope carries one stable idempotency id', () => {
  let state=projectSnapshot(snapshot);
  state=applyEvent(state,{type:'approval_requested',request:{id:'a1'}});
  state=applyEvent(state,{type:'approval_resolved',id:'a1'});
  assert.deepEqual(state.approvals,[]);
  const envelope=commandEnvelope('session-1',{type:'submit_message',session_id:'session-1',content:'hi'});
  assert.equal(envelope.session_id,'session-1');
  assert.equal(envelope.expected_version,null);
  assert.ok(envelope.command_id);
  assert.ok(Date.parse(envelope.issued_at));
});
import { approvalDecisions, clarificationAnswer } from '../src/state.mjs';
test('human consent never offers standing approvals',()=>{
 assert.deepEqual(approvalDecisions({requires_human_consent:true,always_persists:true}),['approve_once','deny']);
});
test('clarifications honor single/multi/text bounds and labels',()=>{
 assert.throws(()=>clarificationAnswer({kind:'single',options:[]},{picks:[],text:'fake'}));
 assert.throws(()=>clarificationAnswer({kind:'multi',options:['A','B'],min_choices:1,max_choices:1},{picks:['A','B']}));
 assert.equal(clarificationAnswer({kind:'multi',options:['A','B'],min_choices:1},{picks:['B']}),'B');
 assert.equal(clarificationAnswer({kind:'text'},{text:'hello'}),'hello');
});
import { applySessionMetadata } from '../src/state.mjs';
test('metadata-only session update preserves runtime live transcript and tool evidence',()=>{
 let state=projectSnapshot(snapshot);
 state=applyEvent(state,{type:'user_message_added',message:{id:'live-user',role:'user',text:'accepted live input'}});
 state=applyEvent(state,{type:'tool_call_started',id:'live-tool',name:'read_file',arguments:'{}'});
 const next=applySessionMetadata(state,{...snapshot,messages:[],goal:'retitled',task_status:'running'});
 assert.equal(next.messages.at(-1).text,'accepted live input');
 assert.equal(next.tools[0].id,'live-tool');
 assert.equal(next.session.goal,'retitled');
 assert.equal(next.status,'running');
});
test('reasoning never becomes transcript or exposed state (Contract v1 §I5)',()=>{
 let state=projectSnapshot({id:'s1',messages:[],status:'running'});
 state=applyEvent(state,{type:'reasoning_delta',delta:'Provider reasoning'});
 // Raw reasoning is not carried at all: it is neither a message nor view state
 // the renderer could paint.
 assert.equal(state.messages.length,0);
 assert.equal('reasoningText' in state,false);
 state=applyEvent(state,{type:'assistant_attempt_reset',message_id:null});
 assert.equal(state.messages.length,0);
});
test('plan updates and metadata come only from runtime projections',()=>{
 let state=projectSnapshot({...snapshot,plan:{steps:[{index:0,description:'Read',status:'pending'}]}});
 assert.equal(state.plan.steps[0].status,'pending');
 state=applyEvent(state,{type:'plan_updated',plan:{steps:[{index:0,description:'Read',status:'done'}]}});
 assert.equal(state.plan.steps[0].status,'done');
 assert.equal(applySessionMetadata(state,{...snapshot,plan:null}).plan,null);
});
test('history tools retain their message boundary across multiple turns',()=>{
 const state=projectSnapshot({...snapshot,messages:[...snapshot.messages,{id:'a1',role:'assistant',text:'first'},{id:'u2',role:'user',text:'second'},{id:'a2',role:'assistant',text:'second answer'}]},[
 {event:{type:'user_message_added',message:{id:'u1'}}},
 {event:{type:'tool_call_started',id:'t1',name:'read_file'}},
 {event:{type:'assistant_message_started',message_id:'a1'}},
 {event:{type:'user_message_added',message:{id:'u2'}}},
 {event:{type:'tool_call_started',id:'t2',name:'bash'}}]);
 assert.deepEqual(state.tools.map(t=>t.anchor),['u1','u2']);
});
test('diff event preserves real changed-file projection',()=>{
 let state=projectSnapshot(snapshot);
 state=applyEvent(state,{type:'diff_updated',diff:{files:[{path:'a.rs',added:2,removed:1,patch:null}]}});
 assert.equal(state.diff.files[0].path,'a.rs');
});
test('unreadable diff discards stale clean evidence and recovers only on real projection',()=>{
 let state=projectSnapshot({...snapshot,diff:{files:[]}});
 state=applyEvent(state,{type:'diff_failed',message:'not a work tree'});
 assert.equal(state.diff,null);
 assert.equal(state.diffError,'not a work tree');
 state=applySessionMetadata(state,{...snapshot,diff:null});
 assert.equal(state.diffError,'not a work tree');
 state=applyEvent(state,{type:'diff_updated',diff:{files:[]}});
 assert.equal(state.diffError,null);
 assert.deepEqual(state.diff.files,[]);
});
test('assistant streaming indicator follows message lifecycle rather than task success',()=>{
 let state=projectSnapshot(snapshot);
 state=applyEvent(state,{type:'assistant_message_started',message_id:'a1'});
 assert.equal(state.streamingMessageId,'a1');
 state=applyEvent(state,{type:'assistant_message_completed',message_id:'a1'});
 assert.equal(state.streamingMessageId,null);
 assert.equal(state.status,'running');
});


test('upload draft keeps early authoritative ref across late queued receipt and deduplicates it', async () => {
 const {uploadDraft}=await import('../src/state.mjs');
 const attachment={id:'ref-1',kind:'image',name:'photo.png'};
 let draft=uploadDraft([], {status:'uploading',key:'pick-1'});
 draft=uploadDraft(draft,{status:'ready',key:'pick-1',attachment});
 draft=uploadDraft(draft,{status:'queued',key:'pick-1',import_id:'command-1'});
 draft=uploadDraft(draft,{status:'ready',attachment});
 assert.equal(draft.length,1);assert.equal(draft[0].status,'ready');assert.deepEqual(draft[0].attachment,attachment);
});
test('upload failure preserves draft evidence and removal does not mutate Runtime ref', async () => {
 const {uploadDraft}=await import('../src/state.mjs');
 const ref={id:'document-1',kind:'document',name:'note.pdf'};
 const draft=uploadDraft([],{status:'ready',attachment:ref});
 const failed=uploadDraft(draft,{status:'unknown',key:'pick-2',error:{message:'outcome unknown'}});
 assert.equal(failed[0].attachment,ref);assert.equal(failed[1].status,'unknown');
 assert.deepEqual(uploadDraft(draft,{remove:'document-1'}),[]);assert.equal(ref.id,'document-1');
});

test('upload events require exact current command id; old and missing ids cannot settle another import', async()=>{
 const {uploadDraft,applyUploadEvent}=await import('../src/state.mjs');
 const draft=uploadDraft([],{key:'pick',status:'queued',import_id:'new-command'});
 const ref={id:'file-2',kind:'image',name:'same.png'};
 for(const command_id of [undefined,'old-command']){
  assert.deepEqual(applyUploadEvent(draft,{type:'attachment_added',command_id,attachment:ref}),draft);
  assert.deepEqual(applyUploadEvent(draft,{type:'attachment_processing_failed',command_id,error:'late failure'}),draft);
 }
 const completed=applyUploadEvent(draft,{type:'attachment_added',command_id:'new-command',attachment:ref});
 assert.equal(completed[0].status,'ready');assert.equal(completed[0].attachment,ref);
 const failed=applyUploadEvent(draft,{type:'attachment_processing_failed',command_id:'new-command',error:'read failed'});
 assert.equal(failed[0].status,'failed');assert.equal(failed[0].error.message,'read failed');
});

test('upload success arriving before queued IPC reply settles only the exact returned import', async()=>{
 const {uploadDraft,reconcileUploadReply}=await import('../src/state.mjs');
 const uploading=uploadDraft([],{key:'pick',status:'uploading'});
 const attachment={id:'real-ref',kind:'image',name:'same.png'};
 const early=[{type:'attachment_added',command_id:'old',attachment:{...attachment,id:'old-ref'}},{type:'attachment_added',command_id:'new',attachment}];
 const result=reconcileUploadReply(uploading,{key:'pick',status:'queued',import_id:'new'},early);
 assert.equal(result[0].status,'ready');assert.equal(result[0].attachment.id,'real-ref');
 const mismatch=reconcileUploadReply(uploading,{key:'pick',status:'queued',import_id:'another'},early);
 assert.equal(mismatch[0].status,'queued');assert.equal(mismatch[0].attachment,undefined);
 const failed=reconcileUploadReply(uploading,{key:'pick',status:'queued',import_id:'new'},[{type:'attachment_processing_failed',command_id:'new',error:'conversion failed'}]);
 assert.equal(failed[0].status,'failed');assert.equal(failed[0].error.message,'conversion failed');
});

test('switching workspace preserves Home text but restores new-task ownership from an existing conversation',async()=>{
 const {workspaceDraftText}=await import('../src/state.mjs');
 assert.equal(workspaceDraftText({isHome:true,text:'Home draft',homeDraft:'older home'}),'Home draft');
 assert.equal(workspaceDraftText({isHome:false,text:'private task draft',homeDraft:'new-task draft'}),'new-task draft');
 assert.equal(workspaceDraftText({isHome:false,text:'private task draft'}),'');
});

test('stored ordinary file gives actionable product error before send and leaves references intact',async()=>{
 const {attachmentSubmissionError}=await import('../src/state.mjs');
 const refs=[{id:'stored-document',kind:'document',name:'notes.txt'}];
 assert.equal(attachmentSubmissionError(refs),'普通文件已上传，当前暂不能发送给模型。移除附件后可发送文字。');
 assert.deepEqual(refs,[{id:'stored-document',kind:'document',name:'notes.txt'}]);
 assert.equal(attachmentSubmissionError([{id:'image',kind:'image'}]),'');
 assert.equal(attachmentSubmissionError([]),'');
});

test('permission commands do not change displayed authority until the selected session snapshot confirms',async()=>{
 const {permissionCommand,permissionConfirmed,applySessionMetadata}=await import('../src/state.mjs');
 const original=projectSnapshot({...snapshot,mode:'assisted'});
 const command=permissionCommand('session-1','full_access');
 assert.deepEqual(command,{type:'set_permission_profile',session_id:'session-1',mode:'full_access'});
 assert.equal(original.session.mode,'assisted');
 assert.equal(permissionConfirmed(original,'session-1','full_access'),false);
 const confirmed=applySessionMetadata(original,{...snapshot,mode:'full_access'});
 assert.equal(permissionConfirmed(confirmed,'session-1','full_access'),true);
 assert.equal(permissionConfirmed({...confirmed,session:{...confirmed.session,id:'next-task'}},'session-1','full_access'),false);
 assert.throws(()=>permissionCommand('session-1','invented_mode'));
 for(const mode of ['full_access','assisted','request_approval'])assert.equal(permissionCommand('session-1',mode).mode,mode);
});

test('Workbench defaults docked at 360, overlays only when center cannot fit, and fullscreen leaves saved width alone',async()=>{
 const {workbenchLayout}=await import('../src/state.mjs');
 const preferences={width:360,docked:true,expanded:false};
 assert.deepEqual(workbenchLayout({...preferences,viewport:1280,sidebarWidth:260}),{overlay:false,width:360,showContext:false});
 assert.deepEqual(workbenchLayout({...preferences,viewport:900,sidebarWidth:260}),{overlay:true,width:360,showContext:false});
 const expanded=workbenchLayout({...preferences,expanded:true,viewport:1280,sidebarWidth:260});
 assert.equal(expanded.width,1280);assert.equal(expanded.showContext,true);assert.equal(preferences.width,360);
 const wide=workbenchLayout({...preferences,width:620,viewport:1600,sidebarWidth:260});
 assert.equal(wide.showContext,true);assert.equal(wide.width,620);
 const narrow=workbenchLayout({...preferences,viewport:320,sidebarWidth:56});assert.ok(narrow.width<=264);
});

test('diff query retains prior evidence until correlated result, ignores foreign responses and keeps broadcasts authoritative',async()=>{
 const {beginDiffQuery}=await import('../src/state.mjs');
 const previous={files:[{path:'before.txt',added:1,removed:0}]};
 let state=beginDiffQuery(projectSnapshot({...snapshot,diff:previous}),'request-1');
 assert.equal(state.diffQuery.status,'pending');assert.deepEqual(state.diff,previous);
 state=applyEvent(state,{type:'diff_updated',query_id:'another-task-query',diff:{files:[]}});
 assert.deepEqual(state.diff,previous);assert.equal(state.diffQuery.status,'pending');
 state=applyEvent(state,{type:'diff_failed',query_id:'old',message:'old error'});assert.equal(state.diffError,null);
 state=applyEvent(state,{type:'diff_updated',diff:{files:[{path:'broadcast.txt'}]}});
 assert.equal(state.diff.files[0].path,'broadcast.txt');assert.equal(state.diffQuery.status,'pending');
 state=applyEvent(state,{type:'diff_updated',query_id:'request-1',diff:{files:[]}});
 assert.deepEqual(state.diff.files,[]);assert.equal(state.diffQuery.status,'confirmed');
});
test('diff timeout is unknown rather than clean and late previous query cannot override a new request',async()=>{
 const {beginDiffQuery,expireDiffQuery}=await import('../src/state.mjs');
 let state=beginDiffQuery(projectSnapshot({...snapshot,diff:{files:[{path:'known.txt'}]}}),'old-query');
 state=expireDiffQuery(state,'old-query','结果未确认');assert.equal(state.diffQuery.status,'unknown');assert.equal(state.diff.files[0].path,'known.txt');
 state=beginDiffQuery(state,'new-query');state=applyEvent(state,{type:'diff_updated',query_id:'old-query',diff:{files:[]}});assert.equal(state.diff.files[0].path,'known.txt');
 state=applyEvent(state,{type:'diff_failed',query_id:'new-query',message:'not a work tree'});assert.equal(state.diff,null);assert.equal(state.diffError,'not a work tree');assert.equal(state.diffQuery.status,'failed');
 const other=projectSnapshot({...snapshot,id:'other-task',diff:{files:[{path:'other.txt'}]}});
 assert.deepEqual(applyEvent(other,{type:'diff_updated',query_id:'new-query',diff:{files:[]}}).diff,other.diff);
});

test('discard acknowledgement removes the stable draft key even when a late exact upload event made it ready',async()=>{
 const {uploadDraft,applyUploadEvent}=await import('../src/state.mjs');
 const unknown=uploadDraft([],{key:'draft-k',import_id:'import-i',status:'unknown'});
 const event={type:'attachment_added',command_id:'import-i',attachment:{id:'saved-ref',kind:'image',name:'photo.png'}};
 const ready=applyUploadEvent(unknown,event);assert.equal(ready[0].status,'ready');
 const removed=uploadDraft(ready,{remove:'draft-k'});assert.deepEqual(removed,[]);
 assert.deepEqual(applyUploadEvent(removed,event),[]);
 const another=uploadDraft(ready,{key:'other-k',import_id:'other-i',status:'unknown'});
 assert.deepEqual(uploadDraft(another,{remove:'draft-k'}).map(item=>item.key),['other-k']);
 assert.deepEqual(uploadDraft(ready,{remove:'saved-ref'}),[]);
});

test('settings reads register before ACK and accept only owned typed results, retaining confirmed facts through unknown',async()=>{
 const {beginReadQuery,applyReadResult,expireReadQuery}=await import('../src/state.mjs');
 const prior={data:{accounting:{total:7}},status:'confirmed'};
 let read=beginReadQuery(prior,'s1','q1','context_loaded');
 assert.equal(read.status,'pending');assert.deepEqual(read.data,prior.data);
 const early={session_id:'s1',event:'runtime',data:{type:'context_loaded',query_id:'q1',accounting:null}};
 assert.equal(applyReadResult(read,{...early,session_id:'s2'}),read);
 assert.equal(applyReadResult(read,{...early,data:{...early.data,query_id:'old'}}),read);
 assert.equal(applyReadResult(read,{...early,data:{type:'notification',message:'warning'}}),read);
 read=applyReadResult(read,early);assert.equal(read.status,'confirmed');assert.equal(read.data.accounting,null);
 assert.equal(expireReadQuery(read,'q1','timeout'),read);
 read=beginReadQuery(read,'s1','q2','agents_loaded');read=expireReadQuery(read,'q2','结果未确认');
 assert.equal(read.status,'unknown');assert.equal(read.data.accounting,null);
 const partial={session_id:'s1',event:'runtime',data:{type:'agents_loaded',query_id:'q2',agents:[],problems:[{error:'broken definition'}]}};
 assert.equal(applyReadResult(read,partial).data.problems.length,1);
});
