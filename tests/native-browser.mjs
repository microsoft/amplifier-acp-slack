import fs from 'node:fs/promises';
import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'../ui/tests/node_modules/playwright/index.mjs');
const input=JSON.parse(await fs.readFile(process.argv[2],'utf8'));
const browser=await chromium.launch({headless:true});
try{
 const page=await browser.newPage({viewport:{width:1100,height:800}});const errors=[];page.on('pageerror',e=>errors.push(e.message));
 await page.goto(input.base+'/#setup='+input.cap);await page.getByRole('heading',{name:'Make this connector yours.'}).waitFor();assert(!page.url().includes('setup='));
 await page.locator('[name=password]').fill(input.password);await page.getByRole('button',{name:'Create account',exact:true}).click();
 await page.waitForFunction(()=>window.slackConnector?.snapshot().config?.id);const state=await page.evaluate(()=>window.slackConnector.snapshot());assert.equal(state.config.enabled,false);assert.equal(state.config.acp.command,'');assert.equal(state.config.setup_complete,false);
 await page.getByRole('button',{name:/Sign out/}).click();await page.getByRole('button',{name:'Sign in',exact:true}).waitFor();assert.equal(await page.getByRole('heading',{name:'Make this connector yours.'}).count(),0);
 await page.locator('[name=password]').fill(input.password);await page.getByRole('button',{name:'Sign in',exact:true}).click();await page.waitForFunction(()=>!!window.slackConnector?.snapshot().config?.id);
 await page.setViewportSize({width:390,height:844});assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth+1));
 assert.deepEqual(errors,[]);console.log('Native first-account browser: passed create, fragment removal, sign out/in, preserved unconfigured state, mobile fit and zero script errors');
}finally{await browser.close();}
