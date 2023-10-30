#![cfg_attr(all(not(feature = "std"), not(test)), no_std)]
#![cfg_attr(feature = "unstable", feature(dropck_eyepatch))]

#[cfg(test)]
#[macro_use]
extern crate lazy_static;

mod mutex;
mod pause;

pub use mutex::{Mutex, MutexGuard, Slot};
