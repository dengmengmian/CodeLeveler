// Chrome labels project protocol facts without exposing workspace paths.
/** @param {{model?:string,provider?:string}|null|undefined} model @returns {string} */
export function modelLabel(model){return typeof model?.model==='string'?model.model:'';}
/** @param {string|null|undefined} path @returns {string} */
export function workspaceLabel(path){return path?path.split(/[\\/]/).filter(Boolean).at(-1)||path:'No Workspace';}
import * as shared from '../../../packages/conversation-presentation/conversation.mjs';

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

/** The round's status in the contract vocabulary. @param {{tools:Array<{status:string}>}} round */
export function contractRoundStatus(round){const statuses=round.tools.map(tool=>contractToolStatus(tool.status));if(statuses.includes('running'))return 'running';if(round.tools.length>0&&statuses.every(status=>status==='ok'))return 'ok';if(statuses.includes('failed'))return 'failed';if(statuses.includes('cancelled'))return 'cancelled';return 'unknown';}

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

// ── Execution Presentation Contract v1 ────────────────────────────────
// The Desktop projection of the execution presentation contract v1. Pure
// functions only: the renderer paints what they return, and the conformance
// test reads the same ones (test/executionPresentation.test.mjs). Nothing here
// guesses a boundary — the round is the runtime's `model_step`, the batch is an
// observed overlap, and Final vs Progress is decided by event order.

/** Tool lifecycle -> the contract's frozen vocabulary. @param {string} status */
export function contractToolStatus(status){
 switch(status){case 'running':case 'run':return 'running';case 'success':case 'done':return 'ok';case 'failed':case 'fail':return 'failed';case 'cancelled':return 'cancelled';default:return 'unknown';}
}
/** @param {boolean} ok @param {string|null|undefined} stop */
export function toolStatusFromOutcome(ok,stop){if(stop==='confirmed')return 'cancelled';if(stop==='unconfirmed')return 'unknown';return ok?'success':'failed';}

/** Whether the runtime stated this call as substantive work.
 *
 * The classification belongs to the runtime, which stamps `answer_effect` on
 * every tool call it projects (`leveler_tools::acts_on_answer` is its one
 * owner). Desktop reads the fact; it must never re-derive it from the tool
 * name, which is what made `FinalAnswer` a second truth source.
 * @param {ExecutionToolRow} tool */
export function actsOnAnswer(tool){return (tool?.answerEffect??'work')==='work';}

/**
 * Failure and output presentation rules live once, in
 * `packages/conversation-presentation/conversation.mjs`, because the Web client
 * and this renderer must name the same failure line. The reference
 * implementation (the terminal's `shell_failure_line`) is the authority.
 */
export const isRuntimeNote = shared.isRuntimeNote;
export const commandOutputBody = shared.commandOutputBody;
export const failureReason = shared.failureLine;
export const displayPreview = shared.displayPreview;
export const isCommandTool = shared.isCommandTool;

/** Conversation presentation rules, from the same shared module. */
export const groupExploration = shared.groupExploration;
export const explorationLabel = shared.explorationLabel;
export const isExplorationTool = shared.isExplorationTool;
export const foldedThoughts = shared.foldedThoughts;
export const confirmedDiffs = shared.confirmedDiffs;
export const confirmedDiffOf = shared.confirmedDiff;
export const turnBlocks = shared.turnBlocks;
export const diffCounts = shared.diffCounts;
export const diffLines = shared.diffLines;

/** Activity class for the legacy fallback only. @param {string} name */
function activityClass(name){const n=name.toLowerCase();if(/apply_patch|edit|write|patch/.test(n))return 'edit';if(/read|cat|open|view/.test(n))return 'read';if(/search|grep|find|glob|list/.test(n))return 'search';if(/bash|shell|exec|command|run|terminal|cargo|npm|git_(?!diff)/.test(n))return 'command';return 'other';}

/** @typedef {{id:string,name:string,status:string,parallel?:boolean,modelStep?:number|null,batch?:number|null,answerEffect?:string}} ExecutionToolRow */
/** @typedef {{modelStep:number|null,tools:ExecutionToolRow[],status:string,allOk:boolean,batches:string[][]}} ExecutionRoundView */

/** @param {ExecutionRoundView} round @param {ExecutionToolRow} tool */
function startsNewRound(round,tool){
 if(round.modelStep!==null&&tool.modelStep!=null)return round.modelStep!==tool.modelStep;
 if(round.tools.some(item=>item.status==='running'))return false;
 const previous=round.tools[round.tools.length-1];
 if(!previous)return false;
 return activityClass(previous.name)!==activityClass(tool.name);
}
/** @param {ExecutionToolRow[]} tools @returns {ExecutionRoundView[]} */
export function groupExecutionRounds(tools){
 /** @type {ExecutionRoundView[]} */ const rounds=[];
 for(const tool of tools){
  const last=rounds[rounds.length-1];
  if(last&&!startsNewRound(last,tool)){last.tools.push(tool);continue;}
  rounds.push({modelStep:tool.modelStep??null,tools:[tool],status:'running',allOk:false,batches:[]});
 }
 for(const round of rounds){
  round.status=roundStatus(round.tools);
  round.allOk=round.tools.length>0&&round.tools.every(tool=>contractToolStatus(tool.status)==='ok');
  round.batches=roundBatches(round.tools);
 }
 return rounds;
}
/** @param {ExecutionToolRow[]} tools */
function roundStatus(tools){
 if(tools.some(tool=>contractToolStatus(tool.status)==='running'))return 'running';
 if(tools.length>0&&tools.every(tool=>contractToolStatus(tool.status)==='ok'))return 'ok';
 if(tools.some(tool=>contractToolStatus(tool.status)==='failed'))return 'failed';
 if(tools.some(tool=>contractToolStatus(tool.status)==='cancelled'))return 'cancelled';
 return 'unknown';
}
/** @param {ExecutionToolRow[]} tools */
function roundBatches(tools){
 /** @type {number[]} */ const order=[];
 /** @type {string[][]} */ const batches=[];
 for(const tool of tools){
  if(tool.batch==null)continue;
  const index=order.indexOf(tool.batch);
  if(index>=0){batches[index].push(tool.id);continue;}
  order.push(tool.batch);batches.push([tool.id]);
 }
 return batches;
}
/** Truthful one-line head; claims all-success only when every call succeeded. @param {ExecutionRoundView} round */
export function roundHeadline(round){
 const total=round.tools.length;
 switch(round.status){
  case 'running':return `执行中 · ${total} 项`;
  case 'ok':return round.allOk?`完成 ${total} 项`:`${total} 项已结束`;
  case 'failed':return `完成 ${total} 项 · ${round.tools.filter(tool=>contractToolStatus(tool.status)==='failed').length} 项失败`;
  case 'cancelled':return `已停止 · ${total} 项`;
  default:return `结果未知 · ${total} 项`;
 }
}
/** @param {ExecutionRoundView} round */
export function roundGlyph(round){switch(round.status){case 'running':return '●';case 'ok':return '✓';case 'failed':return '✗';case 'cancelled':return '■';default:return '◇';}}

/** @typedef {{id?:string,role:string,text:string,kind?:string,btw?:string,seq?:number,anchor?:string}} ProjectionMessage */

/**
 * Arrival order for one tool row. A live call carries the runtime stream's own
 * stamp; a replayed one does not, so it takes its position from the message it
 * was anchored to — the same anchor the renderer inserts the row after.
 * @param {any} tool @param {ProjectionMessage[]} messages @param {number} index
 */
function toolOrder(tool,messages,index){
 if(typeof tool.seq==='number')return tool.seq;
 const anchorIndex=messages.findIndex(message=>message.id===tool.anchor);
 const base=anchorIndex>=0?(messages[anchorIndex].seq??anchorIndex):(messages.length?messages.length-1:-1);
 return base+0.5+index*0.001;
}

/**
 * Project one turn's messages and tool rows onto the contract's items.
 * @param {ProjectionMessage[]} messages @param {ExecutionToolRow[]} tools @param {boolean} turnEnded
 */
export function projectTurn(messages,tools,turnEnded){
 /** @type {Array<{seq:number,message?:ProjectionMessage,tool?:ExecutionToolRow}>} */ const entries=[];
 let start=0;
 for(let i=messages.length-1;i>=0;i-=1){if(messageKind(messages[i])==='user'){start=i;break;}}
 messages.slice(start).forEach((message,index)=>entries.push({seq:message.seq??index,message}));
 tools.forEach((tool,index)=>entries.push({seq:toolOrder(tool,messages,index),tool}));
 entries.sort((a,b)=>a.seq-b.seq);
 /** @type {any[]} */ const nodes=[];
 for(const entry of entries){
  if(entry.message){
   const message=entry.message;
   if(message.btw!==undefined)continue;
   // Only the model's public assistant content is AssistantText. A user line,
   // a runtime notice and a compaction summary (stored on the user role) are
   // not assistant prose.
   const kind=message.kind==='compaction_summary'?'assistant':messageKind(message);
   if(kind!=='assistant')continue;
   if(!message.text.trim())continue;
   nodes.push({kind:'assistant',text:message.text,demoted:false,seq:entry.seq});
   continue;
  }
  const tool=entry.tool;
  if(!tool)continue;
  if(actsOnAnswer(tool)){for(const node of nodes){if(node.kind==='assistant')node.demoted=true;}}
  const last=nodes[nodes.length-1];
  if(last&&last.kind==='round'&&!startsNewRound(last.round,tool)){last.round.tools.push(tool);}
  else nodes.push({kind:'round',seq:entry.seq,round:{modelStep:tool.modelStep??null,tools:[tool],status:'running',allOk:false,batches:[]}});
 }
 /** @type {any[]} */ const items=[];
 for(const node of nodes){
  if(node.kind==='assistant'){items.push({seq:node.seq,kind:turnEnded&&!node.demoted?'final_answer':'assistant_text',text:node.text});continue;}
  node.round.status=roundStatus(node.round.tools);
  node.round.allOk=node.round.tools.length>0&&node.round.tools.every(/** @param {ExecutionToolRow} row */ row=>contractToolStatus(row.status)==='ok');
  node.round.batches=roundBatches(node.round.tools);
  items.push({seq:node.seq,kind:'execution_round',round:node.round});
 }
 return items;
}
/**
 * The turn's committed answer, or null when it ended without one.
 * @param {ProjectionMessage[]} messages @param {ExecutionToolRow[]} tools
 */
export function committedFinalAnswer(messages,tools){
 let answer=null;
 for(const item of projectTurn(messages,tools,true))if(item.kind==='final_answer')answer=item.text;
 return answer;
}
/**
 * The turn terminal, keeping Contract §I9: a Completed/Answered turn with no
 * committed answer reads no_final_answer instead of a green completion.
 * @param {string} type @param {ProjectionMessage[]} messages @param {ExecutionToolRow[]} tools
 */
export function turnTerminalFromEvent(type,messages,tools){
 if(type==='turn_completed'||type==='turn_answered'){
  return committedFinalAnswer(messages,tools)===null?'no_final_answer':(type==='turn_answered'?'answered':'completed');
 }
 if(type==='turn_completed_with_warnings')return 'completed_with_warnings';
 if(type==='turn_truncated')return 'truncated';
 if(type==='turn_incomplete')return 'incomplete';
 if(type==='turn_failed')return 'failed';
 if(type==='turn_cancelled'||type==='task_cancelled')return 'cancelled';
 return null;
}

export { explorationEntries } from '../../../packages/conversation-presentation/conversation.mjs';
