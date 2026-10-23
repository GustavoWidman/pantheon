// Differential oracle: execute the pinned Chat.call and Memory functions.
// Only network dispatch, UI notifications and remote summary answers are faked.
import * as fs from 'node:fs';
import * as os from 'node:os';
import {execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
const pin='0500bf6a8ea4bf0ce047488562022b1b6532cc30';
const upstream=process.argv[2];
if(execFileSync('git',['-C',upstream,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('wrong pin');
execFileSync('git',['-C',upstream,'diff','--exit-code','HEAD','--'],{stdio:'pipe'});
const temp=fs.mkdtempSync(os.tmpdir()+'/pantheon-full-memory-oracle-');
process.env.UNII_DIR=temp;process.env.TZ='UTC';
for(const k of ['ANTHROPIC_API_KEY','OPENAI_API_KEY','RESEND_API_KEY','APNS_KEY'])delete process.env[k];
globalThis.fetch=(()=>{throw Error('NETWORK DISALLOWED')}) as any;
const {clip_page}=await import(upstream+'/Clip.ts');
const {Memory}=await import(upstream+'/Memory.ts');
const {Disk}=await import(upstream+'/Disk.ts');
const {AskAI}=await import(upstream+'/AskAI.ts');
const {Chat}=await import(upstream+'/Server.ts');
const {MASTER,Tools}=await import(upstream+'/Tools.ts');
const config=await import(upstream+'/Config.ts');
Disk.wrote=async()=>{};
const digest=(text:string)=>({bytes:Buffer.byteLength(text),sha256:createHash('sha256').update(text).digest('hex')});
const scheduled:any[]=[];
Memory.job=(m:any,l:number,i:number,text:string)=>{m.busy.set(Memory.slot(l,i),0);scheduled.push({m,l,i,text})};
function drain(){while(scheduled.length){const {m,l,i}=scheduled.shift();Memory.store(m,[{l,i,text:`summary ${l}/${i}`}]);m.busy.delete(Memory.slot(l,i));Memory.pump(m);Memory.fit(m);}}
Chat.change=()=>{};Chat.ended=async()=>false;
let captured:any=null;
AskAI.ask=(ask:any)=>{captured=structuredClone(ask);return {done:Promise.resolve({ok:true,text:'fixture',left:[]}),send:()=>{},stop:()=>{}} as any};
const turns:any[]=[];
for(const texts of [['inspect brief.zip'],['a'.repeat(100000),'b'.repeat(100000)]]){
 const dir=temp+'/case-'+turns.length;fs.mkdirSync(dir);
 const mem=Memory.load(dir,()=>{throw Error('remote jobs disabled')});const groups:any[]=[];
 Memory.push(mem,[{kind:'user',text:'[channel:1 user:2]\ndo work'}]);
 for(const text of texts){const roots=Memory.push(mem,[{kind:'user',text:'[channel:1 user:2]\n'+text}]);groups.push({id:roots[0].i,end:roots.at(-1)!.i+1});}
 drain();
 const chat:any={mem,wait:groups,tools:{devices:new Map(),agents:new Map(),url:'https://synthetic.invalid/mcp/'},socks:new Set(),pick:config.PICK};
 const from=Chat.from(chat);const view=Memory.view(mem,from).map((b:any)=>b.text).join('');
 await Chat.call(chat);
 const task=captured.blocks.at(-1);
 turns.push({lengths:texts.map(s=>s.length),from,view:digest(view),task:{...digest(task.text),mark:task.mark,prefix:task.text.slice(0,30)}});
 Memory.stop(mem);
}
const dir=temp+'/queries';fs.mkdirSync(dir);const mem=Memory.load(dir,()=>{throw Error('remote jobs disabled')});
Memory.push(mem,[{kind:'user',text:'hello',date:'2026-01-02T03:04:05.678Z'},{kind:'unii',text:'reply',date:'2026-01-03T04:05:06.789Z'}]);drain();
const queries=[{id:0,n:1,at:0},{id:0,n:2,at:0},{id:1,n:2,at:0},{id:2,n:1,at:0},{id:0,n:3,at:0}].map(q=>({...q,result:Memory.zoom(mem,q.id,q.n,q.at)}));
const dates=[0,1,2].map(id=>({id,result:Memory.date(mem,id)}));Memory.stop(mem);
const workers:any[]=[];
const workerDir=temp+'/workers';fs.mkdirSync(workerDir);const workerMem=Memory.load(workerDir,()=>{throw Error('remote jobs disabled')});
const name=Memory.agent_new(workerMem);
const entries=[{kind:'note',text:'subagent'},{kind:'user',text:'inspect archive'},{kind:'tool',text:'read {"path":"brief.txt"}'},{kind:'echo',text:'line\n'+'🙂'.repeat(17000)},{kind:'unii',text:'first report'},{kind:'user',text:'now check the appendix'}];
for(const e of entries)Memory.agent_push(workerMem,name,e.kind as any,e.text);
const agent:any={name,role:{$:'sub',effort:null},view:[{text:'<chat>\n0+1|user: frozen\n</chat>\n',mark:true}],call:null,live:[],calls:{},stop:false,replies:[]};
const tools:any={mem:workerMem,devices:new Map(),agents:new Map([[name,agent]]),url:'https://synthetic.invalid/mcp/',hooks:{pick:()=>config.PICK}};
const hot=Tools.ask(tools,agent);agent.view=[];const cold=Tools.ask(tools,agent);
const workerLog=Memory.agent_show(workerMem,name);
workers.push({entries,log:workerLog,hot:hot.blocks.filter((b:any)=>!b.text.startsWith('Devices open now:')),cold:cold.blocks.filter((b:any)=>!b.text.startsWith('Devices open now:')),system:hot.system.split('\n\nDevices:')[0],pages:[0,30000,workerLog.length,workerLog.length+1].map(at=>({at,json:JSON.stringify({text:clip_page(workerLog,at)})}))});
Memory.stop(workerMem);
const edgeDir=temp+'/edges';fs.mkdirSync(edgeDir);const edge=Memory.load(edgeDir,()=>{throw Error('remote jobs disabled')});
Memory.push(edge,[{kind:'user',text:'🙂Z'}]);
const utf16=[0,1,2,3,4,999].map(at=>({at,units:Array.from({length:Memory.zoom(edge,0,1,at).length},(_,i)=>Memory.zoom(edge,0,1,at).charCodeAt(i))}));
const echoUnits=(()=>{const text=Memory.root(1,{kind:'echo',text:Memory.zoom(edge,0,1,1)}).text;return Array.from({length:text.length},(_,i)=>text.charCodeAt(i));})();
const atWork=[[],['Iris'],['Iris','Atlas']].map(names=>({names,text:Tools.at_work({agents:new Map(names.map(name=>[name,{name,call:{}}]))} as any).text}));
const work=Memory.show(Memory.root(0,{kind:'work',text:'[Worker] found evidence'}));Memory.stop(edge);
console.log(JSON.stringify({upstream:pin,master:MASTER,turns,queries,dates,workers,utf16,work,echoUnits,atWork},null,2));
fs.rmSync(temp,{recursive:true,force:true});process.exit(0);
