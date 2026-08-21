//! The apps in the fleet — everything that installs to a Kindle's `/mnt/us`
//! and runs there on its own.
//!
//! Five programs ship to the mount: the picker, bokai, steb, karyll and
//! kfxdedrm-fe. They are standalone — they install to `extensions/`, they do
//! not talk to sidle, and each is built by its own repo. What they share is the
//! shape of what they publish: a **mount-rooted tree** whose entries are paths
//! under `/mnt/us`, carrying its own [`spec::AppSpec`] at
//! `extensions/<id>/app.json`.
//!
//! This module is the reader of that shape. It knows how to find a tree, walk
//! it, and say what each path's install rule is; it knows nothing about any
//! particular app, and no app repo depends on this crate.

pub mod spec;
pub mod tree;

pub use spec::{APP_SPEC_FILE, APP_SPEC_SCHEMA, AppSpec, Apply, FileClass, PathPolicy, PathRule};
pub use tree::{AppFile, AppTree, RECEIPT_FILE, discover, walk};
