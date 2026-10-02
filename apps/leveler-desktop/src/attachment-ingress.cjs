const {open}=require('node:fs/promises');
const {constants}=require('node:fs');
const {basename}=require('node:path');
const {randomUUID}=require('node:crypto');
const MAX_UPLOAD_BYTES=20*1024*1024;
/** @typedef {{id:string,kind:string,name:string,mime_type:string,size_bytes:number,sha256:string,width:number|null,height:number|null}} AttachmentRef */
/** @typedef {{sourceId:string,sessionId:string,vision:boolean}} SelectedSession */
/** @typedef {{context:SelectedSession,generation:number,envelope:UploadEnvelope,status:string,attachment?:AttachmentRef,error?:{message:string,kind:string},timer?:unknown,complete?:()=>void}} PendingUpload */
/** @typedef {{command_id:string,session_id:string,expected_version:null,issued_at:string,command:{type:'add_attachment_data',session_id:string,name:string,data_base64:string}}} UploadEnvelope */
/** @param {unknown} value @returns {value is AttachmentRef} */
function validRef(value){
 if(!value||typeof value!=='object'||Array.isArray(value))return false;
 const ref=/** @type {Record<string,unknown>} */(value);
 const keys=['id','kind','name','mime_type','size_bytes','sha256','width','height'];
 return Object.keys(ref).length===keys.length&&keys.every(key=>Object.hasOwn(ref,key))&&typeof ref.id==='string'&&ref.id.length>0&&ref.id.length<=256&&typeof ref.kind==='string'&&['image','text_file','document','unknown'].includes(ref.kind)&&typeof ref.name==='string'&&ref.name.length>0&&ref.name.length<=1024&&typeof ref.mime_type==='string'&&ref.mime_type.length<=256&&typeof ref.size_bytes==='number'&&Number.isSafeInteger(ref.size_bytes)&&ref.size_bytes>=0&&ref.size_bytes<=MAX_UPLOAD_BYTES&&typeof ref.sha256==='string'&&/^[a-f0-9]{64}$/.test(ref.sha256)&&[ref.width,ref.height].every(d=>d===null||(typeof d==='number'&&Number.isSafeInteger(d)&&d>0&&d<=2048));
}
/**
 * Upload ingress only. The path comes from the native picker, never Renderer
 * input. Main neither interprets content nor creates a persistent file store.
 * @param {string} path @returns {Promise<{name:string,data_base64:string}>}
 */
async function readPickedUpload(path){
 const file=await open(path,constants.O_RDONLY|(constants.O_NOFOLLOW??0)|(constants.O_NONBLOCK??0));
 try{
  const before=await file.stat();
  if(!before.isFile())throw new Error('请选择普通文件');
  if(before.size>MAX_UPLOAD_BYTES)throw new Error('附件不能超过 20 MiB');
  const bytes=Buffer.alloc(Math.min(before.size+1,MAX_UPLOAD_BYTES+1));let offset=0;
  while(offset<bytes.length){const read=await file.read(bytes,offset,bytes.length-offset,offset);if(!read.bytesRead)break;offset+=read.bytesRead;}
  const after=await file.stat();
  if(offset!==before.size||after.size!==before.size||after.mtimeMs!==before.mtimeMs||after.ctimeMs!==before.ctimeMs)throw new Error('文件在上传读取期间发生变化，请重新选择');
  return {name:basename(path),data_base64:bytes.subarray(0,offset).toString('base64')};
 }finally{await file.close();}
}
class AttachmentIngress {
 /** @param {{pickFile:()=>Promise<string|null>,deliver:(envelope:UploadEnvelope)=>Promise<unknown>,readFile?:(path:string)=>Promise<{name:string,data_base64:string}>,schedule?:(callback:()=>void,delayMs:number)=>unknown,cancelSchedule?:(handle:unknown)=>void,deadlineMs?:number}} dependencies */
 constructor({pickFile,deliver,readFile=readPickedUpload,schedule=(callback,delay)=>setTimeout(callback,delay),cancelSchedule=handle=>clearTimeout(/** @type {NodeJS.Timeout} */(handle)),deadlineMs=60000}){
  this.pickFile=pickFile;this.deliver=deliver;this.readFile=readFile;this.schedule=schedule;this.cancelSchedule=cancelSchedule;this.deadlineMs=deadlineMs;
  /** @type {SelectedSession|null} */ this.selected=null;
  /** @type {PendingUpload|null} */ this.pending=null;
  /** @type {Map<string,Map<string,AttachmentRef>>} */ this.refs=new Map();this.generation=0;this.picking=false;
 }
 /** @param {SelectedSession|null} context */
 activate(context){
  if(context?.sourceId!==this.selected?.sourceId||context?.sessionId!==this.selected?.sessionId){this.generation++;if(this.pending){this.pending.status='failed';this.pending.error={kind:'session_not_found',message:'任务已切换，请重新添加附件'};this.finishWait(this.pending);}this.pending=null;}
  this.selected=context;
 }
 /** @param {SelectedSession} context */
 key(context){return JSON.stringify([context.sourceId,context.sessionId]);}
 /** @param {unknown} frame */
 observe(frame){
  if(!frame||typeof frame!=='object'||!('event' in frame)||frame.event!=='runtime'||!('session_id' in frame)||frame.session_id!==this.selected?.sessionId||!('data' in frame))return;
  const data=frame.data;if(!data||typeof data!=='object'||!('type' in data))return;
  const pending=this.pending;
  // Runtime owns the association to this immutable command envelope. A
  // filename/session match cannot distinguish late results after A→B→A.
  if(!pending||pending.generation!==this.generation||!('command_id' in data)||data.command_id!==pending.envelope.command_id)return;
  if(data.type==='attachment_added'&&'attachment' in data&&validRef(data.attachment)){
   const key=this.key(pending.context);let known=this.refs.get(key);if(!known){known=new Map();this.refs.set(key,known);}known.set(data.attachment.id,{...data.attachment});
   pending.status='ready';delete pending.error;pending.attachment=data.attachment;this.finishWait(pending);this.pending=null;
  }else if(data.type==='attachment_processing_failed'){
   pending.status='failed';pending.error={message:'error' in data?String(data.error):'附件处理失败',kind:'rejected'};this.finishWait(pending);this.pending=null;
  }
 }
 /** @param {string} sessionId @param {unknown} ref */
 authorize(sessionId,ref){
  if(!this.selected||this.selected.sessionId!==sessionId||!validRef(ref))return false;
  const known=this.refs.get(this.key(this.selected))?.get(ref.id);
  return !!known&&Object.keys(known).every(key=>known[/** @type {keyof AttachmentRef} */(key)]===ref[/** @type {keyof AttachmentRef} */(key)]);
 }
 /** @param {string} sessionId */
 async choose(sessionId){
  const context=this.selected;
  if(!context||sessionId!==context.sessionId)throw new Error('请先选择当前任务');
  if(this.picking)throw new Error('请先完成当前文件选择');
  if(this.pending){if(this.pending.status!=='unknown')throw new Error('上一附件仍在处理');return this.import(this.pending);}
  this.picking=true;const generation=this.generation;
  try{
   const path=await this.pickFile();
   if(generation!==this.generation)throw new Error('任务已切换，请重新添加附件');
   if(!path)return {ok:true,status:'cancelled'};
   const data=await this.readFile(path);
   if(generation!==this.generation)throw new Error('任务已切换，请重新添加附件');
   // File bytes stay inside Main→trusted local Runtime transport. No raw
   // path or base64 upload API is exposed to the untrusted browser or UI.
   const envelope={command_id:randomUUID(),session_id:sessionId,expected_version:null,issued_at:new Date().toISOString(),command:{type:'add_attachment_data',session_id:sessionId,...data}};
   const pending={context,generation,envelope:/** @type {UploadEnvelope} */(envelope),status:'queued'};this.pending=pending;
   return await this.import(pending);
  }finally{this.picking=false;}
 }
 /** @param {NonNullable<AttachmentIngress['pending']>} pending */
 async import(pending){
  pending.status='queued';delete pending.error;
  try{await this.deliver(pending.envelope);}catch(error){
   if(pending.status==='ready'||pending.status==='failed')return this.result(pending);
   const kind=error&&typeof error==='object'&&'kind' in error?String(error.kind):'outcome_unknown';
   pending.status=['request','rejected','session_not_found','ownership_conflict'].includes(kind)?'failed':'unknown';
   pending.error={message:error instanceof Error?error.message:'附件上传状态未知',kind};
   if(pending.status==='failed'&&this.pending===pending)this.pending=null;
  }
  if(pending.status==='queued')await new Promise(resolve=>{
   pending.complete=()=>resolve(undefined);
   pending.timer=this.schedule(()=>{
    delete pending.timer;
    if(this.pending===pending&&pending.generation===this.generation){pending.status='unknown';pending.error={kind:'outcome_unknown',message:'尚未收到附件处理结果，可重试确认或放弃等待'};}
    this.finishWait(pending);
   },this.deadlineMs);
  });
  return this.result(pending);
 }
 /** @param {PendingUpload} pending */
 finishWait(pending){if(pending.timer!==undefined){this.cancelSchedule(pending.timer);delete pending.timer;}const complete=pending.complete;delete pending.complete;complete?.();}
 /** Abandon local tracking only; Runtime may still complete and retain the file. @param {string} sessionId @param {string} importId */
 discard(sessionId,importId){
  const pending=this.pending;
  if(!this.selected||this.selected.sessionId!==sessionId||!pending||this.key(pending.context)!==this.key(this.selected)||pending.envelope.command_id!==importId||pending.status!=='unknown')throw new Error('只能放弃当前任务状态未知的附件等待');
  this.finishWait(pending);this.pending=null;return {ok:true};
 }
 /** @param {NonNullable<AttachmentIngress['pending']>} pending */
 result(pending){return {ok:pending.status!=='failed'&&pending.status!=='unknown',status:pending.status,import_id:pending.envelope.command_id,...(pending.attachment?{attachment:pending.attachment}:{}),...(pending.error?{error:pending.error}:{})};}
}
module.exports={AttachmentIngress,readPickedUpload,validRef,MAX_UPLOAD_BYTES};
