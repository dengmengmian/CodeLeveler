import {SNAPSHOT_VERSIONED_COMMANDS} from './command-policy.gen.mjs';
// Presentation only: snapshots and events come from the Rust client protocol.
//
// Execution semantics live in `presentation.mjs` (Contract v1); this module
// only carries the runtime facts the renderer and the conformance test project.
import {
  committedFinalAnswer,
  toolStatusFromOutcome,
  turnTerminalFromEvent,
} from './presentation.mjs';

export function projectSnapshot(snapshot, history = []) {
  let state = {session:snapshot, messages:[], nextSeq:0, status:snapshot.task_status ?? snapshot.status ?? 'unknown', tools:[], thoughts:[], reasoning:'', reasoningSuperseded:false, approvals:[], clarifications:[], activity:'', activityElapsedMs:null, plan:snapshot.plan??null,diff:snapshot.diff??null,diffError:null,streamingMessageId:null,lastTerminal:null,turnErrors:[],turnTerminals:[],turnStartedAt:null};
  if (history.length > 0) {
    // The durable log is the conversation; the snapshot's `messages` are the
    // ACTIVE MODEL CONTEXT (`/compact` leaves one summary row there). Replaying
    // the runtime's own normalized events through the SAME mapping the live
    // stream uses is what keeps a reopened session's chronology whole.
    let anchor=null;
    for (const [index, entry] of history.entries()) {
      const raw=entry.event ?? entry;
      if(raw.type==='user_message_added')anchor=raw.message.id;
      if(raw.type==='assistant_message_started')anchor=raw.message_id;
      const event={...raw,seq:index};
      if(raw.type==='reasoning_started'||raw.type==='reasoning_delta'||raw.type==='reasoning_completed')state=applyEvent(state,event);
      else state=applyEvent(state,{...event,anchor});
    }
  } else {
    // No durable turn: the snapshot is all there is (a brand-new session).
    // These rows are a snapshot's ACTIVE MODEL CONTEXT, not a transcript; a
    // later durable replay replaces them, so they are marked as placeholders.
    state.messages = structuredClone(snapshot.messages ?? []).map((message,index)=>({...message,seq:index,snapshotContext:true}));
    state.nextSeq = state.messages.length;
  }
  // Replay stamps every entry with its log index, but only thoughts advance
  // `nextSeq`. Without this reset the FIRST live event after a replay would be
  // numbered 0 again: it would fall into the previous turn's bucket, and that
  // turn's answer would claim the new turn's file changes as its own.
  if (history.length > 0) {
    let replayed = history.length;
    for (const message of state.messages) replayed = Math.max(replayed, (message.seq ?? -1) + 1);
    for (const tool of state.tools) replayed = Math.max(replayed, (tool.seq ?? -1) + 1);
    state.nextSeq = Math.max(state.nextSeq, replayed);
    // A replay has no wall clock. The durable log does carry each turn's own
    // elapsed, so the receipt keeps the runtime's measurement and omits the
    // clock instead of inventing one from the moment the client replayed it.
    const terminals = history.filter(entry => terminalEvents.has((entry.event ?? entry).type));
    state.turnTerminals = (state.turnTerminals ?? []).map((terminal, index) => ({
      ...terminal,
      at: null,
      endedHm: null,
      elapsedMs: Number.isFinite(terminals[index]?.turn_elapsed_ms) ? terminals[index].turn_elapsed_ms : null,
    }));
    state.turnStartedAt = null;
  }
  // Replayed history events carry no terminal classification. The runtime's
  // snapshot is the authority on whether the newest declaration is active or
  // history, so a completed plan is never resurrected as active work here.
  if (snapshot.plan) state.plan = snapshot.plan;
  for (const tool of snapshot.active_tools ?? []) state = applyEvent(state,{type:'tool_call_started',...tool,id:tool.id ?? tool.call_id,name:tool.name,arguments:tool.arguments,snapshotContext:true});
  // The durable history's turn terminal is the runtime's own fact: without it a
  // reopened turn could not tell an answer from interim narration (Contract §I9).
  const terminalEntry = [...history].reverse().find(entry=>terminalEvents.has((entry.event??entry).type));
  const terminal = terminalEntry?.event?.type;
  if (terminal) state = {...state, lastTerminal: turnTerminalFromEvent(terminal, state.messages, state.tools)};

  state.approvals = (snapshot.pending_interactions ?? []).filter(i=>i.type==='approval').map(i=>i.request);
  state.clarifications = (snapshot.pending_interactions ?? []).filter(i=>i.type==='clarification').map(i=>i.request);
  return state;
}
/** The message a process row belongs to: the newest one when the row began. */
function lastMessageId(messages){return messages&&messages.length?messages[messages.length-1].id:null;}

function applyToolEvent(state, event) {
  const tools = state.tools.map(t=>({...t}));
  let nextSeq = state.nextSeq ?? 0;
  let tool = tools.find(t=>t.id===event.id);
  if (event.type === 'tool_call_started') {
    if (!tool) {
      // A batch is OBSERVED here, never inferred later: this call starts while
      // those parallel calls are still running, so they were in flight
      // together (Contract v1 §I3).
      let batch = null;
      if (event.parallel) {
        const inFlight = tools.filter(t=>t.status==='running'&&t.parallel);
        if (inFlight.length > 0) {
          batch = inFlight.find(t=>t.batch!=null)?.batch ?? Math.max(-1,...tools.map(t=>t.batch??-1)) + 1;
          for (const t of inFlight) t.batch = batch;
        }
      }
      tools.push({id:event.id,name:event.name,arguments:event.arguments,status:'running',preview:event.output_tail??'',anchor:'anchor' in event?event.anchor:state.messages?.at(-1)?.id,parallel:!!event.parallel,modelStep:event.model_step??null,
        // The runtime's answer classification. `?? 'work'` is the conservative
        // read of a peer that did not state it (AnswerEffect::unstated), never
        // a local decision about the tool name.
        answerEffect:event.answer_effect??'work',appliedDiff:null,batch,seq:event.seq??nextSeq++,
        ...(event.snapshotContext?{snapshotContext:true}:{live:true})});
    }
  } else if (event.type === 'tool_call_completed' || event.type === 'tool_call_output') {
    if (!tool) {tool={id:event.id,name:'工具',arguments:'',status:'unknown',preview:'',appliedDiff:null,parallel:false,modelStep:null,answerEffect:'work',batch:null,live:true}; tools.push(tool);}
    delete tool.snapshotContext;
    tool.live = true;
    if (event.type === 'tool_call_completed') Object.assign(tool,{status:toolStatusFromOutcome(event.ok,event.stop),preview:event.preview,exit_code:event.exit_code,appliedDiff:event.applied_diff??null});
    // A finished call no longer owns the live label (TUI clears activity on
    // the same event); the next progress event or the turn's clock takes over.
    if (event.type === 'tool_call_completed') return {...state,tools,nextSeq,activity:'',activityElapsedMs:null};
    else tool.preview = (tool.preview + event.chunk).slice(-16000);
  }
  return {...state,tools,nextSeq};
}
/**
 * A durable-history replay replaces the conversation up to the query. Live
 * events that arrived while that query was in flight are not in the replay and
 * must survive it; events it already contains must not be applied twice.
 *
 * The replay owns every fact it carries. A live row is appended only when its
 * identity is absent, or adopted when it has advanced a tool past the replay
 * (running -> terminal). New rows are numbered after the replay, preserving
 * their live arrival order.
 */
export function mergeRestoredConversation(restored, live) {
  const seqMax = entries => entries.reduce((max, entry) => Math.max(max, (entry.seq ?? -1) + 1), 0);
  const floor = Math.max(seqMax(restored.messages), seqMax(restored.tools));
  let nextSeq = Math.max(restored.nextSeq ?? 0, live.nextSeq ?? 0, floor);
  const adopt = entry => {
    const seq = typeof entry.seq === 'number' && entry.seq >= floor ? entry.seq : nextSeq++;
    nextSeq = Math.max(nextSeq, seq + 1);
    return { ...entry, seq };
  };
  const liveMessages = live.messages ?? [];
  const liveMessageById = new Map(liveMessages.map(message => [message.id, message]));
  const matched = new Set();
  for (const message of restored.messages) if (liveMessageById.has(message.id)) matched.add(message.id);
  // Durable history re-creates user message ids on replay, so an id-only match
  // would append every user line a second time. Match unmatched restored rows
  // against the next unmatched live row with the same role and text, in order.
  let cursor = 0;
  for (const message of restored.messages) {
    if (liveMessageById.has(message.id)) continue;
    while (cursor < liveMessages.length && (matched.has(liveMessages[cursor].id) || liveMessages[cursor].snapshotContext)) cursor += 1;
    const candidate = liveMessages[cursor];
    if (!candidate) break;
    const sameKind = candidate.role === message.role && (candidate.kind ?? null) === (message.kind ?? null);
    const sameText = candidate.text === message.text
      || (message.role === 'assistant' && String(candidate.text ?? '').startsWith(String(message.text ?? '')));
    if (sameKind && sameText) { matched.add(candidate.id); cursor += 1; }
  }
  const messages = restored.messages.map(message => {
    const current = liveMessageById.get(message.id);
    if (!current) return message;
    // A stream that continued after the replay keeps the newer text, at the
    // position the durable log already gave it.
    if ((current.text?.length ?? 0) > (message.text?.length ?? 0) || (current.final && !message.final)) {
      return { ...message, ...current, seq: message.seq };
    }
    return message;
  });
  // Only a state that has no durable transcript yet keeps its snapshot rows;
  // once history arrives, a snapshot's ACTIVE CONTEXT is not a transcript.
  const keepSnapshotMessages = restored.messages.length === 0;
  for (const message of liveMessages
    .filter(item => !matched.has(item.id) && (keepSnapshotMessages || !item.snapshotContext))
    .sort((a, b) => (a.seq ?? 0) - (b.seq ?? 0))) {
    messages.push(adopt(message));
  }
  const terminalRank = status => status === 'running' ? 0 : 1;
  const restoredTools = new Set(restored.tools.map(tool => tool.id));
  const liveTools = new Map(live.tools.map(tool => [tool.id, tool]));
  const tools = restored.tools.map(tool => {
    const current = liveTools.get(tool.id);
    if (!current) return tool;
    return terminalRank(current.status) > terminalRank(tool.status) ? { ...tool, ...current, seq: tool.seq } : tool;
  });
  const keepSnapshotTools = restored.tools.length === 0;
  for (const tool of live.tools
    .filter(item => !restoredTools.has(item.id) && (keepSnapshotTools || !item.snapshotContext))
    .sort((a, b) => (a.seq ?? 0) - (b.seq ?? 0))) {
    tools.push(adopt(tool));
  }
  const thoughtIds = new Set(restored.thoughts.map(thought => thought.id));
  const thoughts = [...restored.thoughts];
  for (const thought of (live.thoughts ?? []).filter(item => !thoughtIds.has(item.id))) {
    thoughts.push(adopt(thought));
  }
  const turnTerminals = [...restored.turnTerminals];
  for (const terminal of (live.turnTerminals ?? []).slice(turnTerminals.length)) {
    turnTerminals.push(adopt(terminal));
  }
  const errorIds = new Set(restored.turnErrors.map(error => error.id));
  const turnErrors = [...restored.turnErrors];
  for (const error of live.turnErrors ?? []) if (!errorIds.has(error.id)) turnErrors.push(adopt(error));
  return {
    ...live,
    messages,
    tools,
    thoughts,
    turnTerminals,
    turnErrors,
    nextSeq,
    lastTerminal: turnTerminals.length > restored.turnTerminals.length
      ? live.lastTerminal ?? restored.lastTerminal
      : restored.lastTerminal ?? live.lastTerminal,
  };
}
export function applyEvent(state,event) {
  if(['diff_updated','diff_failed'].includes(event.type)&&event.query_id&&(state.diffQuery?.id!==event.query_id||!['pending','unknown'].includes(state.diffQuery.status)))return state;
  let next=applyToolEvent(state,event);
  next.messages=state.messages.map(m=>({...m}));
  // Arrival order is the fact the Execution Presentation Contract groups by, so
  // every message and tool carries the same monotonic stamp.
  if (event.type==='user_message_added' && !next.messages.some(m=>m.id===event.message.id)) next.messages.push({...event.message,seq:event.seq??next.nextSeq++,live:true});
  else if (event.type==='user_message_added') {const existing=next.messages.find(m=>m.id===event.message.id);delete existing.snapshotContext;existing.live=true;}
  if (event.type==='assistant_message_started' && !next.messages.some(m=>m.id===event.message_id)) next.messages.push({id:event.message_id,role:'assistant',text:'',seq:event.seq??next.nextSeq++,live:true});
  else if (event.type==='assistant_message_started') {const existing=next.messages.find(m=>m.id===event.message_id);delete existing.snapshotContext;existing.live=true;}
  if (event.type==='assistant_text_delta') {
    let message=next.messages.find(m=>m.id===event.message_id);
    if (!message) {message={id:event.message_id,role:'assistant',text:'',seq:event.seq??next.nextSeq++,live:true};next.messages.push(message);}
    delete message.snapshotContext;message.live=true;
    message.text+=event.delta;
  }
  if(event.type==='assistant_message_started'||event.type==='assistant_text_delta')next.streamingMessageId=event.message_id;
  if(event.type==='assistant_message_completed'&&state.streamingMessageId===event.message_id)next.streamingMessageId=null;
  if(terminalEvents.has(event.type)||event.type==='assistant_attempt_reset')next.streamingMessageId=null;
  if(event.type==='plan_updated') next.plan=event.plan?{...event.plan,state:'active'}:null;
  if(event.type==='turn_failed'){
    const text=event.failure?.message||event.error||'Runtime 执行失败';
    const anchor=event.anchor??[...next.messages].reverse().find(message=>message.role==='user')?.id??null;
    const seq=event.seq??next.nextSeq++;
    next.turnErrors=[...(state.turnErrors??[]),{id:`turn-error-${seq}`,text,anchor,seq}];
  }
  if(event.type==='context_compacted'){
    // The compaction is a durable conversation fact, not only a toast: a
    // reopened session must still say where the context was re-based. The row
    // is runtime-authored, never something the person typed.
    next.messages=next.messages.some(m=>m.id===`compact-${event.from}-${event.to}`)?next.messages:[...next.messages,{id:`compact-${event.from}-${event.to}`,role:'user',kind:'runtime_notice',text:`上下文已压缩 ${event.from} → ${event.to} 条`,seq:event.seq??next.nextSeq++}];
  }
  if(event.type==='diff_updated'){next.diff=event.diff;next.diffError=null;if(event.query_id)next.diffQuery={id:event.query_id,status:'confirmed',error:null};}
  if(event.type==='diff_failed'){next.diff=null;next.diffError=event.message;if(event.query_id)next.diffQuery={id:event.query_id,status:'failed',error:event.message};}
  if(event.type==='assistant_attempt_reset') next.messages=next.messages.filter(m=>m.id!==event.message_id);
  if (event.type==='approval_requested') next.approvals=[...state.approvals.filter(a=>a.id!==event.request.id),event.request];
  if (event.type==='approval_resolved') next.approvals=state.approvals.filter(a=>a.id!==event.id);
  if (event.type==='clarification_requested') next.clarifications=[...state.clarifications.filter(a=>a.id!==event.request.id),event.request];
  if (event.type==='clarification_resolved') next.clarifications=state.clarifications.filter(a=>a.id!==event.id);
  if (event.type==='agent_activity' || event.type==='command_progress') {
    next.activity=event.label;
    next.activityElapsedMs=event.type==='command_progress'&&Number.isFinite(event.elapsed_ms)?event.elapsed_ms:null;
  }
  // Reasoning is never assistant prose. A running segment is the live Thinking
  // row; `reasoning_completed` freezes it into a folded Thought with the
  // runtime's own duration. Taking a tool action ends the segment, exactly as
  // the reference does, and the next delta opens a new one.
  next.thoughts=next.thoughts??[];
  next.reasoning=next.reasoning??'';
  if(event.type==='reasoning_delta'){
    if(state.reasoningSuperseded||(next.reasoning||'')===''){next.reasoning='';next.reasoningSuperseded=false;next.reasoningSeq=event.seq??next.nextSeq++;}
    next.reasoning+=event.delta;
  }
  if(event.type==='reasoning_started'){if(state.reasoningSuperseded){next.reasoning='';next.reasoningSuperseded=false;}next.reasoningSeq=event.seq??next.nextSeq++;}
  if(event.type==='reasoning_completed'){
    const text=(next.reasoning||'').trim();
    if(text!=='')next.thoughts=[...next.thoughts,{id:`th-${next.nextSeq++}`,text,elapsedMs:event.elapsed_ms,anchor:lastMessageId(next.messages),seq:next.reasoningSeq??next.nextSeq}];
    next.reasoningSeq=null;
    next.reasoning='';next.reasoningSuperseded=false;
  }
  if(event.type==='tool_call_started'||event.type==='assistant_message_started'){
    // An action ends the reasoning segment. A segment the runtime never
    // completed is INTERRUPTED — kept and folded, never presented as finished —
    // instead of being thrown away with the live buffer.
    const partial=(next.reasoning||'').trim();
    if(partial!==''){
      next.thoughts=[...next.thoughts,{id:`th-${next.nextSeq++}`,text:partial,elapsedMs:0,interrupted:true,anchor:lastMessageId(next.messages),seq:next.reasoningSeq??next.nextSeq}];
      next.reasoningSeq=null;
      next.reasoning='';
    }
    next.reasoningSuperseded=true;
  }
  if(event.type==='user_message_added'){next.reasoning='';next.reasoningSuperseded=false;}
  if (event.type==='user_message_added'||event.type==='assistant_message_started'||event.type==='tool_call_started') {
    // One turn clock. A steer that arrives while a turn runs keeps the clock
    // it already has; the next user line after a terminal starts a new one.
    next.turnStartedAt ??= Date.now();
  }
  if (terminalEvents.has(event.type)) {
    // A terminal turn owns no live activity: leaving the last label behind
    // masks the settled status the runtime reports next (TUI finish_turn clears
    // it for the same reason).
    next.activity = '';
    next.activityElapsedMs = null;
    // The turn's committed answer, recorded on its own message: a frozen turn
    // must not look like it never answered.
    const answer=committedFinalAnswer(next.messages,next.tools);
    if(answer!==null)for(let i=next.messages.length-1;i>=0;i-=1){const message=next.messages[i];if(message.role==='assistant'&&message.text===answer){message.final=true;break;}}
    const status=turnTerminalFromEvent(event.type,next.messages,next.tools);
    next.lastTerminal=status;
    const terminalSeq=event.seq??((next.nextSeq??0)-0.5);
    next.turnTerminals=[...(next.turnTerminals??[]),{
      seq:terminalSeq,
      status,
      at:Date.now(),
      endedHm:new Date().toLocaleTimeString('zh-CN',{hour:'2-digit',minute:'2-digit',hour12:false}),
      elapsedMs:Number.isFinite(next.turnStartedAt)?Math.max(0,Date.now()-next.turnStartedAt):null,
      detail:event.reason??event.error??event.failure?.message??null,
    }];
    next.turnStartedAt=null;
    // A segment the runtime never completed is INTERRUPTED: kept and folded,
    // never presented as finished. Terminalization must not discard it.
    const unfinished=(next.reasoning||'').trim();
    if(unfinished!=='')next.thoughts=[...next.thoughts,{id:`th-interrupted-${next.nextSeq++}`,text:unfinished,elapsedMs:0,interrupted:true,anchor:lastMessageId(next.messages),seq:next.reasoningSeq??next.nextSeq}];
    next.reasoning='';next.reasoningSuperseded=false;next.reasoningSeq=null;
  }
  return next;
}
export function commandEnvelope(sessionId,command,observedVersion) {
  const versioned=SNAPSHOT_VERSIONED_COMMANDS.includes(command.type);
  if(versioned&&(!Number.isSafeInteger(observedVersion)||observedVersion<0))throw new Error('snapshot version required');
  return {command_id:crypto.randomUUID(),session_id:sessionId,expected_version:versioned?observedVersion:null,issued_at:new Date().toISOString(),command};
}
export const terminalEvents = new Set(['turn_completed','turn_completed_with_warnings','turn_answered','turn_failed','turn_cancelled','task_cancelled','turn_truncated','turn_incomplete']);
export function approvalDecisions(request){return request.requires_human_consent?['approve_once','deny']:['approve_once','approve_session',...(request.always_persists?['approve_always']:[]),'deny'];}
export function clarificationAnswer(question,{picks=[],text=''}){
  const kind=question.kind??'single';text=text.trim();
  if(kind==='text'){if(!text)throw new Error('请输入回答');return text;}
  if(text){if(!question.allow_other)throw new Error('此问题不允许自由输入');if(picks.length)throw new Error('请选择选项或填写其他回答');return text;}
  if(picks.some(p=>!(question.options??[]).includes(p)))throw new Error('无效选项');
  if(kind==='single'&&picks.length!==1)throw new Error('请选择一项');
  if(kind==='multi'&&(picks.length<(question.min_choices??0)||picks.length>(question.max_choices??Infinity)))throw new Error(`请选择 ${question.min_choices??0} 至 ${question.max_choices??question.options?.length??0} 项`);
  return picks.length?picks.join(', '):'(none)';
}
/**
 * A terminal resync rebuilds from durable history, which does not store a
 * cancelled segment's partial reasoning. The live segment was a real runtime
 * event; keep it (deduped by anchor+text) until the session is reopened.
 */
export function retainInterruptedThoughts(next,prior){
 const kept=(prior?.thoughts??[]).filter(thought=>thought.interrupted===true);
 if(!kept.length)return next;
 const seen=new Set((next.thoughts??[]).map(thought=>`${thought.anchor??''}\u0000${thought.text}`));
 const extra=kept.filter(thought=>!seen.has(`${thought.anchor??''}\u0000${thought.text}`));
 return extra.length?{...next,thoughts:[...(next.thoughts??[]),...extra]}:next;
}

export function applySessionMetadata(state,snapshot){
  // SessionUpdated is metadata-only by the existing client protocol. A command
  // receipt is not a transcript checkpoint; live messages remain event-derived.
  const metadata={...snapshot};delete metadata.messages;
  return {...state,session:{...state.session,...metadata},status:snapshot.task_status??snapshot.status??state.status,plan:snapshot.plan??null,diff:snapshot.diff??null,diffError:snapshot.diff?null:state.diffError};
}

export function uploadDraft(draft,update){
 if(update.remove)return draft.filter(item=>item.key!==update.remove&&item.attachment?.id!==update.remove);
 const index=draft.findIndex(item=>(update.attachment&&item.attachment?.id===update.attachment.id)||(update.key&&item.key===update.key));
 const previous=draft[index];
 if(previous?.status==='ready'&&update.status!=='ready')return draft;
 const item={...previous,...update,key:previous?.key??update.key??update.attachment?.id};
 if(['ready','uploading'].includes(update.status))delete item.error;
 return index<0?[...draft,item]:draft.map((entry,i)=>i===index?item:entry);
}

export function applyUploadEvent(draft,event){
 if(!event.command_id)return draft;
 const pending=draft.find(item=>item.import_id===event.command_id&&['queued','unknown','uploading'].includes(item.status));
 if(!pending)return draft;
 if(event.type==='attachment_added')return uploadDraft(draft,{key:pending.key,status:'ready',attachment:event.attachment});
 if(event.type==='attachment_processing_failed')return uploadDraft(draft,{key:pending.key,status:'failed',error:{message:event.error}});
 return draft;
}

export function reconcileUploadReply(draft,reply,earlyEvents){const updated=uploadDraft(draft,reply);return earlyEvents.reduce((state,event)=>applyUploadEvent(state,event),updated);}

export function workspaceDraftText({isHome,text,homeDraft}){return isHome?text:homeDraft??'';}

export function attachmentSubmissionError(attachments){return attachments.some(ref=>ref.kind!=='image')?'普通文件已上传，当前暂不能发送给模型。移除附件后可发送文字。':'';}

export function permissionCommand(sessionId,mode){if(!sessionId||!['full_access','assisted','request_approval'].includes(mode))throw new Error('无效的权限模式');return {type:'set_permission_profile',session_id:sessionId,mode};}
export function permissionConfirmed(current,sessionId,mode){return current?.session.id===sessionId&&current.session.mode===mode;}

export function workbenchLayout({viewport,sidebarWidth,width=360,docked=true,expanded=false}){
 if(expanded)return {overlay:false,width:viewport,showContext:viewport>=560};
 // Geometry follows the reference right panel: 300px floor, 400px protected
 // centre, at most 70% of the frame, and an overlay that never exceeds the
 // viewport minus the 56px rail.
 const centerMinimum=400,rightMinimum=300;
 const available=viewport-sidebarWidth-centerMinimum;
 const overlay=!docked||available<rightMinimum;
 const maximum=overlay?Math.max(1,viewport-56):Math.min(Math.max(1,available),Math.round(viewport*0.7));
 const actual=Math.min(Math.max(rightMinimum,width),Math.max(1,maximum));
 return {overlay,width:actual,showContext:actual>=560};
}

export function beginDiffQuery(state,id){return {...state,diffQuery:{id,status:'pending',error:null}};}
export function expireDiffQuery(state,id,message){if(state.diffQuery?.id!==id||state.diffQuery.status!=='pending')return state;return {...state,diffQuery:{...state.diffQuery,status:'unknown',error:message}};}

export function beginReadQuery(prior,sessionId,id,eventType){return {sessionId,id,eventType,status:'pending',data:prior?.data??null,error:null};}
export function applyReadResult(read,frame){if(!read||!['pending','unknown'].includes(read.status)||frame.session_id!==read.sessionId||frame.event!=='runtime'||frame.data?.type!==read.eventType||frame.data.query_id!==read.id)return read;return {...read,status:'confirmed',data:frame.data,error:null};}
export function expireReadQuery(read,id,message){if(read?.id!==id||read.status!=='pending')return read;return {...read,status:'unknown',error:message};}
