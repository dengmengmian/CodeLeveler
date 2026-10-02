import test from 'node:test';
import assert from 'node:assert/strict';
import {settingsMenuItem} from '../src/settings-menu.cjs';

test('native Settings accelerator notifies only the live trusted UI, regardless of focused browser view',()=>{
 const sent=[];let url='leveler-desktop://desktop/index.html',destroyed=false;
 const ui={isDestroyed:()=>destroyed,mainFrame:{get url(){return url;}},send:(...args)=>sent.push(args)};
 const host={isDestroyed:()=>destroyed,webContents:ui};let selected=host;
 const item=settingsMenuItem(()=>selected);
 assert.equal(item.accelerator,'CommandOrControl+,');
 const untrustedFocusedView={webContents:{send(){assert.fail('native browser must never receive desktop capabilities');}}};
 item.click(item,untrustedFocusedView);
 assert.deepEqual(sent,[['desktop:open-settings']]);
 url='https://untrusted.example/';item.click();
 url='leveler-desktop://desktop.evil/index.html';item.click();
 url='leveler-desktop://desktop/index.html';destroyed=true;item.click();
 destroyed=false;selected=null;item.click();
 assert.equal(sent.length,1);
});
