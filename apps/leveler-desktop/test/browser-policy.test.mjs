import {test} from 'node:test';
import assert from 'node:assert/strict';
import policy from '../src/browser-policy.cjs';
test('manual browser accepts explicit http(s) URLs and bare hostnames without search guessing',()=>{
 assert.equal(policy.normalizeURL('example.com/path'),'https://example.com/path');
 assert.equal(policy.normalizeURL('  https://example.com/  '),'https://example.com/');
 assert.equal(policy.normalizeURL('http://localhost:8080'),'http://localhost:8080/');
 for(const url of ['file:///etc/passwd','javascript:alert(1)','data:text/html,x','leveler-desktop://desktop/index.html','mailto:a@example.com','https://u:p@example.com','foo bar','',null,'https://x/'+ 'a'.repeat(9000)])assert.throws(()=>policy.normalizeURL(url));
});
test('frame/redirect URL policy rejects external protocols and privileged domains',()=>{
 for(const url of ['file:///etc/passwd','javascript:alert(1)','data:text/html,x','blob:https://example.com/id','leveler-desktop://desktop/index.html','chrome://settings','ftp://example.com'])assert.equal(policy.isNavigationAllowed(url),false,url);
 assert.equal(policy.isNavigationAllowed('about:blank'),true);
 assert.equal(policy.isNavigationAllowed('https://example.com/a'),true);
});
test('only scoped browser commands are admitted',()=>{
 for(const type of ['new','select','close','navigate','back','forward','reload','external'])assert.equal(policy.validateCommand({type,id:'tab-1',...(type==='navigate'?{url:'example.com'}:{})}).type,type);
 for(const command of [null,{}, {type:'executeJavaScript'}, {type:'navigate',url:'file:///etc/passwd'},{type:'navigate'},{type:'close',id:3},{type:'reload',token:'secret'},{type:'new',id:'a'.repeat(200)}])assert.throws(()=>policy.validateCommand(command));
});
test('browser surface must remain inside current content viewport',()=>{
 const viewport={width:1000,height:700};
 assert.deepEqual(policy.validateSurface({visible:true,bounds:{x:600,y:100,width:400,height:600}},viewport),{visible:true,bounds:{x:600,y:100,width:400,height:600}});
 for(const bounds of [{x:-1,y:0,width:100,height:100},{x:900,y:0,width:200,height:100},{x:0,y:699,width:100,height:2},{x:0,y:0,width:Infinity,height:1},{x:0,y:0,width:1.5,height:2},{x:0,y:0,width:0,height:2}])assert.throws(()=>policy.validateSurface({visible:true,bounds},viewport));
 assert.throws(()=>policy.validateSurface({visible:'yes',bounds:{x:0,y:0,width:1,height:1}},viewport));
 assert.throws(()=>policy.validateSurface({visible:true,bounds:{x:0,y:0,width:1,height:1},extra:true},viewport));
});
