//! The apps in the fleet — everything that installs to a Kindle's `/mnt/us`
//! and runs there on its own.
//!
//! An app installs to `extensions/`, talks to nothing here, and is built by its
//! own repo. What they share is the shape of what they publish: a
//! **mount-rooted tree** whose entries are paths under `/mnt/us`.
//!
//! That shape is the whole interface. Nothing here asks an app to declare
//! itself: [`identity`] reads the tree's own directory names, the KUAL
//! descriptor, and the launcher tile. [`policy`] holds sidle's own install
//! rules, on sidle's side.

pub mod compose;
pub mod identity;
pub mod policy;
pub mod release;
pub mod tree;

pub use compose::{DevicePlan, PlannedFile, plan, plan_from};
pub use identity::AppIdentity;
pub use policy::{Apply, apply_for, is_payload};
pub use release::{Fetched, Repo};
pub use tree::{
    AppFile, AppTree, built_at_of, discover, discover_registrable, validate_mount_rel, walk,
};
