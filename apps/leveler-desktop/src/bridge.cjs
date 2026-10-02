const { spawn } = require('node:child_process');
const { createInterface } = require('node:readline');
class Bridge {
  constructor(binary, options = {}) {
    this.pending = new Map(); this.nextId = 0; this.closed = false; this.onEvent=options.onEvent ?? (()=>{});
    this.child=spawn(binary,['desktop-bridge'],{stdio:['pipe','pipe','pipe'],env:options.env ?? process.env,windowsHide:true});
    this.child.stderr.on('data', data=>process.stderr.write(data));
    const lines=createInterface({input:this.child.stdout});
    lines.on('line',line=>{
      let frame; try { frame=JSON.parse(line); } catch { this.fail(new Error('Runtime bridge returned invalid JSON')); return; }
      if (frame.event) { this.onEvent(frame); return; }
      const pending=this.pending.get(String(frame.id));
      if (!pending) return;
      this.pending.delete(String(frame.id)); clearTimeout(pending.timer);
      if (frame.ok) pending.resolve(frame.result);
      else { const error=new Error(frame.error?.message ?? 'Runtime request failed');error.kind=frame.error?.kind;pending.reject(error); }
    });
    this.child.on('error',error=>this.fail(error));
    this.child.on('exit',(code,signal)=>this.fail(new Error(`Runtime bridge disconnected (${signal ?? code})`)));
  }
  request(method,params={}) {
    if(this.closed) return Promise.reject(new Error('Runtime bridge disconnected; reopen Desktop to reconnect'));
    const id=String(++this.nextId);
    return new Promise((resolve,reject)=>{
      // A timeout is uncertain delivery, never a successful task or automatic retry.
      const timer=setTimeout(()=>{this.pending.delete(id);reject(new Error('Runtime request timed out; delivery may have occurred. Refresh before sending again.'));},45000);
      this.pending.set(id,{resolve,reject,timer});
      this.child.stdin.write(JSON.stringify({id,method,params})+'\n',error=>{if(error){clearTimeout(timer);this.pending.delete(id);reject(error);}});
    });
  }
  fail(error) {
    if(this.closed) return; this.closed=true;
    for(const pending of this.pending.values()){clearTimeout(pending.timer);pending.reject(error);} this.pending.clear();
    this.onEvent({event:'error',error:{message:error.message,kind:'transport'}});
  }
  close() {
    if(this.closing) return this.closing;
    this.child.stdin.end(); this.fail(new Error('Desktop closed'));
    this.closing=new Promise(resolve=>{
      if(this.child.exitCode!==null || this.child.signalCode!==null) return resolve();
      const timer=setTimeout(()=>{this.child.kill();resolve();},1500);
      this.child.once('exit',()=>{clearTimeout(timer);resolve();});
    }); return this.closing;
  }
}
module.exports={Bridge};
