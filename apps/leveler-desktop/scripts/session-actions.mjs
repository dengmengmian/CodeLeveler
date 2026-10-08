import {SNAPSHOT_VERSIONED_COMMANDS} from '../src/command-policy.gen.mjs';
// Real Electron Main/preload -> Rust bridge -> isolated Runtime acceptance.
// A test-only provider is configured, but no inference or tools are invoked.
import {_electron as electron} from 'playwright';
import assert from 'node:assert/strict';
import {mkdtemp,mkdir,writeFile,readFile} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';
import path from 'node:path';
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
async function close(){await application.close();application=null;}
try{
 await launch();const created=await page.evaluate(()=>window.desktop.createTask(null,crypto.randomUUID()));const id=created.session.id;runtime=await page.evaluate(()=>window.desktop.runtimeInfo());
 const original=await snapshot(id);assert.equal(original.repository,null);assert.equal(original.available_models.length,2);assert.deepEqual(original.model,{provider:'fixture',model:'first'});
 const rename=await command(id,{type:'rename_session',session_id:id,name:'Isolated renamed acceptance task'});assert.equal(rename.ok,true);
 const renamed=await snapshot(id);assert.equal(renamed.goal,'Isolated renamed acceptance task');const task=(await index()).tasks.find(t=>t.id===id);assert.equal(task.title,renamed.goal);
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
 const evidence={scope:'real Electron Main/preload + Rust bridge + Runtime; no inference, test-only isolated configuration',temporary_home:home,session_id:id,source_id:task.source_id,runtime_pid:runtime.pid,rename_snapshot_and_index:true,desktop_reopen_rename_persisted:true,select_model_observed:restored.model,invalid_model_rejected:invalid.error.message,invalid_model_preserved_previous:true,default_config_unchanged:true,archive_hidden_from_default_index:true,archive_session_record_retained:true,archive_reattach_possible_with_known_source_and_id:true,archive_still_hidden_after_reattach:true,unarchive_ui_supported:false,provider_requests:fixture.requests.length,page_errors:errors,passed:true};
 await writeFile(path.join(output,'session-actions-evidence.json'),JSON.stringify(evidence,null,2)+'\n');console.log(JSON.stringify(evidence,null,2));
}catch(error){await writeFile(path.join(output,'session-actions-evidence.json'),JSON.stringify({passed:false,error:error.message,page_errors:errors},null,2)+'\n');throw error;}
finally{if(application)await close();await fixture.close();}
