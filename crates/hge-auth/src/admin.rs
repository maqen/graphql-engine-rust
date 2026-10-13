//! Admin secrets, kept only as digests and compared in constant time.

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

#[derive(Default)]
pub(crate) struct AdminSecrets(Vec<[u8; 32]>);

impl AdminSecrets {
    pub(crate) fn new<S: AsRef<[u8]>>(secrets: &[S]) -> Self {
        AdminSecrets(secrets.iter().map(|s| digest(s.as_ref())).collect())
    }

    /// Comparing digests means neither timing nor length reveals anything about a secret.
    pub(crate) fn check(&self, given: &[u8]) -> bool {
        let given = digest(given);
        self.0
            .iter()
            .fold(false, |ok, s| ok | bool::from(s.ct_eq(&given)))
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
