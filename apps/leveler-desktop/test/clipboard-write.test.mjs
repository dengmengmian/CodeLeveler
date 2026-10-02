import {test} from 'node:test';
import assert from 'node:assert/strict';
import {copyText,MAX_COPY_BYTES} from '../src/clipboard-write.cjs';
test('trusted UI copy writes exact bounded text and never reads clipboard',async()=>{
 const writes=[];const clipboard={writeText:value=>writes.push(value)};
 const text='你好\n\nconst text = "literal";';assert.deepEqual(await copyText(text,clipboard),{ok:true});assert.deepEqual(writes,[text]);
 for(const value of [null,{},42,'a'.repeat(MAX_COPY_BYTES+1),'中'.repeat(Math.floor(MAX_COPY_BYTES/3)+1)])await assert.rejects(copyText(value,clipboard));
 assert.equal(writes.length,1);
});
