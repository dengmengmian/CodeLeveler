// Real Desktop + real Runtime + configured provider. No fixtures or fake success.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdir,writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
const root=fileURLToPath(new URL('..',import.meta.url));
if(!process.env.LEVELER_HOME)throw new Error('Set LEVELER_HOME to an isolated home with a working real provider configuration.');
const output=process.env.LEVELER_ACCEPTANCE_OUTPUT||path.join(root,'acceptance-output');
await mkdir(output,{recursive:true});
const marker=`desktop-acceptance-${Date.now()}`;
let application,page;
async function launch(){application=await electron.launch({args:[root,`--user-data-dir=${path.join(output,'electron-profile')}`],env:{...process.env,ELECTRON_RUN_AS_NODE:undefined}});page=await application.firstWindow();page.setDefaultTimeout(60000);await page.waitForFunction(()=>window.desktop&&document.querySelector('#refresh')?.disabled===false);}
async function runtimeInfo(){return page.evaluate(()=>window.desktop.runtimeInfo());}
async function closeWindow(){const processExit=application.process();await page.evaluate(()=>window.close());await new Promise((resolve,reject)=>{if(processExit.exitCode!==null)return resolve();const timer=setTimeout(()=>reject(new Error('Desktop did not exit after its window closed')),10000);processExit.once('exit',()=>{clearTimeout(timer);resolve();});});application=null;}
function ownerEpoch(dbPath,sessionId){const db=new DatabaseSync(dbPath,{readOnly:true});try{return db.prepare('SELECT id, owner_epoch, owner_boot_id FROM tasks WHERE session_id = ? ORDER BY id').all(sessionId).map(row=>({...row}));}finally{db.close();}}
try{
  await launch();
  assert.deepEqual(await page.evaluate(()=>({require:typeof window.require,process:typeof window.process})),{require:'undefined',process:'undefined'});
  await page.locator('#new-task').click();await page.locator('#home-permission').click();await page.waitForFunction(()=>!document.querySelector('#permission-menu').hidden);await page.keyboard.press('Escape');
  await page.waitForFunction(()=>document.querySelector('.task.selected')&&!document.querySelector('#refresh').disabled);
  const sessionId=await page.locator('.task.selected').getAttribute('data-session-id');
  assert.equal(await page.locator('#workspace').innerText(),'选择工作空间');
  await page.locator('#message').fill(`请只回复这段文字，不调用任何工具：${marker}`);
  await page.locator('#message').press('Enter');
  await page.waitForFunction(marker=>Array.from(document.querySelectorAll('.message.assistant .body')).some(n=>n.textContent.includes(marker)),marker,{timeout:180000});
  await page.waitForFunction(()=>document.querySelector('#status').textContent==='已回答'||document.querySelector('#status').textContent==='已完成',null,{timeout:180000});
  const before=await runtimeInfo();assert.ok(before.pid>0);assert.ok(before.runtime_id);
  const task=(await page.evaluate(()=>window.desktop.listTasks())).tasks.find(t=>t.id===sessionId);
  assert.equal(task.primary_workspace,null);
  const snapshotBefore=await page.evaluate(id=>window.desktop.snapshot(id),sessionId);assert.equal(snapshotBefore.repository,null);
  const epochsBefore=ownerEpoch(task.source_id,sessionId);assert.ok(epochsBefore.length>0);
  const answerBefore=await page.locator('.message.assistant .body').allTextContents();
  await page.screenshot({path:path.join(output,'before-close.png')});
  // Switch through a second real session, so history buttons and state reset are exercised.
  await page.locator('#new-task').click();await page.locator('#home-permission').click();await page.waitForFunction(()=>!document.querySelector('#permission-menu').hidden);await page.keyboard.press('Escape');await page.waitForFunction(id=>document.querySelector('.task.selected')?.dataset.sessionId!==id&&!document.querySelector('#refresh').disabled,sessionId);
  await page.locator(`.task[data-session-id="${sessionId}"]`).click();
  await page.waitForFunction(id=>document.querySelector('.task.selected')?.dataset.sessionId===id&&!document.querySelector('#refresh').disabled,sessionId);
  assert.equal(await page.locator('.message.user').count(),1);
  await closeWindow();
  process.kill(before.pid,0); // Actual OS liveness after Desktop and bridge exit.
  await launch();
  await page.locator(`.task[data-session-id="${sessionId}"]`).click();
  await page.waitForFunction(marker=>Array.from(document.querySelectorAll('.message.assistant .body')).some(n=>n.textContent.includes(marker)),marker);
  const after=await runtimeInfo();const snapshotAfter=await page.evaluate(id=>window.desktop.snapshot(id),sessionId);assert.equal(snapshotAfter.repository,null);assert.equal(snapshotAfter.task_status,snapshotBefore.task_status);assert.deepEqual(snapshotAfter.task_terminal,snapshotBefore.task_terminal);
  assert.equal(after.pid,before.pid);assert.equal(after.runtime_id,before.runtime_id);
  assert.deepEqual(ownerEpoch(task.source_id,sessionId),epochsBefore);
  assert.equal(await page.locator('.message.user').count(),1);
  assert.deepEqual(await page.locator('.message.assistant .body').allTextContents(),answerBefore);
  assert.equal(await page.locator('#workspace').innerText(),'选择工作空间');
  await page.screenshot({path:path.join(output,'after-reopen.png')});
  await page.setViewportSize({width:700,height:520});
  assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>window.innerWidth),false);
  await page.screenshot({path:path.join(output,'narrow.png')});
  const evidence={provider:'real configured provider',session_id:sessionId,marker,runtime_before:before,runtime_after:after,owner_epochs:epochsBefore,snapshot_status:snapshotAfter.task_status,task_terminal:snapshotAfter.task_terminal,no_workspace_repository:snapshotAfter.repository,desktop_closed:true,runtime_alive_after_close:true,same_runtime_adopted:true,restored_user_messages:1,restored_assistant_messages:answerBefore.length,history_switch:true,sandbox:true,horizontal_overflow:false};
  await writeFile(path.join(output,'evidence.json'),JSON.stringify(evidence,null,2)+'\n');
  console.log(JSON.stringify(evidence,null,2));
} catch(error){if(page&&!page.isClosed())await page.screenshot({path:path.join(output,'failure.png')}).catch(()=>{});throw error;}
finally{if(application)await application.close();}
