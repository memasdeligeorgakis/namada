//! Utilities for use in tests.

use std::env;
use std::path::PathBuf;

use git2::Repository;

/// Path from the root of the Git repo to the directory under which built test
/// wasms can be found.
pub const WASM_FOR_TESTS_DIR: &str = "wasm_for_tests";

/// Corresponds to wasms that we build for tests (under the `wasm_for_tests/`
/// directory).
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy)]
pub enum TestWasms {
    TxMemoryLimit,
    TxMintTokens,
    TxNoOp,
    TxProposalCode,
    TxReadStorageKey,
    TxWriteStorageKey,
    VpAlwaysFalse,
    VpAlwaysTrue,
    VpEval,
    VpMemoryLimit,
    VpReadStorageKey,
}

impl TestWasms {
    /// Get the path to where this test wasm is expected to be, or panic if not
    /// able to.
    pub fn path(&self) -> PathBuf {
        let filename = match self {
            TestWasms::TxMemoryLimit => "tx_memory_limit.wasm",
            TestWasms::TxMintTokens => "tx_mint_tokens.wasm",
            TestWasms::TxNoOp => "tx_no_op.wasm",
            TestWasms::TxProposalCode => "tx_proposal_code.wasm",
            TestWasms::TxReadStorageKey => "tx_read_storage_key.wasm",
            TestWasms::TxWriteStorageKey => "tx_write_storage_key.wasm",
            TestWasms::VpAlwaysFalse => "vp_always_false.wasm",
            TestWasms::VpAlwaysTrue => "vp_always_true.wasm",
            TestWasms::VpEval => "vp_eval.wasm",
            TestWasms::VpMemoryLimit => "vp_memory_limit.wasm",
            TestWasms::VpReadStorageKey => "vp_read_storage_key.wasm",
        };
        let repo_root =
            Repository::discover(env::current_dir().unwrap()).unwrap();
        repo_root
            .workdir()
            .unwrap()
            .join(WASM_FOR_TESTS_DIR)
            .join(filename)
    }

    /// Attempts to read the contents of this test wasm. Panics if it is not
    /// able to for any reason.
    pub fn bytes(&self) -> Vec<u8> {
        let path = self.path();
        std::fs::read(&path).unwrap_or_else(|_| {
            panic!("Could not read wasm at path {}", path.to_string_lossy())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wasms_path() {
        let path = TestWasms::TxNoOp.path();
        assert!(path.exists());
    }
}
