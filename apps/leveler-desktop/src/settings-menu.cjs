function settingsMenuItem(getWindow){return {label:'设置…',accelerator:'CommandOrControl+,',click(){
 const host=getWindow();if(!host||host.isDestroyed()||host.webContents.isDestroyed())return;
 if(!host.webContents.mainFrame.url.startsWith('leveler-desktop://desktop/'))return;
 host.webContents.send('desktop:open-settings');
}};}
module.exports={settingsMenuItem};
