use capabilities;
use capabilities::CAP_SYS_CHROOT;
use cgroup;
use cmd::Cmd;
use fs;
use ipc;
use libc;
use lsm;
use mountns;
use namespace;
use nix::sys::prctl;
use nix::sys::signal::{self, Signal};
use nix::unistd;
use nix::unistd::{Pid, Uid, Gid};
use pty;
use std::env;
use std::ffi::CStr;
use std::fs::File;
use std::os::unix::io::IntoRawFd;
use std::os::unix::prelude::*;
use std::process;
use types::{Error, Result};
use void::Void;

pub struct ChildOptions<'a> {
    pub container_pid: Pid,
    pub mount_ready_sock: &'a ipc::Socket,
    pub fs: fs::CntrFs,
    pub home: Option<&'a CStr>,
    pub uid: Uid,
    pub gid: Gid,
}

pub fn run(options: &ChildOptions) -> Result<Void> {
    let target_caps = tryfmt!(
        capabilities::get(options.container_pid),
        "failed to get capabilities of target process"
    );

    capabilities::inherit_capabilities().unwrap();

    let lsm_profile = tryfmt!(
        lsm::read_profile(options.container_pid),
        "failed to get lsm profile"
    );

    tryfmt!(
        cgroup::move_to(unistd::getpid(), options.container_pid),
        "failed to change cgroup"
    );

    let cmd = tryfmt!(Cmd::new(options.container_pid, options.home), "");

    let supported_namespaces = tryfmt!(
        namespace::supported_namespaces(),
        "failed to list namespaces"
    );

    if !supported_namespaces.contains(namespace::MOUNT.name) {
        return errfmt!("the system has no support for mount namespaces");
    };

    let mount_namespace = tryfmt!(
        namespace::MOUNT.open(options.container_pid),
        "could not access mount namespace"
    );
    let mut other_namespaces = Vec::new();

    let other_kinds = &[
        namespace::UTS,
        namespace::CGROUP,
        namespace::PID,
        namespace::NET,
        namespace::IPC,
        namespace::USER,
    ];

    for kind in other_kinds {
        if !supported_namespaces.contains(kind.name) {
            continue;
        }
        if kind.is_same(options.container_pid) {
            continue;
        }

        other_namespaces.push(tryfmt!(
            kind.open(options.container_pid),
            "failed to open {} namespace",
            kind.name
        ));
    }

    tryfmt!(mount_namespace.apply(), "failed to apply mount namespace");
    tryfmt!(
        mountns::setup(&options.fs, options.mount_ready_sock, mount_namespace),
        ""
    );

    let mut dropped_groups = false;
    if supported_namespaces.contains(namespace::USER.name) {
        dropped_groups = unistd::setgroups(&[]).is_ok();
    }

    for ns in other_namespaces {
        tryfmt!(ns.apply(), "failed to apply namespace");
    }

    if supported_namespaces.contains(namespace::USER.name) {
        if let Err(e) = unistd::setgroups(&[]) {
            if !dropped_groups {
                tryfmt!(Err(e), "could not set groups");
            }
        }
        tryfmt!(unistd::setgid(options.gid), "could not set group id");
        tryfmt!(unistd::setuid(options.uid), "could not set user id");
    }

    tryfmt!(target_caps.set(), "failed to apply capabilities");

    let pty_master = tryfmt!(pty::open_ptm(), "open pty master");
    tryfmt!(pty::attach_pts(&pty_master), "failed to setup pty master");

    // we have to destroy f manually, since we only borrow fd here.
    let f = unsafe { File::from_raw_fd(pty_master.as_raw_fd()) };
    let res = options.mount_ready_sock.send(&[], &[&f]);
    f.into_raw_fd();
    tryfmt!(res, "failed to send pty file descriptor to parent process");

    if let Err(e) = env::set_current_dir("/var/lib/cntr") {
        warn!("failed to change directory to /var/lib/cntr: {}", e);
    }

    if let Some(profile) = lsm_profile {
        tryfmt!(profile.inherit_profile(), "failed to inherit lsm profile");
    }

    let status = tryfmt!(cmd.run(), "");
    if let Some(signum) = status.signal() {
        let signal = tryfmt!(
            Signal::from_c_int(signum),
            "invalid signal received: {}",
            signum
        );
        tryfmt!(
            signal::kill(unistd::getpid(), signal),
            "failed to send signal {:?} to own pid",
            signal
        );
    }
    if let Some(code) = status.code() {
        process::exit(code);
    }
    panic!(
        "BUG! command exited successfully, \
        but was neither terminated by a signal nor has an exit code"
    );
}
