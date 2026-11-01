# @truespar/traverse-layout

Graph layout for Traverse Studio. The layout runs in a module worker of its own: the page keeps
the graph (Graphology) and the renderer (Sigma), sends the worker the node positions, sizes,
fixed flags and edges, and applies the positions it sends back. Paddock Studio uses a copy of this
package, made by its vendoring script; changes are made here.

## How a layout runs

1. **Coarsening.** Edges are contracted greedily into coarser levels, at most 16, keeping the
   original node ids and summing the weights of parallel edges. Graphs with pinned nodes skip
   this step.
2. **Force layout.** Barnes-Hut ForceAtlas2 (the default) or D3 forces, from the coarsest level
   to the finest.
3. **Packing.** Disconnected components are packed side by side, each keeping its shape. Inputs
   with fixed nodes skip packing.
4. **Collision refinement.** Overlapping nodes are moved apart cell by cell. A cell with more
   than 32 nodes is spread out first; other cells compare each node with at most 9 x 32
   candidates per pass. Fixed nodes never move.
5. **Expansion.** A uniform scale-up of at most 4x removes the remaining overlaps without changing
   angles, and the components are packed again.

When a run ends, the result reports `reason`, `remainingOverlaps`, `crowdedNodes`, `expansion` and
`elapsedMs`. `remainingOverlaps` is `null` when crowded cells prevent an exact count. Overlaps are
measured in graph coordinates, so nodes and labels drawn at a fixed pixel size can still overlap
on screen at some zoom levels.

## Limits

- Input with more than 200,000 nodes, 1,000,000 edges or 64 MiB of data is rejected.
- Each coarsening level stores O(V+E).
- The page has at most one preview frame and one final frame that it hasn't acknowledged, and
  applies only the latest.
- The watchdog allows the requested compute budget plus 5 seconds. The worker checks its
  deadline between iterations; terminating the worker is the hard stop.

## Stopping

Stopping a layout terminates its worker, cancels pending frames and the watchdog, and removes the
graph listeners. The completion callback, collision cleanup and camera reset don't run. Results
from an older worker, or for a graph that has changed since, are discarded.

## Tests

`test/layout.test.mjs` covers a dense 50,000-node graph with coincident positions, fixed
positions, the coarsening hierarchy, component geometry, invalid input, ForceAtlas2 against the
library's own implementation, both solvers, the final overlap check, backpressure, stale results
and cleanup.

The ForceAtlas2 adapter uses internals of `graphology-layout-forceatlas2`, so the dependencies
are pinned exactly (graphology 0.26.0, graphology-layout-forceatlas2 0.10.1, d3-force 3.0.0).
Run the ForceAtlas2 test after upgrading them.

The coarsening, collision and packing code is written for this package; the force iterations call
the installed libraries.

## References

[Graphology ForceAtlas2](https://graphology.github.io/standard-library/layout-forceatlas2.html),
[Graphology noverlap](https://graphology.github.io/standard-library/layout-noverlap.html),
[Sigma lifecycle](https://www.sigmajs.org/docs/advanced/lifecycle/),
[Graphviz sfdp](https://graphviz.org/docs/layouts/sfdp/).
