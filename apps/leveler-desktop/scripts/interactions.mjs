// Bounded UI integration: test-only provider + native picker seam + real Runtime.
import {_electron as electron} from 'playwright';import assert from 'node:assert/strict';import {mkdtemp,mkdir,writeFile,realpath,readFile} from 'node:fs/promises';import {fileURLToPath} from 'node:url';import path from 'node:path';import {DatabaseSync} from 'node:sqlite';import {execFileSync} from 'node:child_process';import {createHash} from 'node:crypto';import {provider} from '../test/fixtures/provider.mjs';
const root=fileURLToPath(new URL('..',import.meta.url));
const temporary=await mkdtemp('/tmp/cl3c-ui-'),home=path.join(temporary,'home'),workspace=path.join(temporary,'workspace');await mkdir(home);await mkdir(workspace);await mkdir(path.join(workspace,'scratch'));await writeFile(path.join(workspace,'scratch','keep.txt'),'must-survive-denial');await writeFile(path.join(workspace,'fixture.txt'),'desktop-real-tool-evidence');
const fixture=await provider();const output=process.env.LEVELER_ACCEPTANCE_OUTPUT||path.join(root,'acceptance-output');await mkdir(output,{recursive:true});
await writeFile(path.join(home,'config.toml'),`default_model = "fixture/m"\n[providers.fixture]\nprotocol = "openai_chat"\nbase_url = "${fixture.baseURL}"\napi_key = "test-only-fixture"\n[models.m]\nprovider = "fixture"\nmodel_id = "fixture-model"\ncontext_window = 131072\nparallel_tool_calls = false\n`,{mode:0o600});
let application,page,runtime,primaryError;const resourceResponses=[],pageErrors=[],runtimeOwners=new Map(),ownedSessions=new Set();
function rows(source,sql,...parameters){const db=new DatabaseSync(source,{readOnly:true});try{return db.prepare(sql).all(...parameters).map(row=>({...row}));}finally{db.close();}}
async function rememberRuntime(){runtime=await page.evaluate(()=>window.desktop.runtimeInfo());runtimeOwners.set(runtime.pid,runtime);return runtime;}
const agentRequests=()=>fixture.requests.filter(request=>request.names.length>0).length;
async function deliver(id,type,content){return page.evaluate(({id,type,content})=>window.desktop.deliver({command_id:crypto.randomUUID(),session_id:id,expected_version:null,issued_at:new Date().toISOString(),command:{type,session_id:id,...(content===undefined?{}:{content})}}),{id,type,content});}
async function waitStatus(id,status){await page.waitForFunction(async({id,status})=>(await window.desktop.snapshot(id)).task_status===status,{id,status});return page.evaluate(id=>window.desktop.snapshot(id),id);}
async function idleExit(pid){const deadline=Date.now()+45000;while(Date.now()<deadline){try{process.kill(pid,0);}catch(error){if(error.code==='ESRCH')return {pid,normal_idle_exit_observed:true,signaled:false};throw error;}await new Promise(resolve=>setTimeout(resolve,100));}throw new Error(`Runtime ${pid} did not exit normally; no signal sent`);}

const binary=process.env.LEVELER_BINARY??path.resolve(root,'../../target/debug/leveler');
const producer={binary_path:binary,binary_version:execFileSync(binary,['--version'],{encoding:'utf8'}).trim(),binary_sha256:createHash('sha256').update(await readFile(binary)).digest('hex'),script_sha256:createHash('sha256').update(await readFile(fileURLToPath(import.meta.url))).digest('hex'),provider_sha256:createHash('sha256').update(await readFile(path.join(root,'test/fixtures/provider.mjs'))).digest('hex')};
await writeFile(path.join(output,'producer-evidence.json'),JSON.stringify(producer,null,2)+'\n');
async function cleanupRuntimes(){
 const cleanup=[];for(const owner of runtimeOwners.values()){try{cleanup.push({...await idleExit(owner.pid),runtime_id:owner.runtime_id});}catch(error){cleanup.push({pid:owner.pid,runtime_id:owner.runtime_id,normal_idle_exit_observed:false,signaled:false,error:error.message});}}
 await writeFile(path.join(output,'cleanup-evidence.json'),JSON.stringify({runtimes:cleanup},null,2)+'\n');
 if(!primaryError&&cleanup.some(result=>!result.normal_idle_exit_observed))throw new Error('Isolated Runtime cleanup unproven; no signal sent');
}
try{
 application=await electron.launch({args:[root,`--user-data-dir=${path.join(temporary,'electron')}`],env:{...process.env,ELECTRON_RUN_AS_NODE:undefined,LEVELER_HOME:home,LEVELER_CONFIG_DIR:undefined,LEVELER_DAEMON_IDLE_TIMEOUT_SECS:'10'}});page=await application.firstWindow();page.on('pageerror',error=>{pageErrors.push(error.message);console.error('RENDERER_ERROR',error.message);});page.on('console',message=>console.error('RENDERER_CONSOLE',message.type(),message.text()));page.on('response',response=>{resourceResponses.push({status:response.status(),url:response.url()});if(response.status()>=400)console.error('RESOURCE_FAILURE',response.status(),response.url());});application.process().stderr.on('data',chunk=>console.error('ELECTRON_STDERR',String(chunk)));page.setDefaultTimeout(15000);await page.waitForLoadState('domcontentloaded');assert.deepEqual(resourceResponses.filter(response=>response.status>=400),[],'actual renderer imports must be served');await page.waitForFunction(()=>window.desktop&&document.querySelector('#refresh')?.disabled===false&&document.querySelector('#desktop-version')?.textContent.startsWith('v'));
 await writeFile(path.join(output,'renderer-assets-evidence.json'),JSON.stringify({resource_responses:resourceResponses,renderer_initialized:true},null,2)+'\n');
 // Explicit fixture seam in Electron Main, not in app code or Renderer.
 await application.evaluate(({dialog},folder)=>{dialog.showOpenDialog=async()=>({canceled:false,filePaths:[folder]});},workspace);
 await page.locator('#space-add').click();await page.waitForFunction(()=>document.querySelector('#workspace').textContent.includes('workspace'));
 await page.locator('#message').fill('fixture-interactions: read fixture.txt and propose removing scratch; I will deny approval.');await page.locator('#message').press('Enter');
 await page.getByRole('button',{name:'拒绝',exact:true}).waitFor();
 const sessionId=await page.locator('.task.selected').getAttribute('data-session-id');ownedSessions.add(sessionId);await rememberRuntime();
 const liveUserCount=await page.locator('.message.user').count();
 const snapshotWaiting=await page.evaluate(id=>window.desktop.snapshot(id),sessionId);
 await writeFile(path.join(output,'waiting-message-evidence.json'),JSON.stringify({snapshot_messages:snapshotWaiting.messages.map(m=>({role:m.role,text:m.text})),live_user_count:liveUserCount},null,2)+'\n');
 assert.equal(liveUserCount,1,'live accepted user message must remain while waiting for approval');
 assert.equal(await page.getByText('任务已创建。输入第一条消息开始对话。',{exact:true}).count(),0);
 assert.equal(snapshotWaiting.messages.filter(m=>m.role==='user').length,1);
 assert.equal(await page.locator('#status').innerText(),'等待你处理');
 assert.equal(snapshotWaiting.repository,await realpath(workspace));
 const pending=snapshotWaiting.pending_interactions.find(interaction=>interaction.type==='approval');assert.ok(pending);
 assert.equal(await readFile(path.join(workspace,'scratch','keep.txt'),'utf8'),'must-survive-denial');
 assert.match(await page.locator('.tool[data-tool-id="fixture-consent"] summary').innerText(),/等待审批/);
 if(pending.request.requires_human_consent){assert.equal(await page.getByRole('button',{name:'本会话允许',exact:true}).count(),0);assert.equal(await page.getByRole('button',{name:'始终允许',exact:true}).count(),0);}
 assert.equal(await page.getByRole('button',{name:'允许一次',exact:true}).isEnabled(),true);
 const readTool=page.locator('.tool[data-tool-id="fixture-read"]');await readTool.waitFor();await readTool.locator('summary').click();await readTool.locator('.tool-output').waitFor();assert.match(await readTool.innerText(),/desktop-real-tool-evidence/);
 await page.screenshot({path:path.join(output,'workspace-live-approval.png')});
 await page.getByRole('button',{name:'拒绝',exact:true}).click();
 await page.waitForFunction(()=>document.querySelector('#status').textContent==='已回答'||document.querySelector('#status').textContent==='已完成');
 await page.waitForFunction(()=>document.querySelector('#refresh').disabled===false&&document.querySelector('#interactions').children.length===0);
 const settled=await page.evaluate(id=>window.desktop.snapshot(id),sessionId);assert.equal(settled.pending_interactions.length,0);assert.equal(await readFile(path.join(workspace,'scratch','keep.txt'),'utf8'),'must-survive-denial');assert.deepEqual(fixture.errors,[]);
 assert.match(await page.locator('.tool[data-tool-id="fixture-consent"]').innerText(),/失败/);
 // History replay must restore tool evidence, never resurrect resolved consent buttons.
 await page.locator('#new-task').click();await page.waitForFunction(()=>document.body.classList.contains('home')&&!document.querySelector('#refresh').disabled);
 await page.locator(`.task[data-session-id="${sessionId}"]`).click();await page.waitForFunction(()=>document.querySelector('.tool[data-tool-id="fixture-read"]'));
 await page.locator('.tool[data-tool-id="fixture-read"] summary').click();await page.locator('.tool[data-tool-id="fixture-read"] .tool-output').waitFor();assert.match(await page.locator('.tool[data-tool-id="fixture-read"]').innerText(),/desktop-real-tool-evidence/);
 assert.equal(await page.getByRole('button',{name:'拒绝',exact:true}).count(),0);
 assert.equal(await page.locator('.message.assistant .body').last().innerText(),'fixture-interactions-complete');
 await page.screenshot({path:path.join(output,'workspace-tool-history.png')});
 // The provider holds a real model response open until cancellation. UI text
 // submission after interruption must continue the same logical task.
 await page.locator('#new-task').click();await page.waitForFunction(()=>document.body.classList.contains('home')&&!document.querySelector('#refresh').disabled);
 await page.locator('#message').fill('fixture-turn-hold');await page.locator('#message').press('Enter');
 await page.getByText('fixture-held-answer',{exact:true}).waitFor();
 const interruptedId=await page.locator('.task.selected').getAttribute('data-session-id');ownedSessions.add(interruptedId);
 const interruptSource=await page.evaluate(async id=>(await window.desktop.listTasks()).tasks.find(task=>task.id===id).source_id,interruptedId);await rememberRuntime();
 const originalTask=rows(interruptSource,'SELECT * FROM tasks WHERE session_id = ?',interruptedId)[0];assert.ok(originalTask.owner_boot_id);
 const originalTurn=rows(interruptSource,'SELECT * FROM turns WHERE session_id = ? ORDER BY ordinal',interruptedId)[0];const originalPayload=JSON.parse(originalTurn.payload);assert.equal(originalPayload.objective.primary,'fixture-turn-hold');
 const cancelTurn=await deliver(interruptedId,'cancel_current_turn');assert.equal(cancelTurn.ok,true);
 const interrupted=await waitStatus(interruptedId,'interrupted');assert.equal(interrupted.task_terminal.outcome,'interrupted');await page.waitForFunction(()=>document.querySelector('#status').textContent==='已中断'&&document.querySelector('#send').getAttribute('aria-label')!=='停止');
 assert.equal(rows(interruptSource,'SELECT COUNT(*) AS count FROM turns WHERE session_id = ?',interruptedId)[0].count,1);
 assert.equal(rows(interruptSource,'SELECT owner_boot_id FROM tasks WHERE session_id = ?',interruptedId)[0].owner_boot_id,null);
 const beforeResume=agentRequests();
 await page.locator('#message').fill('继续，fixture-turn-resume');await page.locator('#message').press('Enter');
 await page.getByText('fixture-resumed-answer',{exact:true}).waitFor();await waitStatus(interruptedId,'answered');
 assert.equal(agentRequests(),beforeResume+1,'continuation must issue one actual agent request');
 const resumedTask=rows(interruptSource,'SELECT * FROM tasks WHERE session_id = ?',interruptedId)[0];assert.equal(resumedTask.id,originalTask.id);
 const resumeTurns=rows(interruptSource,'SELECT * FROM turns WHERE session_id = ? ORDER BY ordinal',interruptedId);assert.equal(resumeTurns.length,2);
 const resumePayload=JSON.parse(resumeTurns[1].payload);
 assert.equal(resumeTurns[1].kind,originalTurn.kind,'Harness resume preserves the original execution kind');
 assert.equal(resumePayload.continuation_root_turn_id,originalTurn.id);
 assert.equal(resumePayload.objective.primary,originalPayload.objective.primary,'continuation must preserve the original objective');
 assert.deepEqual(resumePayload.objective.amendments,['继续，fixture-turn-resume']);
 assert.equal(resumePayload.initiating_message.role,'user');assert.match(JSON.stringify(resumePayload.initiating_message),/继续，fixture-turn-resume/);
 // CancelTask closes a different task and must reject later resume without
 // admitting a turn or reaching the provider.
 await page.locator('#new-task').click();await page.waitForFunction(()=>document.body.classList.contains('home')&&!document.querySelector('#refresh').disabled);
 await page.locator('#message').fill('fixture-turn-hold');await page.locator('#message').press('Enter');await page.getByText('fixture-held-answer',{exact:true}).waitFor();
 const cancelledId=await page.locator('.task.selected').getAttribute('data-session-id');ownedSessions.add(cancelledId);assert.equal((await deliver(cancelledId,'cancel_task')).ok,true);
 const cancelled=await waitStatus(cancelledId,'cancelled');assert.equal(cancelled.task_terminal.outcome,'cancelled');
 const beforeRejectedResume=agentRequests();const rejectedResume=await deliver(cancelledId,'resume_task','继续，fixture-turn-resume');assert.equal(rejectedResume.ok,false);
 assert.equal(agentRequests(),beforeRejectedResume);
 assert.equal(rows(interruptSource,'SELECT COUNT(*) AS count FROM turns WHERE session_id = ?',cancelledId)[0].count,1);
 assert.equal(rows(interruptSource,'SELECT owner_boot_id FROM tasks WHERE session_id = ?',cancelledId)[0].owner_boot_id,null);
 await writeFile(path.join(output,'cancel-resume-evidence.json'),JSON.stringify({interrupted_session_id:interruptedId,task_before:originalTask.id,task_after:resumedTask.id,interrupted_terminal:interrupted.task_terminal,resume_turn_count:2,resume_turn_kind:resumeTurns[1].kind,resume_payload:resumePayload,original_payload:originalPayload,provider_requests_before_resume:beforeResume,provider_requests_after_resume:beforeResume+1,cancelled_session_id:cancelledId,cancelled_terminal:cancelled.task_terminal,rejected_resume:rejectedResume,no_turn_or_provider_after_rejected_resume:true,source_id:interruptSource},null,2)+'\n');
 // A cancelled waiter is expired. Its identity cannot settle a newer waiter.
 await page.waitForFunction(()=>document.querySelector('#status').textContent==='已取消');
 async function workspaceApproval(){
  await page.locator('#new-task').click();await page.waitForFunction(()=>document.body.classList.contains('home')&&!document.querySelector('#refresh').disabled);
  await page.locator('#space-add').click();await page.waitForFunction(()=>document.querySelector('#workspace').textContent.includes('workspace'));
  await page.locator('#message').fill('fixture-expired-approval: read fixture.txt and propose removing scratch');await page.locator('#message').press('Enter');
  await page.getByRole('button',{name:'拒绝',exact:true}).waitFor();await rememberRuntime();
  const id=await page.locator('.task.selected').getAttribute('data-session-id');ownedSessions.add(id);const snapshot=await page.evaluate(id=>window.desktop.snapshot(id),id);const waiter=snapshot.pending_interactions.find(interaction=>interaction.type==='approval');assert.ok(waiter);return {id,request_id:waiter.request.id};
 }
 async function staleReply(id,request_id){return page.evaluate(({id,request_id})=>window.desktop.deliver({command_id:crypto.randomUUID(),session_id:id,expected_version:null,issued_at:new Date().toISOString(),command:{type:'approval_decision',request_id,decision:'approve_once'}}),{id,request_id});}
 const expired=await workspaceApproval();await page.locator('#cancel').click();await waitStatus(expired.id,'interrupted');
 await page.waitForFunction(()=>document.querySelector('#status').textContent==='已中断'&&document.querySelector('#interactions').children.length===0);
 const expiredReply=await staleReply(expired.id,expired.request_id);assert.equal(expiredReply.ok,false);assert.match(expiredReply.error.message,/pending|expired|active|待处理|失效/);
 const newer=await workspaceApproval();assert.notEqual(newer.request_id,expired.request_id);
 const beforeStale=agentRequests();const staleAgainstNew=await staleReply(newer.id,expired.request_id);assert.equal(staleAgainstNew.ok,false);
 const stillPending=await page.evaluate(id=>window.desktop.snapshot(id),newer.id);assert.equal(stillPending.pending_interactions.find(interaction=>interaction.type==='approval').request.id,newer.request_id);
 assert.equal(agentRequests(),beforeStale);assert.equal(await readFile(path.join(workspace,'scratch','keep.txt'),'utf8'),'must-survive-denial');
 await page.getByRole('button',{name:'拒绝',exact:true}).click();await waitStatus(newer.id,'answered');await page.waitForFunction(()=>document.querySelector('#interactions').children.length===0);
 assert.equal(await readFile(path.join(workspace,'scratch','keep.txt'),'utf8'),'must-survive-denial');
 await writeFile(path.join(output,'expired-approval-evidence.json'),JSON.stringify({expired_waiter:expired,expired_reply:expiredReply,new_waiter:newer,stale_against_new:staleAgainstNew,new_waiter_retained:true,stale_reply_no_provider_call:true,actual_new_denial_preserved_file:true},null,2)+'\n');
 assert.deepEqual(pageErrors,[]);assert.deepEqual(fixture.errors,[]);
 const evidence={page_errors:pageErrors,producer,provider:'test-only fixture through real Runtime',native_picker:'test-only Main dialog return seam',workspace_repository:settled.repository,session_id:sessionId,tool_preview:'desktop-real-tool-evidence',live_approval:true,waiting_tool_not_running:true,denial_preserved_file:true,denial_resolved:true,tool_history_restored:true,stale_approval_not_restored:true,requests:fixture.requests.length,fixture_errors:fixture.errors,temporary_home:home};await writeFile(path.join(output,'interactions-evidence.json'),JSON.stringify(evidence,null,2)+'\n');console.log(JSON.stringify(evidence,null,2));
 }catch(error){
 primaryError=error;await writeFile(path.join(output,'failure-evidence.json'),JSON.stringify({error:error.message,resource_responses:resourceResponses},null,2)+'\n');
 if(page&&!page.isClosed()){
  await page.screenshot({path:path.join(output,'interactions-failure.png')});
  const id=await page.locator('.task.selected').getAttribute('data-session-id').catch(()=>null);
  if(ownedSessions.has(id)){
   const snapshot=await page.evaluate(id=>window.desktop.snapshot(id),id);
   if(['running','waiting_user'].includes(snapshot.task_status)){assert.equal((await deliver(id,'cancel_current_turn')).ok,true);await waitStatus(id,'interrupted');}
  }
 }
 throw error;
}
finally{if(application)await application.close();await fixture.close();await cleanupRuntimes();}
