//! Every installed Node write takes the physical domain before SQLite.
use anyhow::Result;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use shenko_types::installation::{InstallationDomain, InstallationPermit};

use crate::Node;

pub(crate) struct InstalledTransaction<'a> {
    transaction: Transaction<'a>,
    permit: Option<InstallationPermit>,
}

impl<'a> std::ops::Deref for InstalledTransaction<'a> {
    type Target = Transaction<'a>;
    fn deref(&self) -> &Self::Target { &self.transaction }
}

impl InstalledTransaction<'_> {
    pub(crate) fn commit(self) -> rusqlite::Result<()> {
        let Self { transaction, permit: _permit } = self;
        transaction.commit()
    }
}

pub(crate) fn begin_write<'a>(conn: &'a mut Connection,
    domain: Option<&InstallationDomain>) -> Result<InstalledTransaction<'a>> {
    let permit = domain.map(|domain| domain.acquire()).transpose()?;
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    Ok(InstalledTransaction { transaction, permit })
}

impl Node {
    pub(crate) fn installation_permit(&self) -> Result<Option<InstallationPermit>> {
        Ok(self.installation.as_ref().map(|domain| domain.acquire()).transpose()?)
    }
}
