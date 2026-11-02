// Passwords arrive only on stdin from the disposable PAM fixture, never argv,
// environment, snapshots, screenshots, traces, or reports. No bearer injection.
import assert from 'node:assert/strict';
import {pathToFileURL} from 'node:url';
const {chromium}=process.env.PLAYWRIGHT_MODULE
  ? await import(pathToFileURL(process.env.PLAYWRIGHT_MODULE).href)
  : await import('playwright');
let input='';
for await(const chunk of process.stdin) input+=chunk;
const {base,username,password}=JSON.parse(input);
input='';
const browser=await chromium.launch({headless:true,
  ...(process.env.CHROMIUM_EXECUTABLE?{executablePath:process.env.CHROMIUM_EXECUTABLE}:{})});
const checks=[],requests=[];
let phase='opening the sign-in page',failure=null;
try {
  // The temporary CA is checked separately by the Python HTTPS requests.
  const context=await browser.newContext({ignoreHTTPSErrors:true});
  await context.addCookies([{name:'ai_other_instance',value:'unrelated-app-session',url:base,secure:true,httpOnly:true}]);
  const page=await context.newPage();
  page.setDefaultTimeout(12000);
  page.on('response',response=>{
    const path=new URL(response.url()).pathname;
    if(path.startsWith('/api/v1/'))requests.push({path,status:response.status()});
  });
  await page.goto(base);
  await page.locator('[data-control="auth.username"]').waitFor();
  await page.waitForFunction(()=>!!document.querySelector('[data-control="auth.username"]')?.value);
  assert.equal(await page.locator('[data-control="auth.username"]').inputValue(),username);
  await page.locator('[data-control="auth.password"]').fill(password);
  phase='opening the dashboard after PAM sign-in with another app cookie present';
  await page.locator('[data-action="auth.login"]').click();
  await page.locator('[data-control="setup.step.0"]').waitFor();
  checks.push('Real browser completes PAM sign-in with another app cookie on the same hostname');
  const cookies=await context.cookies(base);
  assert(cookies.some(c=>c.name==='ai_other_instance'));
  assert(cookies.some(c=>c.name.startsWith('asa_')&&c.secure&&c.httpOnly&&c.sameSite==='Strict'));
  checks.push('Browser retains the secure connector session alongside the other app session');
  phase='restoring the signed-in view after reload';
  await page.reload();
  await page.locator('[data-control="setup.step.0"]').waitFor();
  await page.waitForFunction(()=>window.slackConnector?.snapshot().busy===false);
  await page.locator('[data-control="nav.activity"]').click();
  await page.locator('[data-control="nav.activity"][aria-current="page"]').waitFor();
  checks.push('Reload preserves sign-in and cookie-authenticated view changes pass CSRF checks');
  phase='signing out and signing back in';
  await page.waitForFunction(()=>window.slackConnector?.snapshot().busy===false);
  await page.locator('[data-action="auth.logout"]').click();
  await page.locator('[data-control="auth.password"]').waitFor();
  await page.waitForFunction(()=>window.slackConnector?.snapshot().busy===false);
  await page.locator('[data-control="auth.password"]').fill(password);
  await page.locator('[data-action="auth.login"]').click();
  await page.locator('[data-control="nav.activity"][aria-current="page"]').waitFor();
  checks.push('Logout and a fresh PAM login both work in the same browser');
} catch {
  // Report only the phase and HTTP statuses; never Playwright's captured inputs.
  failure=`Browser failed while ${phase}`;
} finally {await browser.close();}
console.log(JSON.stringify({checks,failure,...(failure?{requests}: {})}));
process.exitCode=failure?1:0;
