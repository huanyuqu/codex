//! Offline tools for evaluating context graphs: indexing recorded rollouts with a model,
//! recalling from them, and the model backends the evaluation harness uses.

pub mod bench;
pub mod probe;
pub mod provider;
pub mod responses;
pub mod session;
#[cfg(test)]
mod test_support;
