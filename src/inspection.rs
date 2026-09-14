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
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectionMetadata {
    pub schema_version: u32,
    #[serde(default, rename = "authoredGroups")]
    pub authored_groups: Vec<AuthoredGroup>,
}
impl Default for InspectionMetadata {
    fn default() -> Self {
        Self {
            schema_version: 1,
            authored_groups: vec![],
        }
    }
}
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
            for id in &g.member_ids {
                unique(&mut members, id, "native group member")?;
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
    /// `debug_pattern` matches claim only unclaimed operators. Zero matches, a
    /// port matching zero or several operators, a split native scope or members
    /// under different native parents are mapping errors naming the group.
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
            if matched.is_empty() {
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
        let mut resolved = self.clone();
        for (gi, g) in resolved.authored_groups.iter_mut().enumerate() {
            let set = &members[gi];
            if set.is_empty() {
                return Err(format!("group {}: no native members", g.id));
            }
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
            }],
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
            },
        ));
        m.authored_groups.push(matched(
            "transformer",
            MemberMatch {
                scope_name: None,
                debug_pattern: Some("ApplyTransformer \\{ transformer: \"Star\"".into()),
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
        });
        assert!(zero.resolve(&graph).unwrap_err().contains("phase"));
        let mut split = InspectionMetadata::default();
        let mut explicit = matched(
            "split",
            MemberMatch {
                scope_name: None,
                debug_pattern: Some("^ApplyTransformer".into()),
            },
        );
        explicit.member_ids = vec!["0:3".into()];
        split.authored_groups.push(explicit);
        split.authored_groups.push(matched(
            "rest",
            MemberMatch {
                scope_name: Some("large-star".into()),
                debug_pattern: None,
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
        });
        assert!(malformed.validate().unwrap_err().contains("debug_pattern"));
        malformed.authored_groups[1].member_match = Some(MemberMatch {
            scope_name: None,
            debug_pattern: None,
        });
        assert!(malformed.validate().is_err());
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
