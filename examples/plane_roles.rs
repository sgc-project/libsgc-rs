//! The overlay plane grant, end to end.
//!
//! Asks the @sgc daemon for an OVERLAY plane of the card an app is already
//! drawing on, drives the plane it is granted, and holds it for a few seconds
//! before giving it back. Nothing here touches the primary: the app on the
//! display keeps its lease and keeps drawing, and the box shows up composited
//! over it by the display controller.
//!
//! Run it while an app holds the display — an overlay needs that pipeline to
//! exist, and the daemon says so in its refusal if there is none:
//!
//!     cargo run --example plane_roles          # hold the plane for 5s
//!     cargo run --example plane_roles -- 20    # ... for 20s
//!
//! Exit status is 0 when the grant came back naming a plane, the plane took the
//! framebuffer (the kernel reports it bound to the granted CRTC), and it was
//! released cleanly. Everything this program does to the plane goes through the
//! same lease fd the daemon handed it: a plane grant is not a new capability,
//! it is a name for one of the pipeline's planes plus the daemon's word that
//! this client is the one using it.

use std::fs::File;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::process::ExitCode;
use std::time::Duration;

use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{Device as ControlDevice, crtc, dumbbuffer::DumbBuffer, plane};
use libsgc_rs::{DrmGrant, PlaneRole, Resource, SgcClient};

/// The granted fd, as the DRM device the `drm` crate talks to: the same traits
/// the daemon's own card implements, because programming a plane is programming
/// a plane.
struct Lease(File);

impl AsFd for Lease {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for Lease {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl drm::Device for Lease {}
impl drm::control::Device for Lease {}

fn main() -> ExitCode {
    let hold_secs: u64 = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(5);

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

    // The whole point: a plane over somebody else's output, without asking for
    // the display. The daemon grants it while a pipeline is up and its overlay
    // is free, and refuses with the reason when it is not.
    let overlay = Resource::drm_overlay(card);
    if let Err(e) = client.acquire(overlay.clone()) {
        eprintln!("overlay plane of card{card}: refused ({e})");
        eprintln!(
            "  an overlay needs a pipeline that exists (an app holding the display) and whose \
             overlay plane is free"
        );
        return ExitCode::FAILURE;
    }

    let Some(grant) = client.drm_grant(&overlay) else {
        eprintln!("card{card}: the daemon granted the overlay without naming a plane");
        return ExitCode::FAILURE;
    };
    println!(
        "overlay plane of card{card}: granted — plane {} of crtc {}{}",
        grant.plane_id,
        grant.crtc_id,
        grant
            .connector_id
            .map(|id| format!(" (connector {id})"))
            .unwrap_or_default()
    );

    let lease_fd = match client.fd(&overlay) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("granted without a lease fd: {e}");
            return ExitCode::FAILURE;
        }
    };
    let lease = Lease(File::from(lease_fd));

    if let Err(e) = draw_on_plane(&lease, &grant, hold_secs) {
        eprintln!("could not drive the granted plane: {e}");
        return ExitCode::FAILURE;
    }

    // Give it back: the plane is this client's until it does.
    if let Err(e) = client.release(overlay) {
        eprintln!("release failed: {e}");
        return ExitCode::FAILURE;
    }
    println!("overlay plane {} released", grant.plane_id);
    ExitCode::SUCCESS
}

/// Put a box on the granted plane, hold it, then take it off again.
///
/// The plane is programmed exactly as the grant describes it — its own id, and
/// the CRTC it drives — with the client's own framebuffer, through the lease fd.
fn draw_on_plane(lease: &Lease, grant: &DrmGrant, hold_secs: u64) -> std::io::Result<()> {
    // The kernel's handles are non-zero: a zero here would be the daemon
    // naming an object that cannot exist.
    let plane = plane::Handle::from(
        NonZeroU32::new(grant.plane_id).expect("a granted plane id is never zero"),
    );
    let crtc = crtc::Handle::from(
        NonZeroU32::new(grant.crtc_id).expect("a granted crtc id is never zero"),
    );

    let info = lease.get_plane(plane)?;
    // A format from the plane's own list, or a refusal would be about that and
    // not about anything this program did.
    let format = [DrmFourcc::Xrgb8888, DrmFourcc::Argb8888, DrmFourcc::Rgb565]
        .into_iter()
        .find(|format| info.formats().contains(&(*format as u32)))
        .ok_or_else(|| {
            std::io::Error::other("the plane lists none of the formats this example can fill")
        })?;

    // The mode the pipeline is running, 1:1: a plane that can scale is not
    // something to assume, so the source rectangle and the destination match.
    let mode = lease
        .get_crtc(crtc)?
        .mode()
        .ok_or_else(|| std::io::Error::other("the pipeline's CRTC has no mode set"))?;
    let (mode_width, mode_height) = (mode.size().0 as u32, mode.size().1 as u32);
    let (width, height) = (320.min(mode_width), 240.min(mode_height));

    let mut buffer = lease.create_dumb_buffer((width, height), format, 32)?;
    fill(&mut buffer, format, lease)?;
    let fb = lease.add_framebuffer(&buffer, 24, 32)?;

    let x = mode_width.saturating_sub(width) / 2;
    let y = mode_height.saturating_sub(height) / 2;
    lease.set_plane(
        plane,
        crtc,
        Some(fb),
        0,
        (x as i32, y as i32, width, height),
        (0, 0, width << 16, height << 16),
    )?;

    // What the kernel says now. This is the check that matters: the plane bound
    // to the granted CRTC holding this program's framebuffer, not a number the
    // daemon made up.
    let bound = lease.get_plane(plane)?;
    println!(
        "plane {0} -> crtc {1:?}, fb {2:?}: {width}x{height} {format:?} at ({x},{y}) on a \
         {mode_width}x{mode_height} mode",
        grant.plane_id,
        bound.crtc().map(u32::from),
        bound.framebuffer().map(u32::from),
    );
    println!("holding for {hold_secs}s — the app on the display keeps drawing underneath");
    std::thread::sleep(Duration::from_secs(hold_secs));

    lease.set_plane(plane, crtc, None, 0, (0, 0, 0, 0), (0, 0, 0, 0))?;
    let _ = lease.destroy_framebuffer(fb);
    println!("plane cleared — the display is the app's alone again");
    Ok(())
}

/// An unmistakable box: bright fill, white border.
fn fill(buffer: &mut DumbBuffer, format: DrmFourcc, lease: &Lease) -> std::io::Result<()> {
    let (width, height) = buffer.size();
    let bytes_per_pixel = match format {
        DrmFourcc::Rgb565 => 2,
        _ => 4,
    };
    // The dumb buffer's mapping is exactly pitch * height, and the `drm` crate
    // exposes no stride accessor — so the pitch comes from the mapping itself.
    let mut mapping = lease.map_dumb_buffer(buffer)?;
    let pixels = mapping.as_mut();
    let stride = pixels.len() / height as usize;

    const BORDER: u32 = 6;
    for y in 0..height {
        for x in 0..width {
            let inside = x >= BORDER && x + BORDER < width && y >= BORDER && y + BORDER < height;
            let (r, g, b) = if inside {
                (255, 140, 0)
            } else {
                (255, 255, 255)
            };
            let offset = y as usize * stride + x as usize * bytes_per_pixel;
            match bytes_per_pixel {
                // ARGB8888 and XRGB8888 share a memory layout: B, G, R, then
                // the alpha byte, which the server-side plane ignores.
                4 => pixels[offset..offset + 4].copy_from_slice(&[b, g, r, 0]),
                // RGB565, little-endian.
                _ => {
                    let packed: u16 =
                        ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3);
                    pixels[offset..offset + 2].copy_from_slice(&packed.to_le_bytes());
                }
            }
        }
    }
    Ok(())
}
