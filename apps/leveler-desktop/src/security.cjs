const {SNAPSHOT_VERSIONED_COMMANDS}=require('./command-policy.gen.cjs');
const allowed=new Set(['submit_message','submit_user_input','steer_current_turn','steer_user_input','cancel_current_turn','cancel_task','resume_task','approval_decision','answer_clarification','definition_trust_decision','capture_secret','revoke_secret','query_session_history','request_diff','list_agents','get_agent','query_context','query_observability','select_model','set_permission_profile','set_product_axes','rename_session','archive_session','unarchive_session','fork_session','compact_context','cancel_background_task','manage_schedule']);
const decisions=new Set(['approve_once','approve_session','approve_always','deny']);
const {validRef}=require('./attachment-ingress.cjs');
function string(value,max=256){return typeof value==='string'&&value.length>0&&value.length<=max;}
function exactKeys(value,keys){return !!value&&typeof value==='object'&&!Array.isArray(value)&&Object.keys(value).length===keys.length&&keys.every(key=>Object.hasOwn(value,key));}
function nonblank(value){return string(value)&&value.trim().length>0;}
const MAX_SECRET_BYTES=64*1024;
function secretValue(value){return typeof value==='string'&&value.length>0&&value.length<=MAX_SECRET_BYTES;}
function secretRef(value){return typeof value==='string'&&/^sec_[0-9a-f]{32}$/.test(value);}
// A capture carries a value and a placeholder that occupies exactly one span
// of the message. The runtime revalidates and owns custody; this boundary
// rejects malformed shapes before any value can reach the vault.
function validCaptures(content,captures,ack){
  if(typeof ack!=='boolean'||!Array.isArray(captures)||captures.length>16)return false;
  if(ack&&captures.length)return false;
  const seen=new Set();
  for(const capture of captures){
    if(!capture||typeof capture!=='object'||Array.isArray(capture))return false;
    const keys=['placeholder','value',...(Object.hasOwn(capture,'key_hint')?['key_hint']:[])];
    if(!exactKeys(capture,keys)||!string(capture.placeholder,256)||!secretValue(capture.value))return false;
    if(Object.hasOwn(capture,'key_hint')&&!string(capture.key_hint,128))return false;
    if(seen.has(capture.placeholder))return false;
    seen.add(capture.placeholder);
    if(content.split(capture.placeholder).length-1!==1)return false;
  }
  return true;
}
function validSecretInput(command){
  if(typeof command.content!=='string'||command.content.length>100000)return false;
  const captures=command.secret_captures??[];
  const ack=command.plaintext_secret_ack??false;
  if(ack&&!command.content)return false;
  return validCaptures(command.content,captures,ack);
}
function validateEnvelope(envelope,authorizeAttachment){
  if(!envelope||!string(envelope.command_id)||!string(envelope.session_id)||typeof envelope.issued_at!=='string'||!Number.isFinite(Date.parse(envelope.issued_at)))throw new Error('Invalid command envelope');
  const command=envelope.command;
  if(!command||!allowed.has(command.type))throw new Error('Unsupported Desktop command');
  if(SNAPSHOT_VERSIONED_COMMANDS.includes(command.type)?(!Number.isSafeInteger(envelope.expected_version)||envelope.expected_version<0):envelope.expected_version!==null)throw new Error('Invalid snapshot version');
  if(command.session_id && command.session_id!==envelope.session_id)throw new Error('Session mismatch');
  if(['submit_message','steer_current_turn'].includes(command.type)&&((!string(command.content,100000)&&!(command.type==='submit_message'&&command.content===''&&Array.isArray(command.attachments)&&command.attachments.length>0))||command.session_id!==envelope.session_id))throw new Error('Invalid message');
  if(command.type==='submit_message'&&Object.keys(command).some(key=>!['type','session_id','content','attachments'].includes(key)))throw new Error('Invalid message fields');
  if(command.type==='submit_message'&&command.attachments!==undefined){
    if(Array.isArray(command.attachments)&&command.attachments.some(ref=>ref?.kind!=='image'))throw new Error('普通文件已上传，但当前 Runtime 不支持将其内容发送给模型；请移除附件后发送文字');
    if(!Array.isArray(command.attachments)||command.attachments.length>16||command.attachments.some(ref=>!validRef(ref)||!authorizeAttachment?.(envelope.session_id,ref)))throw new Error('Invalid or unregistered attachment');
    if(new Set(command.attachments.map(ref=>ref.id)).size!==command.attachments.length)throw new Error('Duplicate attachment');
  }
  if(['cancel_task','resume_task'].includes(command.type)){
    const keys=command.type==='resume_task'?['type','session_id','content']:['type','session_id'];
    if(!exactKeys(command,keys)||!nonblank(command.session_id)||command.session_id!==envelope.session_id)throw new Error('Invalid task command');
    if(command.type==='resume_task'&&!string(command.content,100000))throw new Error('Invalid continuation');
  }
  if(command.type==='cancel_current_turn'&&command.session_id!==envelope.session_id)throw new Error('Invalid cancellation');
  if(command.type==='cancel_background_task'&&(!exactKeys(command,['type','session_id','task_id'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!nonblank(command.task_id)))throw new Error('Invalid background task cancellation');
  if(command.type==='approval_decision'&&(!string(command.request_id)||!decisions.has(command.decision)))throw new Error('Invalid approval decision');
  if(command.type==='definition_trust_decision'){
    // A trust answer is bound to the exact waiter and the session that owns
    // it. The Runtime refuses a mismatched answer; the IPC boundary refuses
    // to build one in the first place.
    if(!exactKeys(command,['type','request_id','decision','session_id'])||!nonblank(command.request_id)||command.request_id.length>256||!['allow_once','trust_current_version','deny'].includes(command.decision)||!nonblank(command.session_id)||command.session_id!==envelope.session_id)throw new Error('Invalid definition trust decision');
  }
  if(command.type==='submit_user_input'){
    // Captures ride beside the message; the runtime is the authority that
    // takes custody and replaces placeholders. A plaintext acknowledgement is
    // a different decision and never travels with captures.
    if(!exactKeys(command,['type','session_id','content','attachments','secret_captures','plaintext_secret_ack'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!validSecretInput(command))throw new Error('Invalid secret submission');
    if(!Array.isArray(command.attachments)||command.attachments.length>16||command.attachments.some(ref=>ref?.kind!=='image'||!validRef(ref)||!authorizeAttachment?.(envelope.session_id,ref)))throw new Error('Invalid or unregistered attachment');
    if(!command.content&&!command.attachments.length)throw new Error('Empty secret submission');
  }
  if(command.type==='steer_user_input'){
    if(!exactKeys(command,['type','session_id','content','secret_captures','plaintext_secret_ack'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!command.content||!validSecretInput(command))throw new Error('Invalid secret steer');
  }
  if(command.type==='capture_secret'){
    const keys=['type','session_id','request_id','value',...(Object.hasOwn(command,'key_hint')?['key_hint']:[])];
    if(!exactKeys(command,keys)||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!string(command.request_id,256)||!secretValue(command.value)||(Object.hasOwn(command,'key_hint')&&!string(command.key_hint,128)))throw new Error('Invalid secret capture');
  }
  if(command.type==='revoke_secret'){
    if(!exactKeys(command,['type','session_id','secret_ref'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!secretRef(command.secret_ref))throw new Error('Invalid secret revoke');
  }
  if(command.type==='answer_clarification'&&(!string(command.request_id)||typeof command.answer!=='string'||command.answer.length>100000))throw new Error('Invalid clarification');
  if(command.type==='query_session_history'&&command.session_id!==envelope.session_id)throw new Error('Invalid history query');
  if(['list_agents','get_agent','query_context','query_observability'].includes(command.type)){
    const keys=['type','session_id','query_id',...(command.type==='get_agent'?['name']:[]),...(command.type==='query_observability'?['center_seq','before','after']:[])];
    if(!exactKeys(command,keys)||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!nonblank(command.query_id))throw new Error('Invalid settings query');
    if(command.type==='get_agent'&&(typeof command.name!=='string'||command.name.length>64||!/^[a-z](?:[a-z0-9-]*[a-z0-9])?$/.test(command.name)||/^(con|prn|aux|nul|com[1-9]|lpt[1-9])$/.test(command.name)))throw new Error('Invalid agent name');
    if(command.type==='query_observability'&&(!(command.center_seq===null||(Number.isSafeInteger(command.center_seq)&&command.center_seq>=0))||![command.before,command.after].every(value=>Number.isInteger(value)&&value>=0&&value<=100)))throw new Error('Invalid observability window');
  }
  if(command.type==='manage_schedule'){
    if(!exactKeys(command,['type','requester_session_id','query_id','request'])||!nonblank(command.requester_session_id)||command.requester_session_id!==envelope.session_id||!string(command.query_id,256)||!command.request||typeof command.request!=='object'||Array.isArray(command.request)||!string(command.request.action,32)||JSON.stringify(command.request).length>200000)throw new Error('Invalid schedule command');
  }
  if(command.type==='request_diff'&&(!exactKeys(command,Object.hasOwn(command,'query_id')?['type','session_id','query_id']:['type','session_id'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||(Object.hasOwn(command,'query_id')&&!nonblank(command.query_id))))throw new Error('Invalid diff query');
  if(command.type==='set_permission_profile'&&(!exactKeys(command,['type','session_id','mode'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!['full_access','assisted','request_approval'].includes(command.mode)))throw new Error('Invalid permission selection');
  // The product mode is a durable session fact written by the Runtime. The
  // renderer never states whether the switch succeeded — only which of the
  // three representable axes it is asking for. The deprecated `work_profile`
  // is accepted because the wire command requires it; it carries no authority.
  if(command.type==='set_product_axes'&&(!exactKeys(command,['type','session_id','work_profile','collaboration'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!string(command.work_profile,64)||!['chat','plan','goal'].includes(command.collaboration)))throw new Error('Invalid collaboration selection');
  if(['fork_session','compact_context'].includes(command.type)&&(!exactKeys(command,['type','session_id'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id))throw new Error('Invalid session command');
  if(command.type==='compact_context'&&envelope.expected_version!==null)throw new Error('Invalid compaction envelope');
  if(['select_model','rename_session','archive_session','unarchive_session'].includes(command.type)){
    // Desktop acts on the selected session; the Rust bridge independently
    // binds the envelope to its current owner before delivering the command.
    const keys=command.type==='select_model'?['type','session_id','model']:command.type==='rename_session'?['type','session_id','name']:['type','session_id'];
    if(!exactKeys(command,keys)||!nonblank(command.session_id)||command.session_id!==envelope.session_id)throw new Error('Invalid session command');
    if(command.type==='rename_session'&&!nonblank(command.name))throw new Error('Invalid session name');
    if(command.type==='select_model'&&(!exactKeys(command.model,['provider','model'])||!nonblank(command.model.provider)||!nonblank(command.model.model)))throw new Error('Invalid model selection');
  }
}
module.exports={validateEnvelope};
