#![allow(non_camel_case_types)]

use std::os::raw::c_int;

pub type off_t = i64;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sigset_t {
    pub __val: [u64; 16],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sigaction {
    pub sa_sigaction: usize,
    pub sa_mask: sigset_t,
    pub sa_flags: c_int,
    pub sa_restorer: Option<unsafe extern "C" fn()>,
}

extern "C" {
    pub fn sendfile(out_fd: c_int, in_fd: c_int, offset: *mut off_t, count: usize) -> isize;
    pub fn sigemptyset(set: *mut sigset_t) -> c_int;
    pub fn sigaction(signum: c_int, act: *const sigaction, oldact: *mut sigaction) -> c_int;
}

pub const SIGINT: c_int = 2;
pub const SIGTERM: c_int = 15;
