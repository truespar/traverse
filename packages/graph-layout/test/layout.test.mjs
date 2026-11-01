import test from 'node:test';
import assert from 'node:assert/strict';
import { layout, validate } from '../engine.js';
import { coarsen, refine } from '../hierarchy.js';
import { separate } from '../collision.js';
import { LayoutController } from '../index.js';
import Graph from 'graphology';
import forceAtlas2 from 'graphology-layout-forceatlas2';
import { createFa2Solver, settingsFor } from '../fa2.js';
import { packComponents } from '../components.js';

function fixture(n, connected = true) {
  return {positions:new Float32Array(n*2), radii:new Float32Array(n).fill(5), fixed:new Uint8Array(n),
    edges:connected ? Uint32Array.from({length:Math.max(0,n-1)*2}, (_,i) => Math.floor(i/2)+i%2) : new Uint32Array(),
    weights:new Float32Array(connected ? Math.max(0,n-1) : 0).fill(1)};
}
test('persistent FA2 adapter matches public solver across 20 iterations', () => {
  const f = fixture(30), graph = new Graph({multi:true,type:'undirected'});
  for (let i = 0; i < 30; i++) {
    f.positions[2*i] = i*20; f.positions[2*i+1] = Math.sin(i)*15;
    graph.addNode(i,{x:f.positions[2*i],y:f.positions[2*i+1],size:5,fixed:i===0});
  }
  f.fixed[0] = 1;
  for (let e = 0; e < f.weights.length; e++) {
    f.weights[e] = e%3+.5;
    graph.addEdge(f.edges[2*e],f.edges[2*e+1],{weight:f.weights[e]});
  }
  const step = createFa2Solver(f);
  for (let i = 0; i < 20; i++) step();
  forceAtlas2.assign(graph,{iterations:20,settings:settingsFor(30)});
  graph.forEachNode((id,a) => {
    assert.equal(f.positions[2*Number(id)],a.x);
    assert.equal(f.positions[2*Number(id)+1],a.y);
  });
});
test('dense coincidence uses bounded work and never stores all pairs', () => {
  for (const n of [128, 1000, 50000]) {
    const f = fixture(n, false);
    const stats = separate(f.positions,f.radii,f.fixed);
    assert.equal(stats.crowdedNodes,0);
    assert.equal(stats.overlapsBeforeLastPass,0);
    assert.ok(stats.candidateChecks <= n*288*8);
    assert.equal(new Set(Array.from({length:n}, (_,i) => `${f.positions[2*i]},${f.positions[2*i+1]}`)).size,n);
  }
});
test('fixed overlaps are reported, not moved', () => {
  const f = fixture(2); f.fixed.fill(1);
  const stats = separate(f.positions,f.radii,f.fixed);
  assert.deepEqual([...f.positions],[0,0,0,0]);
  assert.equal(stats.overlapsBeforeLastPass,1);
});
test('component packing preserves internal geometry and separates boxes', () => {
  const f = fixture(4,false); f.edges = new Uint32Array([0,1,2,3]);
  f.positions.set([0,0,20,0,0,0,20,0]);
  assert.equal(packComponents(f),2);
  for (const [a,b] of [[0,1],[2,3]]) {
    assert.ok(Math.abs(f.positions[2*b]-f.positions[2*a]-20)<1e-5);
    assert.equal(f.positions[2*b+1]-f.positions[2*a+1],0);
  }
  for (const a of [0,1]) for (const b of [2,3]) assert.ok(Math.hypot(f.positions[2*b]-f.positions[2*a],f.positions[2*b+1]-f.positions[2*a+1]) >= 12);
  f.fixed[0]=1; const before=f.positions.slice();
  assert.equal(packComponents(f),0); assert.deepEqual(f.positions,before);
});
test('hierarchy preserves mappings, parallel edge weight, and all fine nodes', () => {
  const f = fixture(512), coarse = coarsen(f);
  assert.equal(coarse.radii.length,256);
  assert.equal(f.parents.length,512);
  assert.ok([...f.parents].every(i => i<256));
  refine(f,coarse);
  assert.equal(f.positions.length,1024);
  assert.ok([...f.positions].every(Number.isFinite));
});
test('buffer validation rejects malformed, nonfinite and invalid endpoints', () => {
  const f = fixture(3); f.edges[0]=3;
  assert.throws(() => validate(f),/endpoint/);
  f.edges[0]=0; f.positions[0]=NaN;
  assert.throws(() => validate(f),/coordinate/);
  f.positions[0]=0; f.radii[0]=-1;
  assert.throws(() => validate(f),/radius/);
});
for (const kind of ['fa2','d3']) test(`${kind} completes multilevel layout with every endpoint intact`, async () => {
  const f = fixture(300);
  const originalEdges = f.edges.slice();
  const result = await layout(f,{kind,maxMs:1000});
  assert.equal(result.stats.nodes,300); assert.equal(result.stats.edges,299);
  assert.ok(result.stats.levels > 1);
  assert.ok([...result.positions].every(Number.isFinite));
  assert.equal(result.stats.remainingOverlaps,0);
  assert.deepEqual(f.edges,originalEdges);
});
test('engine cancellation does not report completion', async () => {
  await assert.rejects(layout(fixture(1000),{},()=>{},()=>true),{name:'AbortError'});
});
test('final collision audit is read-only and reports dense unaudited cells', () => {
  const f=fixture(100,false), before=f.positions.slice();
  const audit=separate(f.positions,f.radii,f.fixed,{passes:1,measureOnly:true});
  assert.equal(audit.crowdedNodes,100); assert.deepEqual(f.positions,before);
});

test('controller cancellation, stale messages, topology changes, and frame backpressure', () => {
  let next = 1; const frames = new Map();
  const oldRaf = globalThis.requestAnimationFrame, oldCancel = globalThis.cancelAnimationFrame;
  globalThis.requestAnimationFrame = fn => {const id=next++; frames.set(id,fn); return id;};
  globalThis.cancelAnimationFrame = id => frames.delete(id);
  const workers=[];
  const factory=()=> {const w={messages:[], dead:false, postMessage(m){this.messages.push(m);}, terminate(){this.dead=true;}}; workers.push(w); return w;};
  const flush=()=> {for (const [id,fn] of [...frames]) {frames.delete(id); fn();}};
  try {
    const graph=new Graph(); graph.addNode('a',{x:0,y:0,size:5});
    let completed=0, events=0, cancelled=0;
    graph.on('eachNodeAttributesUpdated',()=>events++);
    const c=new LayoutController(graph,{workerFactory:factory,onComplete:()=>completed++,onCancel:()=>cancelled++});
    c.start(); const a=workers[0];
    a.onmessage({data:{kind:'progress',positions:new Float32Array([1,2]),stats:{}}});
    a.onmessage({data:{kind:'complete',positions:new Float32Array([3,4]),stats:{phase:'complete'}}});
    assert.equal(frames.size,1); flush();
    assert.equal(events,1); assert.equal(completed,1); assert.equal(a.dead,true);
    c.start(); const b=workers[1]; c.stop();
    b.onmessage({data:{kind:'complete',positions:new Float32Array([99,99]),stats:{}}}); flush();
    assert.equal(completed,1); assert.equal(graph.getNodeAttribute('a','x'),3);
    c.start(); graph.addNode('b'); assert.equal(c.isRunning,false);
    assert.equal(cancelled,1); assert.equal(completed,1);
    assert.equal(graph.listenerCount('nodeAdded'),0);
    c.kill(); assert.throws(()=>c.start(),/disposed/);
  } finally {globalThis.requestAnimationFrame=oldRaf; globalThis.cancelAnimationFrame=oldCancel;}
});
