mod data;
mod error;
mod install;
mod release;
mod take_seekable;

pub use {
	data::{Data, Version},
	error::Error,
	release::{LocalVersion, UpdateStatus},
};
