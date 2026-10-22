// Chrome labels project protocol facts without exposing workspace paths.
/** @param {{model?:string,provider?:string}|null|undefined} model @returns {string} */
export function modelLabel(model){return typeof model?.model==='string'?model.model:'';}
/** `0.1s`-style duration, the same shape the terminal paints. @param {number} elapsedMs */
export function thoughtDuration(elapsedMs){if(elapsedMs<100)return '<0.1s';const seconds=elapsedMs/1000;return seconds<10?`${seconds.toFixed(1)}s`:`${Math.round(seconds)}s`;}
/** @param {string|null|undefined} path @returns {string} */
export function workspaceLabel(path){return path?path.split(/[\\/]/).filter(Boolean).at(-1)||path:'No Workspace';}
import * as shared from '../../../packages/conversation-presentation/conversation.mjs';

/** @typedef {{id?:string,name?:string,arguments?:string|Record<string,unknown>,preview?:string,status?:string}} ToolProjection */
/** @type {Record<string,string>} */
const toolNames={
 read_file:'查看文件',read:'查看文件',grep:'搜索文件内容',search:'搜索文件内容',glob:'查找文件',find_files:'查找文件',list_files:'查看目录',list_directory:'查看目录',
 bash:'运行命令',run_command:'运行命令',shell:'运行命令',shell_command:'运行命令',write_file:'写入文件',edit_file:'修改文件',apply_patch:'修改文件',
 web_search:'网页搜索',web_fetch:'网页获取',browser_navigate:'浏览网页',browser_click:'操作网页',browser_act:'操作网页',browser_inspect:'检查网页',browser_tab:'管理浏览器标签',
 load_skill:'使用技能',update_plan:'更新计划',request_user_input:'询问用户',spawn_agent:'创建子智能体',
 get_task:'查看任务',kill_task:'停止任务',wait_task:'等待任务',read_project_rules:'读取项目规则',
 find_references:'查找引用',find_symbol:'查找符号',read_symbol:'查看符号',view_image:'查看图片',
 git_diff:'查看变更',git_status:'查看状态',diagnostics:'运行诊断',blast_radius:'分析影响面',
 memory:'读取记忆',remember:'记住',forget:'忘记',
};
/** @param {ToolProjection} tool @returns {string} */
export function toolLabel(tool){return toolNames[tool.name??'']??(tool.name?.startsWith('browser_')?'浏览器操作':'工具调用');}
/** @param {ToolProjection} tool @returns {string} */
export function toolTarget(tool){try{const args=typeof tool.arguments==='string'?JSON.parse(tool.arguments):tool.arguments;if(!args||typeof args!=='object')return '';for(const key of ['path','command','cmd','pattern','query','url'])if(typeof args[key]==='string')return args[key];if(typeof args.program==='string')return [args.program,...(Array.isArray(args.args)?args.args.filter(/** @param {unknown} a */ a=>typeof a==='string'):[])].join(' ');return '';}catch{return '';}}
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
export function permissionLabel(mode){const labels=/** @type {Record<string,string>} */ ({full_access:'完全权限',assisted:'工作区内修改',request_approval:'需批准'});return labels[mode??'']??'权限由 Runtime 决定';}
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
/** Stamped by the runtime on the user-role message that replaces compacted history. */
export const COMPACTION_SUMMARY_PREFIX = '对话摘要（已压缩历史）';
/** @param {string} text */
export function isCompactionSummaryText(text){return typeof text==='string'&&text.startsWith(COMPACTION_SUMMARY_PREFIX);}
/** @param {string} text */
export function compactionBody(text){return isCompactionSummaryText(text)?text.slice(COMPACTION_SUMMARY_PREFIX.length).replace(/^：\s*/,'').trimStart():text;}

/** @param {{role:string,kind?:string,text?:string}} message */
export function messageKind(message){if(message.kind==='runtime_notice')return 'notice';if(message.kind==='compaction_summary'||(message.role==='user'&&isCompactionSummaryText(message.text??'')))return 'summary';return ['user','assistant'].includes(message.role)?message.role:'other';}
/** @param {{role:string,kind?:string}} message */
export function messageRoleLabel(message){if(message.kind==='runtime_notice')return '运行提示';if(message.kind==='compaction_summary')return '上下文摘要';return message.role==='user'?'你':message.role==='assistant'?'CodeLeveler':message.role;}
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
export function browserChrome(tab){const blank=!tab?.url||tab.url==='about:blank';return {title:blank?'新标签':(tab.title||tab.url),back:!!tab?.canGoBack,forward:!!tab?.canGoForward,reload:!!tab,external:!!tab?.url&&tab.url!=='about:blank',loading:!!tab?.loading};}

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
export function slashIntent(text,commands){const match=/^\/([a-z-]*)(?:\s+([\s\S]*))?$/.exec(text.trim());if(!match)return {kind:'message'};if(match[1]==='goal')return match[2]?{kind:'message'}:{kind:'blocked',reason:'用法：/goal <目标>'};const command=commands.find(item=>item.id===match[1]);if(!command)return {kind:'blocked',reason:'没有匹配的命令，请选择候选或修改输入。'};if(match[2])return {kind:'blocked',reason:'此命令不接受参数。请选择命令后发送，使用打开的控件完成操作。'};if(!command.enabled)return {kind:'blocked',reason:command.reason??'此命令当前不可用'};return {kind:'action',id:command.id};}
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
export const isEditTool = shared.isEditTool;
export const foldedThoughts = shared.foldedThoughts;
export const confirmedDiffs = shared.confirmedDiffs;
export const confirmedDiffOf = shared.confirmedDiff;
export const turnBlocks = shared.turnBlocks;
export const diffCounts = shared.diffCounts;
export const diffLines = shared.diffLines;

/** Activity class for the legacy fallback only. @param {string} name */
function activityClass(name){const n=name.toLowerCase();if(/apply_patch|edit|write|patch/.test(n))return 'edit';if(/read|cat|open|view/.test(n))return 'read';if(/search|grep|find|glob|list/.test(n))return 'search';if(/bash|shell|exec|command|run|terminal|cargo|npm|git_(?!diff)/.test(n))return 'command';return 'other';}

/** @typedef {{id:string,name:string,status:string,arguments?:string,preview?:string|null,appliedDiff?:string|null,exit_code?:number|null,seq?:number,parallel?:boolean,modelStep?:number|null,batch?:number|null,answerEffect?:string}} ExecutionToolRow */
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
   // a runtime notice and a compaction summary are not assistant prose.
   const kind=messageKind(message);
   if(kind!=='assistant')continue;
   if(!message.text.trim())continue;
   nodes.push({kind:'assistant',text:message.text,message,demoted:false,seq:entry.seq});
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
  if(node.kind==='assistant'){items.push({seq:node.seq,kind:turnEnded&&!node.demoted?'final_answer':'assistant_text',text:node.text,message:node.message});continue;}
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


/** @param {{time?:unknown,created_at?:unknown}} message */
export function messageTime(message) {
  const value = message.time ?? message.created_at;
  if (typeof value !== 'string' || !value.trim()) return null;
  const date = new Date(value);
  return Number.isFinite(date.getTime()) && date.getTime() > 0 ? date.toISOString() : null;
}

// ── Conversation presentation ─────────────────────────────────────────────
// The ONE projection the Desktop renderer paints and the tests read. It keeps
// the runtime's arrival order: a turn is the user line, everything that
// arrived after it, and its terminal receipt. Narration stays where it was
// written; execution groups are siblings of it, never a parent of it.

/** A path that may be presented as a file the runtime actually changed.
 * @param {string|null|undefined} path @returns {boolean} */
export function validDiffPath(path) {
  return typeof path === 'string' && path.length > 0
    && !path.startsWith('/')
    && !/^[a-z][a-z0-9+.-]*:/i.test(path)
    && !/[\\\x00-\x1f]/.test(path)
    && !path.split('/').includes('..');
}

/**
 * The real files inside one confirmed patch. Header markers are not changes,
 * and a section whose path cannot be opened is left out instead of guessed.
 * @param {string} patch
 * @returns {Array<{path:string,added:number,removed:number,patch:string}>}
 */
export function diffFiles(patch) {
  /** @type {Array<{path:string,added:number,removed:number,patch:string}>} */
  const files = [];
  if (typeof patch !== 'string' || patch === '') return files;
  for (const section of patch.split(/(?=^--- )/m)) {
    const match = /^\+\+\+ (?:b\/)?([^\t\n]+)(?:\t[^\n]*)?$/m.exec(section);
    const path = match?.[1];
    if (!path || !validDiffPath(path)) continue;
    const [added, removed] = diffCounts(section);
    const previous = files.find(file => file.path === path);
    if (previous) {
      previous.added += added;
      previous.removed += removed;
      previous.patch += section;
    } else {
      files.push({ path, added, removed, patch: section });
    }
  }
  return files;
}

/**
 * The confirmed files a set of tool calls produced, merged by path. The
 * runtime's `applied_diff` is the only source: a successful tool that
 * confirmed no change contributes nothing.
 * @param {string[]} toolIds
 * @param {ExecutionToolRow[]} tools
 */
export function confirmedChangeFiles(toolIds, tools) {
  const wanted = new Set(toolIds ?? []);
  /** @type {Array<{path:string,added:number,removed:number,patch:string}>} */
  const files = [];
  for (const tool of tools ?? []) {
    if (!wanted.has(tool.id)) continue;
    const patch = confirmedDiffOf(/** @type {any} */ (tool));
    if (!patch) continue;
    for (const file of diffFiles(patch)) {
      const previous = files.find(item => item.path === file.path);
      if (previous) {
        previous.added += file.added;
        previous.removed += file.removed;
        previous.patch += file.patch;
      } else files.push(file);
    }
  }
  return files;
}

/** @param {number|null|undefined} ms */
export function elapsedLabel(ms) {
  if (typeof ms !== 'number' || !Number.isFinite(ms) || ms <= 0) return '';
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${Math.max(1, seconds)} 秒`;
  const minutes = Math.floor(seconds / 60);
  const rest = seconds % 60;
  return rest ? `${minutes} 分 ${rest} 秒` : `${minutes} 分`;
}

/** @type {Record<string,string[]>} */
const RECEIPT_LABELS = {
  answered: ['✓', '回答结束'],
  completed: ['✓', '任务已完成'],
  completed_with_warnings: ['⚠', '已完成，但有警告'],
  truncated: ['⚠', '已完成，但有警告'],
  incomplete: ['⚠', '未完成'],
  no_final_answer: ['⚠', '执行已结束，但未提交最终回答'],
  failed: ['✗', '失败'],
  cancelled: ['⊘', '已取消'],
};
/** @type {Record<string,string>} */
const RECEIPT_TONES = {
  answered: 'success', completed: 'success',
  completed_with_warnings: 'warning', truncated: 'warning',
  incomplete: 'warning', no_final_answer: 'warning',
  failed: 'error', cancelled: 'muted',
};

/** The terminal status as the conversation's own receipt vocabulary.
 * @param {string} status */
export function receiptMarker(status) {
  const [glyph, label] = RECEIPT_LABELS[status] ?? ['·', `未知状态：${status}`];
  return { glyph, label, tone: RECEIPT_TONES[status] ?? 'muted' };
}

/** @param {{endedHm?:string|null,elapsedMs?:number|null,rounds?:number,toolCalls?:number,detail?:string|null}} terminal */
export function receiptStats(terminal) {
  return [
    terminal.endedHm || '',
    elapsedLabel(terminal.elapsedMs),
    (terminal.rounds ?? 0) > 0 ? `${terminal.rounds} 轮` : '',
    (terminal.toolCalls ?? 0) > 0 ? `${terminal.toolCalls} 次工具调用` : '',
  ].filter(Boolean);
}

/** @param {ExecutionToolRow[]} tools @param {string} id */
function toolOfId(tools, id) { return tools.find(tool => tool.id === id); }

/** @param {ExecutionToolRow[]} tools */
function statusCounts(tools) {
  const statuses = tools.map(tool => contractToolStatus(tool.status));
  return {
    running: statuses.includes('running'),
    failed: statuses.filter(status => status === 'failed').length,
    cancelled: statuses.filter(status => status === 'cancelled').length,
    unknown: statuses.filter(status => status === 'unknown').length,
    ok: statuses.length > 0 && statuses.every(status => status === 'ok'),
  };
}

/** @param {ExecutionToolRow[]} tools */
function groupedExploration(tools) {
  const receipt = groupExploration(/** @type {any[]} */ (tools))[0];
  return receipt && receipt.members.length === tools.length ? receipt : null;
}

/** @param {ExecutionToolRow[]} tools */
function isCommandGroup(tools) {
  return tools.length > 0 && tools.every(tool => isCommandTool(tool.name));
}

/**
 * Reference process vocabulary: the collapsed header names the work that
 * happened ("已读取文件并搜索代码"), never a bare item count.
 * @param {string} name
 */
function toolActivity(name) {
  const value = String(name ?? '').toLowerCase();
  if (/read_image|image/.test(value)) return 'readImage';
  if (/web_search|search_web/.test(value)) return 'webSearch';
  if (/web_fetch|web_navigate|browser_|fetch/.test(value)) return 'webFetch';
  if (/grep|search|glob|find|list/.test(value)) return 'search';
  if (/read|cat|open|view/.test(value)) return 'read';
  if (/write/.test(value)) return 'write';
  if (/edit|patch|apply/.test(value)) return 'edit';
  if (/bash|shell|command|run/.test(value)) return 'commands';
  if (/subagent|agent/.test(value)) return 'subagents';
  if (/plan/.test(value)) return 'plan';
  if (/todo/.test(value)) return 'tools';
  return 'tools';
}

/** @type {Record<string,string>} */
const ACTIVITY_LABELS = {
  read: '已读取文件', readImage: '已读取图片', search: '已搜索代码', write: '已写入文件',
  edit: '修改了文件', commands: '执行了命令', webSearch: '已搜索网页', webFetch: '已访问网页',
  subagents: '已协调子智能体', plan: '更新了计划', tools: '已调用工具', thinking: '已完成分析',
};

/**
 * Compose a closed run's title from its top three activity categories, with
 * the same join rule as the reference process header.
 * @param {ExecutionToolRow[]} members
 */
export function processActivityLabel(members) {
  /** @type {Map<string,{count:number,first:number}>} */
  const ranked = new Map();
  for (const [index, tool] of members.entries()) {
    const kind = toolActivity(tool.name);
    const entry = ranked.get(kind);
    if (entry) entry.count += 1; else ranked.set(kind, { count: 1, first: index });
  }
  const kinds = [...ranked].sort((a, b) => b[1].count - a[1].count || a[1].first - b[1].first).map(([kind]) => kind);
  const labels = kinds.slice(0, 3).map(kind => ACTIVITY_LABELS[kind] ?? ACTIVITY_LABELS.tools);
  const first = labels[0] ?? ACTIVITY_LABELS.thinking;
  const continuation = (/** @type {string} */ label) => label.charAt(0).toLowerCase() + label.slice(1);
  const second = labels[1];
  if (second === undefined) return first;
  if (labels.length === 2) {
    const shared = first.startsWith('已') && second.startsWith('已');
    return `${first}并${continuation(shared ? second.slice(1) : second)}`;
  }
  const title = [first, ...labels.slice(1).map(continuation)].join('，');
  return kinds.length > 3 ? `${title}等` : title;
}

/**
 * One view over a set of tool rows. Kept separate from block extraction so a
 * run split across model steps can be re-projected as one continuous group.
 * @param {ExecutionToolRow[]} members
 * @param {Set<string>} approvalIds
 */
function viewForMembers(members, approvalIds) {
  const counts = statusCounts(members);
  const waiting = members.some(tool => approvalIds.has(String(tool.id)));
  const status = counts.running ? 'running'
    : counts.failed ? 'failed'
      : counts.cancelled ? 'cancelled'
        : counts.unknown ? 'unknown' : 'ok';
  const editTools = members.filter(tool => isEditTool(tool.name) && !!confirmedDiffOf(/** @type {any} */ (tool)));
  const files = confirmedChangeFiles(editTools.map(tool => tool.id), members);
  const editGroup = files.length > 0;
  const allExploration = members.length > 0 && members.every(tool => isExplorationTool(tool.name));
  const exploration = allExploration && members.length >= 2 && !counts.failed ? groupedExploration(members) : null;
  const variant = editGroup ? 'edit'
    : exploration ? 'exploration'
      : members.length === 1 ? 'tool' : 'tools';
  /** @type {any} */
  const view = {
    id: members[0]?.id ?? 'empty',
    variant,
    tools: members.map(tool => tool.id),
    toolIds: members.map(tool => tool.id),
    rounds: [...new Set(members.map(tool => tool.modelStep ?? null))],
    status,
    running: counts.running,
    failed: counts.failed,
    cancelled: counts.cancelled,
    unknown: counts.unknown,
    waiting,
    commandGroup: isCommandGroup(members),
    collapsed: true,
    liveAction: '',
    failure: '',
    label: '',
  };
  if (exploration) {
    view.exploration = exploration;
    view.label = exploration.label;
  }
  if (editGroup) {
    const otherOperations = members.length - editTools.length;
    view.files = files;
    view.editTools = editTools.map(tool => tool.id);
    view.added = files.reduce((sum, file) => sum + file.added, 0);
    view.removed = files.reduce((sum, file) => sum + file.removed, 0);
    view.label = files.length
      ? `修改 ${files.length} 个文件 · +${view.added} / -${view.removed}`
      : `修改文件 · ${members.length} 项`;
    if (otherOperations > 0) view.label += ` · 另有 ${otherOperations} 项`;
  }
  if (!editGroup && !exploration && members.length > 1) {
    view.label = processActivityLabel(members);
  }
  if (variant === 'tool') {
    const tool = members[0];
    view.label = toolLabel(/** @type {any} */ (tool));
  }
  const runningTool = members.find(tool => contractToolStatus(tool.status) === 'running');
  if (runningTool) {
    view.liveAction = `${toolLabel(/** @type {any} */ (runningTool))} ${toolTarget(/** @type {any} */ (runningTool))}`.trim();
  }
  const failedTool = members.find(tool => contractToolStatus(tool.status) === 'failed');
  if (failedTool) view.failure = failureReason(failedTool.preview ?? '') ?? '';
  return view;
}

/**
 * The view one execution block presents. `kind` decides the shape the renderer
 * uses; `label` is the collapsed summary the reader sees.
 * @param {{kind:'round'|'receipt',round?:ExecutionRoundView,receipt?:any}} block
 * @param {ExecutionToolRow[]} tools
 * @param {Set<string>} approvalIds
 */
export function executionView(block, tools, approvalIds) {
  /** @type {ExecutionToolRow[]} */
  const members = block.kind === 'receipt'
    ? /** @type {ExecutionToolRow[]} */ (block.receipt.members
      .map(/** @param {{id:string}} member */ member => toolOfId(tools, member.id))
      .filter(Boolean))
    : (block.round?.tools ?? []);
  return viewForMembers(members, approvalIds);
}

/**
 * Consecutive blocks of one turn are one process, not one header per model
 * step: the reference paints a single collapsed row for the whole run.
 * @param {ReturnType<typeof executionView>[]} views
 * @param {ExecutionToolRow[]} tools
 * @param {Set<string>} approvalIds
 */
export function mergeExecutionViews(views, tools, approvalIds) {
  if (views.length === 0) return null;
  if (views.length === 1) return views[0];
  const ids = [...new Set(views.flatMap(view => view.tools))];
  const members = /** @type {ExecutionToolRow[]} */ (ids.map(id => toolOfId(tools, id)).filter(Boolean));
  if (members.length === 0) return views[0];
  const merged = viewForMembers(members, approvalIds);
  // The first block owns the stable identity so an open group survives the
  // next step arriving.
  merged.id = views[0].id;
  merged.rounds = [...new Set(views.flatMap(view => view.rounds ?? []))];
  return merged;
}

/** The status words appended to a collapsed execution summary.
 * @param {ReturnType<typeof executionView>} view */
export function executionStatusLabel(view) {
  if (view.waiting) return '等待审批';
  if (view.running) return '执行中';
  const unit = view.commandGroup ? '条' : '项';
  const parts = [];
  if (view.failed > 0) parts.push(`${view.failed} ${unit}失败`);
  if (view.cancelled > 0) parts.push(`${view.cancelled} ${unit}已停止`);
  if (view.unknown > 0) parts.push(`${view.unknown} ${unit}结果未知`);
  return parts.join(' · ');
}

/**
 * Project one session onto the conversation's painted items, in arrival order.
 *
 * This is the production projection: `renderer.mjs` iterates exactly this
 * array. Classification (final vs narration, edit vs tools, receipt status)
 * happens here, never in the renderer.
 *
 * @param {{messages?:ProjectionMessage[],tools?:ExecutionToolRow[],thoughts?:any[],reasoning?:string,turnTerminals?:any[],turnErrors?:any[],approvals?:any[],streamingMessageId?:string|null,status?:string}} state
 */
export function conversationItems(state) {
  /** @param {{seq?:number}|null|undefined} entry @param {number} index @returns {number} */
  const seqOf = (entry, index) => typeof entry?.seq === 'number' ? entry.seq : index;
  const messages = (state.messages ?? [])
    .map((message, index) => ({ ...message, seq: seqOf(message, index) }))
    .filter(message => message.btw === undefined)
    .sort((a, b) => a.seq - b.seq);
  const tools = (state.tools ?? [])
    .map((tool, index) => ({ ...tool, seq: seqOf(tool, index) }))
    .sort((a, b) => a.seq - b.seq);
  const terminals = (state.turnTerminals ?? [])
    .map((terminal, index) => ({ ...terminal, seq: seqOf(terminal, index) }))
    .sort((a, b) => a.seq - b.seq);
  const approvalIds = new Set((state.approvals ?? []).map(request => String(request.call_id ?? '')));
  const userSeqs = messages.filter(message => messageKind(message) === 'user').map(message => message.seq);
  const segments = [];
  let start = -Infinity;
  for (const seq of userSeqs) {
    segments.push({ start, end: seq });
    start = seq;
  }
  segments.push({ start, end: Infinity });
  /** @type {any[]} */
  const items = [];
  const liveThought = (state.reasoning ?? '').trim();
  for (const [segmentIndex, segment] of segments.entries()) {
    /** @param {{seq:number}} entry */
    const inSegment = entry => entry.seq >= segment.start && entry.seq < segment.end;
    const turnMessages = messages.filter(inSegment);
    const turnTools = tools.filter(inSegment);
    const turnFilePaths = [...new Set(confirmedChangeFiles(turnTools.map(tool => tool.id), turnTools).map(file => file.path))];
    const terminalRecord = terminals.filter(inSegment).at(-1) ?? null;
    const terminalStatus = terminalRecord?.status ?? null;
    const endedWithAnswer = terminalStatus !== null && terminalStatus !== 'no_final_answer';
    const projected = projectTurn(turnMessages, turnTools, endedWithAnswer);
    /** @type {ExecutionRoundView[]} */
    const rounds = [];
    for (const item of projected) {
      if (item.kind === 'execution_round') { rounds.push(item.round); continue; }
      const message = item.message;
      items.push({
        kind: item.kind === 'final_answer' ? 'final' : 'narration',
        seq: item.seq,
        message,
        streaming: state.streamingMessageId === message?.id,
        text: item.text,
        filePaths: turnFilePaths,
      });
    }
    for (const message of turnMessages) {
      const kind = messageKind(message);
      if (kind === 'user') items.push({ kind: 'user', seq: message.seq, message });
      else if (kind === 'notice') items.push({ kind: 'notice', seq: message.seq, message, text: message.text });
      else if (kind === 'summary') items.push({ kind: 'summary', seq: message.seq, message, text: compactionBody(message.text) });
    }
    for (const thought of foldedThoughts(state.thoughts ?? [])) {
      if (!inSegment(thought)) continue;
      // The runtime's reasoning stays in the model's record and in Trajectory;
      // a settled Thought paints no conversation row (the TUI's own rule).
      items.push({ kind: 'thought', seq: thought.seq, thought, visible: false });
    }
    for (const block of turnBlocks([], /** @type {any[]} */ (rounds))) {
      if (block.kind !== 'round' && block.kind !== 'receipt') continue;
      const view = executionView(block, tools, approvalIds);
      if (view.tools.length === 0) continue;
      items.push({ kind: 'execution', seq: block.seq, execution: view });
    }
    for (const error of state.turnErrors ?? []) {
      const seq = seqOf(error, 0);
      if (seq >= segment.start && seq < segment.end) items.push({ kind: 'turn_error', seq, error });
    }
    const lastToolSeq = turnTools.length ? turnTools[turnTools.length - 1].seq : -Infinity;
    const proseAfterTools = turnMessages.some(message =>
      messageKind(message) === 'assistant' && message.seq > lastToolSeq && String(message.text ?? '').trim() !== '');
    const toolCalls = turnTools.length;
    const roundsCount = rounds.length + (proseAfterTools ? 1 : 0);
    if (terminalRecord) {
      let status = terminalRecord.status;
      const hasFinal = items.some(item => item.kind === 'final'
        && item.seq >= segment.start && item.seq < segment.end);
      if (['answered', 'completed'].includes(status) && !hasFinal) status = 'no_final_answer';
      if (status === 'no_final_answer') {
        for (const item of items) {
          if (item.kind === 'final' && item.seq >= segment.start && item.seq < segment.end) item.kind = 'narration';
        }
      }
      const receiptSeq = Number.isFinite(segment.end) ? segment.end - 0.5 : Number.MAX_SAFE_INTEGER - 3;
      items.push({
        kind: 'turn_receipt',
        seq: receiptSeq,
        terminal: {
          ...receiptMarker(status),
          status,
          endedHm: terminalRecord.endedHm ?? null,
          elapsedMs: terminalRecord.elapsedMs ?? null,
          rounds: roundsCount,
          toolCalls,
          detail: terminalRecord.detail ?? null,
          stats: receiptStats({
            endedHm: terminalRecord.endedHm ?? null,
            elapsedMs: terminalRecord.elapsedMs ?? null,
            rounds: roundsCount,
            toolCalls,
          }),
        },
      });
    }
    if (segmentIndex === segments.length - 1 && liveThought !== '') {
      items.push({ kind: 'live_thought', seq: Number.MAX_SAFE_INTEGER - 2, text: liveThought, trailing: true });
    }
  }
  return items.sort((a, b) => a.seq - b.seq);
}
