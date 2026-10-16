//! The plane a DRM resource asks for, end to end.
//!
//! Asks the @sgc daemon for a card's PRIMARY plane — the plane an app on the
//! display scans out through, and the only thing the clients in tree use — and
//! then for an OVERLAY plane of the same card. Role in hand, the daemon's
//! answer for each request is printed.
//!
//! The overlay half is the point of the check: the role is part of the
//! protocol so it can be asked for, and the daemon refuses it for now (an
//! overlay plane is not in the lease it creates, so there is nothing to hand
//! over). Exit status is 0 only when the primary was granted *and* the overlay
//! was refused with a reason.

use std::os::fd::AsRawFd;
use std::process::ExitCode;

use libsgc_rs::{PlaneRole, Resource, SgcClient};

fn main() -> ExitCode {
    let (mut client, advertised) = match SgcClient::connect() {
        Ok(connected) => connected,
        Err(e) => {
            eprintln!("cannot reach the @sgc daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("advertised: {advertised:?}");

    let Some(card) = advertised.iter().find_map(|resource| match resource {
        Resource::Drm {
            card,
            plane: PlaneRole::Primary,
        } => Some(*card),
        _ => None,
    }) else {
        eprintln!("the daemon advertised no DRM card (is a display connected?)");
        return ExitCode::FAILURE;
    };

    // The app on the display: one client at a time, and it takes the inputs
    // that ride with the seat.
    let primary = Resource::drm(card);
    match client.acquire(primary.clone()) {
        Ok(()) => match client.fd(&primary) {
            Ok(fd) => println!(
                "primary plane of card{card}: granted (fd {})",
                fd.as_raw_fd()
            ),
            Err(e) => {
                eprintln!("primary plane of card{card}: granted but no fd: {e}");
                return ExitCode::FAILURE;
            }
        },
        Err(e) => {
            eprintln!(
                "primary plane of card{card}: refused ({e}) — \
                 another client holds the display, stop it and retry"
            );
            return ExitCode::FAILURE;
        }
    }

    // The guest plane: hardware compositing over whoever holds the primary.
    // Part of the protocol, not granted yet.
    let overlay = Resource::drm_overlay(card);
    match client.acquire(overlay.clone()) {
        Ok(()) => {
            eprintln!(
                "overlay plane of card{card}: GRANTED — this server should not grant one yet"
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            println!("overlay plane of card{card}: refused as expected ({e})");
            ExitCode::SUCCESS
        }
    }
}
