// Run after building app-lb. PLAYWRIGHT_MODULE may name an installed Playwright module.
// Real dashboard rendering with asymmetric API fixtures. Rust tests cover credential
// selection and observation transport independently; this is not a live fleet proof.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');
const https = require('node:https');
const {spawn, execFileSync} = require('node:child_process');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'heyo-unified-dashboard-'));
const binary = path.resolve(__dirname, '../target/debug/app-lb');
const children = [], servers = [];
const sleep = ms => new Promise(r => setTimeout(r, ms));
const listen = s => new Promise(r => s.listen(0, '127.0.0.1', () => r(s.address().port)));
const basic = 'Basic ' + Buffer.from('admin:fixture-password').toString('base64');
let browser, unavailable = false;
const inventory = {services:[{serviceId:'shared-app', desiredReplicas:2, replicaRegions:['US','eu1'],
  discoveryVersion:7, endpoints:[{region:'US',revision:'fixture-r1',healthStatus:'healthy',draining:false}],rollout:null}],nextCursor:null};
(async () => {
  execFileSync('openssl', ['req','-x509','-newkey','rsa:2048','-nodes','-days','1','-keyout',root+'/key',
    '-out',root+'/cert','-subj','/CN=localhost','-addext','subjectAltName=DNS:localhost'], {stdio:'ignore'});
  const auth = http.createServer((req,res) => {
    res.setHeader('Content-Type','application/json');
    if(req.headers.authorization !== 'Bearer fixture-admin') {res.statusCode=401;res.end('{}');return;}
    res.end(JSON.stringify({success:true,data:{subject:{userId:'fixture',email:'admin@example.invalid',platformRole:'admin'},scopes:['fleet:admin'],expiresIn:3600}}));
  });
  servers.push(auth); const authPort = await listen(auth);
  const urls = [], adminUrls = [];
  for (let i=0;i<2;i++) {
    const reservation=http.createServer();const port=await listen(reservation);
    await new Promise(r=>reservation.close(r));
    const dir=root+'/'+i;fs.mkdirSync(dir);
    const log=fs.openSync(dir+'/log','w');
    const child=spawn(binary,[],{cwd:dir,env:{...process.env,SSL_CERT_FILE:root+'/cert',
      APP_LB_ADMIN_ADDR:'127.0.0.1:'+port,APP_LB_PROXY_ADDR:'127.0.0.1:0',APP_LB_DAEMON_URL:'http://127.0.0.1:9',
      APP_LB_ADMIN_AUTH:'1',APP_LB_DASHBOARD_AUTH:'1',APP_LB_DASHBOARD_USER:'admin',APP_LB_DASHBOARD_PASSWORD:'fixture-password',
      APP_LB_AUTH_URL:'http://127.0.0.1:'+authPort,APP_LB_SIEM:'0',APP_LB_DISK_TTL_SECS:'0',
      APP_LB_MOUNTS_DIR:dir+'/mounts',APP_LB_WORKSPACES_DIR:dir+'/workspaces',APP_LB_IMAGES_DIR:dir+'/images',APP_LB_BUILD_DIR:dir+'/build'},stdio:['ignore',log,log]});
    children.push(child);fs.closeSync(log);adminUrls.push('http://127.0.0.1:'+port);
    let ready=false;
    for(let n=0;n<100;n++){try{ready=(await fetch(adminUrls[i]+'/healthz')).ok;if(ready)break;}catch{}await sleep(100);}
    assert(ready,fs.readFileSync(dir+'/log','utf8'));
    const proxy=https.createServer({key:fs.readFileSync(root+'/key'),cert:fs.readFileSync(root+'/cert')},(req,res)=>{
      if(i===1 && unavailable){res.statusCode=503;res.end('{}');return;}
      if(req.url==='/orchestration/services'){
        assert.equal(req.headers.authorization,'Bearer authority-token');res.setHeader('Content-Type','application/json');res.end(JSON.stringify(inventory));return;
      }
      const up=http.request({hostname:'127.0.0.1',port,path:req.url,method:req.method,headers:req.headers},r=>{res.writeHead(r.statusCode,r.headers);r.pipe(res)});
      up.on('error',()=>{res.statusCode=502;res.end()});req.pipe(up);
    });
    servers.push(proxy);urls.push('https://localhost:'+await listen(proxy));
  }
  async function api(i,endpoint,data,authorization=basic) {
    const r=await fetch(adminUrls[i]+endpoint,{method:data?'POST':'GET',headers:{Authorization:authorization,'Content-Type':'application/json'},body:data?JSON.stringify(data):undefined});
    assert(r.ok,endpoint+': '+r.status);return r.json();
  }
  const gateways=urls.map((url,i)=>({id:['us3','eu1'][i],region:['US','eu1'][i],url,use_caller_auth:true}));
  for(let i=0;i<2;i++) {
    await api(i,'/secrets',{id:'authority',data:{token:'authority-token'}});
    const config={gateways,control_plane:[{id:'authority',region:'US',url:urls[0],auth:{secret:'authority',key:'token'}}]};
    const r=await fetch(adminUrls[i]+'/control-plane/config',{method:'PUT',headers:{Authorization:basic,'Content-Type':'application/json'},body:JSON.stringify({expected_revision:0,config})});assert.equal(r.status,200);
    for(let n=0;n<[2,5][i];n++)await api(i,'/deployments',{id:'fixture-'+n,routes:[{host:'fixture-'+n+'.invalid'}],upstreams:['127.0.0.1:9']});
    const denied=await api(i,'/fleet');assert(denied.gateways.every(g=>g.error==='Heyo sign-in required for regional observations'));
    const local=await api(i,'/tokens',{name:'local-view',admin:'view',deployments:['*']});
    const localDenied=await api(i,'/fleet',undefined,'Bearer '+local.token);assert(localDenied.gateways.every(g=>g.error==='Heyo sign-in required for regional observations'));
  }
  browser=await chromium.launch({headless:true});
  const c=await browser.newContext({ignoreHTTPSErrors:true,viewport:{width:1280,height:900}});
  await c.route('**/services',r=>r.fulfill({json:{configured:true,inventory}}));
  await c.route('**/fleet',r=>r.fulfill({json:{configured:true,gateways:urls.map((url,i)=>({
    id:['us3','eu1'][i],region:['US','eu1'][i],dashboard_url:url+'/dashboard?view=local',
    error:i===1&&unavailable?'gateway unavailable':null,
    metrics:i===1&&unavailable?null:{generated_at:1790000000,fleet:{deployments:[2,5][i],ready:[1,3][i],draining:0,pending:0,total_in_flight:[7,11][i]}}
  }))}}));
  await c.addCookies([{name:'__Host-heyo-admin',value:'fixture-admin',domain:'localhost',path:'/',secure:true,httpOnly:true,sameSite:'Strict'}]);
  let expected;
  for(let i=0;i<2;i++){
    const p=await c.newPage(), requested=[];
    p.on('request',r=>requested.push(new URL(r.url()).pathname));
    await p.goto(urls[i]+'/dashboard');
    await p.getByRole('link',{name:'Open eu1 gateway details'}).waitFor();
    await p.locator('#global-services').getByRole('heading',{name:'shared-app'}).waitFor();
    assert.equal(await p.locator('#local-view').isVisible(),false);
    assert.equal(await p.getByRole('link',{name:'Security',exact:true}).isVisible(),false);
    assert(!requested.some(x=>['/metrics','/secrets','/tokens','/jobs'].includes(x)),JSON.stringify(requested));
    const content=await p.locator('#global-services').innerText();if(expected)assert.equal(content,expected);else expected=content;
    const regional=await p.locator('#regional-fleet').innerText();assert(regional.includes('Deployments: 2')&&regional.includes('Deployments: 5'));
    if(i===0 && process.env.SCREENSHOT_DIR)await p.screenshot({path:path.join(process.env.SCREENSHOT_DIR,'unified-dashboard-desktop.png')});
    if(i===1){await p.setViewportSize({width:390,height:844});assert(await p.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));if(process.env.SCREENSHOT_DIR)await p.screenshot({path:path.join(process.env.SCREENSHOT_DIR,'unified-dashboard-mobile.png')});}
    await p.goto(urls[i]+'/dashboard?view=local');assert(await p.locator('#local-view').isVisible());assert(await p.getByRole('link',{name:'Security',exact:true}).isVisible());
    await p.waitForFunction(()=>document.getElementById('fleet-tiles').textContent.trim().length>0);
    if(i===0 && process.env.SCREENSHOT_DIR)await p.screenshot({path:path.join(process.env.SCREENSHOT_DIR,'unified-dashboard-local.png')});
    await p.close();
  }
  unavailable=true;
  const p=await c.newPage();await p.goto(urls[0]+'/dashboard');await p.getByText('gateway unavailable',{exact:false}).waitFor();
  const fleet=await p.locator('#regional-fleet').innerText();assert(fleet.includes('Deployments: 2')&&!fleet.includes('Deployments: 5'));
  if(process.env.SCREENSHOT_DIR)await p.screenshot({path:path.join(process.env.SCREENSHOT_DIR,'unified-dashboard-unavailable.png')});
  console.log('PASS dashboard rendering: identical global content, asymmetric regional counts, explicit local view, missing-region state, no local polls. Observation API responses are fixtures.');
})().catch(e=>{console.error(e);process.exitCode=1}).finally(async()=>{
  if(browser)await browser.close();
  for(const p of children)p.kill('SIGKILL');
  await Promise.all(children.map(p=>new Promise(r=>{if(p.exitCode!==null)return r();p.once('exit',r)})));
  for(const s of servers){s.closeAllConnections();s.close();}
  fs.rmSync(root,{recursive:true,force:true});
});
