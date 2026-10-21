import {test} from 'node:test';
import assert from 'node:assert/strict';
import {answerFiles, messageTime} from '../src/presentation.mjs';
const messages=[{id:'u1',role:'user',seq:0},{id:'a1',role:'assistant',seq:2},{id:'u2',role:'user',seq:3},{id:'a2',role:'assistant',seq:5}];
const patch='--- a/src/util.ts\n+++ b/src/util.ts\n@@ -1 +1 @@\n-old\n+new';
test('answer files come only from confirmed edits within that answer turn',()=>{
 const tools=[{id:'t',seq:1,status:'success',appliedDiff:patch},{id:'bad',seq:1,status:'failed',appliedDiff:patch.replaceAll('util','failed')},{id:'later',seq:4,status:'success',appliedDiff:patch.replaceAll('util','later')}];
 assert.deepEqual(answerFiles(messages[1],messages,tools),[{path:'src/util.ts',added:1,removed:1}]);
 assert.deepEqual(answerFiles(messages[3],messages,tools),[{path:'src/later.ts',added:1,removed:1}]);
 assert.deepEqual(answerFiles({...messages[1],text:'Created imaginary.md'},messages,[]),[]);
});
test('unsafe or deleted diff paths never become openable file cards',()=>{
 const tools=['../../escape.ts','/tmp/absolute.ts','/dev/null'].map((path,i)=>({id:String(i),seq:1,status:'success',appliedDiff:`--- a/old.ts\n+++ ${path}\n@@ -0,0 +1 @@\n+x`}));
 assert.deepEqual(answerFiles(messages[1],messages,tools),[]);
});
test('only a supplied valid non-epoch message timestamp is shown',()=>{
 for(const value of [undefined,null,'','invalid','1970-01-01T00:00:00Z'])assert.equal(messageTime({time:value}),null);
 assert.equal(messageTime({created_at:'2026-10-21T01:02:03Z'}),'2026-10-21T01:02:03.000Z');
});
