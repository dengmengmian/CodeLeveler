// Bounded UI integration: test-only provider + native picker seam + real Runtime.
import {_electron as electron} from 'playwright';import assert from 'node:assert/strict';import {mkdtemp,mkdir,writeFile,realpath,readFile} from 'node:fs/promises';import {fileURLToPath} from 'node:url';import path from 'node:path';import {provider} from '../test/fixtures/provider.mjs';
const root=fileURLToPath(new URL('..',import.meta.url));
const temporary=await mkdtemp('/tmp/cl3c-ui-'),home=path.join(temporary,'home'),workspace=path.join(temporary,'workspace');await mkdir(home);await mkdir(workspace);await mkdir(path.join(workspace,'scratch'));await writeFile(path.join(workspace,'scratch','keep.txt'),'must-survive-denial');await writeFile(path.join(workspace,'fixture.txt'),'desktop-real-tool-evidence');
const fixture=await provider();const output=process.env.LEVELER_ACCEPTANCE_OUTPUT||path.join(root,'acceptance-output');await mkdir(output,{recursive:true});
await writeFile(path.join(home,'config.toml'),`default_model = "fixture/m"\n[providers.fixture]\nprotocol = "openai_chat"\nbase_url = "${fixture.baseURL}"\napi_key = "test-only-fixture"\n[models.m]\nprovider = "fixture"\nmodel_id = "fixture-model"\ncontext_window = 131072\nparallel_tool_calls = false\n`,{mode:0o600});
let application,page;
try{
 application=await electron.launch({args:[root,`--user-data-dir=${path.join(temporary,'electron')}`],env:{...process.env,ELECTRON_RUN_AS_NODE:undefined,LEVELER_HOME:home,LEVELER_CONFIG_DIR:undefined,LEVELER_DAEMON_IDLE_TIMEOUT_SECS:'30'}});page=await application.firstWindow();page.setDefaultTimeout(60000);await page.waitForFunction(()=>window.desktop&&document.querySelector('#refresh')?.disabled===false);
 // Explicit fixture seam in Electron Main, not in app code or Renderer.
 await application.evaluate(({dialog},folder)=>{dialog.showOpenDialog=async()=>({canceled:false,filePaths:[folder]});},workspace);
 await page.locator('#space-add').click();await page.waitForFunction(()=>document.querySelector('#workspace').textContent.includes('workspace'));
 await page.locator('#message').fill('fixture-interactions: read fixture.txt and propose removing scratch; I will deny approval.');await page.locator('#message').press('Enter');
 await page.getByRole('button',{name:'拒绝',exact:true}).waitFor();
 const sessionId=await page.locator('.task.selected').getAttribute('data-session-id');
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
 const evidence={provider:'test-only fixture through real Runtime',native_picker:'test-only Main dialog return seam',workspace_repository:settled.repository,session_id:sessionId,tool_preview:'desktop-real-tool-evidence',live_approval:true,waiting_tool_not_running:true,denial_preserved_file:true,denial_resolved:true,tool_history_restored:true,stale_approval_not_restored:true,requests:fixture.requests.length,fixture_errors:fixture.errors,temporary_home:home};await writeFile(path.join(output,'interactions-evidence.json'),JSON.stringify(evidence,null,2)+'\n');console.log(JSON.stringify(evidence,null,2));
}catch(error){if(page&&!page.isClosed())await page.screenshot({path:path.join(output,'interactions-failure.png')}).catch(()=>{});throw error;}
finally{if(application)await application.close();await fixture.close();}
