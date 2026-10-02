const MAX_COPY_BYTES=4*1024*1024;
/** @param {unknown} value @param {{writeText:(text:string)=>Promise<void>|void}} clipboard */
async function copyText(value,clipboard){
 if(typeof value!=='string'||value.length>MAX_COPY_BYTES||Buffer.byteLength(value,'utf8')>MAX_COPY_BYTES)throw new Error('复制内容必须是文本，且不超过 4 MiB');
 await clipboard.writeText(value);return {ok:true};
}
module.exports={copyText,MAX_COPY_BYTES};
