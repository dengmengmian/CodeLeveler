import {test} from 'node:test';
import assert from 'node:assert/strict';
import {confirmedChangeFiles, diffFiles, messageTime, validDiffPath} from '../src/presentation.mjs';
const patch='--- a/src/util.ts\n+++ b/src/util.ts\n@@ -1 +1 @@\n-old\n+new';

test('confirmed change files come only from the runtime applied diff',()=>{
 const tools=[
  {id:'t',status:'success',appliedDiff:patch},
  {id:'bad',status:'failed',appliedDiff:patch.replaceAll('util','failed')},
  {id:'other',status:'success'},
 ];
 assert.deepEqual(confirmedChangeFiles(['t'],tools),[{path:'src/util.ts',added:1,removed:1,patch}]);
 assert.deepEqual(confirmedChangeFiles(['bad'],tools),[]);
 assert.deepEqual(confirmedChangeFiles(['other'],tools),[]);
});

test('one patch keeps every section and merges repeated paths',()=>{
 const two='--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-a\n+b\n--- a/b.ts\n+++ b/b.ts\n@@ -1,2 +1,2 @@\n-c\n+d\n e\n';
 assert.deepEqual(diffFiles(two).map(file=>[file.path,file.added,file.removed]),[['a.ts',1,1],['b.ts',1,1]]);
 const merged=diffFiles(two+two);
 assert.deepEqual(merged.map(file=>[file.path,file.added,file.removed]),[['a.ts',2,2],['b.ts',2,2]]);
 assert.equal((merged[0].patch.match(/^\+\+\+ /gm)??[]).length,2,'both historical sections are kept');
});

test('unsafe or deleted diff paths never become change entries',()=>{
 for(const path of ['../../escape.ts','/tmp/absolute.ts','/dev/null','C:\\tmp\\file.ts']) assert.equal(validDiffPath(path),false,path);
 const tools=['../../escape.ts','/tmp/absolute.ts','/dev/null'].map((path,i)=>({id:String(i),status:'success',appliedDiff:`--- a/old.ts\n+++ ${path}\n@@ -0,0 +1 @@\n+x`}));
 assert.deepEqual(confirmedChangeFiles(['0','1','2'],tools),[]);
});

test('only a supplied valid non-epoch message timestamp is shown',()=>{
 for(const value of [undefined,null,'','invalid','1970-01-01T00:00:00Z'])assert.equal(messageTime({time:value}),null);
 assert.equal(messageTime({created_at:'2026-10-21T01:02:03Z'}),'2026-10-21T01:02:03.000Z');
});
