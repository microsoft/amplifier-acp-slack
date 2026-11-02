import {mkdtemp,readFile,rm,writeFile} from 'node:fs/promises';
import {spawn,execFileSync} from 'node:child_process';
import {tmpdir} from 'node:os';
import {join,resolve} from 'node:path';
import {setTimeout as sleep} from 'node:timers/promises';
import assert from 'node:assert/strict';
// Run after cargo build --release, with PLAYWRIGHT_MODULE and CHROMIUM_PATH.
assert(process.env.PLAYWRIGHT_MODULE && process.env.CHROMIUM_PATH, 'Set PLAYWRIGHT_MODULE and CHROMIUM_PATH');
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE);
await import('node:fs/promises').then(fs=>fs.mkdir('private/report',{recursive:true}));
const home=await mkdtemp(join(tmpdir(),'slack-media-ui-'));const binary=resolve('target/release/acp-slack-adapter');
let server,browser;
try{
 execFileSync(binary,['--home',home,'init','--label','Media settings check','--owner',process.env.USER],{stdio:'ignore'});
 server=spawn(binary,['--home',home,'serve'],{stdio:'ignore'});
 let endpoints;for(let i=0;i<100;i++){try{endpoints=JSON.parse(await readFile(join(home,'endpoints.json'),'utf8'));break;}catch{await sleep(100);}}
 assert(endpoints);const token=(await readFile(join(home,'secrets/control-token'),'utf8')).trim();
 browser=await chromium.launch({executablePath:process.env.CHROMIUM_PATH,headless:true,args:['--no-sandbox']});
 const context=await browser.newContext({ignoreHTTPSErrors:true,extraHTTPHeaders:{Authorization:'Bearer '+token},viewport:{width:1440,height:1000}});
 const page=await context.newPage(),errors=[];page.on('pageerror',e=>errors.push(e.message));
 await page.goto(endpoints.base_url);await page.locator('[data-control="nav.configuration"]').click();
 const media=page.locator('[data-control="section.media"]');await media.waitFor();
 await media.locator('[data-control="config.media.incoming"]').uncheck();
 await media.locator('[data-control="config.media.outgoing"]').uncheck();
 await media.locator('summary').click();
 await media.locator('[data-control="config.media.max_files"]').fill('2');
 await page.locator('[data-control="config.context.owner_slack_user_id"]').fill('U123456789');
 await page.locator('[data-control="config.context.enabled"]').uncheck();
 await page.locator('[data-control="config.save"]').first().click();
 await page.getByText('Connection settings saved.',{exact:true}).first().waitFor({timeout:10000});
 const config=await (await context.request.get(endpoints.base_url+'/api/v1/config')).json();assert.equal(config.media.incoming,false);assert.equal(config.media.outgoing,false);assert.equal(config.media.max_files,2);assert.equal(config.context.enabled,false);assert.equal(config.context.owner_slack_user_id,'U123456789');
 await media.scrollIntoViewIfNeeded();await media.screenshot({path:'private/report/media-desktop.png'});
 await page.setViewportSize({width:390,height:844});await media.scrollIntoViewIfNeeded();await media.screenshot({path:'private/report/media-mobile.png'});
 assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));assert.deepEqual(errors,[]);
 const report={passed:true,controls_saved_through_api:true,desktop_mobile_layout:true,javascript_errors:errors,live_slack_posts:0};await writeFile('private/report/media-browser.json',JSON.stringify(report,null,2)+'\n');console.log(JSON.stringify(report));
}finally{if(browser)await browser.close();if(server){server.kill('SIGTERM');await new Promise(r=>server.once('exit',r));}await rm(home,{recursive:true,force:true});}
