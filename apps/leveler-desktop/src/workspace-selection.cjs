const {stat}=require('node:fs/promises');
const {isAbsolute}=require('node:path');
const {randomUUID}=require('node:crypto');
// A recent item is trusted only after Main receives it from list_tasks. The
// renderer may select that exact item; it cannot turn this into a path probe.
class RecentWorkspaceSelection {
 /** @param {Map<string,string>} folders */
 constructor(folders){this.folders=folders;/** @type {Set<string>} */ this.paths=new Set();}
 /** @param {unknown} index @param {boolean} [includeArchived] */
 update(index,includeArchived=false){
  if(includeArchived)return;
  const paths=new Set();
  if(index&&typeof index==='object'&&'tasks' in index&&Array.isArray(index.tasks)){
   for(const task of index.tasks){
    if(task&&typeof task==='object'&&'primary_workspace' in task){const value=task.primary_workspace;if(typeof value==='string'&&value.length>0&&value.length<=4096&&isAbsolute(value))paths.add(value);}
   }
  }
  this.paths=paths;
 }
 /** @param {unknown} path @returns {Promise<{id:string,path:string}>} */
 async select(path){
  if(typeof path!=='string'||!this.paths.has(path))throw new Error('请从最近工作区列表选择，或打开本地文件夹');
  const directory=await stat(path).catch(()=>{throw new Error('工作区文件夹不可用，请重新选择文件夹');});
  if(!directory.isDirectory())throw new Error('工作区文件夹不可用，请重新选择文件夹');
  if(!this.paths.has(path))throw new Error('最近工作区列表已更新，请重新选择');
  const id=randomUUID();this.folders.set(id,path);return {id,path};
 }
}
/** @param {unknown} value @returns {{include_archived:boolean}} */
function taskListOptions(value){
 if(value===undefined)return {include_archived:false};
 if(!value||typeof value!=='object'||Array.isArray(value)||Object.keys(value).some(key=>key!=='include_archived')||('include_archived' in value&&typeof value.include_archived!=='boolean'))throw new Error('Invalid task list options');
 return {include_archived:'include_archived' in value?value.include_archived===true:false};
}
module.exports={RecentWorkspaceSelection,taskListOptions};
