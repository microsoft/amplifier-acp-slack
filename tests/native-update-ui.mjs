import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source=fs.readFileSync(new URL('../ui/app.js',import.meta.url),'utf8');
const plan='11111111-1111-4111-8111-111111111111',request='22222222-2222-4222-8222-222222222222';
const job=()=>({id:plan,request_id:request,state:'succeeded',worker_active:false,worker_manager:{plan_id:plan,request_id:request,read_only:true,terminal_receipt_observed:true,cleanup_last_observed:'pending'}});
function fixture(storage=new Map(),answer){
 const posts=[];const node={addEventListener(){},classList:{add(){}},querySelectorAll(){return []}};
 const context=vm.createContext({URL,URLSearchParams,Map,JSON,crypto:{randomUUID:()=>request},location:{href:'http://127.0.0.1/',hash:'',pathname:'/',search:''},history:{replaceState(){}},document:{baseURI:'http://127.0.0.1/',title:'Slack',getElementById:()=>node,body:node,addEventListener(){}},window:{addEventListener(){}},sessionStorage:{getItem:k=>storage.get(k)||null,setItem:(k,v)=>storage.set(k,v)},fetch:async(url,options)=>{posts.push({url:String(url),options,body:JSON.parse(options.body)});if(answer)return answer(posts.at(-1));return {ok:true,json:async()=>({schema:'com.microsoft.amplifier.slack-native-update-cleanup/1',plan_id:plan,request_id:request,cleanup:'complete',observation_only:posts.at(-1).body.action==='inspect_worker_retirement'})};},setInterval(){},clearInterval(){},setTimeout(){},clearTimeout(){}});
 vm.runInContext(source.replace('  start().catch(fail);','  globalThis.testHooks={app,nativeCleanupState,finishNativeUpdate,nativeControls};'),context);
 const h=context.testHooks;h.app.config={id:'installation',server:{auth_mode:'local_password'}};h.app.session={authenticated:true};h.app.csrf='fixture-csrf';h.app.status={version:'fixture'};h.app.nativeUpdate={supported:true,current:job()};return {h,posts,storage};
}
let checks=0;
{
 const f=fixture();await f.h.finishNativeUpdate();await f.h.finishNativeUpdate();assert.equal(f.posts.length,1);assert.equal(f.posts[0].body.action,'retire_worker');assert.equal(f.posts[0].body.request_id,request);assert.equal(f.posts[0].options.headers['X-CSRF-Token'],'fixture-csrf');assert.equal(f.h.nativeCleanupState(f.h.app.nativeUpdate.current),'complete');assert.match(f.h.nativeControls(),/Update finished/);checks++;
 const reload=fixture(f.storage);await reload.h.finishNativeUpdate();assert.equal(reload.posts.length,0);checks++;
}
{
 const f=fixture(new Map(),async()=>{throw Error('lost reply')});await f.h.finishNativeUpdate();await f.h.finishNativeUpdate();assert.equal(f.posts.length,1);assert.equal(f.h.nativeCleanupState(f.h.app.nativeUpdate.current),'unknown');assert.match(f.h.nativeControls(),/Check update cleanup/);checks++;
 const reload=fixture(f.storage);await reload.h.finishNativeUpdate();assert.equal(reload.posts.length,0);await reload.h.finishNativeUpdate(true);assert.equal(reload.posts.length,1);assert.equal(reload.posts[0].body.action,'inspect_worker_retirement');assert.equal(reload.h.nativeCleanupState(reload.h.app.nativeUpdate.current),'complete');checks++;
}
for(const change of [j=>j.state='recovery_required',j=>j.worker_active=true,j=>j.worker_active=null,j=>j.worker_manager.terminal_receipt_observed=false,j=>j.worker_manager.request_id=plan,j=>j.worker_manager.read_only=false,j=>j.id='../other']){
 const f=fixture();change(f.h.app.nativeUpdate.current);await f.h.finishNativeUpdate();assert.equal(f.posts.length,0);checks++;
}
for(const bad of [{request_id:plan},{plan_id:request},{observation_only:true},{schema:'wrong'},{cleanup:'unknown'}]){
 const f=fixture(new Map(),async()=>({ok:true,json:async()=>({schema:'com.microsoft.amplifier.slack-native-update-cleanup/1',plan_id:plan,request_id:request,cleanup:'complete',observation_only:false,...bad})}));await f.h.finishNativeUpdate();await f.h.finishNativeUpdate();assert.equal(f.posts.length,1);assert.equal(f.h.nativeCleanupState(f.h.app.nativeUpdate.current),'unknown');checks++;
}
{
 const f=fixture();f.h.app.nativeUpdate.current.worker_manager.cleanup_last_observed='unknown';await f.h.finishNativeUpdate();assert.equal(f.posts.length,0);await f.h.finishNativeUpdate(true);assert.equal(f.posts[0].body.action,'inspect_worker_retirement');assert.equal(f.h.nativeCleanupState(f.h.app.nativeUpdate.current),'complete');checks++;
}
{
 const f=fixture();f.h.app.nativeStartup={configured:false,change_supported:false};assert(!f.h.nativeControls().includes('data-action="desktop.startup"'));f.h.app.nativeStartup={configured:false};assert(f.h.nativeControls().includes('data-action="desktop.startup"'));checks++;
}
console.log(`Native update cleanup UI: ${checks} checks passed; exact one continuation, reload/lost reply, readonly reconciliation and unavailable prerequisites.`);
