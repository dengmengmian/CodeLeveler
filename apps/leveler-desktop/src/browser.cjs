const { randomUUID }=require('node:crypto');
const {MAX_TABS,normalizeURL,isNavigationAllowed,validateCommand,validateSurface}=require('./browser-policy.cjs');
/** @typedef {{id:string,view:Electron.WebContentsView,url:string,error:string|null,navigation:number}} ManualTab */
/** @typedef {{id:string,title:string,url:string,loading:boolean,canGoBack:boolean,canGoForward:boolean,error:string|null}} BrowserTabState */
/** @typedef {{tabs:BrowserTabState[],selectedId:string|null}} BrowserState */

// These views belong to the USER'S manual browser. Runtime Agent browser commands
// still target the existing Rust browser backend; there is no binding between them.
class ManualBrowser {
  /** @param {{window:Electron.BrowserWindow,WebContentsView:typeof import('electron').WebContentsView,session:typeof import('electron').session,shell:Pick<Electron.Shell,'openExternal'>,onState:(state:BrowserState)=>void}} dependencies */
  constructor({window,WebContentsView,session,shell,onState}){
    this.window=window;this.WebContentsView=WebContentsView;this.shell=shell;this.onState=onState;
    /** @type {Map<string,ManualTab>} */ this.tabs=new Map();
    /** @type {string|null} */ this.selectedId=null;
    /** @type {import('./browser-policy.cjs').BrowserSurface|null} */ this.surfaceState=null;
    this.disposed=false;
    this.session=session.fromPartition(`leveler-manual-browser-${randomUUID()}`,{cache:false});
    this.session.setPermissionRequestHandler((_contents,_permission,callback)=>callback(false));
    this.session.setPermissionCheckHandler(()=>false);
    this.session.setDevicePermissionHandler(()=>false);
    this.session.on('will-download',event=>event.preventDefault());
    // The UI's privileged protocol handler is not installed in this session.
    // Reject it here as well, including subresources and worker fetches.
    this.session.webRequest.onBeforeRequest((details,callback)=>{
      const isDocument=details.resourceType==='mainFrame'||details.resourceType==='subFrame';
      let allowed=isNavigationAllowed(details.url);
      if(!isDocument){try{allowed=['http:','https:','ws:','wss:','data:','blob:'].includes(new URL(details.url).protocol);}catch{allowed=false;}}
      callback({cancel:!allowed});
    });
  }
  /** @returns {BrowserState} */
  state(){
    return {tabs:[...this.tabs.values()].map(tab=>{
      const wc=tab.view.webContents;const live=!wc.isDestroyed();
      return {id:tab.id,title:live?wc.getTitle().slice(0,512):'',url:tab.url,loading:live&&wc.isLoading(),canGoBack:live&&wc.navigationHistory.canGoBack(),canGoForward:live&&wc.navigationHistory.canGoForward(),error:tab.error};
    }),selectedId:this.selectedId};
  }
  publish(){if(!this.disposed){this.layout();this.onState(this.state());}}
  layout(){
    const [width,height]=this.window.getContentSize();const surface=this.surfaceState;
    for(const tab of this.tabs.values()){
      const bounds=surface?.bounds;
      const visible=!!(surface?.visible&&tab.id===this.selectedId&&!tab.error&&bounds&&bounds.x<width&&bounds.y<height);
      // Reclip on resize before the renderer sends its next measured rectangle.
      if(visible)tab.view.setBounds({...bounds,width:Math.min(bounds.width,width-bounds.x),height:Math.min(bounds.height,height-bounds.y)});
      tab.view.setVisible(visible);
    }
  }
  /** @param {unknown} value */
  surface(value){
    if(this.disposed)throw new Error('Browser is closed');
    const [width,height]=this.window.getContentSize();this.surfaceState=validateSurface(value,{width,height});this.layout();return this.state();
  }
  add(){
    if(this.tabs.size>=MAX_TABS)throw new Error(`最多打开 ${MAX_TABS} 个浏览器标签`);
    const view=new this.WebContentsView({webPreferences:{session:this.session,sandbox:true,contextIsolation:true,nodeIntegration:false,nodeIntegrationInSubFrames:false,nodeIntegrationInWorker:false,webviewTag:false,webSecurity:true,allowRunningInsecureContent:false,navigateOnDragDrop:false}});
    /** @type {ManualTab} */ const tab={id:randomUUID(),view,url:'about:blank',error:null,navigation:0};const wc=view.webContents;
    this.tabs.set(tab.id,tab);this.selectedId=tab.id;view.setVisible(false);this.window.contentView.addChildView(view);
    wc.setWindowOpenHandler(()=>({action:'deny'}));
    wc.on('will-attach-webview',event=>event.preventDefault());
    /** @param {Electron.Event & {url?:string,isMainFrame?:boolean}} event @param {string} [url] */
    const guard=(event,url)=>{const target=event.url??url;if(!isNavigationAllowed(target)){event.preventDefault();if(event.isMainFrame!==false){tab.error='已阻止不受支持的网页协议';this.publish();}}};
    wc.on('will-navigate',guard);wc.on('will-frame-navigate',guard);wc.on('will-redirect',guard);
    wc.on('did-start-loading',()=>this.publish());wc.on('did-stop-loading',()=>this.publish());wc.on('page-title-updated',()=>this.publish());
    /** @param {Electron.Event} _event @param {string} url */
    const navigated=(_event,url)=>{tab.url=url;tab.error=null;this.publish();};
    wc.on('did-navigate',navigated);wc.on('did-navigate-in-page',(_event,url,isMainFrame)=>{if(isMainFrame)navigated(_event,url);});
    wc.on('did-fail-load',(_event,code,description,_url,isMainFrame)=>{
      if(isMainFrame&&code!==-3){tab.error=`${description.slice(0,256)} (${code})`;this.publish();}
    });
    wc.on('render-process-gone',()=>{tab.error='浏览器页面进程已退出，请重新加载';this.publish();});
    this.publish();return tab;
  }
  /** @param {ManualTab} tab @param {string} url */
  async navigate(tab,url){
    const navigation=++tab.navigation;tab.url=url;tab.error=null;this.publish();
    try{await tab.view.webContents.loadURL(url);}catch(error){
      const code=error&&typeof error==='object'&&'code' in error?String(error.code):'网页无法打开';
      if(!this.disposed&&this.tabs.has(tab.id)&&navigation===tab.navigation&&code!=='ERR_ABORTED'){
        // A failed navigation is a UI error, never a successful browser result.
        tab.error=code.slice(0,256);this.publish();
      }
    }
  }
  /** @param {unknown} input */
  async command(input){
    if(this.disposed)throw new Error('Browser is closed');
    const command=validateCommand(input);
    if(command.type==='new'){const tab=this.add();await this.navigate(tab,command.url??'about:blank');return this.state();}
    const tab=this.tabs.get(command.id??this.selectedId??'');if(!tab)throw new Error('Browser tab is no longer available');
    const wc=tab.view.webContents;
    if(command.type==='select')this.selectedId=tab.id;
    else if(command.type==='close'){
      this.tabs.delete(tab.id);this.window.contentView.removeChildView(tab.view);wc.close();
      if(this.selectedId===tab.id)this.selectedId=[...this.tabs.keys()].at(-1)??null;
    } else if(command.type==='navigate')await this.navigate(tab,normalizeURL(command.url));
    else if(command.type==='back'){tab.error=null;if(wc.navigationHistory.canGoBack())wc.navigationHistory.goBack();}
    else if(command.type==='forward'){tab.error=null;if(wc.navigationHistory.canGoForward())wc.navigationHistory.goForward();}
    else if(command.type==='reload'){tab.error=null;wc.reload();}
    else if(command.type==='external'){
      // Only an explicit trusted UI command opens the observed current page.
      // Never open popup targets or accept an arbitrary external URL argument.
      await this.shell.openExternal(normalizeURL(wc.getURL()));
    }
    this.publish();return this.state();
  }
  destroy(){
    if(this.disposed)return;this.disposed=true;
    for(const tab of this.tabs.values()){this.window.contentView.removeChildView(tab.view);if(!tab.view.webContents.isDestroyed())tab.view.webContents.close();}
    this.tabs.clear();this.selectedId=null;
  }
}
module.exports={ManualBrowser};
