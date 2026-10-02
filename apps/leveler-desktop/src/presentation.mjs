// Chrome labels project protocol facts without exposing workspace paths.
/** @param {{model?:string,provider?:string}|null|undefined} model @returns {string} */
export function modelLabel(model){return typeof model?.model==='string'?model.model:'';}
/** @param {string|null|undefined} path @returns {string} */
export function workspaceLabel(path){return path?path.split(/[\\/]/).filter(Boolean).at(-1)||path:'No Workspace';}
/** @typedef {{id?:string,name?:string,arguments?:string|Record<string,unknown>,preview?:string,status?:string}} ToolProjection */
/** @type {Record<string,string>} */
const toolNames={read_file:'查看文件',read:'查看文件',grep:'搜索内容',search:'搜索内容',glob:'查找文件',find_files:'查找文件',list_files:'查看目录',list_directory:'查看目录',bash:'运行命令',run_command:'运行命令',shell:'运行命令',shell_command:'运行命令',write_file:'写入文件',edit_file:'修改文件',apply_patch:'修改文件',web_search:'搜索网页',web_fetch:'读取网页',browser_navigate:'浏览网页',browser_click:'操作网页'};
/** @param {ToolProjection} tool @returns {string} */
export function toolLabel(tool){return toolNames[tool.name??'']??(tool.name?.startsWith('browser_')?'浏览器操作':'执行工具');}
/** @param {ToolProjection} tool @returns {string} */
export function toolTarget(tool){try{const args=typeof tool.arguments==='string'?JSON.parse(tool.arguments):tool.arguments;if(!args||typeof args!=='object')return '';for(const key of ['path','command','pattern','query','url'])if(typeof args[key]==='string')return args[key];if(typeof args.program==='string')return [args.program,...(Array.isArray(args.args)?args.args.filter(/** @param {unknown} a */ a=>typeof a==='string'):[])].join(' ');return '';}catch{return '';}}
/** @param {ToolProjection} tool */
export function browserActivity(tool){return tool.name?.startsWith('browser_')?{id:tool.id,label:toolLabel(tool),target:toolTarget(tool),preview:tool.preview??'',status:tool.status??'unknown'}:null;}
/** @param {{message:string}[]} errors */
export function sourceErrorSummary(errors){
 const counts=new Map();for(const error of errors)counts.set(error.message,(counts.get(error.message)??0)+1);
 return {summary:errors.length?`${errors.length} 个任务来源读取失败（${counts.size} 种错误）`:'',details:[...counts].map(([message,count])=>({message,count}))};
}
/** @param {string|undefined} time @param {number} [now] @returns {string} */
export function relativeActivity(time,now=Date.now()){
 const value=Date.parse(time??'');if(!Number.isFinite(value))return '';const minutes=Math.max(0,Math.floor((now-value)/60000));if(minutes<1)return '刚刚';if(minutes<60)return `${minutes} 分钟前`;const hours=Math.floor(minutes/60);if(hours<24)return `${hours} 小时前`;return `${Math.floor(hours/24)} 天前`;
}
/** @param {string|undefined} mode @returns {string} */
export function permissionLabel(mode){const labels=/** @type {Record<string,string>} */ ({full_access:'完全开放',assisted:'自动权限',request_approval:'受限权限'});return labels[mode??'']??'权限由 Runtime 决定';}
/** @param {{primary_workspace?:string|null}[]} tasks @param {string} [query] */
export function recentWorkspaces(tasks,query=''){
 const paths=[...new Set(tasks.map(task=>task.primary_workspace).filter(/** @returns {path is string} */ path=>typeof path==='string'&&!!path))];return paths.map(path=>({path,name:workspaceLabel(path)})).filter(workspace=>workspace.name.toLowerCase().includes(query.trim().toLowerCase()));
}
/** @typedef {{id?:string,title?:string,primary_workspace?:string|null,status?:string,last_activity_at?:string,updated_at?:string}} NavigationTask */
/** @template {NavigationTask} T @param {T[]} tasks @param {{status:string,time:string}} filters @param {number} [now] @returns {T[]} */
export function filterTasks(tasks,filters,now=Date.now()){
 const today=new Date(now);today.setHours(0,0,0,0);const since=filters.time==='today'?today.getTime():filters.time==='7d'?now-7*86400000:now-30*86400000;
 return tasks.filter(task=>filters.status==='all'||task.status===filters.status).filter(task=>{if(filters.time==='all')return true;const time=Date.parse(task.last_activity_at??task.updated_at??'');return Number.isFinite(time)&&time>=since&&time<=now;});
}
/** @template {NavigationTask} T @param {T[]} tasks @param {string} query */
export function taskSearchResults(tasks,query){
 const q=query.trim().toLowerCase();const taskMatches=tasks.filter(task=>`${task.title??''} ${task.primary_workspace??''}`.toLowerCase().includes(q)).slice(0,40).map(task=>({type:'task',task}));
 const spaces=recentWorkspaces(tasks,q).slice(0,20).map(workspace=>({type:'workspace',workspace}));return [...taskMatches,...spaces];
}
/** @param {string|null|undefined} sessionId @param {string} name */
export function renameCommand(sessionId,name){if(!sessionId)throw new Error('请先选择任务');name=name.trim();if(!name)throw new Error('请输入标题');if(name.length>256)throw new Error('标题最多 256 个字符');return {type:'rename_session',session_id:sessionId,name};}
/** @param {{id:string,text:string}[]} messages @param {string} query */
export function conversationMatches(messages,query){
 const needle=query.trim();if(!needle)return [];
 const pattern=Array.from(needle).map(char=>'\\^$.*+?()[]{}|'.includes(char)?'\\'+char:char).join('');
 const matcher=new RegExp(pattern,'giu'),matches=[];
 for(const message of messages)for(const match of message.text.matchAll(matcher))matches.push({id:message.id,start:match.index,end:match.index+match[0].length});
 return matches;
}
/** @param {{role:string,kind?:string}} message */
export function messageKind(message){return message.kind==='runtime_notice'?'notice':['user','assistant'].includes(message.role)?message.role:'other';}
/** @param {{role:string,kind?:string}} message */
export function messageRoleLabel(message){return message.kind==='runtime_notice'?'运行提示':message.role==='user'?'你':message.role==='assistant'?'CodeLeveler':message.role;}
/** @template {{role:string,kind?:string}} T @param {T[]} messages @returns {T[]} */
export function historyQuestions(messages){return messages.filter(message=>messageKind(message)==='user');}

/** @param {string} status */
export function planStepLabel(status){const labels=/** @type {Record<string,string>} */({pending:'待执行',running:'进行中',done:'已完成',failed:'失败',skipped:'已跳过'});return labels[status]??`未知状态：${status}`;}
/** @param {{plan?:{steps:unknown[]}|null,diff?:unknown,diffError?:string|null,hasWorkspace?:boolean}} state */
export function availableWorkbenchViews(state){return ['overview',...(state.plan?.steps.length?['plan']:[]),...(state.hasWorkspace!==false&&(state.diff||state.diffError)?['changes']:[])];}

/** @param {number} count @param {number} index @param {string} key */
export function browserTabIndex(count,index,key){if(!count)return -1;if(key==='Home')return 0;if(key==='End')return count-1;if(['ArrowRight','ArrowDown'].includes(key))return (index+1+count)%count;if(['ArrowLeft','ArrowUp'].includes(key))return (index-1+count)%count;return index;}
/** @param {{url:string,title:string,loading:boolean,canGoBack:boolean,canGoForward:boolean}|null} tab */
export function browserChrome(tab){return {title:tab?.title||(tab?.url&&tab.url!=='about:blank'?tab.url:'新标签'),back:!!tab?.canGoBack,forward:!!tab?.canGoForward,reload:!!tab,external:!!tab?.url&&tab.url!=='about:blank',loading:!!tab?.loading};}

/** @param {{x:number,y:number,width:number,height:number}} rect @param {{width:number,height:number}} viewport */
export function browserSurfaceBounds(rect,viewport){const x=Math.ceil(Math.max(0,rect.x)),y=Math.ceil(Math.max(0,rect.y)),right=Math.floor(Math.min(viewport.width,rect.x+rect.width)),bottom=Math.floor(Math.min(viewport.height,rect.y+rect.height));if(right<=x||bottom<=y)return null;return {x,y,width:right-x,height:bottom-y};}

/** @param {{wasOpen:boolean,focusInside:boolean,overlay:boolean}} state */
export function focusWorkbenchEntry(state){return state.overlay&&(!state.wasOpen||!state.focusInside);}
/** @param {{dirty:boolean,rejected:boolean}} state */
export function retainBrowserAddressOnBlur(state){return state.dirty&&state.rejected;}

/** @template {NavigationTask} T @param {T[]} tasks @returns {{loose:T[],spaces:{path:string,name:string,tasks:T[]}[]}} */
export function workspaceTaskGroups(tasks){
 /** @type {T[]} */ const loose=[];
 /** @type {Map<string,{path:string,name:string,tasks:T[]}>} */ const spaces=new Map();
 for(const task of tasks){const path=typeof task.primary_workspace==='string'&&task.primary_workspace?task.primary_workspace:null;if(!path){loose.push(task);continue;}let space=spaces.get(path);if(!space){space={path,name:workspaceLabel(path),tasks:[]};spaces.set(path,space);}space.tasks.push(task);}
 return {loose,spaces:[...spaces.values()]};
}

/** @param {string} message */
export function attachmentErrorPresentation(message){const label=message.includes('附件不能超过 20 MiB')?'附件不能超过 20 MiB，请选择较小的文件。':message.includes('文件在上传读取期间发生变化')?'读取期间文件发生变化，请重新选择。':message.includes('请选择普通文件')?'请选择可读取的普通文件。':'无法读取或上传附件，请检查文件后重试。';return {label,detail:message};}

/** @param {{connected:boolean,visible:boolean,enabled:boolean,inWorkbench:boolean}[]} candidates @param {boolean} overlay */
export function focusRestoreIndex(candidates,overlay){return candidates.findIndex(candidate=>candidate.connected&&candidate.visible&&candidate.enabled&&(!overlay||candidate.inWorkbench));}

/** @typedef {{id:string,label:string,enabled:boolean,reason?:string}} PaletteCommand */
/** @param {string} text @param {PaletteCommand[]} commands */
export function slashIntent(text,commands){const match=/^\/([a-z-]*)(?:\s+([\s\S]*))?$/.exec(text.trim());if(!match)return {kind:'message'};const command=commands.find(item=>item.id===match[1]);if(!command)return {kind:'blocked',reason:'没有匹配的命令，请选择候选或修改输入。'};if(match[2])return {kind:'blocked',reason:'此命令不接受参数。请选择命令后发送，使用打开的控件完成操作。'};if(!command.enabled)return {kind:'blocked',reason:command.reason??'此命令当前不可用'};return {kind:'action',id:command.id};}
/** @param {string} text @param {PaletteCommand[]} commands */
export function commandCandidates(text,commands){const match=/^\/([a-z-]*)$/.exec(text.trim());return match?commands.filter(item=>item.id.startsWith(match[1])):[];}

/** @param {{composing:boolean,modalOpen:boolean,settingsOpen:boolean}} state @param {()=>void} open */
export function activateSettingsShortcut(state,open){if(state.composing||state.modalOpen||state.settingsOpen)return false;open();return true;}
