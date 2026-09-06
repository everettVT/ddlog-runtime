// Kiveris et al., Connected Components in MapReduce and Beyond (2014).
// Large-star rewires larger neighbors to the closed-neighborhood minimum.
// Small-star rewires smaller neighbors AND their center to that minimum.
// Each phase replaces the edge set. Original edges enter at iteration zero;
// Variable::new_from inside Iterate retains the history needed to propagate
// later source insertions and deletions through the same iterative computation.
use differential_dataflow::collection::Collection;
use differential_dataflow::lattice::Lattice;
use differential_dataflow::operators::{Iterate, Reduce, Threshold};
use timely::dataflow::scopes::Scope;

pub fn LargeSmallStar<S, V, N, E, NF, EF, LF>(
    vertices: &Collection<S, V, Weight>,
    unpack_vertex: NF,
    vertex: fn(&N) -> i64,
    edges: &Collection<S, V, Weight>,
    unpack_edge: EF,
    from: fn(&E) -> i64,
    to: fn(&E) -> i64,
    pack_label: LF,
) -> Collection<S, V, Weight>
where
    S: Scope,
    S::Timestamp: Lattice + Ord,
    V: differential_dataflow::Data,
    N: differential_dataflow::ExchangeData,
    E: differential_dataflow::ExchangeData,
    NF: Fn(V) -> N + 'static,
    EF: Fn(V) -> E + 'static,
    LF: Fn(ddlog_std::tuple2<i64, i64>) -> V + 'static,
{
    let pairs = edges
        .map(move |value| {
            let edge = unpack_edge(value);
            let (u, v) = (from(&edge), to(&edge));
            if u > v {
                (u, v)
            } else {
                (v, u)
            }
        })
        .distinct_core::<Weight>();
    let nodes = vertices
        .map(move |value| vertex(&unpack_vertex(value)))
        .concat(&pairs.flat_map(|(u, v)| vec![u, v]))
        .distinct_core::<Weight>();
    let stars = pairs.filter(|(u, v)| u != v).iterate(|current| {
        let symmetric = current.concat(&current.map(|(u, v)| (v, u)));
        let large = symmetric
            .reduce(|center, neighbors, output| {
                // reduce orders values; every integrated edge has set semantics.
                let minimum = (*center).min(*neighbors[0].0);
                for (neighbor, _) in neighbors {
                    if **neighbor > *center {
                        output.push(((**neighbor, minimum), 1));
                    }
                }
            })
            .map(|(_, edge)| edge)
            .distinct_core::<Weight>();
        // Every large-star edge already points from larger to smaller.
        // Keep the orientation explicit to document the small-star map phase.
        large
            .map(|(u, v)| if u > v { (u, v) } else { (v, u) })
            .reduce(|center, neighbors, output| {
                let minimum = (*center).min(*neighbors[0].0);
                if *center != minimum {
                    output.push(((*center, minimum), 1));
                }
                for (neighbor, _) in neighbors {
                    if **neighbor != minimum {
                        output.push(((**neighbor, minimum), 1));
                    }
                }
            })
            .map(|(_, edge)| edge)
            // Consolidation is required at feedback: cancelling differences
            // must dissipate, otherwise an empty logical delta can circulate.
            .distinct_core::<Weight>()
    });
    stars
        .concat(&nodes.map(|node| (node, node)))
        .reduce(|_, labels, output| output.push((*labels[0].0, 1)))
        .map(move |(node, label)| pack_label(ddlog_std::tuple2(node, label)))
}
