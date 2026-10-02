const { contextBridge, ipcRenderer } = require('electron');
// No generic invoke/send or Electron event is exposed to the page.
contextBridge.exposeInMainWorld('desktop',Object.freeze({
  listTasks:options=>ipcRenderer.invoke('desktop:list',options),
  chooseFolder:()=>ipcRenderer.invoke('desktop:folder'),
  chooseAttachment:sessionId=>ipcRenderer.invoke('desktop:attachment',sessionId),
  discardUpload:(sessionId,importId)=>ipcRenderer.invoke('desktop:discard-upload',{sessionId,importId}),
  chooseRecentWorkspace:path=>ipcRenderer.invoke('desktop:recent-folder',path),
  appInfo:()=>ipcRenderer.invoke('desktop:app-info'),
  copyText:text=>ipcRenderer.invoke('desktop:copy-text',text),
  createTask:selection=>ipcRenderer.invoke('desktop:create',selection),
  openTask:task=>ipcRenderer.invoke('desktop:open',task),
  snapshot:sessionId=>ipcRenderer.invoke('desktop:snapshot',sessionId),
  runtimeInfo:()=>ipcRenderer.invoke('desktop:info'),
  deliver:envelope=>ipcRenderer.invoke('desktop:deliver',envelope),
  browserState:()=>ipcRenderer.invoke('desktop:browser-state'),
  browserCommand:command=>ipcRenderer.invoke('desktop:browser-command',command),
  browserSurface:surface=>ipcRenderer.invoke('desktop:browser-surface',surface),
  onBrowserState:callback=>{const listener=(_event,state)=>callback(state);ipcRenderer.on('desktop:browser-event',listener);return ()=>ipcRenderer.removeListener('desktop:browser-event',listener);},
  onOpenSettings:callback=>{const listener=()=>callback();ipcRenderer.on('desktop:open-settings',listener);return ()=>ipcRenderer.removeListener('desktop:open-settings',listener);},
  onEvent:callback=>{const listener=(_event,frame)=>callback(frame);ipcRenderer.on('desktop:event',listener);return ()=>ipcRenderer.removeListener('desktop:event',listener);}
}));
