//! The apps in the fleet — everything that installs to a Kindle's `/mnt/us`
//! and runs there on its own.
//!
//! Five programs ship to the mount: the picker, bokai, steb, karyll and
//! kfxdedrm-fe. They are standalone — they install to `extensions/`, they do
//! not talk to sidle, and each is built by its own repo. What they share is
//! only the shape of what they publish: a **mount-rooted tree** whose entries
//! are paths under `/mnt/us`.
//!
//! That shape is the whole interface, and it is one every one of those repos
//! already has. Nothing here asks an app to declare itself to sidle: the
//! [`identity`] a row shows is read from the tree's own directory names, from
//! the KUAL descriptor the Kindle defines, and from the launcher tile. How a
//! file is installed is [`policy`] — sidle's rules, kept on sidle's side, so no
//! repo has to carry a file it would never use itself.

pub mod compose;
pub mod identity;
pub mod policy;
pub mod tree;

pub use compose::{DevicePlan, PlannedFile, plan, plan_from};
pub use identity::AppIdentity;
pub use policy::{Apply, apply_for, is_payload};
pub use tree::{AppFile, AppTree, discover, discover_registrable, validate_mount_rel, walk};
