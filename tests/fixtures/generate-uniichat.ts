// Offline differential fixture generator. Run with Bun and a path to pinned
// VictorTaelin/uniichat 0500bf6. No API keys or live network are used.
import {createHash} from 'node:crypto';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
const hash=(s:string)=>createHash('sha256').update(s).digest('hex');
const digest=(s:string)=>({bytes:Buffer.byteLength(s),utf16:s.length,sha256:hash(s)});
globalThis.fetch = (() => { throw Error('NETWORK DISALLOWED'); }) as any;
const upstream = process.argv[2];
if (!upstream) throw Error('usage: bun generate-uniichat.ts UPSTREAM_DIRECTORY');
const {Memory} = await import(upstream+'/Memory.ts');
const {clip_pages} = await import(upstream+'/Clip.ts');
const records:any[] = [];
let roots:any[]=[]; const persisted=new Map<number,string>();
const launched = new Map<number,string>();
Memory.save=()=>{};
Memory.log=(_m:any, stream:string, recs:any[])=> {
 if(stream==='main') for(const r of recs) roots[r.i]=r;
 else for(const r of recs) persisted.set(Memory.slot(r.l,r.i),r.text);
};
Memory.message=(_m:any,id:number)=>id>=0 && id<_m.count ? roots[id]??null:null;
Memory.node=(m:any,p:any)=>m.nodes.get(Memory.slot(p.l,p.i))??persisted.get(Memory.slot(p.l,p.i))??null;
Memory.job=(m:any,l:number,i:number,_text:string)=>{
 const key=Memory.slot(l,i);m.busy.set(key,0);
 const upto=Math.min(Memory.ctx(l,i),Memory.first(m));
 launched.set(key,Memory.view(m,upto,m.seen.parts).map((b:any)=>b.text).join(''));
};
function init(high=128000){roots=[];persisted.clear();launched.clear();return {
 count:0,open:0,nodes:new Map(),ready:[[],[]],busy:new Map(),sides:new Map(),calls:new Map(),
 hold:[null,'paused'],view:{parts:[],size:0,batch:false,high},seen:{parts:[],size:0,batch:false,high:32000},
 wait:new Set(),done:false
} as any;}
function state(m:any,label:string) {
 const refs=(v:any)=>v.parts.map(Memory.ref);
 const busy=[...m.busy.keys()].map((key:any)=> {
  let l=0;let n=key+1;while(n%2===0){l++;n/=2}return {l,i:(n-1)/2};
 }).sort((a:any,b:any)=>Memory.ctx(a.l,a.i)-Memory.ctx(b.l,b.i)||b.l-a.l);
 return {label,count:m.count,open:m.open,view:refs(m.view),viewBatch:m.view.batch,viewSize:m.view.size,
 seen:refs(m.seen),seenBatch:m.seen.batch,seenSize:m.seen.size,
 jobs:busy.map((p:any)=>({span:Memory.ref(p),context:digest(launched.get(Memory.slot(p.l,p.i))!)})),
 rendered:digest(Memory.view(m).map((b:any)=>b.text).join(''))};
}
let m=init(4096);const events:any[]=[];
function push(text:string,more=false){Memory.push(m,[{kind:'user',text,more}]);}
function finish(l:number,i:number,text:string) {Memory.store(m,[{l,i,text}]);m.busy.delete(Memory.slot(l,i));launched.delete(Memory.slot(l,i));Memory.pump(m);Memory.fit(m);}
// >64 leaves expose launch window and arbitrary completion order.
for(let k=0;k<70;k++) {push('L'+k+':'+ 'x'.repeat(900)); if([0,1,63,64,69].includes(k))events.push(state(m,'append-'+k));}
for(let i=63;i>=0;i--){finish(0,i,'s'.repeat(512));if([63,1,0].includes(i))events.push(state(m,'finish-'+i));}
for(let i=64;i<70;i++)finish(0,i,'s'.repeat(512));
events.push(state(m,'all-leaves'));
// Finish available parents newest-first in waves, retaining exact gating.
for(let wave=0;wave<8;wave++){
 const busy=[...m.busy.keys()];if(!busy.length)break;
 const ps=busy.map((key:any)=>{let l=0,n=key+1;while(n%2===0){l++;n/=2}return {l,i:(n-1)/2};}).sort((a:any,b:any)=>b.i-a.i);
 for(const p of ps)finish(p.l,p.i,'s'.repeat(512));
 events.push(state(m,'wave-'+wave));
}
records.push({name:'scheduler-gating',events});
// Source-fit chains and grouping; fits never wait on a timer.
m=init();const short:any[]=[];
for(let k=0;k<20;k++){push('short '+k,k<2);short.push(state(m,'short-'+k));}
records.push({name:'free-and-open',events:short});
const texts=['','a'.repeat(30000),'a'.repeat(30001),'\n'+'a'.repeat(30000),'a'.repeat(29999)+'\nZ','a'.repeat(29999)+'😀Z','é'.repeat(30001),'e\u0301'.repeat(15001)];
records.push({name:'pages',inputs:texts.map((text,index)=>({index,pages:clip_pages(text).map(digest)}))});
// Image alignment goes through actual Memory.pages.
records.push({name:'images',inputs:[0,20,21,40,41].flatMap(n=>['short','z'.repeat(30001)].map(text=> {
 const images=Array.from({length:n},(_,i)=>'img'+i);return {long:text.length>100,images,pages:Memory.pages({kind:'user',text,images}).map((p:any)=>({text:digest(p.text),images:p.images,more:p.more===true}))};
}))});
records.push({name:'flat',inputs:[' a  \n   b ','a\r b','a\ufeff\n\ufeffb','a\u0085\n\u0085b','a\n\n b'].map(text=>({text,flat:Memory.flat(text),trim:text.trim()}))});
// Use actual Memory.load's unfinished-tail/open initialization, faking only
// disk indexing/storage as above. This catches restart-vs-uninterrupted push.
const restart:any[]=[];
for(const matching of [false,true]) {
 m=init();push('unfinished A',true);
 const events=[state(m,'open-A')];
 const dir=fs.mkdtempSync(path.join(os.tmpdir(),'uniichat-restart-'));
 const oldIndex=Memory.index,oldSaved=Memory.saved;
 Memory.index=(_m:any,stream:string)=>{
  fs.writeFileSync(dir+'/'+stream+'.index',Buffer.alloc(stream==='main'?roots.length*16:0));return [0,0];
 };
 Memory.saved=()=>null;
 try {m=Memory.load(dir,()=>{throw Error('NETWORK DISALLOWED');});}
 finally {Memory.index=oldIndex;Memory.saved=oldSaved;fs.rmSync(dir,{recursive:true,force:true});}
 events.push(state(m,'reopen'));
 push(matching?'unfinished A':'unrelated B');
 events.push(state(m,'close'));
 restart.push({matching,events,text:Array.from({length:m.count},(_,i)=>Memory.message(m,i).text)});
}
records.push({name:'restart-open-group',inputs:restart});

// Deterministic arbitrary appends/completions exercise fitting, free chains,
// frozen compactor contexts and the 64-leaf frontier across mixed source sizes.
const randomized:any[]=[];
for(const [seed,high] of [[17,4096],[501,8192],[97001,128000]]) {
 let value=seed;const rand=()=>{value^=value<<13;value^=value>>>17;value^=value<<5;return value>>>0};
 m=init(high);const operations:any[]=[];const events:any[]=[];
 const observe=()=>{if(operations.length%16===0)events.push(state(m,String(operations.length)))};
 const complete=()=>{
  const keys=[...m.busy.keys()];const key=keys[rand()%keys.length];let l=0,n=key+1;while(n%2===0){l++;n/=2}const i=(n-1)/2;
  const text='S'+operations.length+':'+(rand()%2?'é':'s').repeat([1,80,250,512,540][rand()%5]);
  finish(l,i,text);operations.push({kind:'finish',l,i,text});observe();
 };
 for(let step=0;step<192;step++) {
  if(m.busy.size && rand()%3===0){complete();continue;}
  const text='P'+step+':'+(rand()%3?'x':'😀').repeat([0,1,128,507,513,900,2000][rand()%7]);
  const more=rand()%13===0;
  push(text,more);operations.push({kind:'push',text,more});observe();
 }
 push('close',false);operations.push({kind:'push',text:'close',more:false});observe();
 while(m.busy.size){complete();if(operations.length>2000)throw Error('oracle did not settle');}
 events.push(state(m,'final'));
 randomized.push({seed,high,operations,events});
}
records.push({name:'randomized-behavior',cases:randomized});
console.log(JSON.stringify(records));
process.exit(0);
