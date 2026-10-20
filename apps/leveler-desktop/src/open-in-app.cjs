const {existsSync,realpathSync,statSync}=require('node:fs');
const path=require('node:path');
const {mkdtemp,readFile,rm}=require('node:fs/promises');
const {tmpdir}=require('node:os');
const {execFile}=require('node:child_process');
const {promisify}=require('node:util');
const runFile=promisify(execFile);

// Supported local applications, in menu order. Finder is the always-present
// file manager; the rest appear only when the bundle exists on this machine.
const APP_CANDIDATES=Object.freeze([
  {id:'finder',name:'访达',path:'/System/Library/CoreServices/Finder.app'},
  {id:'vscode',name:'VS Code',paths:['/Applications/Visual Studio Code.app']},
  {id:'cursor',name:'Cursor',paths:['/Applications/Cursor.app']},
  {id:'zed',name:'Zed',paths:['/Applications/Zed.app']},
  {id:'xcode',name:'Xcode',paths:['/Applications/Xcode.app']},
  {id:'goland',name:'GoLand',paths:['/Applications/GoLand.app']},
  {id:'intellij',name:'IntelliJ IDEA',paths:['/Applications/IntelliJ IDEA.app']},
  {id:'webstorm',name:'WebStorm',paths:['/Applications/WebStorm.app']},
  {id:'pycharm',name:'PyCharm',paths:['/Applications/PyCharm.app']},
  {id:'sublime',name:'Sublime Text',paths:['/Applications/Sublime Text.app']},
  {id:'nova',name:'Nova',paths:['/Applications/Nova.app']},
  {id:'bbedit',name:'BBEdit',paths:['/Applications/BBEdit.app']},
  {id:'ghostty',name:'Ghostty',paths:['/Applications/Ghostty.app']},
  {id:'iterm',name:'iTerm2',paths:['/Applications/iTerm.app']},
  {id:'terminal',name:'终端',paths:['/System/Applications/Utilities/Terminal.app','/Applications/Utilities/Terminal.app']},
  {id:'warp',name:'Warp',paths:['/Applications/Warp.app']},
  {id:'hyper',name:'Hyper',paths:['/Applications/Hyper.app']},
]);

/** Resolve one installed application bundle for a candidate. */
function bundlePath(candidate,exists){
  const paths=candidate.paths ?? [candidate.path];
  return paths.find(candidatePath=>candidatePath&&exists(candidatePath)) ?? null;
}

/** Return only candidates whose bundle exists, each as {id,name,path}. */
function installedCatalog(entries=APP_CANDIDATES,exists=existsSync){
  return entries.map(candidate=>{
    const bundle=bundlePath(candidate,exists);
    return bundle?{id:candidate.id,name:candidate.name,path:bundle}:null;
  }).filter(Boolean);
}

/** Resolve one absolute, existing target inside a registered workspace root. */
function resolveTarget(target,roots){
  if(typeof target!=='string'||!target.trim()||target.length>4096)throw new Error('无效的路径');
  if(!path.isAbsolute(target))throw new Error('路径必须是绝对路径');
  let resolved;
  try{resolved=realpathSync(target);}catch{throw new Error('路径不存在');}
  if(!statSync(resolved).isFile()&&!statSync(resolved).isDirectory())throw new Error('路径不是文件或目录');
  const allowed=(roots??[]).filter(root=>typeof root==='string'&&path.isAbsolute(root));
  for(const root of allowed){
    let realRoot;
    try{realRoot=realpathSync(root);}catch{continue;}
    const relative=path.relative(realRoot,resolved);
    if(relative===''||(!relative.startsWith('..'+path.sep)&&relative!=='..'&&!path.isAbsolute(relative)))return resolved;
  }
  throw new Error('路径不在当前工作区内');
}

/** Decode the bundle's own icon; getFileIcon(.app) returns a MIME-type icon. */
async function applicationIcon(bundle){
  const {stdout}=await runFile('/usr/bin/plutil',['-convert','json','-o','-',path.join(bundle,'Contents','Info.plist')],{timeout:5000,maxBuffer:1024*1024});
  const metadata=JSON.parse(stdout);
  const primary=metadata.CFBundleIcons?.CFBundlePrimaryIcon;
  const iconName=metadata.CFBundleIconFile??primary?.CFBundleIconFiles?.at(-1);
  if(typeof iconName!=='string'||!iconName||iconName.includes('\0')||path.basename(iconName)!==iconName)throw new Error('Application icon resource is unavailable');
  const resources=path.join(bundle,'Contents','Resources');
  const iconPath=path.join(resources,iconName);
  const resource=resolveTarget(existsSync(iconPath)&&statSync(iconPath).isFile()?iconPath:`${iconPath}.icns`,[resources]);
  const temporary=await mkdtemp(path.join(tmpdir(),'leveler-app-icon-'));
  try{
    const png=path.join(temporary,'icon.png');
    // Electron cannot decode ICNS; use the system image decoder at display size.
    await runFile('/usr/bin/sips',['-s','format','png','-Z','64',resource,'--out',png],{timeout:5000,maxBuffer:64*1024});
    return `data:image/png;base64,${(await readFile(png)).toString('base64')}`;
  }finally{await rm(temporary,{recursive:true,force:true});}
}

module.exports={APP_CANDIDATES,installedCatalog,resolveTarget,applicationIcon};
