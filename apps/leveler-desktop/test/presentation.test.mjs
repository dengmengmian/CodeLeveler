import {test} from 'node:test';
import assert from 'node:assert/strict';
import {modelLabel,workspaceLabel} from '../src/presentation.mjs';
test('model chrome renders the actual model reference without object coercion',()=>{
 assert.equal(modelLabel({provider:'fixture',model:'fixture-model'}),'fixture-model');
 assert.equal(modelLabel(null),'');
 assert.equal(modelLabel({provider:'fixture'}),'');
});
test('workspace chrome shows the name and keeps No Workspace explicit',()=>{
 assert.equal(workspaceLabel('/Users/name/Develop/workspace'),'workspace');
 assert.equal(workspaceLabel('C:\\Users\\name\\Project'),'Project');
 assert.equal(workspaceLabel(null),'No Workspace');
});
import {toolLabel,toolTarget,browserActivity} from '../src/presentation.mjs';
test('real tool names and command arguments have useful product labels',()=>{
 assert.equal(toolLabel({name:'shell_command'}),'运行命令');
 assert.equal(toolLabel({name:'list_files'}),'查看目录');
 assert.equal(toolLabel({name:'find_files'}),'查找文件');
 assert.equal(toolLabel({name:'web_search'}),'搜索网页');
 assert.equal(toolLabel({name:'web_fetch'}),'读取网页');
 assert.equal(toolTarget({arguments:JSON.stringify({program:'cargo',args:['test','-p','leveler-app']})}),'cargo test -p leveler-app');
});
test('Agent browser activity is an observed tool report, never a manual browser binding',()=>{
 assert.equal(browserActivity({id:'r1',name:'read_file',arguments:'{}'}),null);
 assert.deepEqual(browserActivity({id:'b1',name:'browser_navigate',arguments:'{"url":"https://example.com"}',preview:'Page title: Example',status:'success'}),{id:'b1',label:'浏览网页',target:'https://example.com',preview:'Page title: Example',status:'success'});
 assert.equal(toolTarget({arguments:'bad-json'}),'');
});
import {sourceErrorSummary} from '../src/presentation.mjs';
test('repeated source errors remain failures with counts and distinct details',()=>{
 const result=sourceErrorSummary([{message:'no such column: owner_boot_id'},{message:'no such column: owner_boot_id'},{message:'workspace unavailable'}]);
 assert.equal(result.summary,'3 个任务来源读取失败（2 种错误）');
 assert.deepEqual(result.details,[{message:'no such column: owner_boot_id',count:2},{message:'workspace unavailable',count:1}]);
 assert.deepEqual(sourceErrorSummary([]),{summary:'',details:[]});
});
import {relativeActivity,permissionLabel,recentWorkspaces} from '../src/presentation.mjs';
test('task navigation uses a compact relative timestamp without changing facts',()=>{
 const now=Date.parse('2026-10-02T10:00:00Z');
 assert.equal(relativeActivity('2026-10-02T09:57:00Z',now),'3 分钟前');
 assert.equal(relativeActivity('2026-10-01T10:00:00Z',now),'1 天前');
 assert.equal(relativeActivity('invalid',now),'');
});
test('permission chrome reports actual runtime mode and never guesses an absent mode',()=>{
 assert.equal(permissionLabel('full_access'),'完全开放');
 assert.equal(permissionLabel('assisted'),'自动权限');
 assert.equal(permissionLabel('request_approval'),'受限权限');
 assert.equal(permissionLabel(undefined),'权限由 Runtime 决定');
});
test('recent workspaces are distinct real task locations and searchable by name',()=>{
 const tasks=[{primary_workspace:'/real/a'},{primary_workspace:null},{primary_workspace:'/real/a'},{primary_workspace:'/real/b'}];
 assert.deepEqual(recentWorkspaces(tasks),[{path:'/real/a',name:'a'},{path:'/real/b',name:'b'}]);
 assert.deepEqual(recentWorkspaces(tasks,'b'),[{path:'/real/b',name:'b'}]);
});
import {filterTasks,taskSearchResults} from '../src/presentation.mjs';
test('task filters combine exact runtime status and actual activity time without dropping unknown timestamps for all time',()=>{
 const now=Date.parse('2026-10-02T10:00:00Z');const tasks=[{id:'a',status:'running',last_activity_at:'2026-10-02T08:00:00Z'},{id:'b',status:'running',last_activity_at:'2026-09-30T08:00:00Z'},{id:'c',status:'failed',last_activity_at:'2026-09-01T08:00:00Z'},{id:'d',status:'running',last_activity_at:'invalid'}];
 assert.deepEqual(filterTasks(tasks,{status:'running',time:'today'},now).map(t=>t.id),['a']);
 assert.deepEqual(filterTasks(tasks,{status:'all',time:'7d'},now).map(t=>t.id),['a','b']);
 assert.deepEqual(filterTasks(tasks,{status:'all',time:'all'},now).map(t=>t.id),['a','b','c','d']);
});
test('global search exposes matching real tasks and distinct workspaces only',()=>{
 const tasks=[{id:'a',title:'修复 Desktop',primary_workspace:'/real/CodeLeveler'},{id:'b',title:'聊天',primary_workspace:null}];
 const results=taskSearchResults(tasks,'CodeLeveler');
 assert.deepEqual(results.map(r=>r.type),['task','workspace']);
 assert.equal(results[0].task.id,'a');
 assert.equal(results[1].workspace.path,'/real/CodeLeveler');
 assert.deepEqual(taskSearchResults(tasks,'no match'),[]);
});
import {renameCommand} from '../src/presentation.mjs';
test('inline rename validates user input before sending one current-session command',()=>{
 assert.deepEqual(renameCommand('current-task','  安装服务  '),{type:'rename_session',session_id:'current-task',name:'安装服务'});
 assert.throws(()=>renameCommand(null,'标题'),/选择任务/);
 assert.throws(()=>renameCommand('current-task',' \n '),/输入标题/);
 assert.throws(()=>renameCommand('current-task','字'.repeat(257)),/256/);
});
import {conversationMatches} from '../src/presentation.mjs';
test('conversation search retains message order and finds every occurrence case-insensitively',()=>{
 const messages=[{id:'u1',text:'Agent agent'},{id:'a1',text:'Another AGENT response'}];
 assert.deepEqual(conversationMatches(messages,'agent'),[{id:'u1',start:0,end:5},{id:'u1',start:6,end:11},{id:'a1',start:8,end:13}]);
 assert.deepEqual(conversationMatches(messages,'  '),[]);
 assert.deepEqual(messages.map(m=>m.text),['Agent agent','Another AGENT response']);
});
test('search offsets stay in visible text with unicode case folding and literal punctuation',()=>{
 assert.deepEqual(conversationMatches([{id:'u',text:'İ Agent [x]'}],'agent'),[{id:'u',start:2,end:7}]);
 assert.deepEqual(conversationMatches([{id:'u',text:'[x]'}],'[x]'),[{id:'u',start:0,end:3}]);
});
import {messageKind,messageRoleLabel} from '../src/presentation.mjs';
test('runtime notices carried as user messages keep runtime attribution',()=>{
 const notice={role:'user',kind:'runtime_notice',text:'Runtime restored'};
 assert.equal(messageKind(notice),'notice');
 assert.equal(messageRoleLabel(notice),'运行提示');
 assert.equal(messageKind({role:'user',text:'I wrote this'}),'user');
 assert.equal(messageRoleLabel({role:'assistant'}),'CodeLeveler');
});
import {historyQuestions} from '../src/presentation.mjs';
test('history prompts include user intent but exclude runtime notices on the same transport role',()=>{
 const messages=[{id:'u',role:'user',text:'Analyze project'},{id:'n',role:'user',kind:'runtime_notice',text:'Runtime restored'},{id:'a',role:'assistant',text:'Answer'}];
 assert.deepEqual(historyQuestions(messages).map(message=>message.id),['u']);
});

test('plan statuses use the actual Runtime states and retain unknown semantics',async()=>{
 const {planStepLabel}=await import('../src/presentation.mjs');
 assert.deepEqual(['pending','running','done','failed','skipped'].map(planStepLabel),['待执行','进行中','已完成','失败','已跳过']);
 assert.equal(planStepLabel('future_state'),'未知状态：future_state');
});
test('Workbench only offers views backed by plan steps, diff evidence or actual read failure',async()=>{
 const {availableWorkbenchViews}=await import('../src/presentation.mjs');
 assert.deepEqual(availableWorkbenchViews({}),['overview']);
 assert.deepEqual(availableWorkbenchViews({plan:{steps:[]}}),['overview']);
 assert.deepEqual(availableWorkbenchViews({plan:{steps:[{status:'pending'}]}}),['overview','plan']);
 assert.deepEqual(availableWorkbenchViews({diff:{files:[]}}),['overview','changes']);
 assert.deepEqual(availableWorkbenchViews({hasWorkspace:false,diff:{files:[]},diffError:'unreadable'}),['overview']);
 assert.deepEqual(availableWorkbenchViews({diffError:'unreadable'}),['overview','changes']);
 assert.deepEqual(availableWorkbenchViews({plan:{steps:[{}]},diff:{files:[]}}),['overview','plan','changes']);
});

test('browser tab keyboard navigation wraps real entries and supports Home End and vertical keys',async()=>{
 const {browserTabIndex}=await import('../src/presentation.mjs');
 assert.equal(browserTabIndex(3,0,'ArrowLeft'),2);assert.equal(browserTabIndex(3,2,'ArrowRight'),0);
 assert.equal(browserTabIndex(3,1,'Home'),0);assert.equal(browserTabIndex(3,0,'End'),2);
 assert.equal(browserTabIndex(3,0,'ArrowDown'),1);assert.equal(browserTabIndex(3,1,'ArrowUp'),0);
 assert.equal(browserTabIndex(0,0,'ArrowRight'),-1);
});
test('browser navigation controls and loading title project actual native tab state',async()=>{
 const {browserChrome}=await import('../src/presentation.mjs');
 assert.deepEqual(browserChrome(null),{title:'新标签',back:false,forward:false,reload:false,external:false,loading:false});
 const loading=browserChrome({url:'https://actual.example/',title:'',loading:true,canGoBack:true,canGoForward:false});
 assert.equal(loading.title,'https://actual.example/');assert.equal(loading.loading,true);assert.equal(loading.back,true);assert.equal(loading.forward,false);assert.equal(loading.reload,true);assert.equal(loading.external,true);
 const blank=browserChrome({url:'about:blank',title:'',loading:false,canGoBack:false,canGoForward:false});assert.equal(blank.title,'新标签');assert.equal(blank.external,false);
});

test('native Browser surface bounds clip fractional edges and reject offscreen or empty rectangles',async()=>{
 const {browserSurfaceBounds}=await import('../src/presentation.mjs');
 assert.deepEqual(browserSurfaceBounds({x:-10,y:-20,width:300,height:200},{width:200,height:100}),{x:0,y:0,width:200,height:100});
 assert.deepEqual(browserSurfaceBounds({x:10.2,y:20.2,width:30.6,height:40.6},{width:200,height:100}),{x:11,y:21,width:29,height:39});
 assert.equal(browserSurfaceBounds({x:0,y:120,width:100,height:50},{width:200,height:100}),null);
 assert.equal(browserSurfaceBounds({x:0,y:0,width:0,height:50},{width:200,height:100}),null);
});

test('overlay tab selection keeps its existing focused tab; actual opening moves focus into the workbench',async()=>{
 const {focusWorkbenchEntry}=await import('../src/presentation.mjs');
 assert.equal(focusWorkbenchEntry({wasOpen:true,focusInside:true,overlay:true}),false);
 assert.equal(focusWorkbenchEntry({wasOpen:false,focusInside:false,overlay:true}),true);
 assert.equal(focusWorkbenchEntry({wasOpen:true,focusInside:false,overlay:true}),true);
 assert.equal(focusWorkbenchEntry({wasOpen:false,focusInside:false,overlay:false}),false);
});
test('rejected address draft survives blur to error details until explicit navigation resolves it',async()=>{
 const {retainBrowserAddressOnBlur}=await import('../src/presentation.mjs');
 assert.equal(retainBrowserAddressOnBlur({dirty:true,rejected:true}),true);
 assert.equal(retainBrowserAddressOnBlur({dirty:false,rejected:true}),false);
 assert.equal(retainBrowserAddressOnBlur({dirty:true,rejected:false}),false);
});

test('sidebar workspace ownership groups exact paths, never basenames, with no duplicated task attribution',async()=>{
 const {workspaceTaskGroups,filterTasks}=await import('../src/presentation.mjs');
 const tasks=[{id:'loose',primary_workspace:null,status:'idle'},{id:'a',primary_workspace:'/alpha/devorder',status:'running'},{id:'b',primary_workspace:'/beta/devorder',status:'idle'},{id:'a2',primary_workspace:'/alpha/devorder',status:'idle'}];
 const grouped=workspaceTaskGroups(tasks);assert.deepEqual(grouped.loose.map(task=>task.id),['loose']);
 assert.deepEqual(grouped.spaces.map(space=>space.path),['/alpha/devorder','/beta/devorder']);
 assert.deepEqual(grouped.spaces[0].tasks.map(task=>task.id),['a','a2']);assert.equal(grouped.spaces[0].name,grouped.spaces[1].name);
 const attributed=[...grouped.loose,...grouped.spaces.flatMap(space=>space.tasks)].map(task=>task.id);
 assert.equal(attributed.length,tasks.length);assert.equal(new Set(attributed).size,tasks.length);
 const filtered=workspaceTaskGroups(filterTasks(tasks,{status:'running',time:'all'}));assert.equal(filtered.loose.length,0);assert.deepEqual(filtered.spaces.flatMap(space=>space.tasks).map(task=>task.id),['a']);
 assert.deepEqual(workspaceTaskGroups([]),{loose:[],spaces:[]});
});

test('attachment size and unreadable failures show actionable product text with complete folded diagnostics',async()=>{
 const {attachmentErrorPresentation}=await import('../src/presentation.mjs');
 const large="Error invoking remote method 'desktop:attachment': Error: 附件不能超过 20 MiB";
 const size=attachmentErrorPresentation(large);assert.equal(size.label,'附件不能超过 20 MiB，请选择较小的文件。');assert.equal(size.detail,large);
 const unreadable="Error invoking remote method 'desktop:attachment': Error: EACCES: permission denied, open '/private/user/file.txt'";
 const failure=attachmentErrorPresentation(unreadable);assert.equal(failure.label,'无法读取或上传附件，请检查文件后重试。');assert.equal(failure.detail,unreadable);
 assert.ok(!failure.label.includes('desktop:')&&!failure.label.includes('/private/'));
});

test('closing a resized popup restores only a visible enabled focus target within an active workbench overlay', async()=>{
 const {focusRestoreIndex}=await import('../src/presentation.mjs');
 const targets=[{connected:true,visible:false,enabled:true,inWorkbench:false},{connected:true,visible:true,enabled:true,inWorkbench:false},{connected:true,visible:true,enabled:true,inWorkbench:true}];
 assert.equal(focusRestoreIndex(targets,false),1);
 assert.equal(focusRestoreIndex(targets,true),2);
 assert.equal(focusRestoreIndex([{connected:false,visible:true,enabled:true,inWorkbench:true},{connected:true,visible:true,enabled:false,inWorkbench:true}],true),-1);
});

test('slash command selection dispatches actual actions and preserves unsupported or disabled drafts instead of sending literals',async()=>{
 const {slashIntent,commandCandidates}=await import('../src/presentation.mjs');
 const commands=[{id:'find',label:'搜索对话',enabled:true},{id:'diff',label:'读取变更',enabled:false,reason:'需要工作空间'}];
 assert.deepEqual(slashIntent('/find',commands),{kind:'action',id:'find'});
 assert.deepEqual(slashIntent('/diff',commands),{kind:'blocked',reason:'需要工作空间'});
 assert.equal(slashIntent('/imaginary',commands).kind,'blocked');
 assert.equal(slashIntent('解释 /find 的含义',commands).kind,'message');
 assert.equal(commandCandidates('/f',commands).length,1);
 assert.equal(commandCandidates('normal text',commands).length,0);
});

test('leading slash names with unsupported arguments stay local while filesystem paths and comments remain prose',async()=>{
 const {slashIntent}=await import('../src/presentation.mjs');const commands=[{id:'settings',label:'设置',enabled:true}];
 assert.equal(slashIntent('/unknown arg',commands).kind,'blocked');
 assert.equal(slashIntent('/settings arg',commands).kind,'blocked');
 assert.equal(slashIntent('/settings   ',commands).kind,'action');
 assert.equal(slashIntent('/Users/a/file',commands).kind,'message');
 assert.equal(slashIntent('// comment',commands).kind,'message');
});

test('the public Settings shortcut opens once and respects composition and an existing modal without replacing its focus',async()=>{
 const {activateSettingsShortcut}=await import('../src/presentation.mjs');let opens=0;const open=()=>opens++;
 assert.equal(activateSettingsShortcut({composing:false,modalOpen:false,settingsOpen:false},open),true);
 assert.equal(opens,1);
 for(const state of [{composing:true,modalOpen:false,settingsOpen:false},{composing:false,modalOpen:true,settingsOpen:false},{composing:false,modalOpen:false,settingsOpen:true}])assert.equal(activateSettingsShortcut(state,open),false);
 assert.equal(opens,1);
});
import {turnTerminalFromEvent} from '../src/presentation.mjs';
test('durable TaskCancelled renders cancellation while publishing progress remains nonterminal',()=>{
 assert.equal(turnTerminalFromEvent('task_cancelled',[],[]),'cancelled');
 assert.equal(turnTerminalFromEvent('turn_finalizing',[],[]),null);
});
