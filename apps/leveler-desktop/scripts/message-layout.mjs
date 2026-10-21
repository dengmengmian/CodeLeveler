// Actual Electron + Runtime, with an explicitly test-only model provider.
// Files are produced by host apply_patch; no snapshot or DOM facts are injected.
import {_electron as electron} from 'playwright';
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {mkdtemp,mkdir,writeFile,readFile} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';
import path from 'node:path';
const root=fileURLToPath(new URL('..',import.meta.url));
const temp=await mkdtemp('/tmp/leveler-message-layout-'),home=path.join(temp,'home'),workspace=path.join(temp,'workspace');
const output=process.env.LEVELER_ACCEPTANCE_OUTPUT??path.join(root,'acceptance-output/message-layout');
await mkdir(home);await mkdir(workspace);await mkdir(output,{recursive:true});
await writeFile(path.join(workspace,'README.md'),'# Before\n');
const answer='两项修改：\n\n1. `README.md` 已更新。\n2. [源码](src/util.ts) 已创建。\n\n普通代码 `imaginary.ts` 不代表已创建文件。';
const patch='*** Begin Patch\n*** Update File: README.md\n@@\n-# Before\n+# After\n*** Add File: src/util.ts\n+export const add = (a: number, b: number) => a + b;\n*** End Patch';
let calls=0;const errors=[];
const server=createServer(async(req,res)=>{
 try{let raw='';for await(const chunk of req)raw+=chunk;const body=JSON.parse(raw);
 if(!body.tools?.length){res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify({id:'memory',choices:[{index:0,message:{role:'assistant',content:'{"candidates":[]}'},finish_reason:'stop'}]}));return;}
 calls++;res.writeHead(200,{'content-type':'text/event-stream'});
 const frame=(delta,finish_reason=null)=>res.write('data: '+JSON.stringify({id:'layout',object:'chat.completion.chunk',created:1,model:'m',choices:[{index:0,delta,finish_reason}]})+'\n\n');
 if(!body.messages.some(message=>message.role==='tool')){frame({tool_calls:[{index:0,id:'layout-edit',type:'function',function:{name:'apply_patch',arguments:JSON.stringify({patch})}}]});frame({},'tool_calls');}
 else{frame({content:JSON.stringify(body.messages).includes('只解释')?'第二轮没有修改。 [源码](src/util.ts) `README.md`':answer});frame({},'stop');}
 res.end('data: [DONE]\n\n');
 }catch(error){errors.push(error.message);res.destroy(error);}
});await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
await writeFile(path.join(home,'config.toml'),`default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${server.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n`,{mode:0o600});
let app,page,runtime;const evidence={scope:'real Electron / Runtime / host files; test-only model provider',workspace,passed:false};
try{
 app=await electron.launch({args:[root,`--user-data-dir=${path.join(temp,'profile')}`],env:{...process.env,ELECTRON_RUN_AS_NODE:undefined,LEVELER_HOME:home,LEVELER_CONFIG_DIR:undefined,LEVELER_BINARY:process.env.LEVELER_BINARY??path.resolve(root,'../../target/debug/leveler'),LEVELER_DAEMON_IDLE_TIMEOUT_SECS:'10'}});
 page=await app.firstWindow();page.setDefaultTimeout(20000);page.on('pageerror',e=>errors.push(e.message));
 await page.waitForFunction(()=>window.desktop&&document.querySelector('#brand-version')?.textContent.startsWith('v'));
 await app.evaluate(({dialog},folder)=>{dialog.showOpenDialog=async()=>({canceled:false,filePaths:[folder]});},workspace);
 await page.locator('#add-workspace').click();
 await page.locator('.directory-picker-modal .button.primary').filter({hasText:'使用系统选择器'}).click();
 await page.waitForSelector('.directory-picker-modal .directory-selected:not([hidden])');
 await page.locator('.directory-picker-modal .button.primary').filter({hasText:/^打开$/}).click();
 await page.waitForFunction(()=>document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
 await page.locator('#message').fill('把 README.md 改成 After，并创建相加函数 src/util.ts。');await page.locator('#send').click();
 await page.waitForFunction(()=>document.querySelector('.message.assistant .markdown')?.textContent.includes('不代表已创建文件。')&&document.querySelectorAll('.answer-file').length===2&&document.querySelectorAll('.message.assistant .markdown-file-link').length===2&&['已回答','已完成'].includes(document.querySelector('#conversation-status').textContent));
 runtime=await page.evaluate(()=>window.desktop.runtimeInfo());evidence.runtime=runtime;
 assert.equal(await readFile(path.join(workspace,'README.md'),'utf8'),'# After\n');assert.match(await readFile(path.join(workspace,'src/util.ts'),'utf8'),/export const add/);
 const assistant=page.locator('.message.assistant').last();
 assert.equal(await assistant.locator('.markdown-file-link').count(),2);
 assert.equal(await assistant.locator('.markdown code').filter({hasText:'imaginary.ts'}).count(),1);
 assert.equal(await assistant.locator('.answer-files').evaluate(n=>getComputedStyle(n).gridTemplateColumns.split(' ').length),2);
 assert.ok((await assistant.locator('.answer-file-summary').allTextContents()).every(text=>/\+\d+ −\d+/.test(text)));
 assert.equal(await page.locator('.message-time').filter({hasText:'1970'}).count(),0);
 const alignment=await page.evaluate(()=>{const u=document.querySelector('.message.user'),b=u.querySelector('.bubble'),a=document.querySelector('.message.assistant');return {userRight:Math.round(u.getBoundingClientRect().right-b.getBoundingClientRect().right),assistantOpen:getComputedStyle(a).backgroundColor,bubble:getComputedStyle(b).backgroundColor};});
 assert.equal(alignment.userRight,0);assert.equal(alignment.assistantOpen,'rgba(0, 0, 0, 0)');evidence.alignment=alignment;
 await page.screenshot({path:path.join(output,'message-layout-wide.png')});
 await assistant.locator('.markdown-file-link').first().click();await page.waitForFunction(()=>document.querySelector('.preview-body')?.textContent.includes('After'));
 evidence.inline_preview=true;
 await assistant.locator('.answer-file-preview').filter({hasText:'util.ts'}).click();await page.waitForFunction(()=>document.querySelector('.preview-body')?.textContent.includes('export const add'));
 evidence.card_preview=true;
 await assistant.locator('.answer-file-open .open-app-more').first().click();await page.getByRole('menuitem',{name:'显示文件位置'}).waitFor();await page.keyboard.press('Escape');evidence.open_in_app_menu=true;
 // Only the OS-launch IPC is stubbed: verify real UI preference, pending state,
 // and requested app id without launching an external editor during acceptance.
 const apps=await page.evaluate(()=>window.desktop.openInAppApps());
 const selectedApp=apps.find(app=>app.id!=='finder');assert.ok(selectedApp,'an alternate installed app is needed for this preference acceptance');
 await app.evaluate(({ipcMain})=>{globalThis.__layoutOpenHandler=ipcMain._invokeHandlers.get('desktop:open-in-app');globalThis.__layoutOpenRequests=[];ipcMain.removeHandler('desktop:open-in-app');ipcMain.handle('desktop:open-in-app',(_event,request)=>{globalThis.__layoutOpenRequests.push(request);return new Promise(resolve=>{globalThis.__layoutReleaseOpen=()=>resolve({ok:true});});});});
 try{
  await page.locator('#open-in-app .open-app-more').click();await page.getByRole('menuitemradio',{name:selectedApp.name,exact:true}).click();
  await page.waitForFunction(()=>[...document.querySelectorAll('.answer-file-open .open-app-primary')].every(button=>button.disabled));
  evidence.open_in_app_busy=true;
  await app.evaluate(()=>globalThis.__layoutReleaseOpen());
  await page.waitForFunction(name=>[...document.querySelectorAll('.answer-file-open .open-app-primary')].every(button=>!button.disabled&&button.getAttribute('aria-label')===`用 ${name} 打开`),selectedApp.name);
  await assistant.locator('.answer-file-open .open-app-primary').first().click();
  const request=await app.evaluate(()=>globalThis.__layoutOpenRequests.at(-1));assert.equal(request.appId,selectedApp.id);assert.match(request.path,/README\.md$/);
  await app.evaluate(()=>globalThis.__layoutReleaseOpen());evidence.open_in_app_preference_and_binding=true;evidence.external_launch_stub_only=true;
 }finally{await app.evaluate(({ipcMain})=>{ipcMain.removeHandler('desktop:open-in-app');ipcMain.handle('desktop:open-in-app',globalThis.__layoutOpenHandler);delete globalThis.__layoutOpenHandler;delete globalThis.__layoutOpenRequests;delete globalThis.__layoutReleaseOpen;});}
 const id=await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
 await page.reload();await page.locator(`.session-row[data-session-id="${id}"]`).click();await page.waitForFunction(()=>document.querySelectorAll('.answer-file').length===2);
 assert.equal(await page.locator('.answer-file-name').filter({hasText:'README.md'}).count(),1);evidence.history_replay_cards=true;
 await page.locator('#message').fill('只解释，不修改文件。');await page.locator('#send').click();
 await page.waitForFunction(()=>document.querySelectorAll('.message.assistant').length===2&&document.querySelector('.message.assistant:last-of-type .markdown')?.textContent.includes('第二轮没有修改。')&&['已回答','已完成'].includes(document.querySelector('#conversation-status').textContent));
 assert.equal(await page.locator('.message.assistant').last().locator('.answer-file').count(),0);
 assert.equal(await page.locator('.message.assistant').last().locator('.markdown-file-link').count(),1);
 assert.equal(await page.locator('.answer-file').count(),2);evidence.next_turn_no_stale_cards=true;evidence.session_id=id;
 if(await page.locator('#rightbar').isVisible())await page.locator('.pane-chrome button[aria-label="收起右侧边栏"]').first().click();
 await app.evaluate(({BrowserWindow})=>BrowserWindow.getAllWindows()[0].setSize(700,860));
 await page.waitForFunction(()=>innerWidth===700);assert.equal(await page.locator('.answer-files').evaluate(n=>getComputedStyle(n).gridTemplateColumns.split(' ').length),1);
 assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);await page.locator('.answer-files').scrollIntoViewIfNeeded();assert.equal(await page.locator('.answer-file').first().isVisible(),true);await page.evaluate(async()=>{await new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));await Promise.all(document.getAnimations().filter(animation=>animation.constructor.name==='CSSTransition').map(animation=>animation.finished));});await page.screenshot({path:path.join(output,'message-layout-narrow.png')});
 assert.deepEqual(errors,[]);Object.assign(evidence,{passed:true,confirmed_host_files:2,inline_file_chips:2,model_only_file_is_code:true,narrow_single_column:true,calls});
}catch(error){evidence.error=error.message;if(page&&!page.isClosed())evidence.observed=await page.locator('.message.assistant').allTextContents();if(page&&!page.isClosed())await page.screenshot({path:path.join(output,'message-layout-failure.png')});throw error;}
finally{if(app)await app.close();await new Promise(resolve=>server.close(resolve));if(runtime){const deadline=Date.now()+30000;let gone=false;while(Date.now()<deadline){try{process.kill(runtime.pid,0);}catch(error){if(error.code==='ESRCH'){gone=true;break;}evidence.cleanup_error=error.message;break;}await new Promise(resolve=>setTimeout(resolve,100));}evidence.runtime_idle_exit=gone;}evidence.errors=errors;await writeFile(path.join(output,'message-layout-evidence.json'),JSON.stringify(evidence,null,2)+'\n');console.log(JSON.stringify(evidence,null,2));}
