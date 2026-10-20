import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,mkdirSync,writeFileSync,rmSync,realpathSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {createRequire} from 'node:module';

const require=createRequire(import.meta.url);
const {resolveTarget,installedCatalog,applicationIcon}=require('../src/open-in-app.cjs');

test('open-in-app resolves a real path only inside a registered workspace root',()=>{
  const base=realpathSync(mkdtempSync(join(tmpdir(),'leveler-open-')));
  const root=join(base,'workspace');
  const outside=join(base,'outside');
  mkdirSync(root);mkdirSync(outside);
  const file=join(root,'intro.md');writeFileSync(file,'# demo');
  writeFileSync(join(outside,'secret.txt'),'secret');
  try{
    assert.equal(resolveTarget(file,[root]),file);
    assert.equal(resolveTarget(root,[root]),root);
    assert.throws(()=>resolveTarget(join(outside,'secret.txt'),[root]),/工作区/);
    assert.throws(()=>resolveTarget('relative/path',[root]),/绝对路径/);
    assert.throws(()=>resolveTarget('',[root]),/路径/);
    assert.throws(()=>resolveTarget(file,[]),/工作区/);
  }finally{rmSync(base,{recursive:true,force:true})}
});

test('open-in-app catalog lists installed applications only and keeps Finder first',()=>{
  const entries=[
    {id:'finder',name:'访达',path:'/System/Library/CoreServices/Finder.app'},
    {id:'code',name:'VS Code',path:'/Applications/Visual Studio Code.app'},
    {id:'missing',name:'Missing',path:'/Applications/Missing.app'},
  ];
  const catalog=installedCatalog(entries,path=>path!=='/Applications/Missing.app');
  assert.deepEqual(catalog.map(app=>app.id),['finder','code']);
  assert.deepEqual(catalog.map(app=>app.path),[
    '/System/Library/CoreServices/Finder.app',
    '/Applications/Visual Studio Code.app',
  ]);
});

// Exercise the real macOS icon decoder: getFileIcon(.app) returns the same
// generic bundle image for these applications, despite their distinct icons.
test('macOS application icons use each bundle resource instead of the generic bundle icon', {skip:process.platform!=='darwin'}, async()=>{
  const catalog=installedCatalog();
  const icons=await Promise.all(catalog.map(candidate=>applicationIcon(candidate.path)));
  assert.ok(icons.length>=2);
  for(const icon of icons)assert.match(icon,/^data:image\/png;base64,iVBOR/);
  assert.equal(new Set(icons).size,icons.length);
});

test('macOS application icons reject missing metadata and resources outside the bundle', {skip:process.platform!=='darwin'}, async()=>{
  const bundle=mkdtempSync(join(tmpdir(),'leveler-icon-bundle-'));
  const contents=join(bundle,'Contents');
  mkdirSync(contents);
  try{
    for(const metadata of [{},{CFBundleIconFile:'../outside.icns'},{CFBundleIconFile:'/outside.icns'},{CFBundleIconFile:'missing'}]){
      writeFileSync(join(contents,'Info.plist'),JSON.stringify(metadata));
      await assert.rejects(applicationIcon(bundle));
    }
  }finally{rmSync(bundle,{recursive:true,force:true});}
});
