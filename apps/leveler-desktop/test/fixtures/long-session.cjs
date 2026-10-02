// Synthetic JSONL transport peer, exclusively for the long-session acceptance harness.
// No provider, Runtime or execution evidence is claimed by this fixture.
const readline = require('node:readline');
const date='2026-10-02T08:00:00Z';
const messages=[],entries=[];
function record(event){entries.push({turn_elapsed_ms:entries.length,turn_start:event.type==='user_message_added',event});}
for(let turn=0;turn<100;turn++){
 const user={id:`u${turn}`,role:'user',text:`Synthetic turn ${turn+1}: inspect the workspace and explain the result.`};
 const assistant={id:`a${turn}`,role:'assistant',text:`Synthetic response ${turn+1}.\n`+'Long-session readable transcript. '.repeat(12)};
 messages.push(user,assistant);record({type:'user_message_added',message:user});record({type:'assistant_message_started',message_id:assistant.id});
 for(let tool=0;tool<3;tool++){
  const id=`t${turn}-${tool}`;
  record({type:'tool_call_started',id,name:tool===2?'bash':'read_file',arguments:JSON.stringify(tool===2?{command:'synthetic build output'}:{path:`src/fixture-${turn}-${tool}.rs`}),parallel:false});
  record({type:'tool_call_completed',id,ok:true,preview:(`SYNTHETIC OUTPUT ${id}\n`+'line: measured render payload, not real execution\n'.repeat(400)).slice(0,16000),duration_ms:1,applied_diff:null,exit_code:tool===2?0:null,stop:null});
 }
 record({type:'assistant_text_delta',message_id:assistant.id,delta:assistant.text});record({type:'assistant_message_completed',message_id:assistant.id});
}
const snapshots={long:{id:'long',goal:'Synthetic 100 turns / 300 tools',repository:'/synthetic/workspace',model:'synthetic/render-fixture',task_status:'running',messages,pending_interactions:[],active_tools:[],plan:{steps:[{index:0,description:'Synthetic long-session UI acceptance',status:'running'}]},diff:null},short:{id:'short',goal:'Synthetic short task',repository:null,task_status:'answered',messages:[{id:'short-a',role:'assistant',text:'Short task switch target'}],pending_interactions:[],active_tools:[]}};
function send(frame){process.stdout.write(JSON.stringify(frame)+'\n');}
function event(session_id,data){send({event:'runtime',session_id,data});}
readline.createInterface({input:process.stdin}).on('line',line=>{
 const request=JSON.parse(line),params=request.params??{};let result={};
 if(request.method==='list_tasks')result={tasks:Object.values(snapshots).map(s=>({id:s.id,title:s.goal,source_id:'synthetic',primary_workspace:s.repository,status:s.task_status,created_at:date,updated_at:date,last_activity_at:date})),source_errors:[]};
 else if(request.method==='open')result={session:snapshots[params.session_id]};
 else if(request.method==='snapshot')result=snapshots[params.session_id];
 else if(request.method==='runtime_info')result={synthetic:true};
 else if(request.method==='deliver'){
  const command=params.envelope.command,session=params.envelope.session_id;
  send({id:request.id,ok:true,result:{ok:true}});
  if(command.type==='query_session_history')setImmediate(()=>{
   event(session,{type:'session_history_loaded',query_id:command.query_id,session_id:session,entries:session==='long'?entries:[],omitted_turns:0});
   if(session==='long')event(session,{type:'reasoning_delta',delta:'Synthetic provider reasoning, no execution claim.\n'.repeat(2200)});
  });
  else if(command.type==='submit_message'){
   let tick=0;const timer=setInterval(()=>{
    event(session,{type:'assistant_text_delta',message_id:'a99',delta:` streaming ${tick}`});
    event(session,{type:'tool_call_output',id:'t99-2',stream:'stdout',chunk:`synthetic live output ${tick}\n`});
    if(++tick===50)clearInterval(timer);
   },20);
  }
  return;
 }else if(request.method!=='connect'){send({id:request.id,ok:false,error:{message:'Unsupported synthetic method',kind:'invalid_request'}});return;}
 send({id:request.id,ok:true,result});
});
