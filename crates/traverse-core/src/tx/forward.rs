//! Macros for `GraphView` wrappers: forward every read to an inner view, refuse every write.
//!
//! A wrapper that implements only some methods silently falls back to the trait
//! defaults for the rest (an empty `label_names`, "no such index", a full scan);
//! generating the whole method set rules that out.

/// [`GraphView`](crate::tx::view::GraphView)'s statement and batch methods, forwarded to a
/// wrapped transaction: `$reads` and `$writes` borrow it.
#[macro_export]
#[doc(hidden)]
macro_rules! forward_graph_view_statements {
    ($self:ident => $reads:expr, $writes:expr) => {
        fn write_count(&$self) -> u64 {
            $crate::tx::view::GraphView::write_count($reads)
        }

        fn is_explicit_transaction(&$self) -> bool {
            $crate::tx::view::GraphView::is_explicit_transaction($reads)
        }

        fn can_commit_batches(&$self) -> bool {
            $crate::tx::view::GraphView::can_commit_batches($reads)
        }

        fn commit_batch(&mut $self) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::commit_batch($writes)
        }

        fn rollback_batch(&mut $self) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::rollback_batch($writes)
        }
    };
}

/// Implements every read method of [`GraphView`](crate::tx::view::GraphView) by forwarding to
/// `$inner`, an expression over `$self` yielding `&impl GraphView` (or `&dyn GraphView`):
/// `forward_graph_view_reads!(self => self.0);`. `as_sync_ref` stays with the wrapper.
#[macro_export]
macro_rules! forward_graph_view_reads {
    ($self:ident => $inner:expr) => {
        fn data_version(&$self) -> ::std::option::Option<(usize, u64)> {
            $crate::tx::view::GraphView::data_version($inner)
        }

        fn get_node(
            &$self,
            id: $crate::graph::types::NodeId,
        ) -> $crate::error::Result<::std::borrow::Cow<'_, $crate::graph::node::Node>> {
            $crate::tx::view::GraphView::get_node($inner, id)
        }

        fn get_edge(
            &$self,
            id: $crate::graph::types::EdgeId,
        ) -> $crate::error::Result<::std::borrow::Cow<'_, $crate::graph::edge::Edge>> {
            $crate::tx::view::GraphView::get_edge($inner, id)
        }

        fn has_label(
            &$self,
            node: $crate::graph::types::NodeId,
            label: $crate::graph::types::LabelId,
        ) -> $crate::error::Result<bool> {
            $crate::tx::view::GraphView::has_label($inner, node, label)
        }

        fn node_count(&$self) -> usize {
            $crate::tx::view::GraphView::node_count($inner)
        }

        fn edge_count(&$self) -> usize {
            $crate::tx::view::GraphView::edge_count($inner)
        }

        fn nodes_vec(&$self) -> ::std::vec::Vec<$crate::graph::types::NodeId> {
            $crate::tx::view::GraphView::nodes_vec($inner)
        }

        fn all_edges_vec(&$self) -> ::std::vec::Vec<$crate::graph::types::EdgeId> {
            $crate::tx::view::GraphView::all_edges_vec($inner)
        }

        fn nodes_by_label(&$self, label: &str) -> ::std::vec::Vec<$crate::graph::types::NodeId> {
            $crate::tx::view::GraphView::nodes_by_label($inner, label)
        }

        fn edges_by_type(
            &$self,
            edge_type: &str,
        ) -> ::std::vec::Vec<$crate::graph::types::EdgeId> {
            $crate::tx::view::GraphView::edges_by_type($inner, edge_type)
        }

        fn label_names(&$self) -> ::std::vec::Vec<::std::string::String> {
            $crate::tx::view::GraphView::label_names($inner)
        }

        fn relationship_type_names(&$self) -> ::std::vec::Vec<::std::string::String> {
            $crate::tx::view::GraphView::relationship_type_names($inner)
        }

        fn property_key_names(&$self) -> ::std::vec::Vec<::std::string::String> {
            $crate::tx::view::GraphView::property_key_names($inner)
        }

        fn neighbors(
            &$self,
            node: $crate::graph::types::NodeId,
            dir: $crate::graph::types::Direction,
            filter: ::std::option::Option<&str>,
        ) -> $crate::error::Result<::std::vec::Vec<$crate::graph::types::NodeId>> {
            $crate::tx::view::GraphView::neighbors($inner, node, dir, filter)
        }

        fn edges_of(
            &$self,
            node: $crate::graph::types::NodeId,
            dir: $crate::graph::types::Direction,
            filter: ::std::option::Option<&str>,
        ) -> $crate::error::Result<::std::vec::Vec<$crate::graph::types::EdgeId>> {
            $crate::tx::view::GraphView::edges_of($inner, node, dir, filter)
        }

        fn get_node_property(
            &$self,
            id: $crate::graph::types::NodeId,
            key: &str,
        ) -> $crate::error::Result<::std::option::Option<$crate::graph::property::PropertyValue>>
        {
            $crate::tx::view::GraphView::get_node_property($inner, id, key)
        }

        fn get_node_property_kid(
            &$self,
            id: $crate::graph::types::NodeId,
            kid: $crate::graph::types::PropertyKeyId,
        ) -> $crate::error::Result<::std::option::Option<$crate::graph::property::PropertyValue>>
        {
            $crate::tx::view::GraphView::get_node_property_kid($inner, id, kid)
        }

        fn get_edge_property(
            &$self,
            id: $crate::graph::types::EdgeId,
            key: &str,
        ) -> $crate::error::Result<::std::option::Option<$crate::graph::property::PropertyValue>>
        {
            $crate::tx::view::GraphView::get_edge_property($inner, id, key)
        }

        fn batch_read_node_properties(
            &$self,
            node_ids: &[$crate::graph::types::NodeId],
            keys: &[$crate::graph::types::PropertyKeyId],
            out: &mut [::std::option::Option<$crate::graph::property::PropertyValue>],
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::batch_read_node_properties($inner, node_ids, keys, out)
        }

        fn batch_read_node_properties_i64(
            &$self,
            node_ids: &[$crate::graph::types::NodeId],
            keys: &[$crate::graph::types::PropertyKeyId],
            out_vals: &mut [i64],
            out_valid: &mut [bool],
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::batch_read_node_properties_i64(
                $inner, node_ids, keys, out_vals, out_valid,
            )
        }

        fn adjacency_snapshot(
            &$self,
        ) -> ::std::option::Option<$crate::graph::adjacency::AdjacencySnapshot<'_>> {
            $crate::tx::view::GraphView::adjacency_snapshot($inner)
        }

        fn highest_node_id(&$self) -> u64 {
            $crate::tx::view::GraphView::highest_node_id($inner)
        }

        fn edges_with_neighbor(
            &$self,
            node: $crate::graph::types::NodeId,
            dir: $crate::graph::types::Direction,
            filter: ::std::option::Option<&str>,
        ) -> $crate::error::Result<::std::vec::Vec<$crate::graph::types::EdgeEndpoint>> {
            $crate::tx::view::GraphView::edges_with_neighbor($inner, node, dir, filter)
        }

        fn neighbors_filtered(
            &$self,
            node: $crate::graph::types::NodeId,
            dir: $crate::graph::types::Direction,
            type_filter: &::std::option::Option<
                $crate::__rustc_hash::FxHashSet<$crate::graph::types::EdgeTypeId>,
            >,
        ) -> $crate::error::Result<::std::vec::Vec<$crate::graph::types::NodeId>> {
            $crate::tx::view::GraphView::neighbors_filtered($inner, node, dir, type_filter)
        }

        fn edge_endpoints_filtered(
            &$self,
            node: $crate::graph::types::NodeId,
            dir: $crate::graph::types::Direction,
            type_filter: &::std::option::Option<
                $crate::__rustc_hash::FxHashSet<$crate::graph::types::EdgeTypeId>,
            >,
        ) -> $crate::error::Result<
            ::std::vec::Vec<($crate::graph::types::EdgeId, $crate::graph::types::NodeId)>,
        > {
            $crate::tx::view::GraphView::edge_endpoints_filtered($inner, node, dir, type_filter)
        }

        fn edges_between(
            &$self,
            from: $crate::graph::types::NodeId,
            to: $crate::graph::types::NodeId,
            dir: $crate::graph::types::Direction,
            type_filter: &::std::option::Option<
                $crate::__rustc_hash::FxHashSet<$crate::graph::types::EdgeTypeId>,
            >,
        ) -> $crate::error::Result<::std::vec::Vec<$crate::graph::types::EdgeId>> {
            $crate::tx::view::GraphView::edges_between($inner, from, to, dir, type_filter)
        }

        fn for_each_neighbor(
            &$self,
            node: $crate::graph::types::NodeId,
            dir: $crate::graph::types::Direction,
            type_filter: &::std::option::Option<
                $crate::__rustc_hash::FxHashSet<$crate::graph::types::EdgeTypeId>,
            >,
            f: &mut dyn FnMut($crate::graph::types::NodeId),
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::for_each_neighbor($inner, node, dir, type_filter, f)
        }

        fn expand_frontier(
            &$self,
            nodes: &[$crate::graph::types::NodeId],
            direction: $crate::graph::types::Direction,
            type_filter: &::std::option::Option<
                $crate::__rustc_hash::FxHashSet<$crate::graph::types::EdgeTypeId>,
            >,
            out: &mut $crate::__rustc_hash::FxHashSet<$crate::graph::types::NodeId>,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::expand_frontier($inner, nodes, direction, type_filter, out)
        }

        fn degree(
            &$self,
            node: $crate::graph::types::NodeId,
            dir: $crate::graph::types::Direction,
        ) -> $crate::error::Result<usize> {
            $crate::tx::view::GraphView::degree($inner, node, dir)
        }

        fn strings(&$self) -> &$crate::interner::StringTable {
            $crate::tx::view::GraphView::strings($inner)
        }

        fn statistics(&$self) -> ::std::option::Option<&$crate::graph::stats::GraphStatistics> {
            $crate::tx::view::GraphView::statistics($inner)
        }

        fn index_lookup(
            &$self,
            label: &str,
            prop: &str,
            value: &$crate::graph::property::IndexKey,
        ) -> ::std::option::Option<::std::vec::Vec<$crate::graph::types::NodeId>> {
            $crate::tx::view::GraphView::index_lookup($inner, label, prop, value)
        }

        fn index_range_lookup(
            &$self,
            label: &str,
            prop: &str,
            lower: ::std::ops::Bound<$crate::graph::property::IndexKey>,
            upper: ::std::ops::Bound<$crate::graph::property::IndexKey>,
        ) -> ::std::option::Option<::std::vec::Vec<$crate::graph::types::NodeId>> {
            $crate::tx::view::GraphView::index_range_lookup($inner, label, prop, lower, upper)
        }

        fn index_catalog(&$self) -> &$crate::graph::index::IndexCatalog {
            $crate::tx::view::GraphView::index_catalog($inner)
        }

        fn index_size(&$self, name: &str) -> ::std::option::Option<usize> {
            $crate::tx::view::GraphView::index_size($inner, name)
        }

        fn has_vector_index(&$self, name: &str) -> bool {
            $crate::tx::view::GraphView::has_vector_index($inner, name)
        }

        fn query_vector_index(
            &$self,
            name: &str,
            query: &[f32],
            k: usize,
            ef: ::std::option::Option<usize>,
        ) -> ::std::option::Option<::std::vec::Vec<($crate::graph::types::NodeId, f64)>> {
            $crate::tx::view::GraphView::query_vector_index($inner, name, query, k, ef)
        }

        fn vector_index_descriptors(
            &$self,
        ) -> ::std::vec::Vec<(
            ::std::string::String,
            ::std::string::String,
            ::std::string::String,
            usize,
            &'static str,
        )> {
            $crate::tx::view::GraphView::vector_index_descriptors($inner)
        }

        fn has_fulltext_index(&$self, name: &str) -> bool {
            $crate::tx::view::GraphView::has_fulltext_index($inner, name)
        }

        fn query_fulltext_index(
            &$self,
            name: &str,
            query: &str,
            limit: usize,
        ) -> ::std::option::Option<::std::vec::Vec<($crate::graph::types::NodeId, f64)>> {
            $crate::tx::view::GraphView::query_fulltext_index($inner, name, query, limit)
        }

        fn fulltext_index_descriptors(
            &$self,
        ) -> ::std::vec::Vec<(
            ::std::string::String,
            ::std::vec::Vec<::std::string::String>,
            ::std::vec::Vec<::std::string::String>,
            ::std::string::String,
        )> {
            $crate::tx::view::GraphView::fulltext_index_descriptors($inner)
        }

        fn has_point_index(&$self, name: &str) -> bool {
            $crate::tx::view::GraphView::has_point_index($inner, name)
        }

        fn has_point_index_for(&$self, label: &str, property: &str) -> bool {
            $crate::tx::view::GraphView::has_point_index_for($inner, label, property)
        }

        fn point_index_within_bbox(
            &$self,
            label: &str,
            property: &str,
            ll: [f64; 3],
            ur: [f64; 3],
            srid: i64,
        ) -> ::std::option::Option<::std::vec::Vec<$crate::graph::types::NodeId>> {
            $crate::tx::view::GraphView::point_index_within_bbox(
                $inner, label, property, ll, ur, srid,
            )
        }

        fn point_index_within_distance(
            &$self,
            label: &str,
            property: &str,
            center: [f64; 3],
            radius: f64,
            srid: i64,
        ) -> ::std::option::Option<::std::vec::Vec<($crate::graph::types::NodeId, f64)>> {
            $crate::tx::view::GraphView::point_index_within_distance(
                $inner, label, property, center, radius, srid,
            )
        }

        fn point_index_knn(
            &$self,
            label: &str,
            property: &str,
            center: [f64; 3],
            k: usize,
            srid: i64,
        ) -> ::std::option::Option<::std::vec::Vec<($crate::graph::types::NodeId, f64)>> {
            $crate::tx::view::GraphView::point_index_knn($inner, label, property, center, k, srid)
        }

        fn point_index_descriptors(
            &$self,
        ) -> ::std::vec::Vec<(
            ::std::string::String,
            ::std::string::String,
            ::std::string::String,
        )> {
            $crate::tx::view::GraphView::point_index_descriptors($inner)
        }

        fn has_text_index(&$self, name: &str) -> bool {
            $crate::tx::view::GraphView::has_text_index($inner, name)
        }

        fn has_text_index_for(&$self, label: &str, property: &str) -> bool {
            $crate::tx::view::GraphView::has_text_index_for($inner, label, property)
        }

        fn text_index_candidates(
            &$self,
            label: &str,
            property: &str,
            kind: $crate::graph::text::TextQueryKind,
            pattern: &str,
        ) -> ::std::option::Option<::std::vec::Vec<$crate::graph::types::NodeId>> {
            $crate::tx::view::GraphView::text_index_candidates($inner, label, property, kind, pattern)
        }

        fn text_index_descriptors(
            &$self,
        ) -> ::std::vec::Vec<(
            ::std::string::String,
            ::std::string::String,
            ::std::string::String,
        )> {
            $crate::tx::view::GraphView::text_index_descriptors($inner)
        }

        fn constraint_registry(&$self) -> &$crate::graph::constraint::ConstraintRegistry {
            $crate::tx::view::GraphView::constraint_registry($inner)
        }

        fn edge_index_lookup(
            &$self,
            edge_type: &str,
            prop: &str,
            value: &$crate::graph::property::IndexKey,
        ) -> ::std::option::Option<::std::vec::Vec<$crate::graph::types::EdgeId>> {
            $crate::tx::view::GraphView::edge_index_lookup($inner, edge_type, prop, value)
        }

        fn edge_index_catalog(&$self) -> &$crate::graph::index::EdgeIndexCatalog {
            $crate::tx::view::GraphView::edge_index_catalog($inner)
        }
    };
}

/// Implements every write method of [`GraphView`](crate::tx::view::GraphView) for a read-only
/// view: mutations return `ReadOnlyTransaction`, index maintenance is a no-op,
/// `intern_property_key` resolves known keys (an unknown one gets an id nothing carries, and the
/// write that wanted it fails), `analyze_statistics` does nothing.
#[macro_export]
macro_rules! reject_graph_view_writes {
    () => {
        fn create_node(
            &mut self,
            _labels: &[&str],
            _props: $crate::graph::property::PropertyMap,
        ) -> $crate::error::Result<$crate::graph::types::NodeId> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn create_edge(
            &mut self,
            _src: $crate::graph::types::NodeId,
            _tgt: $crate::graph::types::NodeId,
            _edge_type: &str,
            _props: $crate::graph::property::PropertyMap,
        ) -> $crate::error::Result<$crate::graph::types::EdgeId> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn update_node_properties(
            &mut self,
            _id: $crate::graph::types::NodeId,
            _props: $crate::graph::property::PropertyMap,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn update_edge_properties(
            &mut self,
            _id: $crate::graph::types::EdgeId,
            _props: $crate::graph::property::PropertyMap,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn delete_node(&mut self, _id: $crate::graph::types::NodeId) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn delete_node_cascading(
            &mut self,
            _id: $crate::graph::types::NodeId,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn delete_edge(&mut self, _id: $crate::graph::types::EdgeId) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn add_label(
            &mut self,
            _id: $crate::graph::types::NodeId,
            _label: &str,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn remove_label(
            &mut self,
            _id: $crate::graph::types::NodeId,
            _label: &str,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn get_node_mut(
            &mut self,
            _id: $crate::graph::types::NodeId,
        ) -> $crate::error::Result<&mut $crate::graph::node::Node> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn get_edge_mut(
            &mut self,
            _id: $crate::graph::types::EdgeId,
        ) -> $crate::error::Result<&mut $crate::graph::edge::Edge> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn create_index(
            &mut self,
            _name: &str,
            _label: &str,
            _prop: &str,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn create_composite_index(
            &mut self,
            _name: &str,
            _label: &str,
            _properties: &[::std::string::String],
            _is_relationship: bool,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn drop_index(&mut self, _name: &str) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn create_vector_index(
            &mut self,
            _name: &str,
            _label: &str,
            _property: &str,
            _dim: usize,
            _sim: $crate::graph::vector::Similarity,
            _params: $crate::graph::vector::HnswParams,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn drop_vector_index(&mut self, _name: &str) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn create_fulltext_index(
            &mut self,
            _name: &str,
            _labels: &[&str],
            _properties: &[&str],
            _analyzer: $crate::graph::fulltext::Analyzer,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn drop_fulltext_index(&mut self, _name: &str) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn create_point_index(
            &mut self,
            _name: &str,
            _label: &str,
            _property: &str,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn drop_point_index(&mut self, _name: &str) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn create_text_index(
            &mut self,
            _name: &str,
            _label: &str,
            _property: &str,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn drop_text_index(&mut self, _name: &str) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn unindex_node(&mut self, _id: $crate::graph::types::NodeId) {}

        fn index_node(&mut self, _id: $crate::graph::types::NodeId) {}

        fn unindex_node_prop(&mut self, _id: $crate::graph::types::NodeId, _key: &str) {}

        fn index_node_prop(&mut self, _id: $crate::graph::types::NodeId, _key: &str) {}

        fn unindex_edge_prop(&mut self, _id: $crate::graph::types::EdgeId, _key: &str) {}

        fn index_edge_prop(&mut self, _id: $crate::graph::types::EdgeId, _key: &str) {}

        fn unindex_edge(&mut self, _id: $crate::graph::types::EdgeId) {}

        fn index_edge(&mut self, _id: $crate::graph::types::EdgeId) {}

        fn create_composite_constraint(
            &mut self,
            _name: &str,
            _label: &str,
            _properties: &[::std::string::String],
            _ctype: $crate::graph::constraint::ConstraintType,
            _is_relationship: bool,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn intern_property_key(&mut self, key: &str) -> $crate::graph::types::PropertyKeyId {
            $crate::graph::types::PropertyKeyId(
                $crate::tx::view::GraphView::strings(self)
                    .get(key)
                    .unwrap_or(u32::MAX),
            )
        }

        fn analyze_statistics(&mut self, _labels: &[::std::string::String]) {}

        fn create_constraint(
            &mut self,
            _name: &str,
            _label: &str,
            _prop: &str,
            _ctype: $crate::graph::constraint::ConstraintType,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn create_relationship_constraint(
            &mut self,
            _name: &str,
            _edge_type: &str,
            _prop: &str,
            _ctype: $crate::graph::constraint::ConstraintType,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn drop_constraint(&mut self, _name: &str) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn create_edge_index(
            &mut self,
            _name: &str,
            _edge_type: &str,
            _prop: ::std::option::Option<&str>,
        ) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }

        fn drop_edge_index(&mut self, _name: &str) -> $crate::error::Result<()> {
            Err($crate::error::Error::ReadOnlyTransaction)
        }
    };
}

/// Implements every write method of [`GraphView`](crate::tx::view::GraphView) by forwarding to
/// `$inner`, an expression over `$self` yielding `&mut impl GraphView`:
/// `forward_graph_view_writes!(self => &mut self.tx);`.
#[macro_export]
macro_rules! forward_graph_view_writes {
    ($self:ident => $inner:expr) => {
        fn create_node(
            &mut $self,
            labels: &[&str],
            props: $crate::graph::property::PropertyMap,
        ) -> $crate::error::Result<$crate::graph::types::NodeId> {
            $crate::tx::view::GraphView::create_node($inner, labels, props)
        }

        fn create_edge(
            &mut $self,
            src: $crate::graph::types::NodeId,
            tgt: $crate::graph::types::NodeId,
            edge_type: &str,
            props: $crate::graph::property::PropertyMap,
        ) -> $crate::error::Result<$crate::graph::types::EdgeId> {
            $crate::tx::view::GraphView::create_edge($inner, src, tgt, edge_type, props)
        }

        fn update_node_properties(
            &mut $self,
            id: $crate::graph::types::NodeId,
            props: $crate::graph::property::PropertyMap,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::update_node_properties($inner, id, props)
        }

        fn update_edge_properties(
            &mut $self,
            id: $crate::graph::types::EdgeId,
            props: $crate::graph::property::PropertyMap,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::update_edge_properties($inner, id, props)
        }

        fn delete_node(&mut $self, id: $crate::graph::types::NodeId) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::delete_node($inner, id)
        }

        fn delete_node_cascading(
            &mut $self,
            id: $crate::graph::types::NodeId,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::delete_node_cascading($inner, id)
        }

        fn delete_edge(&mut $self, id: $crate::graph::types::EdgeId) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::delete_edge($inner, id)
        }

        fn add_label(
            &mut $self,
            id: $crate::graph::types::NodeId,
            label: &str,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::add_label($inner, id, label)
        }

        fn remove_label(
            &mut $self,
            id: $crate::graph::types::NodeId,
            label: &str,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::remove_label($inner, id, label)
        }

        fn get_node_mut(
            &mut $self,
            id: $crate::graph::types::NodeId,
        ) -> $crate::error::Result<&mut $crate::graph::node::Node> {
            $crate::tx::view::GraphView::get_node_mut($inner, id)
        }

        fn get_edge_mut(
            &mut $self,
            id: $crate::graph::types::EdgeId,
        ) -> $crate::error::Result<&mut $crate::graph::edge::Edge> {
            $crate::tx::view::GraphView::get_edge_mut($inner, id)
        }

        fn create_index(
            &mut $self,
            name: &str,
            label: &str,
            prop: &str,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::create_index($inner, name, label, prop)
        }

        fn create_composite_index(
            &mut $self,
            name: &str,
            label: &str,
            properties: &[::std::string::String],
            is_relationship: bool,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::create_composite_index($inner, name, label, properties, is_relationship)
        }

        fn drop_index(&mut $self, name: &str) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::drop_index($inner, name)
        }

        fn create_vector_index(
            &mut $self,
            name: &str,
            label: &str,
            property: &str,
            dim: usize,
            sim: $crate::graph::vector::Similarity,
            params: $crate::graph::vector::HnswParams,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::create_vector_index($inner, name, label, property, dim, sim, params)
        }

        fn drop_vector_index(&mut $self, name: &str) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::drop_vector_index($inner, name)
        }

        fn create_fulltext_index(
            &mut $self,
            name: &str,
            labels: &[&str],
            properties: &[&str],
            analyzer: $crate::graph::fulltext::Analyzer,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::create_fulltext_index($inner, name, labels, properties, analyzer)
        }

        fn drop_fulltext_index(&mut $self, name: &str) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::drop_fulltext_index($inner, name)
        }

        fn create_point_index(
            &mut $self,
            name: &str,
            label: &str,
            property: &str,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::create_point_index($inner, name, label, property)
        }

        fn drop_point_index(&mut $self, name: &str) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::drop_point_index($inner, name)
        }

        fn create_text_index(
            &mut $self,
            name: &str,
            label: &str,
            property: &str,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::create_text_index($inner, name, label, property)
        }

        fn drop_text_index(&mut $self, name: &str) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::drop_text_index($inner, name)
        }

        fn unindex_node(&mut $self, id: $crate::graph::types::NodeId) {
            $crate::tx::view::GraphView::unindex_node($inner, id)
        }

        fn index_node(&mut $self, id: $crate::graph::types::NodeId) {
            $crate::tx::view::GraphView::index_node($inner, id)
        }

        fn unindex_node_prop(&mut $self, id: $crate::graph::types::NodeId, key: &str) {
            $crate::tx::view::GraphView::unindex_node_prop($inner, id, key)
        }

        fn index_node_prop(&mut $self, id: $crate::graph::types::NodeId, key: &str) {
            $crate::tx::view::GraphView::index_node_prop($inner, id, key)
        }

        fn unindex_edge_prop(&mut $self, id: $crate::graph::types::EdgeId, key: &str) {
            $crate::tx::view::GraphView::unindex_edge_prop($inner, id, key)
        }

        fn index_edge_prop(&mut $self, id: $crate::graph::types::EdgeId, key: &str) {
            $crate::tx::view::GraphView::index_edge_prop($inner, id, key)
        }

        fn unindex_edge(&mut $self, id: $crate::graph::types::EdgeId) {
            $crate::tx::view::GraphView::unindex_edge($inner, id)
        }

        fn index_edge(&mut $self, id: $crate::graph::types::EdgeId) {
            $crate::tx::view::GraphView::index_edge($inner, id)
        }

        fn create_composite_constraint(
            &mut $self,
            name: &str,
            label: &str,
            properties: &[::std::string::String],
            ctype: $crate::graph::constraint::ConstraintType,
            is_relationship: bool,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::create_composite_constraint($inner, name, label, properties, ctype, is_relationship)
        }

        fn intern_property_key(&mut $self, key: &str) -> $crate::graph::types::PropertyKeyId {
            $crate::tx::view::GraphView::intern_property_key($inner, key)
        }

        fn analyze_statistics(&mut $self, labels: &[::std::string::String]) {
            $crate::tx::view::GraphView::analyze_statistics($inner, labels)
        }

        fn create_constraint(
            &mut $self,
            name: &str,
            label: &str,
            prop: &str,
            ctype: $crate::graph::constraint::ConstraintType,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::create_constraint($inner, name, label, prop, ctype)
        }

        fn create_relationship_constraint(
            &mut $self,
            name: &str,
            edge_type: &str,
            prop: &str,
            ctype: $crate::graph::constraint::ConstraintType,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::create_relationship_constraint($inner, name, edge_type, prop, ctype)
        }

        fn drop_constraint(&mut $self, name: &str) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::drop_constraint($inner, name)
        }

        fn create_edge_index(
            &mut $self,
            name: &str,
            edge_type: &str,
            prop: ::std::option::Option<&str>,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::create_edge_index($inner, name, edge_type, prop)
        }

        fn drop_edge_index(&mut $self, name: &str) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::drop_edge_index($inner, name)
        }

        fn create_node_with_id(
            &mut $self,
            id: $crate::graph::types::NodeId,
            labels: &[&str],
            props: $crate::graph::property::PropertyMap,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::create_node_with_id($inner, id, labels, props)
        }

        fn create_edge_with_id(
            &mut $self,
            id: $crate::graph::types::EdgeId,
            src: $crate::graph::types::NodeId,
            tgt: $crate::graph::types::NodeId,
            edge_type: &str,
            props: $crate::graph::property::PropertyMap,
        ) -> $crate::error::Result<()> {
            $crate::tx::view::GraphView::create_edge_with_id($inner, id, src, tgt, edge_type, props)
        }
    };
}
