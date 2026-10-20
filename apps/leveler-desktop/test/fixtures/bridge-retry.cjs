// Test-only JSONL peer with one-shot disconnects; never loaded by the app.
const readline=require('node:readline');
const lines=readline.createInterface({input:process.stdin});
lines.on('line',line=>{
  const request=JSON.parse(line);
  if(request.method==='exit')process.exit(2);
  if(request.method==='exit-now')process.exit(2);
  if(request.method==='reject')return process.stdout.write(JSON.stringify({id:request.id,ok:false,error:{message:'uncertain',kind:'outcome_unknown'}})+'\n');
  if(request.method==='hang')return; // never answers: delivery is unknown after a crash
  process.stdout.write(JSON.stringify({id:request.id,ok:true,result:{method:request.method,params:request.params,pid:process.pid}})+'\n');
});
