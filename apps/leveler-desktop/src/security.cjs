const allowed=new Set(['submit_message','steer_current_turn','cancel_current_turn','approval_decision','answer_clarification','query_session_history','request_diff','list_memory','list_agents','get_agent','query_context','query_observability','select_model','set_permission_profile','rename_session','archive_session']);
const decisions=new Set(['approve_once','approve_session','approve_always','deny']);
const {validRef}=require('./attachment-ingress.cjs');
function string(value,max=256){return typeof value==='string'&&value.length>0&&value.length<=max;}
function exactKeys(value,keys){return !!value&&typeof value==='object'&&!Array.isArray(value)&&Object.keys(value).length===keys.length&&keys.every(key=>Object.hasOwn(value,key));}
function nonblank(value){return string(value)&&value.trim().length>0;}
function validateEnvelope(envelope,authorizeAttachment){
  if(!envelope||!string(envelope.command_id)||!string(envelope.session_id)||typeof envelope.issued_at!=='string'||!Number.isFinite(Date.parse(envelope.issued_at))||envelope.expected_version!==null)throw new Error('Invalid command envelope');
  const command=envelope.command;
  if(!command||!allowed.has(command.type))throw new Error('Unsupported Desktop command');
  if(command.session_id && command.session_id!==envelope.session_id)throw new Error('Session mismatch');
  if(['submit_message','steer_current_turn'].includes(command.type)&&((!string(command.content,100000)&&!(command.type==='submit_message'&&command.content===''&&Array.isArray(command.attachments)&&command.attachments.length>0))||command.session_id!==envelope.session_id))throw new Error('Invalid message');
  if(command.type==='submit_message'&&Object.keys(command).some(key=>!['type','session_id','content','attachments'].includes(key)))throw new Error('Invalid message fields');
  if(command.type==='submit_message'&&command.attachments!==undefined){
    if(Array.isArray(command.attachments)&&command.attachments.some(ref=>ref?.kind!=='image'))throw new Error('普通文件已上传，但当前 Runtime 不支持将其内容发送给模型；请移除附件后发送文字');
    if(!Array.isArray(command.attachments)||command.attachments.length>16||command.attachments.some(ref=>!validRef(ref)||!authorizeAttachment?.(envelope.session_id,ref)))throw new Error('Invalid or unregistered attachment');
    if(new Set(command.attachments.map(ref=>ref.id)).size!==command.attachments.length)throw new Error('Duplicate attachment');
  }
  if(command.type==='cancel_current_turn'&&command.session_id!==envelope.session_id)throw new Error('Invalid cancellation');
  if(command.type==='approval_decision'&&(!string(command.request_id)||!decisions.has(command.decision)))throw new Error('Invalid approval decision');
  if(command.type==='answer_clarification'&&(!string(command.request_id)||typeof command.answer!=='string'||command.answer.length>100000))throw new Error('Invalid clarification');
  if(command.type==='query_session_history'&&command.session_id!==envelope.session_id)throw new Error('Invalid history query');
  if(['list_memory','list_agents','get_agent','query_context','query_observability'].includes(command.type)){
    const keys=['type','session_id','query_id',...(command.type==='list_memory'?['include_archived']:[]),...(command.type==='get_agent'?['name']:[]),...(command.type==='query_observability'?['center_seq','before','after']:[])];
    if(!exactKeys(command,keys)||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!nonblank(command.query_id))throw new Error('Invalid settings query');
    if(command.type==='list_memory'&&typeof command.include_archived!=='boolean')throw new Error('Invalid memory query');
    if(command.type==='get_agent'&&(typeof command.name!=='string'||command.name.length>64||!/^[a-z](?:[a-z0-9-]*[a-z0-9])?$/.test(command.name)||/^(con|prn|aux|nul|com[1-9]|lpt[1-9])$/.test(command.name)))throw new Error('Invalid agent name');
    if(command.type==='query_observability'&&(!(command.center_seq===null||(Number.isSafeInteger(command.center_seq)&&command.center_seq>=0))||![command.before,command.after].every(value=>Number.isInteger(value)&&value>=0&&value<=100)))throw new Error('Invalid observability window');
  }
  if(command.type==='request_diff'&&(!exactKeys(command,Object.hasOwn(command,'query_id')?['type','session_id','query_id']:['type','session_id'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||(Object.hasOwn(command,'query_id')&&!nonblank(command.query_id))))throw new Error('Invalid diff query');
  if(command.type==='set_permission_profile'&&(!exactKeys(command,['type','session_id','mode'])||!nonblank(command.session_id)||command.session_id!==envelope.session_id||!['full_access','assisted','request_approval'].includes(command.mode)))throw new Error('Invalid permission selection');
  if(['select_model','rename_session','archive_session'].includes(command.type)){
    // Desktop acts on the selected session; the Rust bridge independently
    // binds the envelope to its current owner before delivering the command.
    const keys=command.type==='select_model'?['type','session_id','model']:command.type==='rename_session'?['type','session_id','name']:['type','session_id'];
    if(!exactKeys(command,keys)||!nonblank(command.session_id)||command.session_id!==envelope.session_id)throw new Error('Invalid session command');
    if(command.type==='rename_session'&&!nonblank(command.name))throw new Error('Invalid session name');
    if(command.type==='select_model'&&(!exactKeys(command.model,['provider','model'])||!nonblank(command.model.provider)||!nonblank(command.model.model)))throw new Error('Invalid model selection');
  }
}
module.exports={validateEnvelope};
