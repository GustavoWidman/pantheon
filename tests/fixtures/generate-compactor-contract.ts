// Real pinned upstream differential oracle. Every model/transport call is fake.
import * as fs from 'node:fs';
import * as os from 'node:os';
const isolatedConfig=fs.mkdtempSync(os.tmpdir()+'/pantheon-upstream-contract-');
process.env.UNII_DIR=isolatedConfig;
for(const k of ['ANTHROPIC_API_KEY','OPENAI_API_KEY','RESEND_API_KEY','APNS_KEY']) delete process.env[k];
import {execFileSync} from 'node:child_process';
const root=process.argv[2];
const pin='0500bf6a8ea4bf0ce047488562022b1b6532cc30';
if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin) throw Error('wrong upstream pin');
globalThis.fetch=(()=>{throw Error('NETWORK DISALLOWED')}) as any;
const {Memory}=await import(root+'/Memory.ts');
const {OpenAI}=await import(root+'/OpenAI.ts');
const {Anthropic}=await import(root+'/Anthropic.ts');
const context='<chat>\n0+1|context\n</chat>\n';
const source='source 🙂 '.repeat(100);
const outcomes=[
 {name:'five-shortest-first-tie',answers:['a'.repeat(553),'é'.repeat(270),'b'.repeat(540),'c'.repeat(580),'d'.repeat(570)].map(text=>({ok:true,text,left:[]}))},
 {name:'fit512',answers:[{ok:true,text:'é'.repeat(256),left:[]}]},
 {name:'empty-first',answers:[{ok:true,text:' \ufeff\n',left:[]}]},
 {name:'fail-first',answers:[{ok:false,text:'refusal',left:[],stop:'fail'}]},
 {name:'best-before-fail',answers:[{ok:true,text:'x'.repeat(553),left:[]},{ok:false,text:'refusal',left:[],stop:'fail'}]},
 {name:'best-before-empty',answers:[{ok:true,text:'x'.repeat(553),left:[]},{ok:true,text:'',left:[]}]},
 {name:'trim-nel',answers:[{ok:true,text:'\ufeff \u0085reply \ufeff',left:[]}]},
];
const records:any[]=[];
// Replace only dispatch and persistence; invoke real build/task/view/over/store selection.
const old={ask:Memory.ask,compact:Memory.compact,store:Memory.store};
for(const test of outcomes){
 const notes:any[]=[]; let selected=''; let at=0;
 const mem:any={count:1,done:false,seen:{parts:[]},nodes:new Map(),ready:[[],[]],sides:new Map(),hold:[null,'paused']};
 // An actually-built context leaf before target1.
 mem.seen.parts=[{l:0,i:0}];mem.nodes.set(Memory.slot(0,0),'context');
 Memory.ask=(_m:any,_l:number,_i:number,_text:string,blocks:any[])=>{notes.push(structuredClone(blocks));return {done:Promise.resolve(test.answers[at++])} as any};
 Memory.compact=(_m:any,_l:number,_i:number,call:any)=>call.done;
 Memory.store=(_m:any,nodes:any[])=>{selected=nodes[0].text};
 await Memory.build(mem,0,1,source,null);
 records.push({name:test.name,answers:test.answers,selected,blocks:notes});
}
Object.assign(Memory,old);
const task=Memory.task(0,1)+source+'\n</input>';
const mem:any={count:1,nodes:new Map([[Memory.slot(0,0),'context']]),view:{parts:[{l:0,i:0}]}};
const blocks=[...Memory.view(mem),{text:task,mark:false}];
const ask:any={vendor:'openai-api',model:'gpt-6-fixture',effort:'medium',speed:'slow',system:'synthetic system',searches:0,server:'',blocks};
const items=[{$:'User',parts:blocks,imgs:[]}];
const bodies={openai:OpenAI.body(ask,items,[],128000),anthropic:Anthropic.body({...ask,vendor:'claude-api',model:'fixture'},items,[],128000)};
// Actual native raw-suffix policy, including text/image limits and last-page rule.
const suffixes:any[]=[];
const oldMessage=Memory.message,oldImages=Memory.images;Memory.images=()=>[];
for(const spec of [
 {name:'batch-two-large',texts:['a'.repeat(100000),'b'.repeat(100000)],images:[0,0]},
 {name:'exact-boundary-plus-label',texts:['a'.repeat(30000),'b'.repeat(30000),'c'.repeat(30000),'d'.repeat(30000),'e'.repeat(7990)],images:[0,0,0,0,0]},
 {name:'image-limit',texts:['first','second'],images:[20,1]},
 {name:'last-page-always',texts:['huge last'],images:[21]},
]) {
 let roots:any[]=[];const groups:any[]=[];
 for(let n=0;n<spec.texts.length;n++) {
  const start=roots.length;
  const pages=Memory.pages({kind:'user',text:spec.texts[n],images:Array.from({length:spec.images[n]},(_,i)=>'image'+i)});
  roots.push(...pages.map((p:any,k:number)=>Memory.root(start+k,p)));
  groups.push({start,end:roots.length});
 }
 const m:any={count:roots.length};Memory.message=(_m:any,id:number)=>roots[id]??null;
 const from=Memory.whole(m,0);
 const texts=groups.filter(g=>g.end>from).map(g=>Memory.say(m,Math.max(g.start,from),g.end).text);
 suffixes.push({...spec,from,pages:roots.length,raw:texts});
}
Memory.message=oldMessage;Memory.images=oldImages;
const streamEvents=[
 {type:'message_start',message:{id:'fixture',role:'assistant',content:[],usage:{input_tokens:12,output_tokens:1}}},
 {type:'content_block_start',index:0,content_block:{type:'thinking',thinking:'',signature:''}},
 {type:'content_block_delta',index:0,delta:{type:'thinking_delta',thinking:'private thought'}},
 {type:'content_block_delta',index:0,delta:{type:'signature_delta',signature:'partial signature'}},
 {type:'content_block_delta',index:0,delta:{type:'signature_delta',signature:'opaque signature'}},
 {type:'content_block_stop',index:0},
 {type:'content_block_start',index:1,content_block:{type:'text',text:''}},
 {type:'content_block_delta',index:1,delta:{type:'text_delta',text:'é🙂 answer'}},
 {type:'content_block_delta',index:1,delta:{type:'citations_delta',citation:{type:'page_location',document_index:0,start_page_number:1,end_page_number:2}}},
 {type:'content_block_stop',index:1},
 {type:'content_block_start',index:2,content_block:{type:'text',text:''}},
 {type:'content_block_stop',index:2},
 {type:'content_block_start',index:3,content_block:{type:'fixture_unknown',input:null}},
 {type:'content_block_stop',index:3},
 {type:'message_delta',delta:{stop_reason:'pause_turn'},usage:{output_tokens:17}},
 {type:'message_stop'},
];
// Stream input assembly occurs at step, even if a block-stop event was absent.
for(const [k,input] of ['', '{not-json', 'null', '{"q":42}'].entries()) {
 streamEvents.splice(streamEvents.length-2,0,
  {type:'content_block_start',index:4+k,content_block:{type:'server_tool_use',id:'native'+k,name:'fixture',input:{}}},
  {type:'content_block_delta',index:4+k,delta:{type:'input_json_delta',partial_json:input}},
  ...(k===1?[]:[{type:'content_block_stop',index:4+k}])
 );
}
const state=Anthropic.wire.start(ask);for(const event of streamEvents)Anthropic.read(state,event,()=>{});
const step=Anthropic.step(state,ask);
const stream={events:streamEvents,out:step.out,entries:step.entries,stop:step.stop};
const pauseJson=[{type:'text',text:''},{type:'thinking',thinking:'private',signature:'sig',input:null},{type:'text',text:'intermediate'}];
const pauseState=Anthropic.wire.start(ask);pauseState.blocks=structuredClone(pauseJson);pauseState.stop='pause_turn';
const pauseNative=Anthropic.step(pauseState,ask).out;
fs.rmSync(isolatedConfig,{recursive:true,force:true});
console.log(JSON.stringify({upstream:pin,context,source,records,bodies,suffixes,stream,pauseJson,pauseNative},null,2));
