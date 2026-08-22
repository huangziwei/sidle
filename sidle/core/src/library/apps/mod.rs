//! The apps in the fleet — what installs to a Kindle's `/mnt/us` under
//! `extensions/` and runs there on its own.
//!
//! One **mount-rooted tree** per app is the whole interface: its entries are
//! paths under `/mnt/us`, and an app declares nothing else.
//!
//! [`identity`] reads a tree's directory names, KUAL descriptor and launcher
//! tile; [`policy`] holds the install rules.

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
