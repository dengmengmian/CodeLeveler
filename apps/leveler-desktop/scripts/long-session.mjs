// Synthetic render/interaction acceptance. This is not real Runtime dogfood.
import {_electron as electron} from 'playwright';
import assert from 'node:assert/strict';
import {mkdtemp,mkdir,writeFile,chmod} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';
import path from 'node:path';
import http from 'node:http';
import {performance} from 'node:perf_hooks';
const root=fileURLToPath(new URL('..',import.meta.url));
const output=process.env.LEVELER_ACCEPTANCE_OUTPUT||path.join(root,'acceptance-output/desktop');await mkdir(output,{recursive:true});
const temporary=await mkdtemp('/tmp/leveler-desktop-long-');const wrapper=path.join(temporary,'bridge');
const quote=value=>"'"+value.replaceAll("'","'\\''")+"'";
await writeFile(wrapper,`#!/bin/sh\nexec ${quote(process.execPath)} ${quote(path.join(root,'test/fixtures/long-session.cjs'))}\n`);await chmod(wrapper,0o700);
const server=http.createServer((request,response)=>{response.writeHead(200,{'Content-Type':'text/html'});response.end('<!doctype html><title>Synthetic browser fixture</title><h1>Real native browser content</h1><p>This is an HTTP test fixture, not an Agent browser.</p>');});await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const url=`http://127.0.0.1:${server.address().port}/`;
let application,page;const timings={},errors=[];
async function timed(name,fn){const start=performance.now();await fn();timings[name]=Math.round(performance.now()-start);assert.ok(timings[name]<3000,`${name} took ${timings[name]}ms; interaction exceeded 3s budget`);}
try{
 application=await electron.launch({args:[root,`--user-data-dir=${path.join(temporary,'electron-profile')}`],env:{...process.env,ELECTRON_RUN_AS_NODE:undefined,LEVELER_BINARY:wrapper}});application.process().stderr.on('data',data=>process.stderr.write(data));page=await application.firstWindow();page.on('console',message=>{if(message.type()==='error')console.error(message.text());});page.setDefaultTimeout(15000);page.on('pageerror',error=>errors.push(error.message));
 await application.evaluate(({BrowserWindow})=>BrowserWindow.getAllWindows()[0].setSize(1600,1000));
 await page.waitForFunction(()=>document.querySelector('.session-row[data-session-id="long"]'));
 await timed('open_history',async()=>{await page.locator('.session-row[data-session-id="long"]').click();await page.waitForFunction(()=>document.querySelectorAll('.tool-row').length===100&&document.querySelectorAll('.message').length===200&&document.querySelector('.thought-row.thought-live .thought-body')?.textContent.length>100000);});
 assert.equal(await page.locator('.message.user').count(),100);assert.equal(await page.locator('.message.assistant').count(),100);
 await timed('scroll_top',()=>page.evaluate(()=>{const list=document.querySelector('#conversation-scroll');list.scrollTop=0;return new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));}));
 await timed('expand_16k_tool',async()=>{await page.locator('.tool-row[data-tool-id="t0-2"] summary').click();assert.equal((await page.locator('.tool-row[data-tool-id="t0-2"] .tool-terminal-output').innerText()).length,16000);});
 await page.keyboard.press('Meta+T');await page.waitForFunction(()=>window.desktop.browserState().then(state=>state.tabs.length===1));await page.locator('.browser-address-wrap input').fill(url);await page.locator('.browser-address-wrap input').press('Enter');
 await page.waitForFunction(url=>window.desktop.browserState().then(state=>state.tabs.some(tab=>tab.url===url&&!tab.loading)),url);
 await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));
 await page.screenshot({path:path.join(output,'long-session-browser.png')});
 await page.locator('#message').fill('synthetic streaming start');await page.locator('#send').click();
 await page.waitForFunction(()=>document.querySelector('[data-message-id="a99"] .markdown').textContent.includes('streaming'));
 await timed('type_while_browser_and_streaming',async()=>{await page.locator('#message').fill('输入响应测试 during streaming');assert.equal(await page.locator('#message').inputValue(),'输入响应测试 during streaming');});
 await timed('scroll_during_streaming',()=>page.evaluate(()=>{const list=document.querySelector('#conversation-scroll');list.scrollTop=list.scrollHeight;return new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));}));
 await page.screenshot({path:path.join(output,'long-session-streaming.png')});
 await timed('switch_short_task',async()=>{await page.locator('.session-row[data-session-id="short"]').click();await page.getByText('Short task switch target',{exact:true}).waitFor();assert.equal(await page.locator('.tool-row').count(),0);});
 await timed('restore_long_task',async()=>{await page.locator('.session-row[data-session-id="long"]').click();await page.waitForFunction(()=>document.querySelectorAll('.tool-row').length===100);});
 assert.deepEqual(errors,[]);
 const evidence={scope:'test-only synthetic JSONL bridge; not real Runtime or Agent Browser dogfood',turns:100,messages:200,tools:300,visible_tool_rows:100,folded_exploration_members:200,each_completed_tool_output_chars:16000,reasoning_chars:await page.locator('.thought-row.thought-live .thought-body').evaluate(n=>n.textContent.length),native_browser:'security preferences covered by browser-dock.mjs',timings_ms:timings,interaction_budget_ms:3000,page_errors:errors,passed:true};
 await writeFile(path.join(output,'long-session-evidence.json'),JSON.stringify(evidence,null,2)+'\n');console.log(JSON.stringify(evidence,null,2));
}catch(error){if(page&&!page.isClosed())await page.screenshot({path:path.join(output,'long-session-failure.png')}).catch(()=>{});await writeFile(path.join(output,'long-session-evidence.json'),JSON.stringify({passed:false,error:error.message,timings_ms:timings,page_errors:errors},null,2)+'\n');throw error;}
finally{if(application)await application.evaluate(({app})=>app.exit(0)).catch(()=>{});await new Promise(resolve=>server.close(resolve));}
