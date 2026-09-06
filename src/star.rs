//! Typed, built-in Differential computation used by ordinary program definitions.
//! Callers select a supported operator and relations, never native source or paths.
use super::{lower::ident, Schema};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

pub const DECLARATION: &str = include_str!("star/lemmalog_star.dl");
pub const IMPLEMENTATION: &str = include_str!("star/lemmalog_star.rs");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operator {
    LargeSmallStar {
        vertices: String,
        edges: String,
        output: String,
    },
}
impl Operator {
    pub fn relations(&self) -> (&str, &str, &str) {
        match self {
            Self::LargeSmallStar {
                vertices,
                edges,
                output,
            } => (vertices, edges, output),
        }
    }
    pub fn renamed(&self, names: &BTreeMap<String, String>) -> Self {
        let (vertices, edges, output) = self.relations();
        Self::LargeSmallStar {
            vertices: names[vertices].clone(),
            edges: names[edges].clone(),
            output: names[output].clone(),
        }
    }
    pub fn validate(&self, schemas: &BTreeMap<String, Schema>) -> Result<(), String> {
        let (vertices, edges, output) = self.relations();
        for (role, name, arity) in [
            ("vertices", vertices, 1),
            ("edges", edges, 2),
            ("output", output, 2),
        ] {
            let schema = schemas.get(name).ok_or_else(|| format!("LargeSmallStar {role} refers to undeclared relation {name}; declare that relation or correct the operator reference"))?;
            if !ident(name) || schema.fields != vec!["int"; arity] {
                return Err(format!("LargeSmallStar {role} relation {name} requires {arity} int fields, found {:?}; use signed integer vertex IDs and correct the schema", schema.fields));
            }
        }
        if schemas[output].input {
            return Err(format!(
                "LargeSmallStar output {output} is an input relation; declare it as derived"
            ));
        }
        Ok(())
    }
    pub fn source(&self, index: usize) -> String {
        let (vertices, edges, output) = self.relations();
        format!("relation StarResult{index}[(signed<64>, signed<64>)]\n\
            function star_vertex{index}(v: R_{vertices}): signed<64> {{ v.f0 }}\n\
            function star_from{index}(e: R_{edges}): signed<64> {{ e.f0 }}\n\
            function star_to{index}(e: R_{edges}): signed<64> {{ e.f1 }}\n\
            apply lemmalog_star::LargeSmallStar(R_{vertices}, star_vertex{index}, R_{edges}, star_from{index}, star_to{index}) -> (StarResult{index})\n\
            R_{output}(v, label) :- StarResult{index}[(v, label)].\n")
    }
}

pub fn prelude() -> String {
    // A generated source hash includes the selected native implementation.
    format!("// lemmalog_star.dl sha256:{:x}\n// lemmalog_star.rs sha256:{:x}\nimport lemmalog_star as lemmalog_star\n", Sha256::digest(DECLARATION), Sha256::digest(IMPLEMENTATION))
}

pub fn write_library(directory: &Path) -> Result<(), String> {
    std::fs::write(directory.join("lemmalog_star.dl"), DECLARATION).map_err(|e| e.to_string())?;
    std::fs::write(directory.join("lemmalog_star.rs"), IMPLEMENTATION).map_err(|e| e.to_string())
}
