//! Procedure registry for CALL: a thread-local store of procedure definitions registered by the TCK
//! harness.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::types::CypherValue;

/// Type of a procedure parameter or output column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcType {
    /// Strings (or null).
    String,
    /// Integers (or null).
    Integer,
    /// Floats (integers promote; or null).
    Float,
    /// Any number, integer or float (or null).
    Number,
}

/// Procedure parameter.
#[derive(Debug, Clone)]
pub struct ProcParam {
    /// Parameter name.
    pub name: std::string::String,
    /// Expected type.
    pub typ: ProcType,
}

/// Procedure output column.
#[derive(Debug, Clone)]
pub struct ProcOutput {
    /// Output column name.
    pub name: std::string::String,
    /// Output type.
    pub typ: ProcType,
}

/// Procedure definition: signature plus preloaded rows.
#[derive(Debug, Clone)]
pub struct Procedure {
    /// Fully qualified name (`test.my.proc`).
    pub name: std::string::String,
    /// Input parameters.
    pub params: Vec<ProcParam>,
    /// Output columns.
    pub outputs: Vec<ProcOutput>,
    /// Preloaded result rows keyed by column.
    pub rows: Vec<HashMap<std::string::String, CypherValue>>,
}

thread_local! {
    static PROCEDURE_REGISTRY: RefCell<HashMap<String, Procedure>> = RefCell::new(HashMap::new());
}

/// Registers a procedure in the thread-local registry.
pub fn register_procedure(proc_def: Procedure) {
    PROCEDURE_REGISTRY.with(|r| {
        r.borrow_mut().insert(proc_def.name.clone(), proc_def);
    });
}

/// Procedure by name.
pub fn get_procedure(name: &str) -> Option<Procedure> {
    PROCEDURE_REGISTRY.with(|r| r.borrow().get(name).cloned())
}

/// Output columns of the built-in, runtime-dispatched schema procedures (Neo4j-compatible), which
/// aren't in the data-driven registry; `None` for other names. They back `CALL db.labels()` /
/// `db.propertyKeys()` / `db.relationshipTypes()` ..., dispatched in
/// `vex::ops::call::CallProcedureOp`, validated as runtime-dispatched in `validate`,
/// slot-allocated in `frame`.
pub(crate) fn builtin_schema_proc_columns(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "db.labels" => Some(&["label"]),
        "db.propertyKeys" => Some(&["propertyKey"]),
        "db.relationshipTypes" => Some(&["relationshipType"]),
        // `db.schema.visualization()`: schema meta-graph, one virtual node per label, one virtual
        // relationship per observed label->type->label
        "db.schema.visualization" => Some(&["nodes", "relationships"]),
        // `dbms.components()`: server version, which drivers ask for on connect
        "dbms.components" => Some(&["name", "versions", "edition"]),
        // `db.awaitIndex(es)()`: no-op, indexes populate synchronously; no output columns
        "db.awaitIndexes" | "db.awaitIndex" => Some(&[]),

        // -- operational no-ops (synchronous engine, no sampling/replanning) --
        "db.prepareForReplanning"
        | "db.resampleIndex"
        | "db.resampleOutdatedIndexes"
        | "db.index.fulltext.awaitEventuallyConsistentIndexRefresh"
        // token pre-creation: tokens intern on first use, so it's a no-op with the same visible
        // outcome
        | "db.createLabel"
        | "db.createProperty"
        | "db.createRelationshipType"
        // write shims (dispatched with mutable graph access before the read-only schema path), no
        // output
        | "db.index.vector.createNodeIndex"
        | "db.create.setNodeVectorProperty"
        | "db.create.setRelationshipVectorProperty" => Some(&[]),
        // deprecated variant of setNodeVectorProperty, returns the node
        "db.create.setVectorProperty" => Some(&["node"]),

        // -- info / config --------------------------------------------
        "db.clearQueryCaches" => Some(&["value"]),
        "db.info" | "dbms.info" => Some(&["id", "name", "creationDate"]),
        "dbms.upgrade" => Some(&["status", "upgradeResult"]),
        "dbms.upgradeStatus" => Some(&["status", "description", "resolution"]),
        "dbms.listCapabilities" => Some(&["name", "description", "value"]),
        "dbms.listConfig" => Some(&["name", "description", "value", "dynamic", "defaultValue"]),
        "db.index.fulltext.listAvailableAnalyzers" => {
            Some(&["analyzer", "description", "stopwords"])
        }

        // -- schema introspection (sampled, like db.schema.visualization) --
        "db.schema.nodeTypeProperties" => {
            Some(&["nodeType", "nodeLabels", "propertyName", "propertyTypes", "mandatory"])
        }
        "db.schema.relTypeProperties" => {
            Some(&["relType", "propertyName", "propertyTypes", "mandatory"])
        }

        // -- db.stats.* (planner stats; a minimal honest surface) --
        "db.stats.status" => Some(&["section", "status", "data"]),
        "db.stats.collect" | "db.stats.stop" | "db.stats.clear" => {
            Some(&["section", "success", "message"])
        }
        "db.stats.retrieve" | "db.stats.retrieveAllAnonymized" => Some(&["section", "data"]),

        // -- connection registry (one embedded connection) ---------
        "dbms.listConnections" => Some(&[
            "connectionId",
            "connectTime",
            "connector",
            "username",
            "userAgent",
            "serverAddress",
            "clientAddress",
        ]),
        "dbms.killConnection" | "dbms.killConnections" => {
            Some(&["connectionId", "username", "message"])
        }

        // -- relationship-index queries: full-text/vector indexes are node-only, so these match
        // nothing --
        "db.index.fulltext.queryRelationships" | "db.index.vector.queryRelationships" => {
            Some(&["relationship", "score"])
        }

        // -- APOC procedures (dispatched in operators/call/apoc) ----
        "apoc.create.node" | "apoc.create.nodes" | "apoc.merge.node" => Some(&["node"]),
        "apoc.create.relationship" | "apoc.merge.relationship" => Some(&["rel"]),
        "apoc.create.uuids" => Some(&["row", "uuid"]),
        "apoc.util.validate" => Some(&[]),
        "apoc.cypher.run" | "apoc.cypher.doit" | "apoc.cypher.doIt" => Some(&["value"]),
        "apoc.when" | "apoc.do.when" => Some(&["value"]),
        "apoc.refactor.mergeNodes" | "apoc.refactor.mergenodes" => Some(&["node"]),
        "apoc.refactor.cloneNodes" | "apoc.refactor.clonenodes" => {
            Some(&["input", "output", "error"])
        }
        "apoc.refactor.setType" | "apoc.refactor.settype" | "apoc.refactor.invert"
        | "apoc.refactor.to" | "apoc.refactor.from" => Some(&["input", "output", "error"]),
        "apoc.refactor.rename.label" | "apoc.refactor.rename.type" => Some(&[
            "batches",
            "total",
            "timeTaken",
            "committedOperations",
            "failedOperations",
        ]),
        "apoc.schema.assert" => Some(&["label", "key", "keys", "unique", "action"]),
        "apoc.periodic.commit" => Some(&["updates", "executions"]),
        "apoc.periodic.truncate" => Some(&[]),
        "apoc.generate.ba" | "apoc.generate.ws" | "apoc.generate.er"
        | "apoc.generate.complete" | "apoc.generate.simple" => Some(&[]),
        "apoc.static.get" | "apoc.static.set" => Some(&["value"]),
        "apoc.static.list" => Some(&["key", "value"]),
        "apoc.export.arrow.stream.all" | "apoc.export.arrow.stream.graph"
        | "apoc.export.arrow.stream.query" | "apoc.load.arrow" | "apoc.load.xml"
        | "apoc.load.xmlSimple" | "apoc.load.xmlsimple" => Some(&["value"]),
        "apoc.export.json.all" | "apoc.export.json.data" | "apoc.export.json.graph"
        | "apoc.export.json.query" | "apoc.export.csv.all" | "apoc.export.csv.data"
        | "apoc.export.csv.graph" | "apoc.export.csv.query" | "apoc.export.cypher.all"
        | "apoc.export.cypher.data" | "apoc.export.cypher.graph"
        | "apoc.export.cypher.query" | "apoc.export.cypher.schema"
        | "apoc.export.graphml.all" | "apoc.export.graphml.data"
        | "apoc.export.graphml.graph" | "apoc.export.graphml.query" | "apoc.import.json"
        | "apoc.export.arrow.all" | "apoc.export.arrow.graph" | "apoc.export.arrow.query"
        | "apoc.import.csv" | "apoc.import.graphml" => Some(&[
            "file",
            "source",
            "format",
            "nodes",
            "relationships",
            "properties",
            "time",
            "rows",
            "batchSize",
            "batches",
            "done",
            "data",
        ]),
        "apoc.periodic.submit" | "apoc.periodic.repeat" | "apoc.periodic.countdown"
        | "apoc.periodic.list" | "apoc.periodic.cancel" => {
            Some(&["name", "delay", "rate", "done", "cancelled"])
        }
        "apoc.spatial.geocode" | "apoc.spatial.geocodeOnce" | "apoc.spatial.geocodeonce"
        | "apoc.spatial.reverseGeocode" | "apoc.spatial.reversegeocode" => {
            Some(&["location", "data", "latitude", "longitude", "description"])
        }
        "apoc.load.jsonParams" | "apoc.load.jsonparams" => Some(&["value"]),
        "apoc.load.jdbc" => Some(&["row"]),
        "apoc.example.movies" => Some(&[
            "file",
            "source",
            "format",
            "nodes",
            "relationships",
            "properties",
            "time",
            "rows",
            "batchSize",
            "batches",
            "done",
            "data",
        ]),
        "apoc.log.stream" => Some(&["lineNo", "line", "path"]),
        "apoc.warmup.run" => Some(&[
            "pageSize",
            "totalTime",
            "nodesTotal",
            "relsTotal",
            "transactionWasTerminated",
        ]),
        "apoc.nodes.group" => {
            Some(&["nodes", "relationships", "node", "relationship"])
        }
        "apoc.import.xml" => Some(&["node"]),
        "apoc.trigger.add" | "apoc.trigger.remove" | "apoc.trigger.removeAll"
        | "apoc.trigger.removeall" | "apoc.trigger.list" | "apoc.trigger.pause"
        | "apoc.trigger.resume" | "apoc.trigger.install" | "apoc.trigger.drop"
        | "apoc.trigger.dropAll" | "apoc.trigger.dropall" | "apoc.trigger.show"
        | "apoc.trigger.start" | "apoc.trigger.stop" => {
            Some(&["name", "query", "selector", "params", "installed", "paused"])
        }
        "apoc.lock.all" | "apoc.lock.nodes" | "apoc.lock.rels" | "apoc.lock.read.nodes"
        | "apoc.lock.read.rels" | "apoc.util.sleep" | "apoc.nodes.link"
        | "apoc.refactor.normalizeAsBoolean" | "apoc.refactor.normalizeasboolean"
        | "apoc.refactor.categorize" | "apoc.convert.setJsonProperty"
        | "apoc.convert.setjsonproperty" => Some(&[]),
        "apoc.nodes.get" => Some(&["node"]),
        "apoc.nodes.rels" => Some(&["rel"]),
        "apoc.nodes.delete" => Some(&["value"]),
        "apoc.nodes.collapse" => Some(&["from", "rel", "to"]),
        "apoc.nodes.cycles" => Some(&["path"]),
        "apoc.coll.pairWithOffset" | "apoc.coll.pairwithoffset" | "apoc.coll.partition"
        | "apoc.coll.split" | "apoc.coll.zipToRows" | "apoc.coll.ziptorows" => Some(&["value"]),
        "apoc.coll.elements" => Some(&[
            "_1", "_2", "_3", "_4", "_5", "_6", "_7", "_8", "_9", "_10", "elements",
        ]),
        "apoc.text.phoneticDelta" | "apoc.text.phoneticdelta" => {
            Some(&["phonetic1", "phonetic2", "delta"])
        }
        "apoc.case" | "apoc.do.case" => Some(&["value"]),
        "apoc.cypher.runWrite" | "apoc.cypher.runwrite" | "apoc.cypher.runSchema"
        | "apoc.cypher.runschema" | "apoc.cypher.runTimeboxed" | "apoc.cypher.runtimeboxed" => {
            Some(&["value"])
        }
        "apoc.cypher.runMany" | "apoc.cypher.runmany" | "apoc.cypher.runManyReadOnly"
        | "apoc.cypher.runmanyreadonly" => Some(&["row", "result"]),
        "apoc.load.jsonArray" | "apoc.load.jsonarray" => Some(&["value"]),
        "apoc.help" => Some(&[
            "type",
            "name",
            "text",
            "signature",
            "roles",
            "writes",
            "core",
            "isDeprecated",
        ]),
        "apoc.stats.degrees" => Some(&[
            "type", "direction", "total", "p50", "p75", "p90", "p95", "p99", "p999", "max",
            "min", "mean",
        ]),
        "apoc.math.regr" => Some(&["r2", "avgX", "avgY", "slope"]),
        "apoc.schema.nodes" | "apoc.schema.relationships" => Some(&[
            "name",
            "label",
            "type",
            "properties",
            "status",
        ]),
        "apoc.schema.properties.distinct" => Some(&["value"]),
        "apoc.schema.properties.distinctCount" | "apoc.schema.properties.distinctcount" => {
            Some(&["label", "key", "value", "count"])
        }
        "apoc.spatial.sortByDistance" | "apoc.spatial.sortbydistance" => {
            Some(&["path", "distance"])
        }
        "apoc.search.node" | "apoc.search.nodeAll" | "apoc.search.nodeall" => Some(&["node"]),
        "apoc.search.nodeReduced" | "apoc.search.nodereduced" | "apoc.search.nodeAllReduced"
        | "apoc.search.nodeallreduced" | "apoc.search.multiSearchReduced"
        | "apoc.search.multisearchreduced" => Some(&["id", "labels", "values"]),
        "apoc.neighbors.athop" | "apoc.neighbors.tohop" => Some(&["node"]),
        "apoc.neighbors.byhop" => Some(&["nodes"]),
        "apoc.neighbors.athop.count" | "apoc.neighbors.tohop.count" => Some(&["value"]),
        "apoc.neighbors.byhop.count" => Some(&["value"]),
        "apoc.atomic.add" | "apoc.atomic.subtract" | "apoc.atomic.concat"
        | "apoc.atomic.insert" | "apoc.atomic.remove" | "apoc.atomic.update" => {
            Some(&["container", "property", "oldValue", "newValue"])
        }
        "apoc.merge.node.eager" => Some(&["node"]),
        "apoc.merge.relationship.eager" => Some(&["rel"]),
        "apoc.merge.nodeWithStats" | "apoc.merge.nodewithstats"
        | "apoc.merge.nodeWithStats.eager" | "apoc.merge.nodewithstats.eager" => {
            Some(&["stats", "node"])
        }
        "apoc.merge.relationshipWithStats" | "apoc.merge.relationshipwithstats"
        | "apoc.merge.relationshipWithStats.eager"
        | "apoc.merge.relationshipwithstats.eager" => Some(&["stats", "rel"]),
        "apoc.create.setProperty" | "apoc.create.setproperty" | "apoc.create.setProperties"
        | "apoc.create.setproperties" | "apoc.create.removeProperties"
        | "apoc.create.removeproperties" => Some(&["node"]),
        "apoc.create.setRelProperty" | "apoc.create.setrelproperty"
        | "apoc.create.setRelProperties" | "apoc.create.setrelproperties"
        | "apoc.create.removeRelProperties" | "apoc.create.removerelproperties" => {
            Some(&["rel"])
        }
        "apoc.create.vNode" | "apoc.create.vnode" | "apoc.create.vNodes"
        | "apoc.create.vnodes" => Some(&["node"]),
        "apoc.create.vRelationship" | "apoc.create.vrelationship" => Some(&["rel"]),
        "apoc.create.virtualPath" | "apoc.create.virtualpath" => Some(&["from", "rel", "to"]),
        "apoc.create.clonePathToVirtual" | "apoc.create.clonepathtovirtual"
        | "apoc.create.clonePathsToVirtual" | "apoc.create.clonepathstovirtual" => {
            Some(&["path"])
        }
        "apoc.refactor.collapseNode" | "apoc.refactor.collapsenode" => {
            Some(&["input", "output", "error"])
        }
        "apoc.refactor.extractNode" | "apoc.refactor.extractnode" => {
            Some(&["input", "output", "error"])
        }
        "apoc.refactor.mergeRelationships" | "apoc.refactor.mergerelationships" => {
            Some(&["rel"])
        }
        "apoc.refactor.deleteAndReconnect" | "apoc.refactor.deleteandreconnect" => {
            Some(&["nodes", "relationships"])
        }
        "apoc.refactor.cloneSubgraph" | "apoc.refactor.clonesubgraph"
        | "apoc.refactor.cloneSubgraphFromPaths" | "apoc.refactor.clonesubgraphfrompaths" => {
            Some(&["input", "output", "error"])
        }
        "apoc.refactor.rename.nodeProperty" | "apoc.refactor.rename.nodeproperty"
        | "apoc.refactor.rename.typeProperty" | "apoc.refactor.rename.typeproperty" => Some(&[
            "batches",
            "total",
            "timeTaken",
            "committedOperations",
            "failedOperations",
        ]),
        "apoc.meta.data" | "apoc.meta.data.of" => Some(&[
            "label",
            "property",
            "count",
            "unique",
            "index",
            "existence",
            "type",
            "array",
            "elementType",
        ]),
        "apoc.meta.schema" => Some(&["value"]),
        "apoc.meta.graph" | "apoc.meta.graph.of" | "apoc.meta.graphSample"
        | "apoc.meta.graphsample" | "apoc.meta.subGraph" | "apoc.meta.subgraph" => {
            Some(&["nodes", "relationships"])
        }
        "apoc.meta.nodeTypeProperties" | "apoc.meta.nodetypeproperties" => Some(&[
            "nodeType",
            "nodeLabels",
            "propertyName",
            "propertyTypes",
            "mandatory",
        ]),
        "apoc.meta.relTypeProperties" | "apoc.meta.reltypeproperties" => Some(&[
            "relType",
            "propertyName",
            "propertyTypes",
            "mandatory",
        ]),
        "apoc.convert.toTree" | "apoc.convert.totree" | "apoc.paths.toJsonTree"
        | "apoc.paths.tojsontree" => Some(&["value"]),
        "apoc.path.expandConfig" | "apoc.path.expandconfig" => Some(&["path"]),
        "apoc.path.subgraphAll" | "apoc.path.subgraphall" => Some(&["nodes", "relationships"]),
        "apoc.graph.from" | "apoc.graph.fromData" | "apoc.graph.fromdata"
        | "apoc.graph.fromPath" | "apoc.graph.frompath" | "apoc.graph.fromPaths"
        | "apoc.graph.frompaths" | "apoc.graph.fromDB" | "apoc.graph.fromdb"
        | "apoc.graph.fromCypher" | "apoc.graph.fromcypher" | "apoc.graph.fromDocument"
        | "apoc.graph.fromdocument" => Some(&["graph"]),
        "apoc.graph.validateDocument" | "apoc.graph.validatedocument" => Some(&["row"]),
        "apoc.algo.cover" => Some(&["rel"]),
        "apoc.algo.aStar" | "apoc.algo.astar" | "apoc.algo.aStarConfig"
        | "apoc.algo.astarconfig" | "apoc.algo.dijkstra" => Some(&["path", "weight"]),
        "apoc.algo.allSimplePaths" | "apoc.algo.allsimplepaths" => Some(&["path"]),
        "apoc.create.addLabels" | "apoc.create.addlabels" | "apoc.create.removeLabels"
        | "apoc.create.removelabels" | "apoc.create.setLabels" | "apoc.create.setlabels" => {
            Some(&["node"])
        }
        "apoc.path.expand" | "apoc.path.spanningTree" | "apoc.path.spanningtree" => {
            Some(&["path"])
        }
        "apoc.path.subgraphNodes" | "apoc.path.subgraphnodes" => Some(&["node"]),
        "apoc.load.json" => Some(&["value"]),
        "apoc.meta.stats" => Some(&[
            "labelCount",
            "relTypeCount",
            "propertyKeyCount",
            "nodeCount",
            "relCount",
            "labels",
            "relTypes",
        ]),
        "apoc.periodic.iterate" => Some(&[
            "batches",
            "total",
            "timeTaken",
            "committedOperations",
            "failedOperations",
            "failedBatches",
            "retries",
            "errorMessages",
            "wasTerminated",
        ]),
        _ => None,
    }
}

/// Clears every registered procedure.
pub fn clear_procedures() {
    PROCEDURE_REGISTRY.with(|r| r.borrow_mut().clear());
}

/// Parses a TCK type like `STRING?`, `INTEGER?`, `NUMBER?`, `FLOAT?`.
pub fn parse_proc_type(s: &str) -> Option<ProcType> {
    let s = s.trim().trim_end_matches('?').trim();
    match s.to_uppercase().as_str() {
        "STRING" => Some(ProcType::String),
        "INTEGER" => Some(ProcType::Integer),
        "FLOAT" => Some(ProcType::Float),
        "NUMBER" => Some(ProcType::Number),
        _ => None,
    }
}
