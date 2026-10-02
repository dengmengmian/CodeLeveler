import {test} from 'node:test';
import assert from 'node:assert/strict';
import {EventEmitter} from 'node:events';
import {ManualBrowser} from '../src/browser.cjs';
function fixture(){
 const views=[];const sessions=[];const external=[];const updates=[];const children=new Set();
 const window={getContentSize:()=>[1000,700],contentView:{addChildView:view=>children.add(view),removeChildView:view=>children.delete(view)}};
 class WebContentsView {
  constructor(options){this.options=options;views.push(this);const wc=this.webContents=new EventEmitter();wc.url='';wc.title='';wc.loading=false;wc.destroyed=false;wc.getURL=()=>wc.url;wc.getTitle=()=>wc.title;wc.isLoading=()=>wc.loading;wc.isDestroyed=()=>wc.destroyed;wc.close=()=>{wc.destroyed=true;};wc.loadURL=async url=>{wc.url=url;wc.emit('did-navigate',{},url);};wc.reload=()=>{wc.reloaded=true;};wc.setWindowOpenHandler=fn=>{wc.openHandler=fn;};wc.navigationHistory={canGoBack:()=>true,canGoForward:()=>true,goBack:()=>{wc.back=true;},goForward:()=>{wc.forward=true;}};
  }
  setVisible(value){this.visible=value;}
  setBounds(value){this.bounds=value;}
 }
 const session={fromPartition:(partition,options)=>{const ses=new EventEmitter();sessions.push({partition,options,ses});ses.setPermissionRequestHandler=fn=>{ses.request=fn;};ses.setPermissionCheckHandler=fn=>{ses.check=fn;};ses.setDevicePermissionHandler=fn=>{ses.device=fn;};ses.webRequest={onBeforeRequest:fn=>{ses.before=fn;}};return ses;}};
 const browser=new ManualBrowser({window,WebContentsView,session,shell:{openExternal:async url=>external.push(url)},onState:state=>updates.push(state)});
 return {browser,views,sessions,external,updates,children};
}
test('manual tabs are real independent views with isolated session and no privileged preload',async()=>{
 const f=fixture();assert.equal(f.browser.state().tabs.length,0);
 const first=await f.browser.command({type:'new',url:'example.com'});assert.equal(first.tabs[0].url,'https://example.com/');
 const prefs=f.views[0].options.webPreferences;assert.equal(prefs.nodeIntegration,false);assert.equal(prefs.sandbox,true);assert.equal(prefs.contextIsolation,true);assert.equal(prefs.webSecurity,true);assert.equal(prefs.preload,undefined);assert.equal(prefs.webviewTag,false);
 assert.ok(!f.sessions[0].partition.startsWith('persist:'));assert.equal(prefs.session,f.sessions[0].ses);
 const second=await f.browser.command({type:'new'});assert.equal(second.tabs.length,2);assert.notEqual(second.selectedId,first.selectedId);
 await f.browser.command({type:'select',id:first.selectedId});assert.equal(f.browser.state().selectedId,first.selectedId);
 f.browser.surface({visible:true,bounds:{x:600,y:100,width:400,height:600}});assert.equal(f.views[0].visible,true);assert.equal(f.views[1].visible,false);
 await f.browser.command({type:'close',id:first.selectedId});assert.equal(f.views[0].webContents.destroyed,true);assert.equal(f.browser.state().tabs.length,1);
 f.browser.destroy();assert.equal(f.views[1].webContents.destroyed,true);assert.equal(f.children.size,0);
});
test('popup/download/permission/protocol requests cannot escape the browser',async()=>{
 const f=fixture();await f.browser.command({type:'new'});const wc=f.views[0].webContents;const ses=f.sessions[0].ses;
 assert.deepEqual(wc.openHandler({url:'https://example.com'}),{action:'deny'});
 let permission;ses.request(wc,'clipboard-read',value=>{permission=value;});assert.equal(permission,false);assert.equal(ses.check(),false);assert.equal(ses.device(),false);
 let prevented=false;ses.emit('will-download',{preventDefault:()=>{prevented=true;}});assert.equal(prevented,true);
 for(const eventName of ['will-frame-navigate','will-navigate','will-redirect'])for(const url of ['file:///tmp/a','leveler-desktop://desktop/index.html','mailto:a@example.com','data:text/html,x']){
  let denied=false;wc.emit(eventName,{url,preventDefault:()=>{denied=true;}},url);assert.equal(denied,true,`${eventName}: ${url}`);
 }
 let cancelled;ses.before({url:'leveler-desktop://desktop/index.html',resourceType:'subFrame'},result=>{cancelled=result.cancel;});assert.equal(cancelled,true);
 assert.equal(f.external.length,0);
});
test('commands validate tab identity, cap resources, and external opening is explicit current HTTP page only',async()=>{
 const f=fixture();await f.browser.command({type:'new',url:'https://example.com'});const id=f.browser.state().selectedId;
 await assert.rejects(f.browser.command({type:'select',id:'missing'}));await assert.rejects(f.browser.command({type:'navigate',url:'file:///etc/passwd'}));
 await f.browser.command({type:'back',id});await f.browser.command({type:'forward',id});await f.browser.command({type:'reload',id});assert.equal(f.views[0].webContents.back,true);assert.equal(f.views[0].webContents.forward,true);assert.equal(f.views[0].webContents.reloaded,true);
 await f.browser.command({type:'external',id});assert.deepEqual(f.external,['https://example.com/']);
 for(let i=1;i<8;i++)await f.browser.command({type:'new'});await assert.rejects(f.browser.command({type:'new'}));
 await assert.rejects(f.browser.command({type:'external'}));
});
test('failure and navigation facts reach UI, hidden surfaces stay hidden, resize clips native view',async()=>{
 const f=fixture();await f.browser.command({type:'new',url:'https://example.com'});const wc=f.views[0].webContents;
 f.browser.surface({visible:true,bounds:{x:600,y:100,width:400,height:600}});wc.emit('did-fail-load',{},-105,'ERR_NAME_NOT_RESOLVED','https://example.com/',true);assert.match(f.browser.state().tabs[0].error,/ERR_NAME_NOT_RESOLVED/);assert.equal(f.views[0].visible,false);
 await f.browser.command({type:'navigate',url:'https://example.org'});assert.equal(f.browser.state().tabs[0].error,null);assert.equal(f.views[0].visible,true);
 f.browser.surface({visible:false,bounds:{x:600,y:100,width:400,height:600}});assert.equal(f.views[0].visible,false);
 assert.throws(()=>f.browser.surface({visible:true,bounds:{x:999,y:0,width:100,height:1}}));
});
