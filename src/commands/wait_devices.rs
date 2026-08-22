//! `bcachefs wait-devices` - retained, and deliberately does nothing.
//!
//! It existed for `bcachefs-wait-devices@.service`, which fstab entries could
//! depend on via `x-systemd.requires=` so that a multi-device mount was not
//! attempted before udev had probed every member. mount.bcachefs now waits for
//! its own members (see [`crate::device_scan`] and the `missing_dev_timeout`
//! option), so the race it closed is closed a second time, one layer down.
//!
//! Which would make it merely redundant. It is worse than that: the wait had
//! no deadline of its own - `poll()` with a NULL timeout - so a member that
//! was never coming back blocked until systemd's DefaultTimeoutStartSec killed
//! the unit and failed the mount job. Ninety seconds, then an emergency shell,
//! for precisely the case the degraded prompt exists to handle. A dependency
//! meant to protect the boot pre-empted the code that could have saved it.
//!
//! So the command stays and succeeds immediately. It is the *command* that has
//! to be the no-op rather than the unit: the unit ships as a template, and
//! anyone who copied it into /etc/systemd/system/ to edit owns their copy
//! forever - that copy still calls this. Removing the command instead would
//! turn their satisfied dependency into a failed one, which is the boot
//! failure we are trying to stop happening.

use anyhow::Result;
use clap::Parser;
use log::warn;

use crate::device_scan;

/// Does nothing; kept so that units and fstab entries depending on it succeed.
#[derive(Parser, Debug)]
#[command(
    about,
    long_about = "Does nothing, and exits zero. mount.bcachefs waits for its \
own member devices now, so this no longer has anything to do - and waiting \
here was actively harmful, because it had no timeout and would fail the mount \
job for a device that was never coming back. Kept so that existing fstab \
entries using x-systemd.requires=bcachefs-wait-devices@<uuid>.service keep \
booting."
)]
pub struct Cli {
    /// A device string in the UUID=\<UUID\> format.
    device: String,
}

fn cmd_wait_devices(cli: Cli) -> Result<()> {
    // Zero even on a bad argument: this must never be why a boot stops, and
    // the mount that follows reports the real problem with the real context.
    match device_scan::parse_uuid_equals(&cli.device) {
        Ok(Some(_)) => {}
        Ok(None) | Err(_) => warn!(
            "wait-devices: not a UUID=<uuid> device string: {}",
            cli.device
        ),
    }

    warn!("wait-devices does nothing now; mount waits for its own devices");
    Ok(())
}

pub const CMD: super::CmdDef =
    typed_cmd!("wait-devices", "Does nothing; kept so dependent units still succeed", Cli, cmd_wait_devices);

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, thread, time::Duration};

    use clap::Parser;

    use super::*;

    /// The point of the change is the *absence* of a wait, so test for that
    /// rather than for the return value: reintroducing the poll would make
    /// this hang, and a hang is not a test failure, it is a stuck CI job. Run
    /// it on a thread and give it a deadline.
    ///
    /// Verified against the break: on the previous implementation this command
    /// blocks indefinitely on a UUID no device carries - `timeout 10 bcachefs
    /// wait-devices UUID=deadbeef-...` exits 124 having printed nothing. No
    /// root and no VM needed to see it, which is why this is a cargo test and
    /// not a ktest one.
    fn returns_promptly(arg: &str) {
        let (tx, rx) = mpsc::channel();
        let owned = arg.to_owned();

        thread::spawn(move || {
            let cli = Cli::parse_from(["wait-devices", &owned]);
            let _ = tx.send(cmd_wait_devices(cli));
        });

        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(r) => assert!(r.is_ok(), "{arg:?} should exit zero, got {r:?}"),
            Err(_) => panic!("{arg:?}: wait-devices blocked; a unit depending on it would fail the boot"),
        }
    }

    #[test]
    fn does_not_wait_for_a_uuid_no_device_carries() {
        returns_promptly("UUID=deadbeef-0000-0000-0000-000000000000");
    }

    /// A garbled fstab instance name still must not stop the boot.
    #[test]
    fn a_malformed_device_string_still_exits_zero() {
        returns_promptly("not-a-uuid-at-all");
    }
}
