import test from 'node:test';
import assert from 'node:assert/strict';
import {RequestQueue,payloadBytes} from './request-queue.js';
test('one physical request; bounded admission; active cancellation retains slot', async()=>{
  const sent=[], q=new RequestQueue(m=>sent.push(m),{maxRequests:2,maxBytes:1000});
  const c=new AbortController(), a=q.call('query',{cypher:'RETURN 1'},{signal:c.signal});
  const rejected=assert.rejects(a,e=>e.name==='AbortError' && e.executionMayHaveCompleted===true);
  const b=q.call('query',{cypher:'RETURN 2'});
  await assert.rejects(q.call('query',{}),/full/);
  c.abort(); await rejected;
  assert.equal(sent.length,1); assert.equal(q.stats.active,1);
  q.reply({id:sent[0].id,ok:true,result:1}); assert.equal(sent.length,2);
  q.reply({id:sent[1].id,ok:true,result:2}); assert.equal(await b,2); assert.equal(q.stats.bytes,0);
});
test('queued cancellation never dispatches; close cleans waiters and refuses new calls', async()=>{
  const sent=[],q=new RequestQueue(m=>sent.push(m)), c=new AbortController();
  const a=q.call('init',{}), b=q.call('query',{}, {signal:c.signal});
  const rejected=assert.rejects(b,e=>e.name==='AbortError' && e.executionMayHaveCompleted===false);
  c.abort(); await rejected; assert.equal(q.stats.queued,0);
  const stopped=assert.rejects(a,{name:'AbortError'}); q.close(); await stopped;
  assert.equal(q.stats.bytes,0); assert.equal(sent.length,1);
  await assert.rejects(q.call('query',{}),{name:'InvalidStateError'});
});
test('byte admission and clone failures leave the queue usable', async()=>{
  const q=new RequestQueue(()=>{throw new Error('cannot clone');},{maxBytes:1000});
  await assert.rejects(q.call('load',{bytes:new Uint8Array(2000)}),/full/);
  await assert.rejects(q.call('query',{}),/cannot clone/);
  assert.equal(q.stats.bytes,0); assert.equal(q.stats.active,0);
  const bytes=new Uint8Array(10); assert.ok(payloadBytes({a:bytes,b:bytes})<200);
});
test('queued payload is a bounded call-time snapshot; stale replies do not release it', async()=>{
  const sent=[],q=new RequestQueue(m=>sent.push(m));
  const a=q.call('init',{}), params={text:'original'};
  const b=q.call('query',params); params.text='changed';
  q.reply({id:999,ok:true,result:null}); assert.equal(q.stats.queued,1);
  q.reply({id:sent[0].id,ok:true,result:0}); await a;
  assert.equal(sent[1].payload.text,'original');
  q.reply({id:sent[1].id,ok:true,result:1}); assert.equal(await b,1);
  assert.throws(()=>payloadBytes(new Map([['x','large']])) ,/JSON-like/);
  assert.throws(()=>payloadBytes(Array.from({length:100001},()=>0)),/budget/);
});
