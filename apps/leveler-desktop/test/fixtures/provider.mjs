// Test-only provider. Auxiliary memory extraction and agent turns are distinct.
import {createServer} from 'node:http';
export async function provider(){
 const requests=[],errors=[];
 const server=createServer(async(request,response)=>{
  try{
   let text='';for await(const part of request)text+=part;const body=JSON.parse(text);
   const names=(body.tools??[]).map(tool=>tool.function?.name);requests.push({names,roles:(body.messages??[]).map(m=>m.role)});
   if(names.length===0){response.writeHead(200,{'content-type':'application/json'});response.end(JSON.stringify({id:'fixture-memory',object:'chat.completion',created:1,model:'fixture-model',choices:[{index:0,message:{role:'assistant',content:'{"candidates":[]}'},finish_reason:'stop'}]}));return;}
   const completed=(body.messages??[]).filter(message=>message.role==='tool').length;
   let delta,finish;
   if(completed===0){if(!names.includes('read_file'))throw new Error('Agent request does not expose read_file');delta={role:'assistant',tool_calls:[{index:0,id:'fixture-read',type:'function',function:{name:'read_file',arguments:JSON.stringify({path:'fixture.txt'})}}]};finish='tool_calls';}
   else if(completed===1){const name=names.includes('run_command')?'run_command':'shell_command';if(!names.includes(name))throw new Error('Agent request does not expose command tool');const args=name==='run_command'?{program:'rm',args:['-rf','scratch']}:{cmd:'rm -rf scratch'};delta={role:'assistant',tool_calls:[{index:0,id:'fixture-consent',type:'function',function:{name,arguments:JSON.stringify(args)}}]};finish='tool_calls';}
   else {delta={role:'assistant',content:'fixture-interactions-complete'};finish='stop';}
   response.writeHead(200,{'content-type':'text/event-stream'});const chunk=(delta,finish_reason)=>({id:'fixture',object:'chat.completion.chunk',created:1,model:'fixture-model',choices:[{index:0,delta,finish_reason}]});response.write('data: '+JSON.stringify(chunk(delta,null))+'\n\n');response.write('data: '+JSON.stringify(chunk({},finish))+'\n\n');response.end('data: [DONE]\n\n');
  }catch(error){errors.push(error.message);response.writeHead(500,{'content-type':'application/json'});response.end(JSON.stringify({error:{message:error.message}}));}
 });
 await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
 return {baseURL:`http://127.0.0.1:${server.address().port}`,requests,errors,close:()=>new Promise(resolve=>server.close(resolve))};
}
