import http from "node:http";
import fs from "node:fs/promises";
import path from "node:path";
import {fileURLToPath} from "node:url";
import assert from "node:assert/strict";

const playwrightPath=process.env.PLAYWRIGHT_MODULE||"playwright";
const {chromium}=await import(playwrightPath);
const ui=path.resolve(path.dirname(fileURLToPath(import.meta.url)),"..");
const artifacts=process.env.UI_ARTIFACTS||"/tmp/acp-slack-adapter-ui";
await fs.mkdir(artifacts,{recursive:true});
const prefix="/embedded/connector/";
let privateSecrets={},loggedIn=false,credentials=false,socketState="connected",views=new Map(),savedTokens=[],checks=[],savedCredentialCount=0,configWrites=0;
let dropSessionCookie=true,failConfig=false,failView=false,failAcp=false,failImage=false,imageWrites=0;
const viewBodies=[];
let config={schema:1,id:"fixture-connector",label:"Helper in Slack",revision:1,enabled:false,setup_complete:false,participation:"mentions",context:{enabled:false,owner_slack_user_id:""},media:{incoming:false,outgoing:false,max_files:4,max_file_bytes:2097152},acp:{command:"/installed/agent-instance",args:["acp","--home","/installed/helper"],env:{},secret_env:{},cwd:"/installed/helper/workspace",timeout_seconds:180,client:{read_text_file:false,write_text_file:false,terminal:false,workspace_roots:[],approval_users:[],permission_timeout_seconds:180,max_file_bytes:1048576,terminal_output_bytes:262144,terminal_timeout_seconds:120}},slack:{team_id:"",app_id:"",bot_user_id:"",bot_id:"",channels:[],allow_dms:true,api_base:"https://slack.com/api/",loopback_test:false},attention:{settle_ms:1500,max_replies_per_agent_per_hour:30,max_replies_per_context_per_hour:12,max_bot_replies_without_human:2,max_decisions_per_agent_per_hour:120,minimum_reply_interval_seconds:6},server:{bind:"0.0.0.0",port:41337,owner:"fixture-owner",pam_service:"login",public_url:null,embed_origins:[]}};
let permissions=[],mediaChecked=null,fileScopes=false,portrait={};
const mediaResult=()=>({verified:true,checked_at:Date.now(),ready:fileScopes,granted_scopes:fileScopes?["files:read","files:write"]:[],missing_scopes:fileScopes?[]:["files:read","files:write"],settings_url:"https://api.slack.com/apps/A_FIXTURE/oauth",note:fileScopes?"Slack file permissions are installed.":"Add the missing Bot Token Scopes and reinstall this existing app."});
const events=[{id:5,time:Date.now(),kind:"decision",data:{decision:"pass",reason:"The humans are already working through the question."}},{id:4,time:Date.now(),kind:"delivery",data:{status:"sent",note:""}},{id:3,time:Date.now(),kind:"delivery",data:{status:"uncertain",note:"Inspect Slack before resending."}},{id:2,time:Date.now(),kind:"connection",data:{state:"connected"}},{id:1,time:Date.now(),kind:"view.ack",data:{}}];
const conversations=[{id:"context-one",channel:"C_FIRST",thread:"",session_id:"session-one",resumable:true,updated_at:Date.now(),turns:3,last_outcome:"pass"},{id:"context-two",channel:"C_SECOND",thread:"1234.1",session_id:"session-two",resumable:true,updated_at:Date.now(),turns:2,last_outcome:"speak"}];
const status=()=>({id:config.id,label:config.label,version:"fixture",enabled:config.enabled,setup_complete:config.setup_complete,config_revision:config.revision,runtime:{state:config.enabled?"running":"stopped",reason:"",updated_at:Date.now()},credentials:{slack:credentials},portrait,media:mediaChecked,state:{jobs:{pending:1,running:1,pass:4,speak:3},outbox:{sent:3,ready:1,uncertain:1},contexts:2,slack_connection:{state:socketState}},connections:[],addresses:{base_url:"https://fixture.invalid/"},restart_required:false});
function put(o,p,v){const keys=p.split(".");for(const k of keys.slice(0,-1)){if(!o[k]||typeof o[k]!=="object")o[k]={};o=o[k];}o[keys.at(-1)]=v;}
function apply(view,commands){for(const c of commands){if(c.type==="navigate"){view.state.route=c.route;if(c.step!==undefined)view.state.step=c.step;}else if(c.type==="set_draft")put(view.state.draft,c.path,c.value);else if(c.type==="set_filter")view.state.filters[c.key]=c.value;else if(c.type==="select_conversation")view.state.selected_conversation=c.conversation_id;else if(c.type==="focus")view.state.focus=c.target;else if(c.type==="scroll")view.state.scroll={x:c.x,y:c.y};else if(c.type==="open_modal")view.state.modal=c.modal;else if(c.type==="close_modal")view.state.modal=null;else throw Error("Unknown view command");}view.revision++;view.ack=null;}
const server=http.createServer(async(req,res)=>{
  const url=new URL(req.url,"http://localhost");
  const send=(data,status=200,headers={})=>{res.writeHead(status,{"Content-Type":"application/json","Cache-Control":"no-store",...headers});res.end(JSON.stringify(data));};
  if(!url.pathname.startsWith(prefix))return send({error:"Wrong prefix"},404);
  let endpoint=url.pathname.slice(prefix.length);
  if(!endpoint.startsWith("api/v1/")){try{const name=endpoint||"index.html";if(!["index.html","app.js","app.css"].includes(name))return send({error:"Missing"},404);res.writeHead(200,{"Content-Type":name.endsWith("js")?"text/javascript":name.endsWith("css")?"text/css":"text/html"});res.end(await fs.readFile(path.join(ui,name)));}catch{return send({error:"Missing"},404);}return;}
  endpoint=endpoint.slice(7);let raw="";for await(const chunk of req)raw+=chunk;const data=raw?JSON.parse(raw):{};
  const authenticated=loggedIn&&(req.headers.cookie||"").split(";").some(value=>value.trim()==="asa_fixture=fixture-session");
  if(endpoint==="auth/session")return send({authenticated,owner:"fixture-owner",csrf:authenticated?"fixture-csrf":null});
  if(endpoint==="auth/login"){assert.equal(data.username,"fixture-owner");loggedIn=true;return send({authenticated:true,owner:"fixture-owner",csrf:"fixture-csrf"},200,dropSessionCookie?{}:{"Set-Cookie":`asa_fixture=fixture-session; Path=${prefix}; HttpOnly; SameSite=Strict`});}
  if(!authenticated)return send({error:"Sign in"},401);
  if(req.method!=="GET"&&req.headers["x-csrf-token"]!=="fixture-csrf")return send({error:"CSRF missing"},403);
  if(endpoint==="auth/logout"){loggedIn=false;return send({ok:true});}
  if(endpoint==="config"){if(failConfig)return send({error:"Temporary configuration outage"},503);if(req.method==="PUT"){if(data.expected_revision!==config.revision)return send({error:"Configuration changed"},409);config={...data.config,revision:config.revision+1};configWrites++;}return send(config);}
  if(endpoint==="status")return send(status());
  if(endpoint==="media/readiness"){mediaChecked=mediaResult();return send(mediaChecked);}
  if(endpoint==="media/enable"){if(!fileScopes)return send({error:"File access is not ready"},400);config.media.incoming=config.media.outgoing=true;config.revision++;return send({config,media:mediaResult()});}
  if(endpoint==="permissions")return send({items:permissions});
  if(endpoint.startsWith("permissions/")&&endpoint.endsWith("/decision")){const item=permissions.find(p=>p.id===endpoint.split("/")[1]);if(!item||item.status!=="pending")return send({error:"Not pending"},400);item.status=data.option_id==="yes"?"approved":data.option_id==="no"?"denied":"cancelled";return send({ok:true});}
  if(endpoint==="activity")return send({items:events});
  if(endpoint==="conversations")return send({items:conversations});
  if(endpoint==="manifest")return send({display_information:{name:config.label},settings:{socket_mode_enabled:true}});
  if(endpoint==="check"){checks.push(data.target);if(failAcp)return send({ok:false,acp:{ok:false,error:"Agent is temporarily unavailable"}});return send({ok:true,...(["all","acp"].includes(data.target)?{acp:{ok:true,result:{protocolVersion:1}}}:{}),...(["all","slack"].includes(data.target)?{slack:{ok:true,result:{team_id:"T_WORKSHOP"}}}:{})});}
  if(endpoint==="credentials/slack"){assert.equal(config.enabled,false);assert(data.app_token.startsWith("xapp-")&&data.bot_token.startsWith("xoxb-"));assert.equal(data.app_id,undefined);credentials=true;savedCredentialCount++;config.slack={...config.slack,team_id:"T_WORKSHOP",app_id:"A_FIXTURE",bot_user_id:"U_FIXTURE",bot_id:"B_FIXTURE"};config.revision++;return send({connected:true,identity:config.slack,config_revision:config.revision});}
  if(endpoint==="identity/credential"){if(failImage)return send({error:"Image access unavailable"},503);assert(data.token.startsWith("xoxe.xoxp-"));imageWrites++;portrait={state:"waiting",credential_configured:true,message:"Image updates will start automatically"};return send(portrait);}
  if(endpoint==="identity"&&req.method==="POST"){portrait={...portrait,state:"synced",synced_at:Date.now(),message:"Slack is using the current image"};return send(portrait);}
  if(endpoint==="setup/complete"){assert(credentials);config.setup_complete=true;config.enabled=true;config.revision++;return send(config);}
  if(endpoint==="lifecycle"){config.enabled=data.action==="start";config.revision++;return send(status());}
  if(endpoint==="secrets")return send({items:Object.keys(privateSecrets)});
  if(endpoint.startsWith("secrets/")){const name=decodeURIComponent(endpoint.slice(8));if(req.method==="DELETE")delete privateSecrets[name];else privateSecrets[name]=data.value;return send({name,stored:true});}
  if(endpoint==="tokens"){if(req.method==="POST"){const t={id:"access-one",name:data.name,scopes:data.scopes,expires_at:Date.now()+data.expires_in_seconds*1000};savedTokens.push(t);return send({...t,token:"one-time-management-secret"});}return send({items:savedTokens});}
  if(endpoint.startsWith("tokens/")){savedTokens=[];return send({ok:true});}
  if(endpoint==="views"){if(failView)return send({error:"Temporary display outage"},503);if(req.method==="POST"){const id=`view-${views.size+1}`,v={id,label:data.label,revision:1,ack:null,state:{route:config.setup_complete?"dashboard":"setup",step:0,draft:{},filters:{},selected_conversation:null,focus:null,scroll:{x:0,y:0},modal:null}};views.set(id,v);return send(v);}return send({items:[...views.values()]});}
  if(endpoint.startsWith("views/")){const [,id,suffix]=endpoint.split("/"),v=views.get(id);if(!v)return send({error:"Missing view"},404);if(suffix==="commands"){viewBodies.push(data);if(data.expected_revision!==v.revision)return send({error:"View changed"},409);if(JSON.stringify(data).includes("xapp-")||JSON.stringify(data).includes("xoxb-"))return send({error:"Credentials in view"},400);apply(v,data.commands);}if(suffix==="ack"){if(data.revision!==v.revision)return send({error:"View changed"},409);v.ack=data;}return send(v);}
  return send({error:`Unknown endpoint ${endpoint}`},404);
});
await new Promise(r=>server.listen(0,"127.0.0.1",r));
const origin=`http://127.0.0.1:${server.address().port}`;
const browser=await chromium.launch({headless:true,...(process.env.CHROMIUM_EXECUTABLE?{executablePath:process.env.CHROMIUM_EXECUTABLE}:{}),args:["--no-sandbox"]});
const context=await browser.newContext({viewport:{width:1440,height:1050},permissions:["clipboard-read","clipboard-write"]});
const page=await context.newPage(),errors=[];
page.on("pageerror",e=>errors.push(e.message));
let passed=0;
const ok=label=>{passed++;process.stdout.write(`PASS ${label}\n`);};
const waitIdle=()=>page.waitForFunction(()=>window.slackConnector?.snapshot().busy===false);
try{
  await page.goto(origin+prefix);await page.locator('[data-control="auth.username"]').waitFor();assert.equal(await page.locator('[data-control="auth.username"]').inputValue(),"fixture-owner");await page.locator('[data-control="auth.password"]').fill("test-password");await page.locator('[data-action="auth.login"]').click();
  await page.waitForFunction(()=>document.querySelector('.banner.error')?.textContent.includes("restore your sign-in session"));await waitIdle();assert(await page.locator('[data-action="auth.login"]').isEnabled());ok("A missing browser session after valid login produces an actionable error");
  dropSessionCookie=false;failConfig=true;await page.locator('[data-control="auth.password"]').fill("test-password");await page.locator('[data-action="auth.login"]').click();
  await page.waitForFunction(()=>document.querySelector('[data-control="auth.opening"]')?.textContent.includes("Temporary configuration outage"));await waitIdle();ok("Configuration load failure is visible after successful sign-in");
  failConfig=false;failView=true;await page.locator('[data-action="auth.resume"]').click();await page.waitForFunction(()=>document.querySelector('[data-control="auth.opening"]')?.textContent.includes("Temporary display outage"));await waitIdle();ok("Display initialization failure offers a working retry without another password");
  failView=false;assert(await page.evaluate(()=>window.slackConnector.actions().includes("auth.resume")));await page.evaluate(()=>window.slackConnector.act("auth.resume"));await page.locator('[data-control="setup.connect-screen"]').waitFor();
  const view=views.get("view-1");
  assert.equal(await page.locator('[data-draft="config.acp.command"]').count(),0);
  assert.equal(await page.locator('[data-draft="config.label"]').count(),0);
  assert.equal(await page.locator('[data-action="setup.next"]').count(),0);
  assert.equal(await page.locator('[data-action="check.acp"]').count(),0);
  assert((await page.locator(".endpoint-summary").innerText()).includes("filled in"));
  assert.equal(await page.locator('[data-action="manifest.download"]').count(),0);
  await page.screenshot({path:path.join(artifacts,"setup-simple.png"),fullPage:true});
  ok("One setup screen uses the installed agent and defaults without mandatory wizard or check buttons");
  const priorSize=page.viewportSize();await page.setViewportSize({width:390,height:844});
  assert(await page.evaluate(()=>document.documentElement.scrollWidth<=window.innerWidth));
  await page.screenshot({path:path.join(artifacts,"setup-mobile.png"),fullPage:true});await page.setViewportSize(priorSize);
  ok("Setup fits a phone-sized screen without horizontal scrolling");
  const installedCommand=config.acp.command;config.acp.command="";config.revision++;
  await page.evaluate(()=>window.slackConnector.act("refresh"));
  assert(await page.locator('[data-action="setup.connect"]').isEnabled());
  const missing=await page.evaluate(()=>window.slackConnector.act("setup.connect",{}).catch(e=>e.message));
  assert(missing.includes("Choose an agent connection"));assert.equal(savedCredentialCount,0);
  config.acp.command=installedCommand;config.revision++;await page.evaluate(()=>window.slackConnector.act("refresh"));
  ok("A missing agent points to Advanced settings before requesting Slack credentials");
  const createUrl=new URL(await page.locator('[data-control="slack.app.create"]').getAttribute("href"));
  assert.equal(JSON.parse(createUrl.searchParams.get("manifest_json")).display_information.name,"Helper in Slack");
  assert((await page.locator("#setup-connect-form").innerText()).includes("connections:write"));
  await page.locator('[data-action="setup.manifest-help"]').click();
  const downloadEvent=page.waitForEvent("download");await page.locator('[data-action="manifest.download"]').click();const download=await downloadEvent;
  assert(JSON.parse(await fs.readFile(await download.path(),"utf8")).settings.socket_mode_enabled);
  await page.locator('[data-action="setup.manifest-help"]').click();
  ok("Prepared Slack settings open directly; copy/download recovery is available on demand");
  await page.locator('[data-control="credentials.app_token"]').fill("xapp-fixture-private");await page.locator('[data-control="credentials.bot_token"]').fill("xoxb-fixture-private");
  await page.locator('[data-control="setup.preferences"]').click();
  assert.equal(await page.locator('[data-control="credentials.app_token"]').inputValue(),"xapp-fixture-private");
  await page.locator('[data-control="participation.ambient"]').check();
  await page.locator('[data-control="setup.preferences"]').click();
  await page.locator('[data-control="setup.images"]').check();
  await page.locator('[data-control="setup.identity.token"]').fill("xoxe.xoxp-image-fixture-private");
  await page.locator('[data-control="setup.identity.refresh"]').fill("xoxe-refresh-fixture-private");
  await page.locator('[data-control="setup.preferences"]').click();
  assert.equal(await page.locator('[data-control="setup.identity.refresh"]').inputValue(),"xoxe-refresh-fixture-private");
  await page.locator('[data-control="setup.preferences"]').click();
  assert(!JSON.stringify(viewBodies).includes("fixture-private"));
  ok("Optional preferences and image access share the setup flow without leaking or losing typed secrets");
  failAcp=true;await page.locator('[data-action="setup.connect"]').click();
  await page.waitForFunction(()=>document.querySelector('.banner.error')?.textContent.includes("saved Slack access is kept"));await waitIdle();
  assert.equal(savedCredentialCount,1);assert.equal(imageWrites,1);assert.equal(config.enabled,false);
  assert.equal(await page.locator('[data-control="credentials.app_token"]').count(),0);
  assert.equal(await page.locator('[data-control="setup.identity.token"]').count(),0);
  assert.equal(await page.locator('[data-control="slack.app.create"]').count(),0);
  assert((await page.locator('[data-control="checks.results"]').innerText()).includes("temporarily unavailable"));
  failAcp=false;await page.locator('[data-action="setup.connect"]').click();
  await page.locator('[data-control="nav.dashboard"][aria-current="page"]').waitFor();
  assert.equal(savedCredentialCount,1);assert.equal(imageWrites,1);assert.equal(config.participation,"ambient");
  assert(config.enabled&&config.setup_complete);assert(checks.includes("acp")&&checks.includes("all"));
  assert(!JSON.stringify(view).includes("fixture-private"));
  assert.equal(await page.locator('[data-control="slack.agent.open"]').getAttribute("href"),"https://slack.com/app_redirect?app=A_FIXTURE&team=T_WORKSHOP");
  await page.screenshot({path:path.join(artifacts,"dashboard.png"),fullPage:true});
  ok("Connect verifies and starts; a failed agent check retries without new token copies or app creation");
  // An optional image failure cannot strand an otherwise working connection.
  config={...config,enabled:false,setup_complete:false,revision:config.revision+1};credentials=false;portrait={};failImage=true;
  await page.evaluate(()=>window.slackConnector.act("refresh"));await page.evaluate(()=>window.slackConnector.act("navigate",{route:"setup"}));
  assert(await page.evaluate(()=>window.slackConnector.actions().includes("setup.connect")));
  await page.evaluate(()=>window.slackConnector.act("setup.connect",{app_token:"xapp-fixture-private",bot_token:"xoxb-fixture-private",image_token:"xoxe.xoxp-image-fixture-private",image_refresh_token:"xoxe-refresh-fixture-private"}));
  assert(config.enabled&&config.setup_complete);assert((await page.locator('.banner[role="status"]').innerText()).includes("Image access wasn’t saved"));
  assert.equal(savedCredentialCount,2);failImage=false;
  assert(!JSON.stringify(viewBodies).includes("fixture-private"));
  ok("The same one-action setup is available to agents; optional image failures do not block messaging");
  socketState="reconnecting";await page.locator('[data-action="refresh"]').click();await page.waitForFunction(()=>document.querySelector(".hero")?.textContent.includes("Connecting to Slack"));assert((await page.locator(".health-row").nth(1).innerText()).includes("Reconnecting"));socketState="connected";await page.locator('[data-action="refresh"]').click();await page.waitForFunction(()=>document.querySelector(".hero")?.textContent.includes("Ready for the conversation"));ok("Actual Slack socket health drives status, not runtime process alone");
  apply(view,[{type:"navigate",route:"configuration"},{type:"set_filter",key:"advanced",value:true},{type:"set_draft",path:"config.label",value:"Helper · team conversations"}]);await page.locator('[data-draft="config.acp.command"]').waitFor();await page.waitForFunction(()=>document.querySelector('[data-draft="config.label"]')?.value==="Helper · team conversations");assert.equal(await page.locator('[data-action="advanced.toggle"]').getAttribute("aria-expanded"),"true");apply(view,[{type:"focus",target:"config.acp.cwd"},{type:"scroll",x:0,y:450}]);await page.waitForFunction(()=>document.activeElement?.dataset.control==="config.acp.cwd");await page.waitForFunction(()=>window.scrollY>400);assert.equal(view.ack?.status,"applied");await page.locator('[data-action="config.save"]').first().click();await page.waitForFunction(()=>document.querySelector(".banner")?.textContent.includes("settings saved"));assert.equal(config.label,"Helper · team conversations");ok("External controller drives route, advanced section, drafts, focus, scroll, and acknowledgement");
  portrait={agent_name:"Test teammate",state:"needs_attention",message:"The connector could not refresh image access (invalid_arguments).",credential_configured:true};
  await page.evaluate(()=>window.slackConnector.act("refresh"));
  let imageStatus=await page.locator('[data-control="identity.status"]').innerText();
  assert(imageStatus.includes("Image not yet synced"));assert(!imageStatus.includes("Waiting for an image"));
  portrait={...portrait,state:"waiting_for_image"};await page.evaluate(()=>window.slackConnector.act("refresh"));
  assert((await page.locator('[data-control="identity.status"]').innerText()).includes("Waiting for an image"));
  portrait={...portrait,state:"synced",synced_at:Date.now()};await page.waitForFunction(()=>document.querySelector('[data-control="identity.status"]')?.textContent.includes("Last updated"));
  assert((await page.locator('[data-control="identity.status"]').innerText()).includes("Last updated"));
  ok("Image sync failures do not imply the agent is missing its image");
  await page.locator('[data-action="identity.open"]').click();
  await page.locator('[data-control="identity.token"]').fill("xoxe.xoxp-image-fixture-private");
  await page.locator('[data-control="identity.refresh"]').fill("xoxe-refresh-fixture-private");
  await page.evaluate(()=>window.slackConnector.command([{type:"focus",target:"identity.token"}]));
  assert.equal(await page.locator('[data-control="identity.token"]').inputValue(),"xoxe.xoxp-image-fixture-private");
  assert.equal(await page.locator('[data-control="identity.refresh"]').inputValue(),"xoxe-refresh-fixture-private");
  await page.getByRole("button",{name:"Enable image updates",exact:true}).click();
  await page.getByRole("dialog").waitFor({state:"hidden"});await waitIdle();
  assert((await page.locator('[data-control="identity.status"]').innerText()).includes("current image"));
  assert(!JSON.stringify(viewBodies).includes("fixture-private"));
  ok("Existing connections can replace image access without mixing private input values");

  await page.locator('[data-action="media.check"]').click();await waitIdle();assert((await page.locator('[data-control="media.readiness"]').innerText()).includes("files:read, files:write"));assert.equal(await page.locator('[data-action="media.enable"]').count(),0);assert.equal(await page.locator('[data-control="media.readiness"] a').getAttribute("href"),"https://api.slack.com/apps/A_FIXTURE/oauth");fileScopes=true;await page.locator('[data-action="media.check"]').click();await waitIdle();await page.locator('[data-action="media.enable"]').click();await waitIdle();assert(config.media.incoming&&config.media.outgoing);assert(await page.locator('[data-action="media.enable"]').isDisabled());ok("File access shows installed scopes and only enables after rechecking grants");
  apply(view,[{type:"navigate",route:"conversations"},{type:"select_conversation",conversation_id:"context-two"}]);await page.locator('[data-control="conversation.detail"]').waitFor();assert((await page.locator('[data-control="conversation.detail"]').innerText()).includes("session-two"));assert.equal(await page.locator("tbody tr").count(),2);await page.locator('[data-control="conversations.search"]').fill("C_FIRST");await page.waitForFunction(()=>document.querySelectorAll("tbody tr").length===1);assert.equal(view.state.filters.conversation_search,"C_FIRST");ok("Conversation contexts remain independently inspectable and searchable");
  apply(view,[{type:"navigate",route:"activity"}]);await page.locator('[data-control="activity.results"]').waitFor();assert(!(await page.locator('[data-control="activity.results"]').innerText()).includes("View Ack"));assert((await page.locator('[data-control="activity.results"]').innerText()).includes("Listened without replying"));await page.locator('[data-control="activity.category"]').selectOption("problems");await page.waitForFunction(()=>document.querySelectorAll(".timeline-item").length===1);assert((await page.locator(".timeline-item").innerText()).includes("needs checking"));await page.locator('[data-action="activity.inspect"]').click();await page.getByRole("dialog").waitFor();await page.keyboard.press("Escape");await page.getByRole("dialog").waitFor({state:"hidden"});await page.screenshot({path:path.join(artifacts,"activity.png"),fullPage:true});ok("Activity highlights participation and delivery problems without display noise");
  apply(view,[{type:"navigate",route:"access"}]);await page.locator('[data-action="token.open"]').click();await page.locator('[data-draft="token_name"]').fill("Observation tool");await page.locator('[data-action="token.create"]').click();await page.locator(".token-value").waitFor();assert.equal(savedTokens[0].scopes[0],"observe");assert.equal(await page.locator(".token-value").innerText(),"one-time-management-secret");assert(!JSON.stringify(view).includes("one-time-management-secret"));await page.locator('[data-action="modal.close"]').click();await page.getByRole("dialog").waitFor({state:"hidden"});assert(!(await page.locator("body").innerText()).includes("one-time-management-secret"));ok("Scoped access token shown once and excluded from display history");
  await page.locator('[data-action="access.advanced"]').click();await page.locator('[data-action="secret.open"]').click();await page.locator('[data-draft="secret_name"]').fill("agent-key");await page.locator('[data-control="secret.value"]').fill("private-agent-value");await page.locator('[data-action="secret.save"]').click();await page.getByRole("dialog").waitFor({state:"hidden"});assert.equal(privateSecrets["agent-key"],"private-agent-value");assert(!JSON.stringify(viewBodies).includes("private-agent-value"));assert(!(await page.locator("body").innerText()).includes("private-agent-value"));await waitIdle();await page.evaluate(()=>window.slackConnector.act("secret.delete",{secretName:"agent-key"}));assert.deepEqual(privateSecrets,{});ok("ACP secrets are write-only and available without agent-specific coupling");
  await waitIdle();await page.evaluate(()=>window.slackConnector.act("lifecycle.stop"));assert.equal(config.enabled,false);await page.evaluate(()=>window.slackConnector.act("credentials.open"));await page.locator('[data-control="credentials.app_token"]').waitFor();await page.locator('[data-action="modal.close"]').click();await waitIdle();await page.evaluate(()=>window.slackConnector.act("lifecycle.start"));assert(config.enabled);ok("Programmatic actions pause and resume the same connection");
  permissions=[{id:"permission-fixture",status:"pending",approver:"U_TEST_OWNER",expires:Date.now()+60000,request:{toolCall:{title:"Write the launch notes",kind:"edit",rawInput:{path:"/workspace/launch.md",content:"Reviewed launch notes"}},options:[{optionId:"yes",name:"Allow once",kind:"allow_once"},{optionId:"no",name:"Deny once",kind:"reject_once"}]}}];
  await page.evaluate(()=>window.slackConnector.act("navigate",{route:"dashboard"}));await page.evaluate(()=>window.slackConnector.act("refresh"));await page.getByRole("button",{name:"Review request"}).click();await page.getByRole("dialog").waitFor();assert((await page.getByRole("dialog").innerText()).includes("Reviewed launch notes"));await page.getByRole("button",{name:"Deny once",exact:true}).click();await page.getByRole("dialog").waitFor({state:"hidden"});assert.equal(permissions[0].status,"denied");ok("Permission review shows concrete tool details and records a deliberate dashboard decision");
  permissions[0].status="pending";await page.evaluate(()=>window.slackConnector.act("refresh"));await page.evaluate(()=>window.slackConnector.act("permission.inspect",{permissionId:"permission-fixture"}));await page.getByRole("dialog").waitFor();await page.evaluate(()=>window.slackConnector.act("permission.decide",{permissionId:"permission-fixture",optionId:"yes"}));assert.equal(permissions[0].status,"approved");ok("Permission view and decision have programmatic action parity");
  const second=await context.newPage();await second.goto(origin+prefix+"?embed=1");await second.waitForFunction(()=>window.slackConnector?.snapshot().view?.id==="view-2");assert.equal(views.size,2);assert.equal(await second.locator(".sidebar").isVisible(),false);assert.equal(await second.evaluate(()=>document.body.classList.contains("embed")),true);await second.close();ok("Independent browser views work behind a URL prefix and in embed mode");
  await page.setViewportSize({width:390,height:844});await page.evaluate(()=>window.slackConnector.act("navigate",{route:"dashboard"}));assert(await page.evaluate(()=>document.documentElement.scrollWidth<=window.innerWidth));await page.screenshot({path:path.join(artifacts,"mobile.png"),fullPage:true});ok("Responsive layout fits a narrow screen");
  // A view acknowledgement can finish after logout, or even after the next
  // successful login. Its old 401 must not clear the new session or its form.
  let releaseAck,ackSeen;
  const heldAck=new Promise(resolve=>{releaseAck=resolve;});
  const ackStarted=new Promise(resolve=>{ackSeen=resolve;});
  let holdAck=true;
  await page.route("**/api/v1/views/*/ack",async intercepted=>{
    if(!holdAck)return intercepted.continue();
    holdAck=false;ackSeen();await heldAck;
    await intercepted.fulfill({status:401,contentType:"application/json",body:JSON.stringify({error:"Old session expired"})});
  });
  await page.evaluate(()=>window.slackConnector.command([{type:"focus",target:"nav.dashboard"}]));
  await ackStarted;
  await page.evaluate(()=>window.slackConnector.act("auth.logout"));
  await page.locator('[data-control="auth.password"]').fill("test-password");
  await page.locator('[data-action="auth.login"]').click();
  await page.locator('[data-control="nav.dashboard"][aria-current="page"]').waitFor();
  await waitIdle();releaseAck();await page.waitForTimeout(150);
  assert.equal(await page.locator('[data-control="auth.password"]').count(),0);
  assert(await page.locator('[data-control="nav.dashboard"]').isVisible());
  ok("Late requests from a logged-out session cannot erase a fresh sign-in");
  assert.deepEqual(errors,[]);assert(!JSON.stringify(viewBodies).includes("test-password"));assert(!JSON.stringify(viewBodies).includes("one-time-management-secret"));assert(!JSON.stringify(viewBodies).includes("private-agent-value"));ok("No browser errors or secrets in semantic view traffic");
  process.stdout.write(`${passed} browser checks passed. Screenshots: ${artifacts}\n`);
}finally{await browser.close();await new Promise(r=>server.close(r));}
