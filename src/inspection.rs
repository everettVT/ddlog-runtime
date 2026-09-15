//! Versioned inspection contracts. Layout is client-owned; native IDs are opaque.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
pub const INSPECTION_SCHEMA_VERSION: u32 = 1;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub repository: String,
    pub revision: String,
    pub source: Option<SourceLocation>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLocation {
    pub path: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortDirection {
    Input,
    Output,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePort {
    pub operator_id: String,
    pub index: u64,
}
/// Snapshot-time port binding: the one member operator whose `debug` matches
/// `debug_pattern` (a regular expression), at native port `index`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeMatch {
    pub debug_pattern: String,
    pub index: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredPort {
    pub id: String,
    pub name: String,
    pub direction: PortDirection,
    pub data_type: String,
    #[serde(default)]
    pub native_ports: Vec<NativePort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_match: Option<NativeMatch>,
}
/// Snapshot-time membership: `scope_name` claims every native scope of that
/// name with its whole subtree; `debug_pattern` (a regular expression over the
/// operator `debug` text) claims matching operators nobody else claimed first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_pattern: Option<String>,
    /// After matching, unclaimed operators adopt the majority group of their
    /// channel neighbours among propagating groups (A13), until stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub propagate: Option<bool>,
}
/// `module`: a composition block (A13). Blocks skip the layout invariants,
/// may nest by id (`parent/child`) and are never drawn as boxes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupKind {
    Module,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredGroup {
    pub id: String,
    pub name: String,
    pub member_key: String,
    pub provenance: Provenance,
    #[serde(default, rename = "memberIds")]
    pub member_ids: Vec<String>,
    #[serde(default)]
    pub ports: Vec<AuthoredPort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_match: Option<MemberMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<GroupKind>,
}
impl AuthoredGroup {
    pub fn is_module(&self) -> bool {
        self.kind == Some(GroupKind::Module)
    }
    fn propagates(&self) -> bool {
        self.member_match
            .as_ref()
            .is_some_and(|m| m.propagate == Some(true))
    }
}
/// Per-group attribution counts of one resolution: `matched` operators were
/// claimed by ids, scope or pattern, `propagated` ones adopted through channel
/// neighbours. `unattributed` operators belong to no group.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupReport {
    pub matched: u64,
    pub propagated: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingReport {
    pub groups: BTreeMap<String, GroupReport>,
    pub unattributed: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectionMetadata {
    pub schema_version: u32,
    #[serde(default, rename = "authoredGroups")]
    pub authored_groups: Vec<AuthoredGroup>,
    /// Present on resolved metadata only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping_report: Option<MappingReport>,
}
impl Default for InspectionMetadata {
    fn default() -> Self {
        Self {
            schema_version: 1,
            authored_groups: vec![],
            mapping_report: None,
        }
    }
}
/// Neighbour propagation stops after this many rounds even if still changing.
pub const PROPAGATION_ROUNDS: usize = 16;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeOperator {
    pub id: String,
    pub operator_id: u64,
    pub worker: u64,
    pub address: Vec<u64>,
    pub name: String,
    pub debug: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeChannel {
    pub id: String,
    pub channel_id: u64,
    pub source_address: Vec<u64>,
    pub target_address: Vec<u64>,
    pub worker: u64,
    pub scope: Vec<u64>,
    pub source: String,
    pub target: String,
    pub source_port: u64,
    pub target_port: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeGraph {
    pub nodes: Vec<NativeOperator>,
    pub edges: Vec<NativeChannel>,
}
fn nonempty(s: &str, label: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        Err(format!("{label} must not be empty"))
    } else {
        Ok(())
    }
}
fn unique(set: &mut BTreeSet<String>, id: &str, label: &str) -> Result<(), String> {
    nonempty(id, label)?;
    if !set.insert(id.to_owned()) {
        return Err(format!("duplicate {label}: {id}"));
    }
    Ok(())
}
fn pattern(text: &str, label: &str) -> Result<regex::Regex, String> {
    nonempty(text, label)?;
    if text.len() > 1024 {
        return Err(format!("{label} exceeds 1024 bytes"));
    }
    regex::RegexBuilder::new(text)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| format!("invalid {label}: {e}"))
}
/// Parent/child containment by worker and address prefix, as the layout does.
struct Family<'a> {
    graph: &'a NativeGraph,
    parent: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
}
impl<'a> Family<'a> {
    fn new(graph: &'a NativeGraph) -> Self {
        let mut parent = vec![None; graph.nodes.len()];
        let mut children = vec![vec![]; graph.nodes.len()];
        for (index, node) in graph.nodes.iter().enumerate() {
            let mut best: Option<usize> = None;
            for (candidate, other) in graph.nodes.iter().enumerate() {
                if other.worker != node.worker
                    || other.address.is_empty()
                    || other.address.len() >= node.address.len()
                    || !node.address.starts_with(&other.address)
                {
                    continue;
                }
                if best.is_none_or(|b| other.address.len() > graph.nodes[b].address.len()) {
                    best = Some(candidate);
                }
            }
            parent[index] = best;
            if let Some(best) = best {
                children[best].push(index);
            }
        }
        Self {
            graph,
            parent,
            children,
        }
    }
    fn subtree(&self, index: usize) -> Vec<usize> {
        let scope = &self.graph.nodes[index];
        (0..self.graph.nodes.len())
            .filter(|&i| {
                i == index
                    || (self.graph.nodes[i].worker == scope.worker
                        && self.graph.nodes[i].address.len() > scope.address.len()
                        && self.graph.nodes[i].address.starts_with(&scope.address))
            })
            .collect()
    }
}
impl InspectionMetadata {
    /// Validate definitions before registration without launching a process.
    /// Match-based groups and ports are checked for shape only; their
    /// membership is resolved against a live graph by [`Self::resolve`].
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != INSPECTION_SCHEMA_VERSION {
            return Err("unsupported inspection schema version".into());
        }
        let (mut groups, mut keys, mut members) =
            (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        for g in &self.authored_groups {
            unique(&mut groups, &g.id, "group id")?;
            unique(&mut keys, &g.member_key, "member key")?;
            nonempty(&g.name, "group name")?;
            nonempty(&g.provenance.repository, "repository")?;
            nonempty(&g.provenance.revision, "revision")?;
            if let Some(s) = &g.provenance.source {
                nonempty(&s.path, "source path")?;
                if s.start_line == 0
                    || s.start_column == 0
                    || s.end_line == 0
                    || s.end_column == 0
                    || (s.end_line, s.end_column) < (s.start_line, s.start_column)
                {
                    return Err("invalid source location".into());
                }
            }
            if g.is_module() && g.member_match.is_none() && g.member_ids.is_empty() {
                return Err(format!(
                    "group {}: a module group needs member_match or memberIds",
                    g.id
                ));
            }
            if let Some(m) = &g.member_match {
                if m.scope_name.is_none() && m.debug_pattern.is_none() {
                    return Err(format!(
                        "group {}: member_match needs scope_name or debug_pattern",
                        g.id
                    ));
                }
                if let Some(name) = &m.scope_name {
                    nonempty(name, "scope_name")?;
                }
                if let Some(text) = &m.debug_pattern {
                    pattern(text, "debug_pattern")?;
                }
            }
            // Boxes are disjoint; a block's resolved members repeat those of
            // its nested blocks, so blocks are only checked within themselves.
            let mut own = BTreeSet::new();
            for id in &g.member_ids {
                unique(
                    if g.is_module() {
                        &mut own
                    } else {
                        &mut members
                    },
                    id,
                    "native group member",
                )?;
            }
            let mut ports = BTreeSet::new();
            for p in &g.ports {
                unique(&mut ports, &p.id, "port id")?;
                nonempty(&p.name, "port name")?;
                nonempty(&p.data_type, "port type")?;
                if let Some(m) = &p.native_match {
                    pattern(&m.debug_pattern, "native_match debug_pattern")?;
                }
                let mut mappings = BTreeSet::new();
                for n in &p.native_ports {
                    if g.member_match.is_none() && !g.member_ids.contains(&n.operator_id) {
                        return Err("port maps outside authored group".into());
                    }
                    if !mappings.insert((&n.operator_id, n.index)) {
                        return Err("duplicate native port mapping".into());
                    }
                }
            }
        }
        Ok(())
    }
    /// Resolve match-based membership against a live graph, deterministically:
    /// groups in declaration order; explicit `memberIds` and `scope_name`
    /// matches claim first (a matched scope claims its whole subtree), then
    /// `debug_pattern` matches claim only unclaimed operators, then unclaimed
    /// operators adopt the plurality group of their channel neighbours among
    /// `propagate` groups (ties to the lowest group index; synchronous rounds,
    /// at most [`PROPAGATION_ROUNDS`]). Zero matches, a port matching zero or
    /// several operators, a split native scope or members under different
    /// native parents are mapping errors naming the group; `kind: module`
    /// groups skip the zero-match and layout checks (a block may be empty) and
    /// a block's members include those of its nested `<id>/…` blocks.
    pub fn resolve(&self, graph: &NativeGraph) -> Result<InspectionMetadata, String> {
        self.validate()?;
        let family = Family::new(graph);
        let index_of: BTreeMap<&str, usize> = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();
        let mut owner: Vec<Option<usize>> = vec![None; graph.nodes.len()];
        let mut members: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); self.authored_groups.len()];
        let mut claim =
            |group: usize, node: usize, owner: &mut Vec<Option<usize>>| match owner[node] {
                Some(other) if other != group => Err(format!(
                    "group {}: native operator {} is already claimed by group {}",
                    self.authored_groups[group].id,
                    graph.nodes[node].id,
                    self.authored_groups[other].id
                )),
                _ => {
                    owner[node] = Some(group);
                    members[group].insert(node);
                    Ok(())
                }
            };
        for (gi, g) in self.authored_groups.iter().enumerate() {
            for id in &g.member_ids {
                let node = *index_of
                    .get(id.as_str())
                    .ok_or_else(|| format!("group {}: unknown native operator: {id}", g.id))?;
                claim(gi, node, &mut owner)?;
            }
            if let Some(name) = g
                .member_match
                .as_ref()
                .and_then(|m| m.scope_name.as_deref())
            {
                let scopes: Vec<usize> = (0..graph.nodes.len())
                    .filter(|&i| graph.nodes[i].name == name && !family.children[i].is_empty())
                    .collect();
                if scopes.is_empty() {
                    return Err(format!(
                        "group {}: scope_name {name:?} matched no native scope",
                        g.id
                    ));
                }
                for scope in scopes {
                    for node in family.subtree(scope) {
                        claim(gi, node, &mut owner)?;
                    }
                }
            }
        }
        for (gi, g) in self.authored_groups.iter().enumerate() {
            let Some(text) = g
                .member_match
                .as_ref()
                .and_then(|m| m.debug_pattern.as_deref())
            else {
                continue;
            };
            let regex = pattern(text, "debug_pattern")?;
            let matched: Vec<usize> = (0..graph.nodes.len())
                .filter(|&i| regex.is_match(&graph.nodes[i].debug))
                .collect();
            if matched.is_empty() && !g.is_module() {
                return Err(format!(
                    "group {}: debug_pattern {text:?} matched no native operator",
                    g.id
                ));
            }
            for node in matched {
                if owner[node].is_none() {
                    claim(gi, node, &mut owner)?;
                }
            }
        }
        let mut report = MappingReport::default();
        for (gi, g) in self.authored_groups.iter().enumerate() {
            report.groups.insert(
                g.id.clone(),
                GroupReport {
                    matched: members[gi].len() as u64,
                    propagated: 0,
                },
            );
        }
        let propagating: Vec<bool> = self
            .authored_groups
            .iter()
            .map(|g| g.propagates())
            .collect();
        if propagating.iter().any(|&p| p) {
            let mut neighbours: Vec<Vec<usize>> = vec![vec![]; graph.nodes.len()];
            for e in &graph.edges {
                if let (Some(&s), Some(&t)) = (
                    index_of.get(e.source.as_str()),
                    index_of.get(e.target.as_str()),
                ) {
                    if s != t {
                        neighbours[s].push(t);
                        neighbours[t].push(s);
                    }
                }
            }
            for _ in 0..PROPAGATION_ROUNDS {
                let adoptions: Vec<(usize, usize)> = (0..graph.nodes.len())
                    .filter(|&node| owner[node].is_none())
                    .filter_map(|node| {
                        let mut votes: BTreeMap<usize, usize> = BTreeMap::new();
                        for &other in &neighbours[node] {
                            if let Some(g) = owner[other].filter(|&g| propagating[g]) {
                                *votes.entry(g).or_default() += 1;
                            }
                        }
                        // BTreeMap iterates ascending, so `>` keeps the lowest index on ties.
                        votes
                            .iter()
                            .fold(None, |best: Option<(usize, usize)>, (&g, &n)| {
                                if best.is_none_or(|(_, m)| n > m) {
                                    Some((g, n))
                                } else {
                                    best
                                }
                            })
                            .map(|(g, _)| (node, g))
                    })
                    .collect();
                if adoptions.is_empty() {
                    break;
                }
                for (node, g) in adoptions {
                    owner[node] = Some(g);
                    members[g].insert(node);
                    report
                        .groups
                        .get_mut(&self.authored_groups[g].id)
                        .unwrap()
                        .propagated += 1;
                }
            }
        }
        report.unattributed = owner.iter().filter(|o| o.is_none()).count() as u64;
        // A block's members include its nested blocks' members (`parent/child`).
        let blocks: Vec<BTreeSet<usize>> = (0..self.authored_groups.len())
            .map(|gi| {
                let g = &self.authored_groups[gi];
                if !g.is_module() {
                    return members[gi].clone();
                }
                let prefix = format!("{}/", g.id);
                self.authored_groups
                    .iter()
                    .enumerate()
                    .filter(|(hi, h)| *hi == gi || (h.is_module() && h.id.starts_with(&prefix)))
                    .flat_map(|(hi, _)| members[hi].iter().copied())
                    .collect()
            })
            .collect();
        let mut resolved = self.clone();
        resolved.mapping_report = Some(report);
        for (gi, g) in resolved.authored_groups.iter_mut().enumerate() {
            let set = &blocks[gi];
            if set.is_empty() && !g.is_module() {
                return Err(format!("group {}: no native members", g.id));
            }
            if !g.is_module() {
                for &node in set {
                    if let Some(child) = family.children[node]
                        .iter()
                        .find(|child| !set.contains(child))
                    {
                        return Err(format!(
                            "group {}: would split native scope {} (child {} is not a member)",
                            g.id, graph.nodes[node].id, graph.nodes[*child].id
                        ));
                    }
                }
                let parents: BTreeSet<Option<usize>> = set
                    .iter()
                    .map(|&node| family.parent[node])
                    .filter(|parent| parent.is_none_or(|p| !set.contains(&p)))
                    .collect();
                if parents.len() > 1 {
                    return Err(format!(
                        "group {}: members cross native parent boundaries",
                        g.id
                    ));
                }
            }
            g.member_ids = set
                .iter()
                .map(|&node| graph.nodes[node].id.clone())
                .collect();
            for p in &mut g.ports {
                if let Some(m) = &p.native_match {
                    let regex = pattern(&m.debug_pattern, "native_match debug_pattern")?;
                    let hits: Vec<usize> = set
                        .iter()
                        .copied()
                        .filter(|&node| regex.is_match(&graph.nodes[node].debug))
                        .collect();
                    if hits.len() != 1 {
                        return Err(format!(
                            "group {}: port {} native_match matched {} member operators, expected exactly one",
                            g.id, p.id, hits.len()
                        ));
                    }
                    let mapping = NativePort {
                        operator_id: graph.nodes[hits[0]].id.clone(),
                        index: m.index,
                    };
                    if !p.native_ports.contains(&mapping) {
                        p.native_ports.push(mapping);
                    }
                }
                if let Some(n) = p
                    .native_ports
                    .iter()
                    .find(|n| !g.member_ids.contains(&n.operator_id))
                {
                    return Err(format!(
                        "group {}: port {} maps outside the group ({})",
                        g.id, p.id, n.operator_id
                    ));
                }
            }
        }
        resolved.validate_with_graph(graph)?;
        Ok(resolved)
    }
    /// Verify live mappings against actual channels; unconnected ports are unverified.
    pub fn validate_with_graph(&self, graph: &NativeGraph) -> Result<(), String> {
        self.validate()?;
        graph.validate()?;
        for g in &self.authored_groups {
            if graph.nodes.iter().any(|n| n.id == g.id) {
                return Err("authored group id collides with native operator".into());
            }
            for id in &g.member_ids {
                if !graph.nodes.iter().any(|n| &n.id == id) {
                    return Err(format!("unknown native operator: {id}"));
                }
            }
            for p in &g.ports {
                for n in &p.native_ports {
                    if !graph.edges.iter().any(|e| match p.direction {
                        PortDirection::Input => {
                            e.target == n.operator_id && e.target_port == n.index
                        }
                        PortDirection::Output => {
                            e.source == n.operator_id && e.source_port == n.index
                        }
                    }) {
                        return Err("native port is not observed".into());
                    }
                }
            }
        }
        Ok(())
    }
}
impl NativeGraph {
    pub fn validate(&self) -> Result<(), String> {
        let (mut ids, mut addresses, mut channels) =
            (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        for n in &self.nodes {
            unique(&mut ids, &n.id, "operator id")?;
            if n.address.is_empty() || !addresses.insert((n.worker, &n.address)) {
                return Err("empty or duplicate worker address".into());
            }
        }
        for e in &self.edges {
            unique(&mut channels, &e.id, "channel id")?;
            for id in [&e.source, &e.target] {
                let n = self
                    .nodes
                    .iter()
                    .find(|n| &n.id == id)
                    .ok_or_else(|| format!("unknown channel endpoint: {id}"))?;
                if n.worker != e.worker {
                    return Err("channel endpoint worker mismatch".into());
                }
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (InspectionMetadata, NativeGraph) {
        let g = NativeGraph {
            nodes: vec![NativeOperator {
                id: "0:1".into(),
                operator_id: 1,
                worker: 0,
                address: vec![0, 1],
                name: "Input".into(),
                debug: "exact".into(),
            }],
            edges: vec![NativeChannel {
                id: "0:9".into(),
                channel_id: 9,
                source_address: vec![0, 1],
                target_address: vec![0, 1],
                worker: 0,
                scope: vec![0],
                source: "0:1".into(),
                target: "0:1".into(),
                source_port: 3,
                target_port: 4,
            }],
        };
        let m = InspectionMetadata {
            schema_version: 1,
            authored_groups: vec![AuthoredGroup {
                id: "group".into(),
                name: "Group".into(),
                member_key: "root/group".into(),
                provenance: Provenance {
                    repository: "repo".into(),
                    revision: "commit".into(),
                    source: None,
                },
                member_ids: vec!["0:1".into()],
                ports: vec![AuthoredPort {
                    id: "in".into(),
                    name: "Input".into(),
                    direction: PortDirection::Input,
                    data_type: "u64".into(),
                    native_ports: vec![NativePort {
                        operator_id: "0:1".into(),
                        index: 4,
                    }],
                    native_match: None,
                }],
                member_match: None,
                kind: None,
            }],
            mapping_report: None,
        };
        (m, g)
    }
    fn operator(id: &str, address: &[u64], name: &str, debug: &str) -> NativeOperator {
        NativeOperator {
            id: id.into(),
            operator_id: id[2..].parse().unwrap(),
            worker: 0,
            address: address.to_vec(),
            name: name.into(),
            debug: debug.into(),
        }
    }
    fn matched(id: &str, member_match: MemberMatch) -> AuthoredGroup {
        AuthoredGroup {
            id: id.into(),
            name: id.into(),
            member_key: id.into(),
            provenance: Provenance {
                repository: "repo".into(),
                revision: "commit".into(),
                source: None,
            },
            member_ids: vec![],
            ports: vec![],
            member_match: Some(member_match),
            kind: None,
        }
    }
    fn scoped_graph() -> NativeGraph {
        NativeGraph {
            nodes: vec![
                operator("0:0", &[0], "Dataflow", ""),
                operator("0:1", &[0, 1], "Input", "Input { rel: \"R_edge\" }"),
                operator(
                    "0:2",
                    &[0, 2],
                    "large-star",
                    "ApplyTransformer { transformer: \"Star\" } scope",
                ),
                operator(
                    "0:3",
                    &[0, 2, 1],
                    "Map",
                    "ApplyTransformer { transformer: \"Star\" } map",
                ),
                operator(
                    "0:4",
                    &[0, 2, 2],
                    "Reduce",
                    "ApplyTransformer { transformer: \"Star\" }",
                ),
                operator(
                    "0:5",
                    &[0, 3],
                    "Probe",
                    "ApplyTransformer { transformer: \"Star\" }",
                ),
                operator("0:6", &[0, 4], "Map", "other"),
            ],
            edges: vec![NativeChannel {
                id: "0:9".into(),
                channel_id: 9,
                source_address: vec![0, 1],
                target_address: vec![0, 2],
                worker: 0,
                scope: vec![0],
                source: "0:1".into(),
                target: "0:2".into(),
                source_port: 0,
                target_port: 0,
            }],
        }
    }
    #[test]
    fn match_based_groups_validate_by_shape_and_resolve_deterministically() {
        let graph = scoped_graph();
        let mut m = InspectionMetadata::default();
        m.authored_groups.push(matched(
            "phase",
            MemberMatch {
                scope_name: Some("large-star".into()),
                debug_pattern: None,
                propagate: None,
            },
        ));
        m.authored_groups.push(matched(
            "transformer",
            MemberMatch {
                scope_name: None,
                debug_pattern: Some("ApplyTransformer \\{ transformer: \"Star\"".into()),
                propagate: None,
            },
        ));
        m.validate().unwrap();
        let wire = serde_json::to_value(&m).unwrap();
        assert_eq!(
            wire["authoredGroups"][0]["memberIds"],
            serde_json::json!([])
        );
        assert_eq!(
            wire["authoredGroups"][0]["member_match"],
            serde_json::json!({"scope_name":"large-star"})
        );
        assert!(wire["authoredGroups"][0]["ports"]
            .as_array()
            .unwrap()
            .is_empty());
        let resolved = m.resolve(&graph).unwrap();
        // The scope claims its subtree first; the pattern then claims only 0:5.
        assert_eq!(
            resolved.authored_groups[0].member_ids,
            vec!["0:2", "0:3", "0:4"]
        );
        assert_eq!(resolved.authored_groups[1].member_ids, vec!["0:5"]);
        assert_eq!(resolved.resolve(&graph).unwrap(), resolved);
        // Zero matches, split scopes and overlapping claims are named errors.
        let mut zero = m.clone();
        zero.authored_groups[0].member_match = Some(MemberMatch {
            scope_name: Some("missing".into()),
            debug_pattern: None,
            propagate: None,
        });
        assert!(zero.resolve(&graph).unwrap_err().contains("phase"));
        let mut split = InspectionMetadata::default();
        let mut explicit = matched(
            "split",
            MemberMatch {
                scope_name: None,
                debug_pattern: Some("^ApplyTransformer".into()),
                propagate: None,
            },
        );
        explicit.member_ids = vec!["0:3".into()];
        split.authored_groups.push(explicit);
        split.authored_groups.push(matched(
            "rest",
            MemberMatch {
                scope_name: Some("large-star".into()),
                debug_pattern: None,
                propagate: None,
            },
        ));
        assert!(split
            .resolve(&graph)
            .unwrap_err()
            .contains("already claimed"));
        let mut torn = InspectionMetadata::default();
        torn.authored_groups.push(matched(
            "torn",
            MemberMatch {
                scope_name: None,
                debug_pattern: Some("scope$".into()),
                propagate: None,
            },
        ));
        assert!(torn
            .resolve(&graph)
            .unwrap_err()
            .contains("split native scope"));
        let mut crossing = InspectionMetadata::default();
        crossing.authored_groups.push(matched(
            "crossing",
            MemberMatch {
                scope_name: None,
                debug_pattern: Some("R_edge|map$".into()),
                propagate: None,
            },
        ));
        assert!(crossing
            .resolve(&graph)
            .unwrap_err()
            .contains("cross native parent"));
        // Ports resolve to exactly one member operator on an observed channel.
        let mut ported = InspectionMetadata::default();
        let mut group = matched(
            "ported",
            MemberMatch {
                scope_name: Some("large-star".into()),
                debug_pattern: None,
                propagate: None,
            },
        );
        group.ports.push(AuthoredPort {
            id: "in".into(),
            name: "in".into(),
            direction: PortDirection::Input,
            data_type: "edge".into(),
            native_ports: vec![],
            native_match: Some(NativeMatch {
                debug_pattern: "Star".into(),
                index: 0,
            }),
        });
        ported.authored_groups.push(group.clone());
        assert!(ported
            .resolve(&graph)
            .unwrap_err()
            .contains("expected exactly one"));
        ported.authored_groups[0].ports[0].native_match = Some(NativeMatch {
            debug_pattern: "Star".into(),
            index: 0,
        });
        ported.authored_groups[0].member_match = Some(MemberMatch {
            scope_name: Some("large-star".into()),
            debug_pattern: None,
            propagate: None,
        });
        let mut single = graph.clone();
        single.nodes[3].debug = "plain".into();
        single.nodes[4].debug = "plain".into();
        let resolved = ported.resolve(&single).unwrap();
        assert_eq!(
            resolved.authored_groups[0].ports[0].native_ports,
            vec![NativePort {
                operator_id: "0:2".into(),
                index: 0
            }]
        );
        ported.authored_groups[0].ports[0]
            .native_match
            .as_mut()
            .unwrap()
            .index = 7;
        assert!(ported
            .resolve(&single)
            .unwrap_err()
            .contains("not observed"));
        let mut malformed = m;
        malformed.authored_groups[1].member_match = Some(MemberMatch {
            scope_name: None,
            debug_pattern: Some("(".into()),
            propagate: None,
        });
        assert!(malformed.validate().unwrap_err().contains("debug_pattern"));
        malformed.authored_groups[1].member_match = Some(MemberMatch {
            scope_name: None,
            debug_pattern: None,
            propagate: None,
        });
        assert!(malformed.validate().is_err());
    }
    fn channel(id: u64, source: &str, target: &str) -> NativeChannel {
        NativeChannel {
            id: format!("0:{id}"),
            channel_id: id,
            source_address: vec![0, source[2..].parse().unwrap()],
            target_address: vec![0, target[2..].parse().unwrap()],
            worker: 0,
            scope: vec![0],
            source: source.into(),
            target: target.into(),
            source_port: 0,
            target_port: 0,
        }
    }
    fn module(id: &str, pattern: &str) -> AuthoredGroup {
        let mut g = matched(
            id,
            MemberMatch {
                scope_name: None,
                debug_pattern: Some(pattern.into()),
                propagate: Some(true),
            },
        );
        g.kind = Some(GroupKind::Module);
        g
    }
    #[test]
    fn module_groups_propagate_nest_and_report() {
        // 0:1 input → 0:2 (M0) → 0:3 (unnamed) → 0:4 (M1) → 0:5 (unnamed) → 0:6 output;
        // 0:7 is unnamed with M0 and M1 neighbours (tie → lowest index); 0:8 is
        // isolated; 0:9 names a nested composite relation; 0:10 is its module.
        let graph = NativeGraph {
            nodes: vec![
                operator("0:0", &[0], "Dataflow", ""),
                operator("0:1", &[0, 1], "Input", "Input { rel: \"R_Input_rows\" }"),
                operator(
                    "0:2",
                    &[0, 2],
                    "Map",
                    "DistinctRelation { rel: \"R_Module0_echo\" }",
                ),
                operator("0:3", &[0, 3], "FlatMap", "Head { source_pos: Unknown }"),
                operator(
                    "0:4",
                    &[0, 4],
                    "Map",
                    "DistinctRelation { rel: \"R_Module1_echo\" }",
                ),
                operator("0:5", &[0, 5], "FlatMap", "Head { source_pos: Unknown }"),
                operator(
                    "0:6",
                    &[0, 6],
                    "Probe",
                    "ProbeOutput { rel: \"R_Output_copies\" }",
                ),
                operator("0:7", &[0, 7], "Map", "Head { source_pos: Unknown }"),
                operator("0:8", &[0, 8], "Input", "Input { rel: \"__Null\" }"),
                operator(
                    "0:9",
                    &[0, 9],
                    "Map",
                    "DistinctRelation { rel: \"R_Composite2_Output_x\" }",
                ),
                operator(
                    "0:10",
                    &[0, 10],
                    "Map",
                    "DistinctRelation { rel: \"R_Module3_echo\" }",
                ),
            ],
            edges: vec![
                channel(1, "0:1", "0:2"),
                channel(2, "0:2", "0:3"),
                channel(3, "0:3", "0:4"),
                channel(4, "0:4", "0:5"),
                channel(5, "0:5", "0:6"),
                channel(6, "0:2", "0:7"),
                channel(7, "0:7", "0:4"),
                channel(8, "0:9", "0:10"),
            ],
        };
        let mut m = InspectionMetadata::default();
        m.authored_groups.push(module("first", "\\bR_Module0_"));
        m.authored_groups.push(module("second", "\\bR_Module1_"));
        m.authored_groups.push(module("inner", "\\bR_Composite2_"));
        m.authored_groups
            .push(module("inner/leaf", "\\bR_Module3_"));
        m.authored_groups.push(module("$inputs", "\\bR_Input_"));
        m.authored_groups.push(module("$outputs", "\\bR_Output_"));
        m.validate().unwrap();
        let wire = serde_json::to_value(&m).unwrap();
        assert_eq!(wire["authoredGroups"][0]["kind"], "module");
        assert_eq!(wire["authoredGroups"][0]["member_match"]["propagate"], true);
        assert!(wire.get("mapping_report").is_none());
        let resolved = m.resolve(&graph).unwrap();
        let members = |id: &str| -> Vec<String> {
            resolved
                .authored_groups
                .iter()
                .find(|g| g.id == id)
                .unwrap()
                .member_ids
                .clone()
        };
        // 0:3 sees M0 only in round one; 0:5 sees M1; 0:7 ties M0/M1 → M0.
        assert_eq!(members("first"), vec!["0:2", "0:3", "0:7"]);
        assert_eq!(members("second"), vec!["0:4", "0:5"]);
        assert_eq!(members("inner/leaf"), vec!["0:10"]);
        assert_eq!(
            members("inner"),
            vec!["0:9", "0:10"],
            "a block includes its children"
        );
        assert_eq!(members("$inputs"), vec!["0:1"]);
        assert_eq!(members("$outputs"), vec!["0:6"]);
        let report = resolved.mapping_report.clone().unwrap();
        assert_eq!(
            report.unattributed, 2,
            "the root scope and __Null stay outside"
        );
        assert_eq!(
            report.groups["first"],
            GroupReport {
                matched: 1,
                propagated: 2
            }
        );
        assert_eq!(
            report.groups["second"],
            GroupReport {
                matched: 1,
                propagated: 1
            }
        );
        assert_eq!(
            report.groups["inner"],
            GroupReport {
                matched: 1,
                propagated: 0
            }
        );
        assert_eq!(
            serde_json::to_value(&resolved).unwrap()["mapping_report"]["unattributed"],
            2
        );
        // Blocks skip the layout invariants and may be empty; boxes do not.
        let mut empty = m.clone();
        empty.authored_groups.push(module("gone", "\\bR_Module9_"));
        let resolved = empty.resolve(&graph).unwrap();
        assert_eq!(resolved.authored_groups[6].member_ids, Vec::<String>::new());
        assert_eq!(
            resolved.mapping_report.unwrap().groups["gone"],
            GroupReport::default()
        );
        // With 0:7 nested under 0:4, block `second` tolerates the split scope
        // and block `first` the two parents; the same group as a box does not.
        let mut scoped = graph.clone();
        scoped.nodes[7].address = vec![0, 4, 1];
        assert_eq!(
            m.resolve(&scoped).unwrap().authored_groups[0].member_ids,
            vec!["0:2", "0:3", "0:7"]
        );
        let mut boxed = m.clone();
        boxed.authored_groups[0].kind = None;
        assert!(boxed
            .resolve(&scoped)
            .unwrap_err()
            .contains("cross native parent"));
        let mut torn = m.clone();
        torn.authored_groups[1].kind = None;
        assert!(torn
            .resolve(&scoped)
            .unwrap_err()
            .contains("split native scope"));
        // Only propagating groups vote: a hand-authored neighbour never adopts.
        let mut quiet = m.clone();
        for g in &mut quiet.authored_groups {
            g.member_match.as_mut().unwrap().propagate = None;
        }
        let resolved = quiet.resolve(&graph).unwrap();
        assert_eq!(resolved.authored_groups[0].member_ids, vec!["0:2"]);
        assert_eq!(resolved.mapping_report.unwrap().unattributed, 5);
        // Propagation is bounded: a chain longer than the round limit stays partly unattributed.
        let mut chain = NativeGraph {
            nodes: vec![operator("0:1", &[0, 1], "Map", "R_Module0_x")],
            edges: vec![],
        };
        for i in 2..=(PROPAGATION_ROUNDS as u64 + 3) {
            chain
                .nodes
                .push(operator(&format!("0:{i}"), &[0, i], "Map", "Head"));
            chain
                .edges
                .push(channel(i, &format!("0:{}", i - 1), &format!("0:{i}")));
        }
        let mut long = InspectionMetadata::default();
        long.authored_groups.push(module("only", "\\bR_Module0_"));
        let report = long.resolve(&chain).unwrap().mapping_report.unwrap();
        assert_eq!(
            report.groups["only"].propagated as usize,
            PROPAGATION_ROUNDS
        );
        assert_eq!(report.unattributed, 2);
        let mut shapeless = InspectionMetadata::default();
        let mut bare = module("bare", "x");
        bare.member_match = None;
        shapeless.authored_groups.push(bare);
        assert!(shapeless.validate().unwrap_err().contains("module group"));
    }
    #[test]
    fn lossless_contract() {
        let (m, g) = fixture();
        let before = g.clone();
        m.validate_with_graph(&g).unwrap();
        assert_eq!(g, before);
        let wire = serde_json::to_value(&m).unwrap();
        assert_eq!(wire["authoredGroups"][0]["memberIds"][0], "0:1");
        assert_eq!(
            serde_json::from_value::<InspectionMetadata>(wire).unwrap(),
            m
        );
        assert_eq!(
            serde_json::from_value::<NativeGraph>(serde_json::to_value(&g).unwrap()).unwrap(),
            g
        );
    }
    #[test]
    fn rejects_invalid_metadata() {
        let (m, g) = fixture();
        let mut x = m.clone();
        x.schema_version = 2;
        assert!(x.validate().is_err());
        let mut x = m.clone();
        x.authored_groups.push(x.authored_groups[0].clone());
        assert!(x.validate().is_err());
        let mut x = m.clone();
        x.authored_groups[0].member_ids.push("0:1".into());
        assert!(x.validate().is_err());
        let mut x = m.clone();
        x.authored_groups[0].ports[0].native_ports[0].index = 99;
        assert!(x.validate_with_graph(&g).is_err());
        let mut x = m.clone();
        x.authored_groups[0].member_ids.push("missing".into());
        assert!(x.validate_with_graph(&g).is_err());
        let mut x = m;
        x.authored_groups[0].ports[0].data_type.clear();
        assert!(x.validate().is_err());
    }
    #[test]
    fn rejects_invalid_native_graph() {
        let (_, g) = fixture();
        let mut x = g.clone();
        x.nodes.push(x.nodes[0].clone());
        assert!(x.validate().is_err());
        let mut x = g.clone();
        x.edges[0].target = "missing".into();
        assert!(x.validate().is_err());
        let mut x = g.clone();
        x.edges[0].worker = 1;
        assert!(x.validate().is_err());
        let mut x = g;
        x.edges.push(x.edges[0].clone());
        assert!(x.validate().is_err());
    }
}
