// Kitty graphics protocol helpers.

use std::io::Write;
use std::num::NonZeroU32;

use crossterm::{cursor::MoveTo, event::EventStream, execute};
use image::DynamicImage;
use kittage::{
    AsyncInputReader, ImageDimensions, ImageId, NumberOrId, PixelFormat,
    action::Action,
    delete::{ClearOrDelete, DeleteConfig, WhichToDelete},
    display::{CursorMovementPolicy, DisplayConfig, DisplayLocation},
    error::TransmitError,
    image::Image,
    medium::Medium,
};

use crate::image_pipeline::MaybeTransferred;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Terminal cursor position in cells.
#[derive(Debug, Clone, Copy)]
pub struct Pos {
    pub x: u16,
    pub y: u16,
}

pub struct KittyReadyToDisplay<'a> {
    pub img: &'a mut MaybeTransferred,
    pub page_num: usize,
    pub pos: Pos,
    pub display_loc: DisplayLocation,
}

pub enum KittyDisplay<'a> {
    DisplayImages(Vec<KittyReadyToDisplay<'a>>),
}

// ---------------------------------------------------------------------------
// Debug writer (logs outgoing bytes in debug builds)
// ---------------------------------------------------------------------------

struct DbgWriter<W: Write> {
    w: W,
    #[cfg(debug_assertions)]
    buf: String,
}

impl<W: std::io::Write> std::io::Write for DbgWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        #[cfg(debug_assertions)]
        if let Ok(s) = std::str::from_utf8(buf) {
            self.buf.push_str(s);
        }
        self.w.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        #[cfg(debug_assertions)]
        {
            log::debug!("kitty write: {:?}", self.buf);
            self.buf.clear();
        }
        self.w.flush()
    }
}

// ---------------------------------------------------------------------------
// Core async helpers
// ---------------------------------------------------------------------------

/// Execute a single kittage action and await the terminal's response.
pub async fn run_action<'es>(
    action: Action<'_, '_>,
    ev_stream: &'es mut EventStream,
) -> Result<ImageId, TransmitError<<&'es mut EventStream as AsyncInputReader>::Error>> {
    let writer = DbgWriter {
        w: std::io::stdout().lock(),
        #[cfg(debug_assertions)]
        buf: String::new(),
    };
    action
        .execute_async(writer, ev_stream)
        .await
        .map(|(_, id)| id)
}

/// Test whether the terminal supports the Kitty graphics protocol at all.
pub async fn supports_kitty_graphics(ev_stream: &mut EventStream) -> bool {
    let mut img: Image<'static> = DynamicImage::new_rgb8(1, 1).into();
    img.num_or_id = NumberOrId::Id(NonZeroU32::new(u32::MAX - 1).unwrap());
    run_action(Action::Query(&img), ev_stream).await.is_ok()
}

/// Test whether shared-memory image transfer works in this terminal.
/// Returns `true` if SHM is supported (Kitty/Ghostty on Linux/macOS).
pub async fn do_shms_work(ev_stream: &mut EventStream) -> bool {
    let img = DynamicImage::new_rgb8(1, 1);
    let pid = std::process::id();
    let shm_name = format!("treader_test_{pid}");

    #[cfg(unix)]
    let shm_name = &*shm_name;

    let Ok(mut k_img) = kittage::image::Image::shm_from(img, shm_name) else {
        return false;
    };

    k_img.num_or_id = NumberOrId::Id(NonZeroU32::new(u32::MAX).unwrap());

    // Raw mode must already be enabled by TerminalGuard before calling this.
    let res = run_action(Action::Query(&k_img), ev_stream).await;
    res.is_ok()
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

/// Re-display a set of Kitty images.
///
/// On error, returns the page numbers that failed so the main loop can
/// trigger a re-render for them.
pub async fn display_kitty_images<'es>(
    display: KittyDisplay<'_>,
    ev_stream: &'es mut EventStream,
) -> Result<
    (),
    (
        Vec<usize>,
        &'static str,
        TransmitError<<&'es mut EventStream as AsyncInputReader>::Error>,
    ),
> {
    let KittyDisplay::DisplayImages(images) = display;

    let mut err: Option<(Vec<usize>, TransmitError<_>)> = None;

    for KittyReadyToDisplay {
        img,
        page_num,
        pos,
        display_loc,
    } in images
    {
        let config = DisplayConfig {
            location: display_loc,
            cursor_movement: CursorMovementPolicy::DontMove,
            ..DisplayConfig::default()
        };

        execute!(std::io::stdout(), MoveTo(pos.x, pos.y)).unwrap();

        log::debug!(
            "displaying page {page_num} at {:?}, loc {:?}",
            pos,
            config.location
        );

        let this_err = match img {
            MaybeTransferred::NotYet(image) => {
                // Swap out the real image so we can move it into the action.
                let mut placeholder = Image {
                    num_or_id: image.num_or_id,
                    format: PixelFormat::Rgb24(
                        ImageDimensions {
                            width: 0,
                            height: 0,
                        },
                        None,
                    ),
                    medium: Medium::Direct {
                        chunk_size: None,
                        data: (&[]).into(),
                    },
                };
                std::mem::swap(image, &mut placeholder);

                match run_action(
                    Action::TransmitAndDisplay {
                        image: placeholder,
                        config,
                        placement_id: None,
                    },
                    ev_stream,
                )
                .await
                {
                    Ok(img_id) => {
                        *img = MaybeTransferred::Transferred(img_id);
                        Ok(())
                    }
                    Err(e) => Err((page_num, e)),
                }
            }
            MaybeTransferred::Transferred(image_id) => run_action(
                Action::Display {
                    image_id: *image_id,
                    placement_id: *image_id,
                    config,
                },
                ev_stream,
            )
            .await
            .map(|_| ())
            .map_err(|e| (page_num, e)),
        };

        if let Err((id, e)) = this_err {
            let entry = err.get_or_insert_with(|| (vec![], e));
            entry.0.push(id);
        }
    }

    match err {
        Some((pages, e)) => Err((pages, "couldn't transfer image to terminal", e)),
        None => Ok(()),
    }
}

pub async fn clear_kitty_image<'es>(
    image_id: ImageId,
    ev_stream: &'es mut EventStream,
) -> Result<(), TransmitError<<&'es mut EventStream as AsyncInputReader>::Error>> {
    run_action(
        Action::Delete(DeleteConfig {
            effect: ClearOrDelete::Clear,
            which: WhichToDelete::ImageId(image_id, None),
        }),
        ev_stream,
    )
    .await
    .map(|_| ())
}

pub async fn delete_kitty_images<'es>(
    image_ids: Vec<ImageId>,
    ev_stream: &'es mut EventStream,
) -> Result<(), TransmitError<<&'es mut EventStream as AsyncInputReader>::Error>> {
    for image_id in image_ids {
        run_action(
            Action::Delete(DeleteConfig {
                effect: ClearOrDelete::Delete,
                which: WhichToDelete::ImageId(image_id, None),
            }),
            ev_stream,
        )
        .await?;
    }

    Ok(())
}
