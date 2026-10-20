const { contextBridge, ipcRenderer } = require('electron');
// Electron prefixes every rejected invoke with the internal channel name.
// That is transport detail, not product feedback: keep only the Main error.
const invoke = (...args) => ipcRenderer.invoke(...args).catch(error => {
  const message = String(error?.message ?? error);
  const match = /^Error invoking remote method '[^']+': (?:Error: )?([\s\S]*)$/.exec(message);
  throw match ? new Error(match[1]) : error;
});
// No generic invoke/send or Electron event is exposed to the page.
contextBridge.exposeInMainWorld('desktop',Object.freeze({
  // Static host fact for platform chrome (traffic-light clearance, vibrancy).
  platform:process.platform,
  listTasks:options=>invoke('desktop:list',options),
  chooseFolder:()=>invoke('desktop:folder'),
  chooseAttachment:sessionId=>invoke('desktop:attachment',sessionId),
  discardUpload:(sessionId,importId)=>invoke('desktop:discard-upload',{sessionId,importId}),
  chooseRecentWorkspace:path=>invoke('desktop:recent-folder',path),
  appInfo:()=>invoke('desktop:app-info'),
  checkUpdate:()=>invoke('desktop:update-check'),
  onQuitInspect:callback=>{const listener=()=>callback();ipcRenderer.on('desktop:quit-inspect',listener);return ()=>ipcRenderer.removeListener('desktop:quit-inspect',listener);},
  quitInspectResult:running=>ipcRenderer.send('desktop:quit-inspect-result',{running:running===true}),
  copyText:text=>invoke('desktop:copy-text',text),
  createTerminal:request=>invoke('desktop:terminal-create',request),
  writeTerminal:request=>invoke('desktop:terminal-write',request),
  resizeTerminal:request=>invoke('desktop:terminal-resize',request),
  closeTerminal:request=>invoke('desktop:terminal-close',request),
  observeTerminal:request=>invoke('desktop:terminal-observe',request),
  filesList:request=>invoke('desktop:files-list',request),
  filesRead:request=>invoke('desktop:files-read',request),
  filesImage:request=>invoke('desktop:files-image',request),
  openExternal:url=>invoke('desktop:open-external',url),
  openConfig:()=>invoke('desktop:open-config'),
  openInAppApps:()=>invoke('desktop:open-in-app-apps'),
  openInApp:request=>invoke('desktop:open-in-app',request),
  createTask:(selection,requestId)=>invoke('desktop:create-identified',{selection,requestId}),
  openTask:task=>invoke('desktop:open',task),
  snapshot:sessionId=>invoke('desktop:snapshot',sessionId),
  connectionStatus:()=>invoke('desktop:connection-status'),
  reconnect:()=>invoke('desktop:reconnect'),
  runtimeInfo:()=>invoke('desktop:info'),
  deliver:envelope=>invoke('desktop:deliver',envelope),
  browserState:()=>invoke('desktop:browser-state'),
  browserCommand:command=>invoke('desktop:browser-command',command),
  browserSurface:surface=>invoke('desktop:browser-surface',surface),
  onBrowserState:callback=>{const listener=(_event,state)=>callback(state);ipcRenderer.on('desktop:browser-event',listener);return ()=>ipcRenderer.removeListener('desktop:browser-event',listener);},
  onOpenSettings:callback=>{const listener=()=>callback();ipcRenderer.on('desktop:open-settings',listener);return ()=>ipcRenderer.removeListener('desktop:open-settings',listener);},
  onEvent:callback=>{const listener=(_event,frame)=>callback(frame);ipcRenderer.on('desktop:event',listener);return ()=>ipcRenderer.removeListener('desktop:event',listener);}
}));
