use fxhash::FxHashMap;
use heed::{BytesDecode, RoTxn};
use roaring::{MultiOps, RoaringBitmap};
use std::collections::{HashMap, HashSet, VecDeque};

use super::db_cache::DatabaseCache;
use super::query_term::{LocatedQueryTerm, QueryTerm, WordDerivations};
use super::QueryGraph;
use crate::{Index, Result, RoaringBitmapCodec};

// TODO: manual performance metrics: access to DB, bitmap deserializations/operations, etc.

// TODO: reuse NodeDocidsCache in between calls to resolve_query_graph
#[derive(Default)]
pub struct NodeDocIdsCache {
    pub cache: FxHashMap<u32, RoaringBitmap>,
}
impl NodeDocIdsCache {
    fn get_docids<'cache, 'transaction>(
        &'cache mut self,
        index: &Index,
        txn: &'transaction RoTxn,
        db_cache: &mut DatabaseCache<'transaction>,
        term: &QueryTerm,
        node_idx: u32,
    ) -> Result<&'cache RoaringBitmap> {
        if self.cache.contains_key(&node_idx) {
            return Ok(&self.cache[&node_idx]);
        };
        let docids = match term {
            QueryTerm::Phrase(_) => {
                todo!("resolve phrase")
            }
            QueryTerm::Word {
                derivations:
                    WordDerivations { original, zero_typo, one_typo, two_typos, use_prefix_db },
            } => {
                let derivations_docids = {
                    let mut or_docids = vec![];
                    for word in zero_typo.iter().chain(one_typo.iter()).chain(two_typos.iter()) {
                        if let Some(word_docids) = db_cache.get_word_docids(index, txn, word)? {
                            or_docids.push(word_docids);
                        }
                    }
                    if *use_prefix_db {
                        if let Some(prefix_docids) =
                            db_cache.get_prefix_docids(index, txn, original.as_str())?
                        {
                            or_docids.push(prefix_docids);
                        }
                    }
                    or_docids
                };
                let derivations_iter = derivations_docids
                    .into_iter()
                    .map(|slice| RoaringBitmapCodec::bytes_decode(slice).unwrap());
                MultiOps::union(derivations_iter)
                // TODO: if `or` is empty, register that somewhere, and immediately return an empty bitmap
                // On the other hand, `or` *cannot* be empty, only its intersection with the universe can
                //
                // TODO: Or we don't do anything and accumulate all these operations in a tree of operations
                // between frozen roaring bitmap that is resolved only at the very end
            }
        };
        let _ = self.cache.insert(node_idx, docids);
        let docids = &self.cache[&node_idx];
        Ok(docids)
    }
}

pub fn resolve_query_graph<'transaction>(
    index: &Index,
    txn: &'transaction RoTxn,
    db_cache: &mut DatabaseCache<'transaction>,
    node_docids_cache: &mut NodeDocIdsCache,
    q: &QueryGraph,
    universe: &RoaringBitmap,
) -> Result<RoaringBitmap> {
    // TODO: there is definitely a faster way to compute this big
    // roaring bitmap expression

    // resolve_query_graph_rec(index, txn, q, q.root_node, &mut docids, &mut cache)?;

    let mut nodes_resolved = RoaringBitmap::new();
    // TODO: should be given as an argument and kept between invocations of resolve query graph
    let mut path_nodes_docids = vec![RoaringBitmap::new(); q.nodes.len()];

    let mut next_nodes_to_visit = VecDeque::new();
    next_nodes_to_visit.push_front(q.root_node);

    while let Some(node) = next_nodes_to_visit.pop_front() {
        let predecessors = &q.edges[node as usize].predecessors;
        if !predecessors.is_subset(&nodes_resolved) {
            next_nodes_to_visit.push_back(node);
            continue;
        }
        // Take union of all predecessors
        let predecessors_iter = predecessors.iter().map(|p| &path_nodes_docids[p as usize]);
        let predecessors_docids = MultiOps::union(predecessors_iter);

        let n = &q.nodes[node as usize];
        // println!("resolving {node} {n:?}, predecessors: {predecessors:?}, their docids: {predecessors_docids:?}");
        let node_docids = match n {
            super::QueryNode::Term(located_term) => {
                let term = &located_term.value;
                let derivations_docids =
                    node_docids_cache.get_docids(index, txn, db_cache, term, node)?;
                predecessors_docids & derivations_docids
            }
            super::QueryNode::Deleted => {
                panic!()
            }
            super::QueryNode::Start => universe.clone(),
            super::QueryNode::End => {
                return Ok(predecessors_docids);
            }
        };
        nodes_resolved.insert(node);
        path_nodes_docids[node as usize] = node_docids;

        for succ in q.edges[node as usize].successors.iter() {
            if !next_nodes_to_visit.contains(&succ) && !nodes_resolved.contains(succ) {
                next_nodes_to_visit.push_back(succ);
            }
        }
        // This is currently slow but could easily be implemented very efficiently
        for prec in q.edges[node as usize].predecessors.iter() {
            if q.edges[prec as usize].successors.is_subset(&nodes_resolved) {
                path_nodes_docids[prec as usize].clear();
            }
        }
        // println!("cached docids: {nodes_docids:?}");
    }

    panic!()
}

#[cfg(test)]
mod tests {
    use charabia::Tokenize;

    use super::resolve_query_graph;
    use crate::db_snap;
    use crate::index::tests::TempIndex;
    use crate::new::db_cache::DatabaseCache;
    use crate::new::resolve_query_graph::NodeDocIdsCache;
    use crate::search::new::query_term::{word_derivations, LocatedQueryTerm};
    use crate::search::new::QueryGraph;

    #[test]
    fn test_resolve_query_graph() {
        let index = TempIndex::new();

        index
            .update_settings(|s| {
                s.set_searchable_fields(vec!["text".to_owned()]);
            })
            .unwrap();

        index
            .add_documents(documents!([
                {"id": 0, "text": "0"},
                {"id": 1, "text": "1"},
                {"id": 2, "text": "2"},
                {"id": 3, "text": "3"},
                {"id": 4, "text": "4"},
                {"id": 5, "text": "5"},
                {"id": 6, "text": "6"},
                {"id": 7, "text": "7"},
                {"id": 8, "text": "0 1 2 3 4 5 6 7"},
                {"id": 9, "text": "7 6 5 4 3 2 1 0"},
                {"id": 10, "text": "01 234 56 7"},
                {"id": 11, "text": "7 56 0 1 23 5 4"},
                {"id": 12, "text": "0 1 2 3 4 5 6"},
                {"id": 13, "text": "01 23 4 5 7"},
            ]))
            .unwrap();
        db_snap!(index, word_docids, @"7512d0b80659f6bf37d98b374ada8098");

        let txn = index.read_txn().unwrap();
        let mut db_cache = DatabaseCache::default();
        let fst = index.words_fst(&txn).unwrap();
        let query = LocatedQueryTerm::from_query(
            "no 0 1 2 3 no 4 5 6 7".tokenize(),
            None,
            |word, is_prefix| {
                word_derivations(
                    &index,
                    &txn,
                    word,
                    if word.len() < 3 {
                        0
                    } else if word.len() < 6 {
                        1
                    } else {
                        2
                    },
                    is_prefix,
                    &fst,
                )
            },
        )
        .unwrap();
        let graph = QueryGraph::from_query(&index, &txn, &mut db_cache, query).unwrap();
        println!("{}", graph.graphviz());
        let mut node_docids_cache = NodeDocIdsCache::default();
        let universe = index.documents_ids(&txn).unwrap();
        insta::assert_debug_snapshot!(universe, @"RoaringBitmap<[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]>");
        let docids = resolve_query_graph(
            &index,
            &txn,
            &mut db_cache,
            &mut node_docids_cache,
            &graph,
            &universe,
        )
        .unwrap();
        insta::assert_debug_snapshot!(docids, @"RoaringBitmap<[8, 9, 11]>");

        // TODO: test with a reduced universe
    }
}