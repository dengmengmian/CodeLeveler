const { app, BrowserWindow, WebContentsView, Menu, ipcMain, dialog, protocol, net, session, shell, clipboard }=require('electron');
const path=require('node:path');
const { pathToFileURL }=require('node:url');
const { randomUUID }=require('node:crypto');
const { Bridge }=require('./bridge.cjs');
const { validateEnvelope }=require('./security.cjs');
const { ManualBrowser }=require('./browser.cjs');
const { RecentWorkspaceSelection,taskListOptions }=require('./workspace-selection.cjs');
const { AttachmentIngress }=require('./attachment-ingress.cjs');
const { copyText }=require('./clipboard-write.cjs');
const { settingsMenuItem }=require('./settings-menu.cjs');
protocol.registerSchemesAsPrivileged([{scheme:'leveler-desktop',privileges:{standard:true,secure:true,supportFetchAPI:true}}]);
let window,bridge,browser;const folders=new Map();
let selectedSource=null;
const attachments=new AttachmentIngress({pickFile:async()=>{const result=await dialog.showOpenDialog(window,{title:'上传附件',properties:['openFile']});return result.canceled?null:result.filePaths[0]??null;},deliver:envelope=>bridge.request('deliver',{envelope})});
function attachmentSession(snapshot){if(selectedSource&&snapshot?.id)attachments.activate({sourceId:selectedSource,sessionId:snapshot.id,vision:snapshot.vision===true});}
const recentWorkspaces=new RecentWorkspaceSelection(folders);
function authorize(event){if(!window || event.sender!==window.webContents || event.senderFrame!==window.webContents.mainFrame || !event.senderFrame.url.startsWith('leveler-desktop://desktop/'))throw new Error('Unauthorized desktop frame');}
function handle(channel,action){ipcMain.handle(channel,(event,...args)=>{authorize(event);return action(...args);});}
function text(value,label){if(typeof value!=='string'||!value||value.length>4096)throw new Error(`Invalid ${label}`);return value;}
async function createWindow(){
  bridge=new Bridge(process.env.LEVELER_BINARY || path.resolve(__dirname,'../../../target/debug/leveler'),{onEvent:frame=>{if(frame.session_id===attachments.selected?.sessionId){if(frame.event==='snapshot')attachmentSession(frame.data);if(frame.event==='runtime'&&frame.data?.type==='session_updated')attachmentSession(frame.data.session);}attachments.observe(frame);if(window && !window.isDestroyed())window.webContents.send('desktop:event',frame);}});
  window=new BrowserWindow({width:1280,height:860,minWidth:680,minHeight:460,title:'CodeLeveler',backgroundColor:'#f8f8f6',...(process.platform==='darwin'?{titleBarStyle:'hiddenInset',trafficLightPosition:{x:14,y:16}}:{}),webPreferences:{preload:path.join(__dirname,'preload.cjs'),sandbox:true,contextIsolation:true,nodeIntegration:false,webSecurity:true}});
  browser=new ManualBrowser({window,WebContentsView,session,shell,onState:state=>{if(window&&!window.isDestroyed())window.webContents.send('desktop:browser-event',state);}});
  window.webContents.setWindowOpenHandler(()=>({action:'deny'}));
  window.webContents.on('will-navigate',event=>event.preventDefault());
  window.webContents.on('will-attach-webview',event=>event.preventDefault());
  window.on('resize',()=>browser.layout());
  window.on('close',()=>browser.destroy());
  window.on('closed',()=>{attachments.activate(null);window=null;browser=null;folders.clear();app.quit();});
  await window.loadURL('leveler-desktop://desktop/index.html');
}
app.whenReady().then(async()=>{
  const assets=new Set(['index.html','renderer.mjs','state.mjs','presentation.mjs','markdown.mjs','styles.css','command-policy.gen.mjs']);
  protocol.handle('leveler-desktop',request=>{const url=new URL(request.url);const name=url.pathname.slice(1);if(url.hostname!=='desktop')return new Response('Not found',{status:404});if(name==='packages/conversation-presentation/conversation.mjs')return net.fetch(pathToFileURL(path.join(__dirname,'../../../packages/conversation-presentation/conversation.mjs')).toString());if(name==='node_modules/marked/lib/marked.esm.js')return net.fetch(pathToFileURL(path.join(__dirname,'../node_modules/marked/lib/marked.esm.js')).toString());if(!assets.has(name))return new Response('Not found',{status:404});return net.fetch(pathToFileURL(path.join(__dirname,name)).toString());});
  session.defaultSession.setPermissionRequestHandler((_contents,_permission,callback)=>callback(false));
  session.defaultSession.setPermissionCheckHandler(()=>false);
  handle('desktop:list',options=>{const params=taskListOptions(options);return bridge.request('list_tasks',params).then(index=>{recentWorkspaces.update(index,params.include_archived);return index;});});
  handle('desktop:info',()=>bridge.request('runtime_info'));
  handle('desktop:app-info',()=>({version:app.getVersion()}));
  handle('desktop:copy-text',value=>copyText(value,clipboard));
  handle('desktop:recent-folder',path=>recentWorkspaces.select(path));
  handle('desktop:attachment',sessionId=>attachments.choose(text(sessionId,'session')));
  handle('desktop:discard-upload',value=>{if(!value||typeof value!=='object'||Array.isArray(value)||Object.keys(value).length!==2||!Object.hasOwn(value,'sessionId')||!Object.hasOwn(value,'importId'))throw new Error('Invalid upload discard');return attachments.discard(text(value.sessionId,'session'),text(value.importId,'import'));});
  handle('desktop:folder',async()=>{const result=await dialog.showOpenDialog(window,{title:'选择 Workspace 文件夹',properties:['openDirectory']});if(result.canceled)return null;const id=randomUUID();folders.set(id,result.filePaths[0]);return {id,path:result.filePaths[0]};});
  handle('desktop:create',async selection=>{let workspace=null;if(selection!==null){workspace=folders.get(selection);if(!workspace)throw new Error('请重新选择 Workspace 文件夹');}attachments.activate(null);const connection=await bridge.request('connect',{workspace});selectedSource=connection.source_id;const result=await bridge.request('create',{goal:''});attachmentSession(result.session);return result;});
  handle('desktop:create-identified',async ({selection,requestId})=>{if(typeof requestId!=='string'||!requestId)throw new Error('identified creation requires request_id');let workspace=null;if(selection!==null){workspace=folders.get(selection);if(!workspace)throw new Error('请重新选择 Workspace 文件夹');}attachments.activate(null);const connection=await bridge.request('connect',{workspace});selectedSource=connection.source_id;const result=await bridge.request('create_identified',{goal:'',request_id:requestId});attachmentSession(result.session);return result;});
  handle('desktop:open',async task=>{if(!task||typeof task!=='object')throw new Error('Invalid task');attachments.activate(null);const result=await bridge.request('open',{source_id:text(task.source_id,'source'),session_id:text(task.id,'session')});selectedSource=result.source_id;attachmentSession(result.session);return result;});
  handle('desktop:snapshot',async sessionId=>{const snapshot=await bridge.request('snapshot',{session_id:text(sessionId,'session')});attachmentSession(snapshot);return snapshot;});
  handle('desktop:deliver',envelope=>{validateEnvelope(envelope,(sessionId,ref)=>attachments.authorize(sessionId,ref));if(envelope.command.type==='submit_message'&&envelope.command.attachments?.length&&attachments.selected?.vision!==true)throw new Error('当前模型不支持图片，请切换到视觉模型或移除附件');return bridge.request('deliver',{envelope}).then(()=>({ok:true}),error=>({ok:false,error:{message:error.message,kind:error.kind ?? 'outcome_unknown'}}));});
  handle('desktop:browser-state',()=>browser.state());
  handle('desktop:browser-command',command=>browser.command(command));
  handle('desktop:browser-surface',surface=>browser.surface(surface));
  await createWindow();
  const settings=settingsMenuItem(()=>window);
  Menu.setApplicationMenu(Menu.buildFromTemplate([
    ...(process.platform==='darwin'?[{label:'CodeLeveler',submenu:[{role:'about'},{type:'separator'},settings,{type:'separator'},{role:'services'},{type:'separator'},{role:'hide'},{role:'hideOthers'},{role:'unhide'},{type:'separator'},{role:'quit'}]}]:[]),
    {label:'File',submenu:[...(process.platform==='darwin'?[]:[settings,{type:'separator'}]),{role:'close'}]},
    {role:'editMenu'},{role:'viewMenu'},{role:'windowMenu'}
  ]));
}).catch(error=>{console.error(error.message);app.quit();});
app.on('window-all-closed',()=>app.quit());
let quitting=false;
app.on('before-quit',event=>{if(bridge&&!quitting){event.preventDefault();quitting=true;bridge.close().finally(()=>app.quit());}});
