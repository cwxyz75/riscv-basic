//! A box that can say no (KT9, T177): stable Rust has no fallible `Box::new`, and the kernel's syscall layer is `forbid(unsafe_code)`. A
//! `Vec` can reserve fallibly, and a `Vec` of exactly one element converts to a `Box<[T; 1]>` without another allocation.

#![allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unreachable
)] // INVARIANT: chunk and entry indices come from the map's own searches over non-empty chunks; counts are bounded by memory; a one-element vector is a one-element box

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};

/// A heap cell holding one `T`, made without aborting when memory is short.
#[derive(Debug)]
pub struct FBox<T>(Box<[T; 1]>);

impl<T> FBox<T> {
    /// Make the box, or give the value back.
    pub fn try_new(v: T) -> Result<FBox<T>, T> {
        let mut vec: Vec<T> = Vec::new();
        if vec.try_reserve_exact(1).is_err() {
            return Err(v);
        }
        vec.push(v);
        match vec.into_boxed_slice().try_into() {
            Ok(b) => Ok(FBox(b)),
            // (cannot happen: the length is one; the value would be lost, so this is unreachable by construction)
            Err(_) => unreachable!("a one-element vector is a one-element box"),
        }
    }

    /// Make the box, then the value in it: `make` runs only once the room is had, so a large value is built in its own frame and moved
    /// once, not carried through the caller's (KT13). Without memory, `make` is given back unrun.
    pub fn try_new_with<F: FnOnce() -> T>(make: F) -> Result<FBox<T>, F> {
        let mut vec: Vec<T> = Vec::new();
        if vec.try_reserve_exact(1).is_err() {
            return Err(make);
        }
        vec.push(make());
        match vec.into_boxed_slice().try_into() {
            Ok(b) => Ok(FBox(b)),
            // (cannot happen: the length is one)
            Err(_) => unreachable!("a one-element vector is a one-element box"),
        }
    }

    /// The room for a box, had now: [`Room::fill_with`] then cannot fail. `None` without memory.
    pub fn room() -> Option<Room<T>> {
        let mut vec: Vec<T> = Vec::new();
        vec.try_reserve_exact(1).ok()?;
        Some(Room(vec))
    }

    pub fn into_inner(self) -> T {
        let [v] = *self.0;
        v
    }
}

/// The room for one `FBox<T>` (`FBox::room`): an empty vector with room for exactly one element.
#[derive(Debug)]
pub struct Room<T>(Vec<T>);

impl<T> Room<T> {
    /// The box, with the value `make` builds in it (`make` runs in its own frame, so a large value is not carried through the caller's: KT13).
    /// Allocates nothing.
    pub fn fill_with<F: FnOnce() -> T>(mut self, make: F) -> FBox<T> {
        self.0.push(make());
        match self.0.into_boxed_slice().try_into() {
            Ok(b) => FBox(b),
            // (cannot happen: the length is one)
            Err(_) => unreachable!("a one-element vector is a one-element box"),
        }
    }
}

impl<T> Deref for FBox<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0[0]
    }
}

impl<T> DerefMut for FBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0[0]
    }
}

impl<T: PartialEq> PartialEq for FBox<T> {
    fn eq(&self, o: &Self) -> bool {
        **self == **o
    }
}

impl<T: Eq> Eq for FBox<T> {}
