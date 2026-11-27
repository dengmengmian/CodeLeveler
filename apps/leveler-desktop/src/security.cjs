const {SNAPSHOT_VERSIONED_COMMANDS}=require('./command-policy.gen.cjs');
const allowed=new Set(['btw','cancel_btw','recap','restore_checkpoint','acknowledge_crash_window','query_diagnostics','submit_message','submit_user_input','steer_current_turn','steer_user_input','cancel_current_turn','cancel_task','resume_task','approval_decision','answer_clarification','definition_trust_decision','capture_secret','revoke_secret','query_session_history','query_tool_output','request_diff','list_agents','get_agent','list_skills','get_skill','query_context','query_observability','select_model','set_permission_profile','set_product_axes','rename_session','run_goal','set_thinking_level','archive_session','unarchive_session','fork_session','compact_context','cancel_background_task','manage_schedule','manage_mcp','run_review','cancel_review','query_review_report','list_reviews','query_review_scopes']);
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
// MCP declarations are structured data, never a resolved credential. A
// reference names an environment variable; the runtime resolves it at launch.
// These checks are the IPC shape boundary, not the authority: the runtime
// validates names, URLs, transport exclusivity and references again.
const MCP_ACTIONS=new Set(['list','reload','reconnect','add','update','remove','set_enabled']);
function mcpName(value){return string(value,64)&&/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(value);}
function mcpRef(value){
  return !!value&&typeof value==='object'&&!Array.isArray(value)&&exactKeys(value,['key','source'])
    &&string(value.key,256)&&/^[A-Za-z0-9_][A-Za-z0-9_-]*$/.test(value.key)
    &&string(value.source,256)&&/^[A-Z_][A-Z0-9_]*$/.test(value.source);
}
function mcpRefs(value){return Array.isArray(value)&&value.length<=64&&value.every(mcpRef);}
function mcpUrl(value){
  if(!string(value,2048)||!/^https?:\/\//.test(value)||/\s/.test(value))return false;
  try{
    const url=new URL(value);
    return (url.protocol==='http:'||url.protocol==='https:')&&!url.username&&!url.password&&!url.search;
  }catch{return false;}
}
function mcpSpec(value){
  if(!value||typeof value!=='object'||Array.isArray(value))return false;
  const keys=['name','transport','command','args','env','url','headers','bearer_token_env','enabled'];
  if(Object.keys(value).some(key=>!keys.includes(key)))return false;
  if(!mcpName(value.name)||!['stdio','streamable_http'].includes(value.transport))return false;
  if(value.enabled!==undefined&&typeof value.enabled!=='boolean')return false;
  if(value.args!==undefined&&(!Array.isArray(value.args)||value.args.length>256||value.args.some(arg=>!string(arg,8192))))return false;
  const absent=v=>v===undefined||v===null;
  if(value.transport==='stdio'){
    if(!string(value.command,4096)||!value.command.trim())return false;
    if(!absent(value.url))return false;
    if(value.headers!==undefined&&(!Array.isArray(value.headers)||value.headers.length))return false;
    if(!absent(value.bearer_token_env))return false;
    if(value.env!==undefined&&!mcpRefs(value.env))return false;
  }else{
    if(!mcpUrl(value.url))return false;
    if(!absent(value.command))return false;
    if(value.args!==undefined&&(!Array.isArray(value.args)||value.args.length))return false;
    if(value.env!==undefined&&(!Array.isArray(value.env)||value.env.length))return false;
    if(value.headers!==undefined&&!mcpRefs(value.headers))return false;
    if(!absent(value.bearer_token_env)&&!(string(value.bearer_token_env,256)&&/^[A-Z_][A-Z0-9_]*$/.test(value.bearer_token_env)))return false;
  }
  return true;
}
function validMcpRequest(request){
  if(!request||typeof request!=='object'||Array.isArray(request)||!MCP_ACTIONS.has(request.action))return false;
  if(JSON.stringify(request).length>200000)return false;
  switch(request.action){
    case 'list': case 'reload': return exactKeys(request,['action']);
    case 'reconnect': case 'remove': return exactKeys(request,['action','server'])&&mcpName(request.server);
    case 'set_enabled': return exactKeys(request,['action','server','enabled'])&&mcpName(request.server)&&typeof request.enabled==='boolean';
    case 'add': return exactKeys(request,['action','spec'])&&mcpSpec(request.spec);
    case 'update': return exactKeys(request,['action','original_name','spec'])&&mcpName(request.original_name)&&mcpSpec(request.spec);
  }
  return false;
}
const REVIEW_SCOPE_KINDS=new Set(['auto','current_task','workspace','project','session','commit','branch','paths']);
// The runtime's durable review identity (`review-<uuid>`). The renderer only
// echoes an id it received; the boundary refuses a name of its own choosing.
function reviewId(value){return typeof value==='string'&&value.length>7&&value.length<=128&&value.startsWith('review-')&&/^[A-Za-z0-9._:-]+$/.test(value);}
function reviewRev(value){return typeof value==='string'&&value.trim().length>0&&value.length<=200&&!value.startsWith('-')&&!/[\u0000-\u001f\u007f]/.test(value);}
// The runtime bounds the focus in UTF-8 bytes; counting code units here would
// admit a multi-byte focus the runtime then refuses.
function utf8Bytes(value){let bytes=0;for(const char of String(value??'')){const code=char.codePointAt(0);bytes+=code<=0x7f?1:code<=0x7ff?2:code<=0xffff?3:4;}return bytes;}
// The object a review may name. Shapes mirror the Rust `UiReviewScope` wire
// form; a Desktop client can only review the selected session, so a named
// session id is not representable. Paths stay workspace-relative.
function reviewScope(value){
  if(!value||typeof value!=='object'||Array.isArray(value)||!REVIEW_SCOPE_KINDS.has(value.kind))return false;
  switch(value.kind){
    case 'auto': case 'current_task': case 'workspace': case 'project': return exactKeys(value,['kind']);
    case 'session': return exactKeys(value,['kind'])||(exactKeys(value,['kind','session_id'])&&value.session_id===null);
    case 'commit': return exactKeys(value,['kind','rev'])&&reviewRev(value.rev);
    case 'branch': return exactKeys(value,['kind','base'])&&reviewRev(value.base);
    case 'paths': {
      if(!exactKeys(value,['kind','paths'])||!Array.isArray(value.paths)||value.paths.length<1||value.paths.length>50)return false;
      return value.paths.every(path=>typeof path==='string'&&path.length>0&&path.length<=500&&!path.startsWith('/')&&!path.startsWith('\\')&&!path.includes('\0')&&!path.split('/').includes('..'));
    }
  }
  return false;
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
  // An explicit goal turn is a typed Runtime command. Its objective is user
  // text, bounded here; `require_new_turn` is the only optional field and is
  // a boolean, never inferred from prose.
  if(command.type==='run_goal'){
    const keys=Object.keys(command);
    if(!keys.includes('type')||!keys.includes('session_id')||!keys.includes('content')
      ||keys.some(key=>!['type','session_id','content','require_new_turn'].includes(key))
      ||!string(command.content,100000)||!command.content.trim()
      ||(command.require_new_turn!==undefined&&typeof command.require_new_turn!=='boolean'))throw new Error('Invalid goal command');
  }
  // Thinking is a session setting with a closed vocabulary; the Runtime maps
  // the level to the model's declared capability.
  if(command.type==='set_thinking_level'){
    const levels=['auto','off','minimal','low','medium','high','max'];
    if(!exactKeys(command,['type','session_id','level'])
      ||(command.level!==null&&!levels.includes(command.level)))throw new Error('Invalid thinking level');
  }
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
  if(command.type==='query_tool_output'){
    // The renderer names a call, a part index and a byte window — never a
    // path. The Runtime resolves the stored result and serves only its own
    // artifact store, so this boundary only has to keep the request well
    // formed and bounded.
    const keys=['type','session_id','query_id','call_id','offset','max_bytes',...(Object.hasOwn(command,'index')?['index']:[])];
    if(!exactKeys(command,keys)||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!string(command.query_id,256)||!nonblank(command.call_id)||command.call_id.length>256||!/^[A-Za-z0-9_.:-]+$/.test(command.call_id)||!Number.isSafeInteger(command.offset)||command.offset<0||command.offset>1073741824||!Number.isInteger(command.max_bytes)||command.max_bytes<1||command.max_bytes>262144)throw new Error('Invalid tool output query');
    if(Object.hasOwn(command,'index')&&(!Number.isInteger(command.index)||command.index<0||command.index>1024))throw new Error('Invalid tool output part');
  }
  if(['list_agents','get_agent','list_skills','get_skill','query_context','query_observability'].includes(command.type)){
    const keys=['type','session_id','query_id',...(command.type==='get_agent'||command.type==='get_skill'?['name']:[]),...(command.type==='query_observability'?['center_seq','before','after']:[])];
    if(!exactKeys(command,keys)||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!nonblank(command.query_id))throw new Error('Invalid settings query');
    if(command.type==='get_agent'&&(typeof command.name!=='string'||command.name.length>64||!/^[a-z](?:[a-z0-9-]*[a-z0-9])?$/.test(command.name)||/^(con|prn|aux|nul|com[1-9]|lpt[1-9])$/.test(command.name)))throw new Error('Invalid agent name');
    // A Skill name is a directory identity, not prose: one bounded path
    // component. The Runtime still resolves it against its own registry; the
    // IPC boundary only refuses a shape it could never address.
    if(command.type==='get_skill'&&(typeof command.name!=='string'||!command.name||command.name.length>64||command.name.startsWith('.')||!/^[A-Za-z0-9._-]+$/.test(command.name)))throw new Error('Invalid skill name');
    if(command.type==='query_observability'&&(!(command.center_seq===null||(Number.isSafeInteger(command.center_seq)&&command.center_seq>=0))||![command.before,command.after].every(value=>Number.isInteger(value)&&value>=0&&value<=100)))throw new Error('Invalid observability window');
  }
  if(command.type==='manage_schedule'){
    if(!exactKeys(command,['type','requester_session_id','query_id','request'])||!nonblank(command.requester_session_id)||command.requester_session_id!==envelope.session_id||!string(command.query_id,256)||!command.request||typeof command.request!=='object'||Array.isArray(command.request)||!string(command.request.action,32)||JSON.stringify(command.request).length>200000)throw new Error('Invalid schedule command');
  }
  if(command.type==='manage_mcp'){
    // The runtime is the sole MCP authority. The renderer names the selected
    // session and a structured request; it never supplies a resolved secret or
    // a file path. A request for another session is not representable.
    if(!exactKeys(command,['type','requester_session_id','query_id','request'])||!nonblank(command.requester_session_id)||command.requester_session_id!==envelope.session_id||!string(command.query_id,256)||!validMcpRequest(command.request))throw new Error('Invalid MCP management request');
  }
  if(command.type==='run_review'){
    // Review is an explicit, read-only investigation. The renderer names one
    // object the selected session can already see plus the words the person
    // typed; it never names another session, a path outside the workspace or
    // a resolved credential. The Rust bridge and the runtime revalidate the
    // same facts before anything starts.
    const keys=['type','session_id','focus','scope',...(Object.hasOwn(command,'model')?['model']:[])];
    if(!exactKeys(command,keys)||!nonblank(command.session_id)||command.session_id!==envelope.session_id||typeof command.focus!=='string'||utf8Bytes(command.focus)>8*1024||!reviewScope(command.scope))throw new Error('Invalid review request');
    if(Object.hasOwn(command,'model')&&(!exactKeys(command.model,['provider','model'])||!nonblank(command.model.provider)||command.model.provider.length>256||!nonblank(command.model.model)||command.model.model.length>256))throw new Error('Invalid review model');
  }
  if(command.type==='cancel_review'){
    if(!exactKeys(command,['type','session_id','review_id'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!reviewId(command.review_id))throw new Error('Invalid review cancellation');
  }
  if(command.type==='query_review_report'){
    if(!exactKeys(command,['type','session_id','review_id','query_id'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!reviewId(command.review_id)||!string(command.query_id,256))throw new Error('Invalid review report query');
  }
  if(command.type==='list_reviews'||command.type==='query_review_scopes'){
    if(!exactKeys(command,['type','session_id','query_id'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!string(command.query_id,256))throw new Error('Invalid review history query');
  }
  if(command.type==='request_diff'&&(!exactKeys(command,Object.hasOwn(command,'query_id')?['type','session_id','query_id']:['type','session_id'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||(Object.hasOwn(command,'query_id')&&!nonblank(command.query_id))))throw new Error('Invalid diff query');
  if(command.type==='set_permission_profile'){
    const keys=['type','session_id','mode',...(Object.hasOwn(command,'contract')?['contract']:[])];
    if(!exactKeys(command,keys)||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!['full_access','assisted','request_approval'].includes(command.mode))throw new Error('Invalid permission selection');
    if(Object.hasOwn(command,'contract')&&command.contract!=='v2')throw new Error('Invalid permission contract');
  }
  if(command.type==='btw'){
    if(!exactKeys(command,['type','session_id','question'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!nonblank(command.question)||utf8Bytes(command.question)>64*1024)throw new Error('Invalid side question');
  }
  if(['cancel_btw','recap','acknowledge_crash_window'].includes(command.type)&&(!exactKeys(command,['type','session_id'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id))throw new Error('Invalid session command');
  if(command.type==='restore_checkpoint'&&(!exactKeys(command,['type','session_id','checkpoint_id'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!nonblank(command.checkpoint_id)||command.checkpoint_id.length>256))throw new Error('Invalid checkpoint restore');
  if(command.type==='query_diagnostics'){
    const keys=['type','requester_session_id','query_id',...(Object.hasOwn(command,'locale')?['locale']:[])];
    if(!exactKeys(command,keys)||!nonblank(command.requester_session_id)||command.requester_session_id!==envelope.session_id||!string(command.query_id,256)||(Object.hasOwn(command,'locale')&&!['zh','en'].includes(command.locale)))throw new Error('Invalid diagnostics query');
  }
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
