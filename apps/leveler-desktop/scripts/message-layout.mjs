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
 await page.waitForFunction(()=>document.querySelector('.message.assistant .markdown')?.textContent.includes('不代表已创建文件。')&&document.querySelector('.execution-group.execution-edit')&&['已回答','已完成'].includes(document.querySelector('#conversation-status').textContent));
 runtime=await page.evaluate(()=>window.desktop.runtimeInfo());evidence.runtime=runtime;
 assert.equal(await readFile(path.join(workspace,'README.md'),'utf8'),'# After\n');assert.match(await readFile(path.join(workspace,'src/util.ts'),'utf8'),/export const add/);
 const assistant=page.locator('.message.assistant').last();
 assert.equal(await assistant.locator('.markdown-file-link').count(),2);
 assert.equal(await assistant.locator('.markdown code').filter({hasText:'imaginary.ts'}).count(),1);
 assert.equal(await page.locator('.answer-file').count(),0,'a duplicate answer file card is painted');
 const edit=page.locator('.execution-group.execution-edit').last();
 assert.match(await edit.locator('.execution-label').innerText(),/修改 2 个文件 · \+2 \/ -1/);
 assert.equal(await edit.evaluate(node=>node.open),false,'the modification group is not collapsed by default');
 assert.equal(await page.locator('.message-time').filter({hasText:'1970'}).count(),0);
 const alignment=await page.evaluate(()=>{const u=document.querySelector('.message.user'),b=u.querySelector('.bubble'),a=document.querySelector('.message.assistant');return {userRight:Math.round(u.getBoundingClientRect().right-b.getBoundingClientRect().right),assistantOpen:getComputedStyle(a).backgroundColor,bubble:getComputedStyle(b).backgroundColor};});
 assert.equal(alignment.userRight,0);assert.equal(alignment.assistantOpen,'rgba(0, 0, 0, 0)');evidence.alignment=alignment;
 evidence.children=await page.evaluate(()=>[...document.querySelector('#conversation').children].map(node=>{const rect=node.getBoundingClientRect();return{cls:String(node.className),key:node.dataset.key??'',top:Math.round(rect.top),bottom:Math.round(rect.bottom),height:Math.round(rect.height)};}));
 evidence.user_children=await page.evaluate(()=>[...document.querySelector('.message.user').children].map(node=>{const rect=node.getBoundingClientRect();return{cls:String(node.className),top:Math.round(rect.top),bottom:Math.round(rect.bottom),height:Math.round(rect.height)};}));
 const [userRow,processRow,answerRow]=evidence.children;
 assert.equal(userRow.height,76,'user row is bubble 42 + gap 6 + actions 28');
 assert.equal(processRow.top-userRow.bottom,6,'process follows the user turn by 6px');
 assert.equal(answerRow.top-processRow.bottom,12,'an answer is separated from process by 12px');
 await page.screenshot({path:path.join(output,'message-layout-wide.png')});
 // Layout parity at the reference breakpoints. Expected values come from the
 // frozen reference CSS (ChatView/ConversationRoot), not from this render:
 // column = clamp(680, 64% of the conversation body, 920); composer card =
 // column + 32; process row = 24.
 const resize = async (width,height) => {
   await app.evaluate(({BrowserWindow},size)=>BrowserWindow.getAllWindows()[0].setContentSize(size.width,size.height),{width,height});
   await page.waitForFunction(w=>innerWidth===w,width);
   // The responsive sidebar collapse is a transition; sample the settled frame.
   await page.waitForTimeout(350);
   await page.evaluate(async()=>{await new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));await Promise.all(document.getAnimations().filter(animation=>animation.constructor.name==='CSSTransition').map(animation=>animation.finished.catch(()=>{})));});
 };
 const measure = () => page.evaluate(()=>{
   const box = node => { const rect=node.getBoundingClientRect(); return {left:Math.round(rect.left),right:Math.round(rect.right),width:Math.round(rect.width),height:Math.round(rect.height)}; };
   const main=document.querySelector('.main').getBoundingClientRect();
   return {
     inner:[innerWidth,innerHeight],
     main:{width:Math.round(main.width)},
     conversation:box(document.querySelector('#conversation')),
     bubble:box(document.querySelector('.message.user .bubble')),
     userRow:box(document.querySelector('.message.user')),
     composer:box(document.querySelector('#composer-card')),
     processRow:box(document.querySelector('.execution-group .execution-summary')),
     assistant:box(document.querySelector('.message.assistant')),
   };
 });
 const columnTarget = width => Math.min(Math.max(680,0.64*width),920);
 const layouts={};
 for(const [label,size] of [['1440',{width:1440,height:900}],['960',{width:960,height:600}]]){
   await resize(size.width,size.height);
   const m=await measure();
   const available=Math.round(m.main.width-64);
   const expected=Math.min(Math.round(columnTarget(m.main.width)),available);
   assert.ok(Math.abs(m.conversation.width-expected)<=2,`${label}: column ${m.conversation.width} vs ${expected}`);
   const cardTarget=Math.min(m.conversation.width+32,m.main.width-32);
   assert.ok(Math.abs(m.composer.width-cardTarget)<=2,`${label}: composer ${m.composer.width} vs ${cardTarget}`);
   assert.equal(m.userRow.right,m.conversation.right,`${label}: user row is not right-aligned to the column`);
   assert.equal(m.bubble.right,m.userRow.right,`${label}: bubble is not right-aligned`);
   assert.ok(Math.abs(m.processRow.height-24)<=1,`${label}: process row ${m.processRow.height} vs 24`);
   assert.equal(m.assistant.left,m.conversation.left,`${label}: assistant text is not column-aligned`);
   layouts[label]=m;
   await page.screenshot({path:path.join(output,`message-layout-${label}.png`)});
 }
 evidence.layouts=layouts;
 evidence.column_targets={wide:columnTarget(layouts['1440'].main.width),narrow:columnTarget(layouts['960'].main.width)};
 await resize(1280,820);
 await assistant.locator('.markdown-file-link').first().click();await page.waitForFunction(()=>document.querySelector('.preview-body')?.textContent.includes('After'));
 evidence.inline_preview=true;
 // The modification summary opens the confirmed Workbench, and its file rows
 // locate the selected file inside that complete Diff.
 await edit.locator('.execution-toggle').click();
 await page.waitForFunction(()=>document.querySelectorAll('.execution-group.execution-edit .execution-file-row').length===2);
 const fileRows=await page.locator('.execution-group.execution-edit .execution-file-row').allInnerTexts();
 assert.deepEqual(fileRows.map(text=>text.split('\n')[0]),['README.md','src/util.ts']);
 await page.locator('.execution-group.execution-edit .execution-file-row').filter({hasText:'src/util.ts'}).click();
 await page.waitForFunction(()=>document.querySelector('.review-scope')?.textContent.includes('已确认'));
 assert.equal(await page.locator('.review-path').innerText(),'src/util.ts');
 assert.match(await page.locator('.review-body').innerText(),/export const add/);
 evidence.card_preview=true;
 await page.locator('.review-tools button[aria-label="打开完整文件"]').click();
 await page.waitForFunction(()=>document.querySelector('.preview-body')?.textContent.includes('export const add'));
 await page.locator('.pane-tab').filter({hasText:'本次修改'}).click();
 await page.waitForFunction(()=>document.querySelector('.review-scope')?.textContent.includes('已确认'));
 // The Workbench keeps its own Open In App controls. The launch IPC itself is
 // covered by `open-in-app.test.mjs`; this layout lane only checks the entry.
 await page.locator('.open-in-app-pane .open-app-more').click();
 await page.waitForSelector('.menu [role="menuitem"], .menu [role="menuitemradio"]');
 await page.keyboard.press('Escape');evidence.open_in_app_menu=true;
 await page.locator('.pane-chrome button[aria-label="收起右侧边栏"]').first().click();
 await page.waitForFunction(()=>!document.body.classList.contains('rightbar-open'));
 const id=await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
 await page.reload();await page.locator(`.session-row[data-session-id="${id}"]`).click();
 await page.waitForFunction(()=>document.querySelector('.execution-group.execution-edit'));
 await page.locator('.execution-group.execution-edit .execution-toggle').last().click();
 await page.waitForFunction(()=>document.querySelectorAll('.execution-group.execution-edit .execution-file-row').length===2);
 assert.equal(await page.locator('.execution-file-path').filter({hasText:'README.md'}).count(),1);evidence.history_replay_cards=true;
 await page.locator('#message').fill('只解释，不修改文件。');await page.locator('#send').click();
 await page.waitForFunction(()=>document.querySelectorAll('.message.assistant').length===2&&document.querySelector('.message.assistant:last-of-type .markdown')?.textContent.includes('第二轮没有修改。')&&['已回答','已完成'].includes(document.querySelector('#conversation-status').textContent));
 assert.equal(await page.locator('.message.assistant').last().locator('.markdown-file-link').count(),1);
 assert.equal(await page.locator('.execution-group.execution-edit').count(),1,'the second turn grew a stale modification group');
 assert.equal(await page.locator('.answer-file').count(),0);evidence.next_turn_no_stale_cards=true;evidence.session_id=id;
 if(await page.locator('#rightbar').isVisible())await page.locator('.pane-chrome button[aria-label="收起右侧边栏"]').first().click();
 await app.evaluate(({BrowserWindow})=>BrowserWindow.getAllWindows()[0].setSize(700,860));
 await page.waitForFunction(()=>innerWidth===700);
 if(await page.evaluate(()=>document.body.classList.contains('sidebar-open')))await page.locator('#sidebar-toggle').click();
 await page.waitForFunction(()=>document.querySelector('#sidebar').getBoundingClientRect().right<=0);
 assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
 const narrowEdit=page.locator('.execution-group.execution-edit').last();
 if(!(await narrowEdit.evaluate(node=>node.open)))await narrowEdit.locator('.execution-toggle').click();
 await page.waitForFunction(()=>document.querySelectorAll('.execution-group.execution-edit .execution-file-row').length===2);
 await narrowEdit.scrollIntoViewIfNeeded();assert.equal(await narrowEdit.locator('.execution-file-row').first().isVisible(),true);
 await page.evaluate(async()=>{await new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));await Promise.all(document.getAnimations().filter(animation=>animation.constructor.name==='CSSTransition').map(animation=>animation.finished));});await page.screenshot({path:path.join(output,'message-layout-narrow.png')});
 assert.deepEqual(errors,[]);Object.assign(evidence,{passed:true,confirmed_host_files:2,inline_file_chips:2,model_only_file_is_code:true,narrow_single_column:true,calls});
}catch(error){evidence.error=error.message;if(page&&!page.isClosed())evidence.observed=await page.locator('.message.assistant').allTextContents();if(page&&!page.isClosed())await page.screenshot({path:path.join(output,'message-layout-failure.png')});throw error;}
finally{if(app)await app.close();await new Promise(resolve=>server.close(resolve));if(runtime){const deadline=Date.now()+30000;let gone=false;while(Date.now()<deadline){try{process.kill(runtime.pid,0);}catch(error){if(error.code==='ESRCH'){gone=true;break;}evidence.cleanup_error=error.message;break;}await new Promise(resolve=>setTimeout(resolve,100));}evidence.runtime_idle_exit=gone;}evidence.errors=errors;await writeFile(path.join(output,'message-layout-evidence.json'),JSON.stringify(evidence,null,2)+'\n');console.log(JSON.stringify(evidence,null,2));}
