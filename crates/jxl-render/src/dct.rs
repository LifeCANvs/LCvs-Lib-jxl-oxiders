mod consts;
mod generic;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum DctDirection {
    Forward,
    Inverse,
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub use generic::*;

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::*;
