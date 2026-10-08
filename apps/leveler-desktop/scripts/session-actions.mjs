import {SNAPSHOT_VERSIONED_COMMANDS} from '../src/command-policy.gen.mjs';
// Real Electron Main/preload -> Rust bridge -> isolated Runtime acceptance.
// A test-only provider is configured, but no inference or tools are invoked.
import {_electron as electron} from 'playwright';
import assert from 'node:assert/strict';
import {mkdtemp,mkdir,writeFile,readFile} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';
import path from 'node:path';
import {randomUUID} from 'node:crypto';
import {DatabaseSync} from 'node:sqlite';
import {provider} from '../test/fixtures/provider.mjs';
const root=fileURLToPath(new URL('..',import.meta.url));
const temporary=await mkdtemp('/tmp/leveler-desktop-session-actions-'),home=path.join(temporary,'home');await mkdir(home);
const output=process.env.LEVELER_ACCEPTANCE_OUTPUT||path.join(root,'acceptance-output/desktop');await mkdir(output,{recursive:true});
const fixture=await provider();
const config=`default_model = "fixture/first"\n[providers.fixture]\nprotocol = "openai_chat"\nbase_url = "${fixture.baseURL}"\napi_key = "test-only-fixture"\n[models.first]\nprovider = "fixture"\nmodel_id = "first-model"\ncontext_window = 131072\n[models.second]\nprovider = "fixture"\nmodel_id = "second-model"\ncontext_window = 131072\n`;
await writeFile(path.join(home,'config.toml'),config,{mode:0o600});
let application,page,runtime;
const errors=[];
async function launch(){
 application=await electron.launch({args:[root,`--user-data-dir=${path.join(temporary,'electron-profile')}`],env:{...process.env,ELECTRON_RUN_AS_NODE:undefined,LEVELER_BINARY:process.env.LEVELER_BINARY??path.resolve(root,'../../target/debug/leveler'),LEVELER_HOME:home,LEVELER_CONFIG_DIR:undefined,LEVELER_DAEMON_IDLE_TIMEOUT_SECS:'10'}});
 page=await application.firstWindow();page.setDefaultTimeout(15000);page.on('pageerror',error=>errors.push(error.message));
 await page.waitForFunction(()=>window.desktop&&document.querySelector('#refresh')?.disabled===false);
}
async function command(sessionId,command){const observed=SNAPSHOT_VERSIONED_COMMANDS.includes(command.type)?await snapshot(sessionId):null;const expected_version=observed?observed.last_sequence??0:null;return page.evaluate(({sessionId,command,expected_version})=>window.desktop.deliver({command_id:crypto.randomUUID(),session_id:sessionId,expected_version,issued_at:new Date().toISOString(),command}),{sessionId,command,expected_version});}
const snapshot=id=>page.evaluate(id=>window.desktop.snapshot(id),id);
const index=()=>page.evaluate(()=>window.desktop.listTasks());
function rows(source,sql,...parameters){const db=new DatabaseSync(source,{readOnly:true});try{return db.prepare(sql).all(...parameters).map(row=>({...row}));}finally{db.close();}}
const deliver=envelope=>page.evaluate(envelope=>window.desktop.deliver(envelope),envelope);
function envelope(sessionId,command,expectedVersion=null){return {command_id:randomUUID(),session_id:sessionId,expected_version:expectedVersion,issued_at:new Date().toISOString(),command};}
async function close(){await application.close();application=null;}
async function awaitIdleExit(pid){
 const deadline=Date.now()+30000;
 while(Date.now()<deadline){
  try{process.kill(pid,0);}catch(error){if(error.code==='ESRCH')return {pid,normal_idle_exit_observed:true,signaled:false};throw error;}
  await new Promise(resolve=>setTimeout(resolve,100));
 }
 throw new Error(`Isolated Runtime ${pid} did not exit normally before deadline; no signal sent`);
}
async function recordIdleExit(pid){
 try{await writeFile(path.join(output,'cleanup-evidence.json'),JSON.stringify(await awaitIdleExit(pid),null,2)+'\n');}
 catch(error){await writeFile(path.join(output,'cleanup-evidence.json'),JSON.stringify({pid,normal_idle_exit_observed:false,signaled:false,error:error.message},null,2)+'\n');throw error;}
}
try{
 await launch();
 const assetStatuses=await application.evaluate(async({net})=>{const paths=['command-policy.gen.mjs','packages/conversation-presentation/conversation.mjs','unknown.mjs','%2e%2e%2fsecret','index.html'];const urls=paths.map(path=>'leveler-desktop://desktop/'+path).concat('leveler-desktop://unknown/index.html');return Promise.all(urls.map(async url=>({url,status:(await net.fetch(url)).status})));});
 assert.deepEqual(assetStatuses.map(asset=>asset.status),[200,200,404,404,200,404]);
 await page.waitForFunction(()=>document.querySelector('#desktop-version')?.textContent.startsWith('v'));
 await writeFile(path.join(output,'renderer-assets-evidence.json'),JSON.stringify({asset_statuses:assetStatuses,renderer_initialized:true},null,2)+'\n');
 const creationId=randomUUID();const created=await page.evaluate(requestId=>window.desktop.createTask(null,requestId),creationId);const id=created.session.id;runtime=await page.evaluate(()=>window.desktop.runtimeInfo());
 const duplicateCreate=await page.evaluate(requestId=>window.desktop.createTask(null,requestId),creationId);assert.equal(duplicateCreate.session.id,id);
 const initialTask=(await index()).tasks.find(task=>task.id===id);assert.ok(initialTask);
 assert.equal(rows(initialTask.source_id,'SELECT COUNT(*) AS count FROM sessions')[0].count,1);
 assert.equal(rows(initialTask.source_id,'SELECT COUNT(*) AS count FROM tasks')[0].count,1);
 assert.equal(rows(initialTask.source_id,'SELECT COUNT(*) AS count FROM command_receipts WHERE command_id = ? AND status = ?',creationId,'completed')[0].count,1);
 const original=await snapshot(id);assert.equal(original.repository,null);assert.equal(original.available_models.length,2);assert.deepEqual(original.model,{provider:'fixture',model:'first'});
 const sameVersion=original.last_sequence??0;
 const raceInputs=['First concurrent title','Second concurrent title'].map(name=>envelope(id,{type:'rename_session',session_id:id,name},sameVersion));
 const race=await page.evaluate(inputs=>Promise.all(inputs.map(input=>window.desktop.deliver(input))),raceInputs);
 assert.equal(race.filter(reply=>reply.ok).length,1);const winner=race.findIndex(reply=>reply.ok);const loser=1-winner;assert.match(race[loser].error.message,/version conflict/);
 const raced=await snapshot(id);assert.equal(raced.goal,raceInputs[winner].command.name);assert.equal(raced.last_sequence,sameVersion+1);
 assert.equal(rows(initialTask.source_id,'SELECT COALESCE(MAX(sequence),0) AS version FROM events WHERE session_id = ?',id)[0].version,raced.last_sequence);
 const renameInput=envelope(id,{type:'rename_session',session_id:id,name:'Isolated renamed acceptance task'},raced.last_sequence);
 const rename=await deliver(renameInput);assert.equal(rename.ok,true);
 const renamed=await snapshot(id);const duplicateRename=await deliver(renameInput);assert.equal(duplicateRename.ok,true);
 assert.deepEqual(await snapshot(id),renamed,'same CommandId replay must not change the snapshot');
 assert.equal(rows(initialTask.source_id,'SELECT COALESCE(MAX(sequence),0) AS version FROM events WHERE session_id = ?',id)[0].version,renamed.last_sequence);
 assert.equal(rows(initialTask.source_id,'SELECT COUNT(*) AS count FROM command_receipts WHERE command_id = ? AND status = ?',renameInput.command_id,'completed')[0].count,1);
assert.equal(renamed.goal,'Isolated renamed acceptance task');const task=(await index()).tasks.find(t=>t.id===id);assert.equal(task.title,renamed.goal);
 const select=await command(id,{type:'select_model',session_id:id,model:{provider:'fixture',model:'second'}});assert.equal(select.ok,true);assert.deepEqual((await snapshot(id)).model,{provider:'fixture',model:'second'});
 const invalid=await command(id,{type:'select_model',session_id:id,model:{provider:'fixture',model:'not-configured'}});assert.equal(invalid.ok,false);assert.match(invalid.error.message,/not configured/);assert.deepEqual((await snapshot(id)).model,{provider:'fixture',model:'second'});
 // Close/reopen the Desktop client; the real detached Runtime remains its owner.
 await close();process.kill(runtime.pid,0);await launch();const refreshed=(await index()).tasks.find(t=>t.id===id);assert.equal(refreshed.title,renamed.goal);
 await page.evaluate(task=>window.desktop.openTask(task),{id,source_id:task.source_id});const restored=await snapshot(id);assert.equal(restored.goal,renamed.goal);assert.deepEqual(restored.model,{provider:'fixture',model:'second'});
 const archive=await command(id,{type:'archive_session',session_id:id});assert.equal(archive.ok,true);assert.equal((await index()).tasks.some(t=>t.id===id),false);
 // Archiving hides navigation entries and preserves the existing session record.
 const preserved=await snapshot(id);assert.equal(preserved.goal,renamed.goal);assert.deepEqual(preserved.model,restored.model);
 await close();await launch();assert.equal((await index()).tasks.some(t=>t.id===id),false);
 await page.evaluate(task=>window.desktop.openTask(task),{id,source_id:task.source_id});const archivedReattach=await snapshot(id);assert.equal(archivedReattach.goal,renamed.goal);assert.equal((await index()).tasks.some(t=>t.id===id),false);
 assert.equal(await readFile(path.join(home,'config.toml'),'utf8'),config,'SelectModel must not rewrite the user default');assert.deepEqual(errors,[]);assert.equal(fixture.requests.length,0);
 const evidence={creation_request_id:creationId,same_creation_id_returned_original_session:true,durable_sessions:1,durable_tasks:1,same_snapshot_rename_replies:race,rename_winner:raceInputs[winner].command.name,duplicate_command_id:renameInput.command_id,duplicate_command_no_new_version:true,scope:'real Electron Main/preload + Rust bridge + Runtime; no inference, test-only isolated configuration',temporary_home:home,session_id:id,source_id:task.source_id,runtime_pid:runtime.pid,rename_snapshot_and_index:true,desktop_reopen_rename_persisted:true,select_model_observed:restored.model,invalid_model_rejected:invalid.error.message,invalid_model_preserved_previous:true,default_config_unchanged:true,archive_hidden_from_default_index:true,archive_session_record_retained:true,archive_reattach_possible_with_known_source_and_id:true,archive_still_hidden_after_reattach:true,unarchive_ui_supported:false,provider_requests:fixture.requests.length,page_errors:errors,passed:true};
 await writeFile(path.join(output,'session-actions-evidence.json'),JSON.stringify(evidence,null,2)+'\n');console.log(JSON.stringify(evidence,null,2));
}catch(error){await writeFile(path.join(output,'session-actions-evidence.json'),JSON.stringify({passed:false,error:error.message,page_errors:errors},null,2)+'\n');throw error;}
finally{
 if(application)await close();await fixture.close();
 if(runtime?.pid)await recordIdleExit(runtime.pid);
}
