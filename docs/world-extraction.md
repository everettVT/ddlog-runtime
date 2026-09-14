# Program extraction: distinct from inspection groups

The control-plane work does not turn authored display groups into independent
programs. A future user-directed extraction action must operate on a selected
rule/operator subgraph, infer and present its typed boundary, and register an
immutable child program. It must rewrite the parent composition to the child's
exact version and preserve public relation names, types and update semantics.

Admission must reuse the existing parser/lowering, typed interface and composition
validation paths. Reject cuts through registered-operation ownership, unsafe
negation/recursion, private native state or unsupported operator boundaries. Show
the candidate child definition, typed inputs/outputs, parent wiring and source
provenance before publishing. Compilation/admission failure must leave the saved
parent and any active instances unchanged.

Validation must compare original and rewritten programs on user-selected regression
traces including insertions and retractions, recursive fixed points and empty
inputs. Example traces are evidence, not a universal proof of equivalence. When a
structural equivalence argument cannot be established, report that limitation
explicitly and require review of the proposed transformation. Publishing definitions
and choosing to run the rewritten composition are separate actions.

The current observer can inspect declared child blocks and recursively navigate
exact pins. Leaf topology requires a compiled/captured native artifact; browsing
does not compile or launch a world implicitly. No extraction mutation is currently
exposed by the control-plane protocol.
