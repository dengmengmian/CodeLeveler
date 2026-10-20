import {test} from 'node:test';
import assert from 'node:assert/strict';
import {projectTrajectory,searchTrajectory,buildTimeline,trajectoryWindow} from '../src/trajectory.mjs';

const entry=(turn_elapsed_ms,event,turn_start=false)=>({turn_elapsed_ms,turn_start,event});

test('trajectory projects recorded events into turn-aware rows without inventing data',()=>{
  const entries=[
    entry(0,{type:'user_message_added',message:{id:'u1',text:'inspect the repo',images:1}},true),
    entry(4,{type:'reasoning_started'}),
    entry(9,{type:'reasoning_delta',delta:'I should list files.'}),
    entry(12,{type:'reasoning_completed',elapsed_ms:8}),
    entry(14,{type:'tool_call_started',id:'t1',name:'list_files',arguments:'{"path":"."}',model_step:1}),
    entry(20,{type:'tool_call_completed',id:'t1',ok:true,preview:'README.md',duration_ms:6,exit_code:0}),
    entry(30,{type:'assistant_message_started',message_id:'a1'}),
    entry(34,{type:'assistant_text_delta',message_id:'a1',delta:'It is a demo repo.'}),
    entry(36,{type:'assistant_message_completed',message_id:'a1'}),
  ];
  const projected=projectTrajectory(entries);
  assert.equal(projected.turns.length,1);
  assert.equal(projected.turns[0].index,1);
  assert.deepEqual(projected.records.map(record=>record.kind),['user','assistant','tool','assistant']);
  assert.deepEqual(projected.records.map(record=>record.request),[0,1,1,2]);
  const [user,reasoning,tool,answer]=projected.records;
  assert.equal(user.text,'inspect the repo');
  assert.equal(user.images,1);
  assert.equal(reasoning.text,'I should list files.');
  assert.equal(reasoning.durationMs,8);
  assert.equal(tool.toolName,'list_files');
  assert.equal(tool.arguments,'{"path":"."}');
  assert.equal(tool.preview,'README.md');
  assert.equal(tool.durationMs,6);
  assert.equal(tool.ok,true);
  assert.equal(answer.text,'It is a demo repo.');
  assert.equal(answer.status,'completed');
  assert.equal(projected.system.available,false);
  assert.equal(projected.system.reason,'Runtime 未记录系统提示词');
});

test('trajectory keeps unavailable usage and timing explicit instead of zero',()=>{
  const projected=projectTrajectory([
    entry(0,{type:'user_message_added',message:{id:'u1',text:'hello'}},true),
    entry(5,{type:'assistant_message_started',message_id:'a1'}),
    entry(6,{type:'assistant_text_delta',message_id:'a1',delta:'hi'}),
    entry(7,{type:'assistant_message_completed',message_id:'a1'}),
  ]);
  const answer=projected.records.at(-1);
  assert.equal(answer.usage,null);
  assert.equal(answer.ttftMs,1);
  assert.equal(answer.durationMs,1);
  assert.equal(projected.usageAvailable,false);
});

test('trajectory accepts live token usage without backfilling earlier requests',()=>{
  const projected=projectTrajectory([
    entry(0,{type:'user_message_added',message:{id:'u1',text:'hello'}},true),
    entry(5,{type:'assistant_message_started',message_id:'a1'}),
    entry(6,{type:'assistant_text_delta',message_id:'a1',delta:'hi'}),
    entry(7,{type:'assistant_message_completed',message_id:'a1'}),
  ],{usage:{input_tokens:120,output_tokens:7,cached_input_tokens:80}});
  assert.equal(projected.usageAvailable,true);
  assert.deepEqual(projected.records.at(-1).usage,{input:120,output:7,cached:80});
});

test('trajectory search matches recorded text, arguments, and results only',()=>{
  const projected=projectTrajectory([
    entry(0,{type:'user_message_added',message:{id:'u1',text:'find auth'}},true),
    entry(5,{type:'tool_call_started',id:'t1',name:'read_file',arguments:'{"path":"auth.ts"}'}),
    entry(8,{type:'tool_call_completed',id:'t1',ok:true,preview:'export function login()',duration_ms:3}),
  ]);
  assert.deepEqual([...searchTrajectory(projected.records,'auth')].sort(),['tool:t1','user:u1']);
  assert.deepEqual([...searchTrajectory(projected.records,'missing')],[]);
});

test('timeline uses recorded spans when asked and equal slots otherwise',()=>{
  const records=[
    {id:'u',kind:'user',startedMs:0,endedMs:0},
    {id:'r',kind:'assistant',startedMs:10,endedMs:20},
    {id:'t',kind:'tool',startedMs:15,endedMs:40,durationMs:25},
  ];
  const real=buildTimeline(records,true);
  assert.equal(real.total,40);
  assert.deepEqual(real.items.find(item=>item.id==='t'),{id:'t',kind:'tool',lane:'tool',start:15,end:40});
  const equal=buildTimeline(records,false);
  assert.equal(equal.total,1);
  assert.deepEqual(equal.items.map(item=>[item.id,item.start,item.end]),[['u',0,1],['r',0,1],['t',0,1]]);
});

test('trajectory turn numbers are one-based and contiguous across the recorded log',()=>{
  const projected=projectTrajectory([
    entry(0,{type:'user_message_added',message:{id:'u1',text:'first'}},true),
    entry(10,{type:'user_message_added',message:{id:'u2',text:'second'}},true),
  ]);
  assert.deepEqual(projected.turns.map(turn=>turn.index),[1,2]);
  assert.deepEqual(projected.records.map(record=>record.turn),[1,2]);
});


test('ledger window stays anchored at the newest records and reports older pages',()=>{
  const records=Array.from({length:120},(_,index)=>({id:String(index)}));
  const tail=trajectoryWindow(records,50);
  assert.equal(tail.total,120);
  assert.equal(tail.hasOlder,true);
  assert.equal(tail.records.length,50);
  assert.equal(tail.records[0].id,'70');
  assert.equal(tail.records.at(-1).id,'119');
  assert.deepEqual(trajectoryWindow(records.slice(0,10),50),{records:records.slice(0,10),total:10,hasOlder:false});
});
