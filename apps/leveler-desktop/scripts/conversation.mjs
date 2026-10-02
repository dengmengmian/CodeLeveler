// Real Runtime + test-only streaming provider. Markdown is returned through the
// real model protocol and never injected into the UI DOM by the harness.
import {_electron as electron} from 'playwright';
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {mkdtemp,mkdir,writeFile} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';
import path from 'node:path';
const root=fileURLToPath(new URL('..',import.meta.url));
const temporary=await mkdtemp('/tmp/leveler-desktop-conversation-'),home=path.join(temporary,'home');await mkdir(home);
const output=process.env.LEVELER_ACCEPTANCE_OUTPUT||path.join(root,'acceptance-output/desktop');await mkdir(output,{recursive:true});
const literalCode='const sample = "<script>window.__markdownExecuted = true</script>";\nconsole.log(sample);';
let rich='',providerCalls=0,imageRequests=0,documentRequests=0,streamWrites=0;const providerErrors=[];
const server=createServer(async(request,response)=>{
 try{
  if(request.url==='/track.png'){imageRequests++;response.writeHead(200,{'content-type':'image/png'});response.end();return;}
  if(request.url==='/docs'){documentRequests++;response.writeHead(200,{'content-type':'text/html'});response.end('<!doctype html><title>Conversation linked page</title><h1>Actual manual browser page</h1>');return;}
  if(request.method!=='POST'){response.writeHead(404);response.end();return;}
  let text='';for await(const chunk of request)text+=chunk;const body=JSON.parse(text);
  if(!(body.tools??[]).length){response.writeHead(200,{'content-type':'application/json'});response.end(JSON.stringify({id:'memory',choices:[{index:0,message:{role:'assistant',content:'{"candidates":[]}'},finish_reason:'stop'}]}));return;}
  providerCalls++;const users=body.messages.filter(m=>m.role==='user');const long=String(users.at(-1)?.content).includes('conversation-long');
  const answer=long?'# Long streaming response\n\n'+Array.from({length:180},(_,i)=>`第 ${i+1} 段：**真实流式测试**，用于验证会话滚动和 Composer 并发输入。\n\n`).join('')+'\nLONG_STREAM_COMPLETE':rich;
  response.writeHead(200,{'content-type':'text/event-stream'});
  const write=(delta,finish_reason=null)=>{streamWrites++;response.write('data: '+JSON.stringify({id:'conversation-fixture',object:'chat.completion.chunk',created:1,model:'fixture-model',choices:[{index:0,delta,finish_reason}]})+'\n\n');};
  for(let offset=0;offset<answer.length;offset+=long?80:45){write({content:answer.slice(offset,offset+(long?80:45))});await new Promise(resolve=>setTimeout(resolve,long?30:20));}
  write({},'stop');response.end('data: [DONE]\n\n');
 }catch(error){providerErrors.push(error.message);response.destroy(error);}
});await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const baseURL=`http://127.0.0.1:${server.address().port}`;
rich=`# Rich conversation acceptance\n\nThis is **rendered bold** and *rendered emphasis*, with \`inline code\`.\n\n- First bullet\n- Second bullet\n\n1. First ordered\n2. Second ordered\n\n> A visible quoted result.\n\n\`\`\`js\n${literalCode}\n\`\`\`\n\n| Name | Count | Alpha | Beta | Gamma | Delta | Epsilon | Zeta |\n| --- | --- | --- | --- | --- | --- | --- | --- |\n| Files | 3 | one | two | three | four | five | six |\n\n[Safe documentation](${baseURL}/docs)\n\n<script>window.__markdownExecuted = true</script>\n\n<img src="${baseURL}/track.png" onerror="window.__markdownExecuted = true">\n\n[Dangerous link](javascript:alert%281%29)\n\n![External tracking image](${baseURL}/track.png)\n\nRICH_STREAM_COMPLETE`;
await writeFile(path.join(home,'config.toml'),`default_model = "fixture/m"\n[providers.fixture]\nprotocol = "openai_chat"\nbase_url = "${baseURL}"\napi_key = "test-only-fixture"\n[models.m]\nprovider = "fixture"\nmodel_id = "fixture-model"\ncontext_window = 131072\n`,{mode:0o600});
let application,page,clipboardCaptured=false;const pageErrors=[],evidence={scope:'real Desktop / Runtime streaming test-only provider, isolated home',temporary_home:home};
async function capture(name){await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));await page.screenshot();const image=await application.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));await writeFile(path.join(output,name+'.png'),Buffer.from(image,'base64'));}
async function settled(marker){await page.waitForFunction(marker=>document.querySelector('.message.assistant:last-of-type .body')?.textContent.includes(marker)&&['已回答','已完成'].includes(document.querySelector('#status').textContent),marker,{timeout:60000});}
try{
 application=await electron.launch({args:[root,`--user-data-dir=${path.join(temporary,'electron-profile')}`],env:{...process.env,ELECTRON_RUN_AS_NODE:undefined,LEVELER_HOME:home,LEVELER_CONFIG_DIR:undefined,LEVELER_BINARY:path.resolve(root,'../../target/debug/leveler'),LEVELER_DAEMON_IDLE_TIMEOUT_SECS:'10'}});page=await application.firstWindow();page.setDefaultTimeout(15000);page.on('pageerror',error=>pageErrors.push(error.message));await page.waitForFunction(()=>window.desktop&&document.querySelector('#refresh')?.disabled===false);
 await page.locator('#message').fill('conversation-rich: render the structured answer');await page.locator('#send').click();await settled('RICH_STREAM_COMPLETE');
 const body=page.locator('.message.assistant .body').last();
 for(const tag of ['h1','strong','em','ul','ol','blockquote','pre code','table th','table td'])assert.ok(await body.locator(tag).count(),`${tag} missing from real assistant response`);
 assert.equal(await body.locator('strong').first().textContent(),'rendered bold');assert.equal(await body.locator('pre code').textContent(),literalCode);
 assert.equal(await body.locator('script,img,svg,iframe,object').count(),0);
 assert.equal(await page.evaluate(()=>typeof window.__markdownExecuted),'undefined');assert.equal(imageRequests,0);
 const badLinks=await body.locator('a[href]').evaluateAll(nodes=>nodes.map(n=>n.getAttribute('href')).filter(href=>!/^https?:\/\//i.test(href)));assert.deepEqual(badLinks,[]);
 await capture('conversation-rich-markdown');
 await application.evaluate(({BrowserWindow})=>BrowserWindow.getAllWindows()[0].setSize(700,860));await page.waitForFunction(()=>innerWidth===700);
 const narrow=await page.evaluate(()=>({mainOverflow:document.documentElement.scrollWidth>innerWidth,codeOverflow:getComputedStyle(document.querySelector('.message.assistant pre')).overflowX,tableOverflow:getComputedStyle(document.querySelector('.message.assistant table')).overflowX,codeScrolls:document.querySelector('.message.assistant pre').scrollWidth>document.querySelector('.message.assistant pre').clientWidth,tableScrolls:document.querySelector('.message.assistant table').scrollWidth>document.querySelector('.message.assistant table').clientWidth}));assert.equal(narrow.mainOverflow,false);assert.equal(narrow.codeOverflow,'auto');assert.equal(narrow.tableOverflow,'auto');assert.equal(narrow.codeScrolls,true);assert.equal(narrow.tableScrolls,true);evidence.narrow=narrow;await body.locator('table').evaluate(n=>n.scrollIntoView({block:'center'}));await capture('conversation-rich-narrow');await application.evaluate(({BrowserWindow})=>BrowserWindow.getAllWindows()[0].setSize(1280,860));
 // UI copy actions must put the actual content on the operating-system clipboard.
 await application.evaluate(async({clipboard,ClipboardItem})=>{const saved=[];for(const item of await clipboard.read()){const data=Object.create(null);for(const type of item.types)data[type]=await item.getType(type);saved.push(new ClipboardItem(data));}globalThis.__conversationClipboard=saved;});clipboardCaptured=true;
 const message=body.locator('..');const copyCode=message.locator('[data-copy-code]');await copyCode.click();await page.waitForFunction(()=>['已复制','复制失败'].includes(document.querySelector('.message.assistant [data-copy-code]').textContent));assert.equal(await copyCode.textContent(),'已复制',await copyCode.getAttribute('title'));const copiedCode=await application.evaluate(({clipboard})=>clipboard.readText());assert.equal(copiedCode,literalCode);
 const copyMessage=message.locator('[data-copy-message]');await copyMessage.click();await page.waitForFunction(()=>['已复制','复制失败'].includes(document.querySelector('.message.assistant [data-copy-message]').textContent));assert.equal(await copyMessage.textContent(),'已复制');const copiedMessage=await application.evaluate(({clipboard})=>clipboard.readText());assert.equal(copiedMessage,rich);
 // Explicit test-only API fault seam; production failure feedback is unchanged.
 await application.evaluate(({clipboard})=>{globalThis.__conversationOriginalWriteText=clipboard.writeText;clipboard.writeText=async()=>{throw new Error('Test-only clipboard write rejected');};});
 try{await copyCode.click();await page.waitForFunction(()=>document.querySelector('.message.assistant [data-copy-code]').textContent==='复制失败');assert.match(await copyCode.getAttribute('title'),/Test-only clipboard write rejected/);}finally{await application.evaluate(({clipboard})=>{clipboard.writeText=globalThis.__conversationOriginalWriteText;delete globalThis.__conversationOriginalWriteText;});}evidence.copy_failure_feedback_test_seam=true;
 await page.locator('#conversation-search-toggle').click();await page.locator('#conversation-query').fill('rendered bold');await page.waitForFunction(()=>document.querySelector('#conversation-match-count').textContent==='1 / 1');
 assert.equal(await body.locator('strong').first().textContent(),'rendered bold');await capture('conversation-rich-search');await page.locator('#conversation-search-close').click();
 await body.locator('button.markdown-link').click();await page.waitForFunction(async()=>{const state=await window.desktop.browserState();return state.tabs.some(tab=>tab.title==='Conversation linked page');});const nativeState=await page.evaluate(()=>window.desktop.browserState());assert.equal(nativeState.tabs.length,1);assert.equal(nativeState.tabs[0].url,baseURL+'/docs');assert.equal(documentRequests,1);evidence.safe_link_native_manual_browser=true;await page.locator('#workbench-close').click();
 await page.locator('#message').fill('conversation-long: stream long Markdown');await page.locator('#send').click();await page.waitForFunction(()=>document.querySelectorAll('.message.assistant').length===2&&document.querySelector('.message.assistant:last-of-type .body')?.textContent.includes('第 5 段'));
 const start=Date.now();await page.locator('#message').fill('Draft remains editable while streaming');assert.equal(await page.locator('#message').inputValue(),'Draft remains editable while streaming');evidence.streaming_input_ms=Date.now()-start;assert.ok(evidence.streaming_input_ms<1500);
 const charsBefore=await page.locator('.message.assistant .body').last().evaluate(n=>n.textContent.length);await page.evaluate(()=>{document.querySelector('#conversation').scrollTop=0;});await page.waitForFunction(chars=>document.querySelector('.message.assistant:last-of-type .body').textContent.length>chars+200,charsBefore);assert.ok(await page.locator('#conversation').evaluate(n=>n.scrollTop<100),'incoming chunks must preserve the reader position at the top');await capture('conversation-streaming-top');
 await settled('LONG_STREAM_COMPLETE');assert.equal(await page.locator('#message').inputValue(),'Draft remains editable while streaming');evidence.final_reader_scroll_top=await page.locator('#conversation').evaluate(n=>n.scrollTop);assert.ok(evidence.final_reader_scroll_top<100,'terminal refresh must preserve reader position');
 assert.ok(await page.locator('.message.assistant').last().locator('strong').count()>=180);assert.ok(await page.locator('#conversation').evaluate(n=>n.scrollHeight>n.clientHeight));
 assert.equal(imageRequests,0);assert.deepEqual(providerErrors,[]);assert.deepEqual(pageErrors,[]);
 Object.assign(evidence,{semantic_markdown:true,code_literal_preserved:true,message_clipboard_exact:true,code_clipboard_exact:true,rendered_text_search:true,raw_html_and_script_inert:true,image_nodes:0,image_requests:imageRequests,provider_calls:providerCalls,stream_writes:streamWrites,long_paragraphs:180,draft_preserved:true,page_errors:pageErrors,provider_errors:providerErrors,passed:true});await writeFile(path.join(output,'conversation-evidence.json'),JSON.stringify(evidence,null,2)+'\n');console.log(JSON.stringify(evidence,null,2));
}catch(error){Object.assign(evidence,{passed:false,error:error.message,page_errors:pageErrors,provider_errors:providerErrors});await writeFile(path.join(output,'conversation-evidence.json'),JSON.stringify(evidence,null,2)+'\n');if(page&&!page.isClosed())await capture('conversation-failure').catch(()=>{});throw error;}
finally{
 try{if(application&&clipboardCaptured)await application.evaluate(async({clipboard})=>{const original=globalThis.__conversationClipboard;if(original.length)await clipboard.write(original);else clipboard.clear();delete globalThis.__conversationClipboard;});}
 finally{if(application)await application.close();await new Promise(resolve=>server.close(resolve));}
}
