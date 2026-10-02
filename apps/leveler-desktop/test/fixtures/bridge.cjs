// Test-only JSONL peer; never loaded by the production app.
const readline=require('node:readline');
const lines=readline.createInterface({input:process.stdin});
lines.on('line',line=>{const request=JSON.parse(line);if(request.method==='exit')process.exit(2);if(request.method==='reject')return process.stdout.write(JSON.stringify({id:request.id,ok:false,error:{message:'uncertain',kind:'outcome_unknown'}})+'\n');process.stdout.write(JSON.stringify({event:'runtime',session_id:'s1',data:{type:'runtime_ready'}})+'\n');process.stdout.write(JSON.stringify({id:request.id,ok:true,result:request.params})+'\n');});
