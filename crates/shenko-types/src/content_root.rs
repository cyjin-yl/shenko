//! The narrow shared-content-store capability a durable publisher needs.
//!
//! An installation's shared content store is reclaimed against the set of
//! objects its owners still name. Two powers make that safe, and a durable
//! publisher needs exactly those two:
//!
//! * the **writer lease** — a shared exclusion held while a writer seals an
//!   object and publishes the reference that names it, so a reference is
//!   never published against an object reclamation is about to remove; and
//! * the **reader pin** — an exact record of the objects one live reader
//!   still reaches, kept in the store's own directory so a collector in
//!   another process can see it.
//!
//! The contract is declared here, in the leaf crate every participant
//! already depends on, and implemented by the crate that owns the content
//! store. A crate that only publishes references therefore never depends on
//! the store's implementation, and the dependency graph gains no edge
//! between the contract and the store it reads.
//!
//! A pin is not a recovery authority: it names objects for a live reader and
//! nothing else. It carries no durable claim, and a reader that dies leaves
//! nothing behind.

use std::fmt::Debug;

use crate::error::ShenkoError;
use crate::object::ObjectRef;

/// A writer's shared lease on the shared content store.
///
/// The lease is held while a writer seals objects and publishes the
/// references that name them. Dropping it releases the exclusion.
pub trait RootLease: Send + Debug {}

/// A live reader's exact object roots.
///
/// The pin keeps exactly the objects it names — and their closure — alive
/// for as long as it is held, without excluding reclamation for anything
/// else. Dropping it releases exactly those roots.
pub trait ReaderPin: Send + Debug {}

/// The shared content store's durable-root capability.
///
/// It is deliberately narrow: it grants a writer lease and a reader pin, and
/// nothing else. A publisher never needs the store's ledger, its queries or
/// its rows.
pub trait ContentRootGuard: Send + Sync {
    /// Take the shared writer lease.
    ///
    /// A writer waits for the collector's finite pass rather than racing it.
    fn acquire_write_lease(&self) -> Result<Box<dyn RootLease>, ShenkoError>;

    /// Pin exactly these objects for a reader that outlives its publication.
    ///
    /// Every object is verified against the store before it is pinned, so a
    /// pin never names bytes the store does not hold.
    fn acquire_reader_pin(
        &self,
        objects: &[&ObjectRef],
    ) -> Result<Box<dyn ReaderPin>, ShenkoError>;
}
