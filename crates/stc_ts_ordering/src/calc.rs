use std::hash::Hash;

use fxhash::{FxHashMap, FxHashSet};
use petgraph::EdgeDirection::Outgoing;
use swc_fast_graph::digraph::FastDiGraphMap;
use swc_graph_analyzer::{DepGraph, GraphAnalyzer};
use tracing::{span, trace, Level};

pub(crate) struct Deps<'a, I>
where
    I: Eq + Hash,
{
    pub declared_by: &'a FxHashMap<I, Vec<usize>>,
    pub used_by_idx: &'a FxHashMap<usize, FxHashSet<I>>,
}

/// Returns `(cycles, graph)`.
pub(crate) fn to_graph<I>(deps: &Deps<I>, len: usize) -> (Vec<Vec<usize>>, FastDiGraphMap<usize, ()>)
where
    I: Eq + Hash,
{
    let mut a = GraphAnalyzer::new(deps);

    for idx in 0..len {
        a.load(idx);
    }

    let res = a.into_result();

    (res.cycles, res.graph)
}

impl<I> DepGraph for Deps<'_, I>
where
    I: Eq + Hash,
{
    type ModuleId = usize;

    fn deps_of(&self, module_id: Self::ModuleId) -> Vec<Self::ModuleId> {
        let used = self.used_by_idx.get(&module_id);
        let used = match used {
            Some(v) => v,
            None => return Default::default(),
        };

        let mut buf = vec![];

        for used in used {
            let deps = self.declared_by.get(used);

            if let Some(deps) = deps {
                buf.extend(deps.iter());
            }
        }

        buf
    }
}

pub(crate) fn calc_order(cycles: Vec<Vec<usize>>, graph: &mut FastDiGraphMap<usize, ()>, len: usize) -> Vec<Vec<usize>> {
    let mut done = FxHashSet::default();
    let mut orders = vec![];

    'outer: loop {
        if (0..len).all(|idx| done.contains(&idx)) {
            break;
        }

        for idx in 0..len {
            if cycles.iter().any(|v| v.contains(&idx)) {
                // Skip `idx` if it's in any cycle.
                continue;
            }

            let next = calc_one(&done, &cycles, graph, idx);

            done.extend(next.iter().copied());

            if !next.is_empty() {
                orders.push(next);
                continue 'outer;
            }
        }

        // We handle cycles here.
        for idx in 0..len {
            if cycles.iter().all(|v| !v.contains(&idx)) {
                // Skip `idx` if it's not in any cycle.
                continue;
            }

            let next = calc_one(&done, &cycles, graph, idx);

            done.extend(next.iter().copied());

            if !next.is_empty() {
                orders.push(next);
                continue 'outer;
            }
        }
    }

    orders
}

fn calc_one(done: &FxHashSet<usize>, cycles: &[Vec<usize>], graph: &mut FastDiGraphMap<usize, ()>, idx: usize) -> Vec<usize> {
    if cfg!(debug_assertions) && cfg!(feature = "debug") {
        trace!("calc_one(idx = {:?})", idx);
    }

    if done.contains(&idx) {
        return vec![];
    }

    if let Some(cycle) = cycles.iter().find(|v| v.contains(&idx)) {
        if cfg!(debug_assertions) && cfg!(feature = "debug") {
            trace!("Cycle: {:?}", cycle);
        }

        return cycle.clone();
    }

    let deps = graph.neighbors_directed(idx, Outgoing).collect::<Vec<_>>();

    for dep in deps {
        let _tracing = if cfg!(feature = "debug") {
            Some(span!(Level::ERROR, "deps_of({})", idx).entered())
        } else {
            None
        };

        let v = calc_one(done, cycles, graph, dep);
        if v.is_empty() {
            continue;
        }
        return v;
    }

    if cfg!(debug_assertions) && cfg!(feature = "debug") {
        trace!("Done: {:?}", idx);
    }

    vec![idx]
}
