// Presentation only: snapshots and events come from the Rust client protocol.
export function projectSnapshot(snapshot, history = []) {
  let state = {session:snapshot, messages:structuredClone(snapshot.messages ?? []), status:snapshot.task_status ?? snapshot.status ?? 'unknown', tools:[], approvals:[], clarifications:[], activity:'',plan:snapshot.plan??null,diff:snapshot.diff??null,diffError:null,reasoningText:'',streamingMessageId:null};
  let anchor=null;
  for (const entry of history) {
    if(entry.event.type==='user_message_added')anchor=entry.event.message.id;
    if(entry.event.type==='assistant_message_started')anchor=entry.event.message_id;
    state=applyToolEvent(state,{...entry.event,anchor});
  }
  for (const tool of snapshot.active_tools ?? []) state = applyToolEvent(state,{type:'tool_call_started',...tool,id:tool.id ?? tool.call_id,name:tool.name,arguments:tool.arguments});
  state.approvals = (snapshot.pending_interactions ?? []).filter(i=>i.type==='approval').map(i=>i.request);
  state.clarifications = (snapshot.pending_interactions ?? []).filter(i=>i.type==='clarification').map(i=>i.request);
  return state;
}
function applyToolEvent(state, event) {
  const tools = state.tools.map(t=>({...t}));
  let tool = tools.find(t=>t.id===event.id);
  if (event.type === 'tool_call_started') {
    if (!tool) tools.push({id:event.id,name:event.name,arguments:event.arguments,status:'running',preview:event.output_tail??'',anchor:'anchor' in event?event.anchor:state.messages?.at(-1)?.id});
  } else if (event.type === 'tool_call_completed' || event.type === 'tool_call_output') {
    if (!tool) {tool={id:event.id,name:'工具',arguments:'',status:'unknown',preview:''}; tools.push(tool);}
    if (event.type === 'tool_call_completed') Object.assign(tool,{status:event.ok?'success':'failed',preview:event.preview,exit_code:event.exit_code});
    else tool.preview = (tool.preview + event.chunk).slice(-16000);
  }
  return {...state,tools};
}
export function applyEvent(state,event) {
  if(['diff_updated','diff_failed'].includes(event.type)&&event.query_id&&(state.diffQuery?.id!==event.query_id||!['pending','unknown'].includes(state.diffQuery.status)))return state;
  let next=applyToolEvent(state,event);
  next.messages=state.messages.map(m=>({...m}));
  if (event.type==='user_message_added' && !next.messages.some(m=>m.id===event.message.id)) next.messages.push(event.message);
  if (event.type==='assistant_message_started' && !next.messages.some(m=>m.id===event.message_id)) next.messages.push({id:event.message_id,role:'assistant',text:''});
  if (event.type==='assistant_text_delta') {
    let message=next.messages.find(m=>m.id===event.message_id);
    if (!message) {message={id:event.message_id,role:'assistant',text:''};next.messages.push(message);}
    message.text+=event.delta;
  }
  if(event.type==='assistant_message_started'||event.type==='assistant_text_delta')next.streamingMessageId=event.message_id;
  if(event.type==='assistant_message_completed'&&state.streamingMessageId===event.message_id)next.streamingMessageId=null;
  if(terminalEvents.has(event.type)||event.type==='assistant_attempt_reset')next.streamingMessageId=null;
  if(event.type==='reasoning_delta') next.reasoningText=(state.reasoningText??'')+event.delta;
  if(event.type==='plan_updated') next.plan=event.plan;
  if(event.type==='diff_updated'){next.diff=event.diff;next.diffError=null;if(event.query_id)next.diffQuery={id:event.query_id,status:'confirmed',error:null};}
  if(event.type==='diff_failed'){next.diff=null;next.diffError=event.message;if(event.query_id)next.diffQuery={id:event.query_id,status:'failed',error:event.message};}
  if(event.type==='assistant_attempt_reset') next.reasoningText='';
  if (event.type==='assistant_attempt_reset') next.messages=next.messages.filter(m=>m.id!==event.message_id);
  if (event.type==='approval_requested') next.approvals=[...state.approvals.filter(a=>a.id!==event.request.id),event.request];
  if (event.type==='approval_resolved') next.approvals=state.approvals.filter(a=>a.id!==event.id);
  if (event.type==='clarification_requested') next.clarifications=[...state.clarifications.filter(a=>a.id!==event.request.id),event.request];
  if (event.type==='clarification_resolved') next.clarifications=state.clarifications.filter(a=>a.id!==event.id);
  if (event.type==='agent_activity' || event.type==='command_progress') next.activity=event.label;
  return next;
}
export function commandEnvelope(sessionId,command) {
  return {command_id:crypto.randomUUID(),session_id:sessionId,expected_version:null,issued_at:new Date().toISOString(),command};
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
 const overlay=!docked||viewport-sidebarWidth-360<480;
 const maximum=overlay?Math.max(1,viewport-56):Math.max(360,viewport-sidebarWidth-480);
 const actual=Math.min(Math.max(360,width),Math.min(700,maximum));
 return {overlay,width:actual,showContext:actual>=560};
}

export function beginDiffQuery(state,id){return {...state,diffQuery:{id,status:'pending',error:null}};}
export function expireDiffQuery(state,id,message){if(state.diffQuery?.id!==id||state.diffQuery.status!=='pending')return state;return {...state,diffQuery:{...state.diffQuery,status:'unknown',error:message}};}

export function beginReadQuery(prior,sessionId,id,eventType){return {sessionId,id,eventType,status:'pending',data:prior?.data??null,error:null};}
export function applyReadResult(read,frame){if(!read||!['pending','unknown'].includes(read.status)||frame.session_id!==read.sessionId||frame.event!=='runtime'||frame.data?.type!==read.eventType||frame.data.query_id!==read.id)return read;return {...read,status:'confirmed',data:frame.data,error:null};}
export function expireReadQuery(read,id,message){if(read?.id!==id||read.status!=='pending')return read;return {...read,status:'unknown',error:message};}
