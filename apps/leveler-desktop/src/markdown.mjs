import {marked} from '../node_modules/marked/lib/marked.esm.js';

// Parse Markdown only. Model-authored HTML never reaches an HTML parser or sink;
// every rendered tag and attribute below is owned by this allowlisted DOM writer.
const entities={amp:'&',lt:'<',gt:'>',quot:'"',apos:"'",nbsp:'\u00a0'};
function decode(text){
 return String(text??'').replace(/&(#x[0-9a-f]+|#\d+|amp|lt|gt|quot|apos|nbsp);/gi,(raw,name)=>{
  if(!name.startsWith('#'))return entities[name.toLowerCase()]??raw;
  const code=name[1].toLowerCase()==='x'?parseInt(name.slice(2),16):parseInt(name.slice(1),10);
  return code>0&&code<=0x10ffff&&!(code>=0xd800&&code<=0xdfff)?String.fromCodePoint(code):'\ufffd';
 });
}
function httpDestination(href){
 try{const url=new URL(decode(href));return ['https:','http:'].includes(url.protocol)&&!url.username&&!url.password?url.href:null;}catch{return null;}
}
/** Render safe Markdown through a DOM allowlist. Navigation is an explicit caller callback.
 * @param {HTMLElement} container @param {string} text @param {{onLink?:(url:string)=>void}} [options] */
export function renderMarkdown(container,text,options={}){
 const document=container.ownerDocument;
 function node(tag){return document.createElement(tag);}
 function literal(parent,value){parent.append(document.createTextNode(String(value??'')));}
 function children(parent,tokens){for(const token of tokens??[])write(parent,token);}
 function inline(parent,token){children(parent,token.tokens??marked.Lexer.lexInline(token.text??''));}
 function write(parent,token){
  let element;
  switch(token.type){
   case 'space':case 'def':return;
   case 'heading':element=node(`h${Math.max(1,Math.min(6,token.depth))}`);inline(element,token);break;
   case 'paragraph':element=node('p');inline(element,token);break;
   case 'text':if(token.tokens){children(parent,token.tokens);}else literal(parent,decode(token.text));return;
   case 'escape':literal(parent,decode(token.text));return;
   case 'strong':case 'em':case 'del':element=node(token.type);inline(element,token);break;
   case 'codespan':element=node('code');literal(element,token.text);break;
   case 'code':{
    element=node('pre');const code=node('code');literal(code,token.text);element.append(code);break;
   }
   case 'br':element=node('br');break;
   case 'hr':element=node('hr');break;
   case 'blockquote':element=node('blockquote');children(element,token.tokens);break;
   case 'list':{
    element=node(token.ordered?'ol':'ul');
    if(token.ordered&&Number.isSafeInteger(token.start)&&token.start>1)element.setAttribute('start',String(token.start));
    for(const item of token.items){const li=node('li');if(item.task)literal(li,item.checked?'☑ ':'☐ ');children(li,item.tokens);element.append(li);}break;
   }
   case 'table':{
    element=node('table');const head=node('thead'),body=node('tbody'),header=node('tr');
    for(const cell of token.header){const th=node('th');inline(th,cell);header.append(th);}head.append(header);
    for(const row of token.rows){const tr=node('tr');for(const cell of row){const td=node('td');inline(td,cell);tr.append(td);}body.append(tr);}element.append(head,body);break;
   }
   case 'link':{
    const href=httpDestination(token.href);if(!href){inline(parent,token);return;}
    element=node(options.onLink?'button':'span');element.setAttribute('class','markdown-link');element.setAttribute('title',href);if(options.onLink){element.setAttribute('type','button');element.addEventListener('click',()=>options.onLink(href));}inline(element,token);break;
   }
   case 'image':literal(parent,`[图片暂不支持：${decode(token.text)}]`);return;
   case 'html':literal(parent,token.raw??token.text);return;
   default:literal(parent,token.raw??token.text);return;
  }
  parent.append(element);
 }
 const fragment=document.createDocumentFragment();
 children(fragment,marked.lexer(String(text??''),{gfm:true,breaks:false}));
 container.replaceChildren(fragment);
}
