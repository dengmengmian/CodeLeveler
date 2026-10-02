import {test} from 'node:test';
import assert from 'node:assert/strict';
import {renderMarkdown} from '../src/markdown.mjs';
// Minimal DOM contract: dangerous sinks throw, while nodes preserve inspectable text.
class Node {
 constructor(tag='',text=''){this.tag=tag;this.value=text;this.children=[];this.attributes={};this.listeners={};this.ownerDocument=dom;}
 append(...children){this.children.push(...children);}
 replaceChildren(...children){this.children=children;this.value='';}
 set textContent(value){this.value=value;this.children=[];}
 get textContent(){return this.value+this.children.map(n=>n.textContent).join('');}
 set innerHTML(_value){throw new Error('Untrusted HTML sink');}
 addEventListener(name,callback){this.listeners[name]=callback;}
 setAttribute(name,value){this.attributes[name]=value;}
}
const dom={createElement:tag=>new Node(tag),createTextNode:text=>new Node('',text),createDocumentFragment:()=>new Node()};
function render(text){const container=new Node('div');renderMarkdown(container,text);return container;}
function nodes(node){return [node,...node.children.flatMap(nodes)];}
function tags(node,tag){return nodes(node).filter(n=>n.tag===tag);}
test('renders assistant structure: emphasis, inline code, nested lists, headings, quote and GFM table',()=>{
 const root=render('# Result\n\n**Strong** and *emphasis* with `inline`\n\n- first\n  - nested\n- second\n\n3. third\n4. fourth\n\n> quote\n\n| Name | Count |\n| --- | ---: |\n| files | 3 |');
 for(const tag of ['h1','strong','em','code','ul','ol','li','blockquote','table','thead','tbody','th','td'])assert.ok(tags(root,tag).length,`${tag} is missing`);
 assert.equal(tags(root,'strong')[0].textContent,'Strong');assert.equal(tags(root,'code')[0].textContent,'inline');assert.equal(tags(root,'ol')[0].attributes.start,'3');assert.equal(tags(root,'ul').length,2);
});
test('fenced code is literal and supports an unfinished streaming fence',()=>{
 const text='```html\n<script>window.bad=true</script>\n**not bold**';
 const root=render(text);assert.equal(tags(root,'pre').length,1);assert.equal(tags(root,'code')[0].textContent,'<script>window.bad=true</script>\n**not bold**');assert.equal(tags(root,'strong').length,0);
 renderMarkdown(root,text+'\n```\n\n**finished**');assert.equal(tags(root,'strong')[0].textContent,'finished');
});
test('raw HTML, event handlers, SVG, image URLs and malicious links never create executable nodes or network loads',()=>{
 const attack='<script>alert(1)</script>\n\n<img src="https://example.com/track" onerror="alert(1)">\n\n<svg onload="alert(1)"></svg>\n\n[bad](javascript:alert%281%29) [data](data:text/html,evil) ![tracking](https://example.com/track)';
 const root=render(attack);for(const tag of ['script','img','svg','iframe','object','a','input'])assert.equal(tags(root,tag).length,0,tag);
 assert.match(root.textContent,/<script>/);assert.match(root.textContent,/tracking/);
 for(const node of nodes(root)){for(const key of Object.keys(node.attributes))assert.ok(!key.startsWith('on')&&!['src','href','srcdoc'].includes(key));}
});
test('links are inert readable text with only HTTP(S) destinations attached as text metadata',()=>{
 const root=render('[docs](https://example.com/docs) [file](file:///secret) [custom](leveler-desktop://desktop/)');
 const links=tags(root,'span').filter(n=>n.attributes.class==='markdown-link');assert.equal(links.length,1);assert.equal(links[0].attributes.title,'https://example.com/docs');assert.match(root.textContent,/docs/);assert.match(root.textContent,/file/);
 assert.equal(tags(root,'a').length,0);
});
test('entities render as text rather than activating markup and updates replace old content',()=>{
 const root=render('Fish &amp; chips &#x3c;script&#x3e;');assert.equal(root.textContent,'Fish & chips <script>');assert.equal(tags(root,'script').length,0);renderMarkdown(root,'**next**');assert.equal(root.textContent,'next');
});
test('HTTP(S) links invoke an explicit callback only on activation; unsafe destinations never do',()=>{
 const opened=[];const root=new Node('div');
 renderMarkdown(root,'[docs](https://example.com/docs) [secret](https://user:pass@example.com/) [bad](javascript:alert%281%29)',{onLink:url=>opened.push(url)});
 assert.equal(opened.length,0);
 const links=tags(root,'button').filter(node=>node.attributes.class==='markdown-link');
 assert.equal(links.length,1);
 links[0].listeners.click();assert.deepEqual(opened,['https://example.com/docs']);
 assert.equal(tags(root,'a').length,0);
});
