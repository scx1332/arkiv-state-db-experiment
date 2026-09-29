// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Arkiv-Network/arkiv at 0fd58ce5bd6a0a196b0079f60d4679539346adc6.
//! Bounds on how big a query may be.
//!
//! Policy limits copied from the source grammar. They bound parser recursion
//! and allocation before any node execution or proof work.

/// Longest query string accepted, in bytes.
pub const MAX_QUERY_BYTES: usize = 8 * 1024;

/// Most predicates one query may combine.
pub const MAX_PREDICATES: usize = 64;

/// Deepest nesting of parentheses and `NOT`. Bounds parser and evaluator
/// recursion together, since the evaluator walks the tree the parser builds.
pub const MAX_NESTING_DEPTH: usize = 32;

/// Longest attribute name, in bytes — the width the ABI's `Ident32` carries.
pub const MAX_ATTRIBUTE_NAME_BYTES: usize = 32;
