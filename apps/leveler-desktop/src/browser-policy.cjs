// No runtime authority is granted here: this policy covers the user's manual browser only.
/** @typedef {'new'|'select'|'close'|'navigate'|'back'|'forward'|'reload'|'external'} BrowserCommandType */
/** @typedef {{type:BrowserCommandType,id?:string,url?:string}} BrowserCommand */
/** @typedef {{x:number,y:number,width:number,height:number}} BrowserBounds */
/** @typedef {{visible:boolean,bounds:BrowserBounds}} BrowserSurface */
const MAX_TABS=8;
/** @param {unknown} value @param {string[]} keys @param {string} label @returns {Record<string,unknown>} */
function record(value,keys,label){
 if(!value||typeof value!=='object'||Array.isArray(value)||Object.keys(value).some(key=>!keys.includes(key)))throw new Error(`Invalid ${label}`);
 return /** @type {Record<string,unknown>} */ (value);
}
/** @param {unknown} value @returns {string} */
function httpURL(value){
 if(typeof value!=='string'||!value||value.length>8192||/[\u0000-\u0020\u007f]/.test(value))throw new Error('请输入有效的 HTTP / HTTPS 地址');
 const url=new URL(value);
 if(!['http:','https:'].includes(url.protocol)||!url.hostname||url.username||url.password)throw new Error('仅支持不含凭据的 HTTP / HTTPS 地址');
 return url.href;
}
/** @param {unknown} value @returns {string} */
function normalizeURL(value){
 if(typeof value!=='string')throw new Error('请输入网页地址');
 const trimmed=value.trim();
 // Host input is deliberately not interpreted as a search query.
 return httpURL(/^[a-z][a-z\d+.-]*:/i.test(trimmed)?trimmed:`https://${trimmed}`);
}
/** @param {unknown} value @returns {boolean} */
function isNavigationAllowed(value){if(value==='about:blank')return true;try{return typeof value==='string'&&httpURL(value)===new URL(value).href;}catch{return false;}}
/** @param {unknown} value @returns {BrowserCommand} */
function validateCommand(value){
 const input=record(value,['type','id','url'],'browser command');
 if(typeof input.type!=='string'||!['new','select','close','navigate','back','forward','reload','external'].includes(input.type))throw new Error('Invalid browser command');
 if(input.id!==undefined&&(typeof input.id!=='string'||!input.id||input.id.length>128))throw new Error('Invalid browser tab');
 if(input.url!==undefined&&!['new','navigate'].includes(input.type))throw new Error('Invalid browser URL argument');
 if(input.type==='navigate'&&input.url===undefined)throw new Error('请输入网页地址');
 const command={type:/** @type {BrowserCommandType} */ (input.type)};
 return {...command,...(typeof input.id==='string'?{id:input.id}:{}),...(input.url!==undefined?{url:normalizeURL(input.url)}:{})};
}
/** @param {unknown} value @param {{width:number,height:number}} viewport @returns {BrowserSurface} */
function validateSurface(value,viewport){
 const input=record(value,['visible','bounds'],'browser surface');
 if(typeof input.visible!=='boolean')throw new Error('Invalid browser visibility');
 const bounds=record(input.bounds,['x','y','width','height'],'browser bounds');
 const {x,y,width,height}=bounds;
 if(typeof x!=='number'||typeof y!=='number'||typeof width!=='number'||typeof height!=='number'||![x,y,width,height].every(Number.isSafeInteger)||x<0||y<0||width<1||height<1||x+width>viewport.width||y+height>viewport.height)throw new Error('Browser bounds exceed window viewport');
 return {visible:input.visible,bounds:{x,y,width,height}};
}
module.exports={MAX_TABS,normalizeURL,isNavigationAllowed,validateCommand,validateSurface};
