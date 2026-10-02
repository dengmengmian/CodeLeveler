import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtemp,mkdir,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';
import workspaceSelection from '../src/workspace-selection.cjs';
const {RecentWorkspaceSelection,taskListOptions}=workspaceSelection;
test('recent selection admits only directories from the latest successful Main-owned task index',async()=>{
 const root=await mkdtemp(path.join(tmpdir(),'leveler-workspace-'));const known=path.join(root,'known');const other=path.join(root,'other');const file=path.join(root,'file');await mkdir(known);await mkdir(other);await writeFile(file,'x');
 try{
  const folders=new Map();const selection=new RecentWorkspaceSelection(folders);
  await assert.rejects(selection.select(known));
  selection.update({tasks:[{primary_workspace:known},{primary_workspace:known},{primary_workspace:null},{primary_workspace:file}]});
  const selected=await selection.select(known);assert.equal(selected.path,known);assert.match(selected.id,/^[a-f\d-]{36}$/);assert.equal(folders.get(selected.id),known);
  const again=await selection.select(known);assert.notEqual(again.id,selected.id);
  await assert.rejects(selection.select(other));await assert.rejects(selection.select(file));await assert.rejects(selection.select({path:known}));
  selection.update({tasks:[{primary_workspace:other}]});await assert.rejects(selection.select(known));assert.equal((await selection.select(other)).path,other);
  await rm(other,{recursive:true});await assert.rejects(selection.select(other));
 }finally{await rm(root,{recursive:true,force:true});}
});
test('malformed task index never grants renderer arbitrary filesystem paths',async()=>{
 const selection=new RecentWorkspaceSelection(new Map());
 for(const index of [null,{}, {tasks:[]},{tasks:[null,{primary_workspace:123},{primary_workspace:''}]}]){selection.update(index);await assert.rejects(selection.select('/'));}
});

test('task list options are strictly read-only and archived results cannot replace normal workspace grants',async()=>{
 const selection=new RecentWorkspaceSelection(new Map());selection.update({tasks:[{primary_workspace:'/normal'}]});
 selection.update({tasks:[{primary_workspace:'/archived'}]},true);assert.deepEqual([...selection.paths],['/normal']);
 selection.update({tasks:[{primary_workspace:'/next'}]},false);assert.deepEqual([...selection.paths],['/next']);
 assert.deepEqual(taskListOptions(undefined),{include_archived:false});assert.deepEqual(taskListOptions({}),{include_archived:false});
 for(const include_archived of [true,false])assert.deepEqual(taskListOptions({include_archived}),{include_archived});
 for(const value of [null,true,[],{include_archived:1},{include_archived:null},{include_archived:'true'},{workspace:'/private'},{include_archived:true,mutate:true}])assert.throws(()=>taskListOptions(value));
});
