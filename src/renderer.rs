// Blocking PDF render thread.

use std::{collections::VecDeque, num::NonZeroUsize, path::Path, thread::sleep, time::Duration};

use flume::{Receiver, SendError, Sender, TryRecvError};
use mupdf::{
    Colorspace, Document, Matrix, MetadataName, Page, Pixmap, Quad, TextPageFlags,
    text_page::SearchHitResponse,
};

use crate::{error::RenderError, image_pipeline::InterleavedAroundWithMax, perf};

pub const MUPDF_BLACK: i32 = 0;
pub const MUPDF_WHITE: i32 = i32::from_be_bytes([0, 0xff, 0xff, 0xff]);
const KITTY_MAX_W_OR_H: f32 = 10_000.0;
const EPUB_LAYOUT_MIN_W: f32 = 260.0;
const EPUB_LAYOUT_MAX_W: f32 = 396.0;
const EPUB_LAYOUT_ASPECT: f32 = 595.0 / 420.0;
const EPUB_EM_MIN: f32 = 9.0;
const EPUB_EM_MAX: f32 = 12.0;
const EPUB_APPROX_CHAR_WIDTH_EM: f32 = 0.55;
const EPUB_TARGET_CHARS_PER_LINE: f32 = 54.0;
const RENDER_WINDOW: usize = 24;

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum RenderNotif {
    /// The available pixel area for rendering changed (resize or startup).
    Area { width_px: f32, height_px: f32 },
    /// Jump to a specific (0-indexed) page.
    JumpToPage(usize),
    /// Re-render a page whose Kitty image was deleted by the terminal.
    PageNeedsReRender(usize),
    /// Update the text search term.
    Search(String),
    /// Rotate all pages 90° clockwise.
    Rotate,
    /// Toggle colour inversion.
    Invert,
    /// Toggle warm tint.
    Tint,
    /// Adjust EPUB font size (`+1` larger, `-1` smaller).
    AdjustFontSize(i8),
    /// Set EPUB font size to an absolute value.
    SetFontSize(f32),
    /// Manual full refresh (Ctrl-R).
    Refresh,
    /// Request links for a page (one-shot query).
    GetLinks(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    Fixed,
    Reflowable,
}

pub enum RenderInfo {
    DocumentKind(DocumentKind),
    EpubFontSize(f32),
    NumPages(usize),
    Page(PageInfo),
    SearchResults { page_num: usize, num_results: usize },
    Toc(Vec<TocEntry>),
    Metadata(Vec<(String, String)>),
    Links(Vec<LinkInfo>),
}

// ---------------------------------------------------------------------------
// Link types
// ---------------------------------------------------------------------------

pub struct LinkInfo {
    pub text: String,
    pub uri: String,
    pub page: Option<usize>,
}

/// Tint colour constants (warm sepia tone).
pub const TINT_BLACK: i32 = i32::from_be_bytes([0, 0x70, 0x42, 0x14]);
pub const TINT_WHITE: i32 = i32::from_be_bytes([0, 0xF5, 0xE6, 0xC8]);

#[derive(Clone, Debug)]
pub struct TocEntry {
    pub title: String,
    pub page: usize,
    pub level: usize,
}

// ---------------------------------------------------------------------------
// Data types returned by the renderer
// ---------------------------------------------------------------------------

pub struct PageInfo {
    pub page_num: usize,
    pub img_data: ImageData,
    pub result_rects: Vec<HighlightRect>,
}

pub struct ImageData {
    /// Packed RGB pixel bytes owned by the render thread.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub row_stride: usize,
}

#[derive(Clone, Debug)]
pub struct HighlightRect {
    pub ul_x: u32,
    pub ul_y: u32,
    pub lr_x: u32,
    pub lr_y: u32,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PrevRender {
    successful: bool,
    num_search_found: Option<usize>,
    search_quads: Option<Vec<Quad>>,
}

#[derive(Debug, Clone, Copy)]
pub enum RotateDirection {
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

pub fn fill_default<T: Default>(vec: &mut Vec<T>, size: usize) {
    vec.clear();
    vec.resize_with(size, T::default);
}

fn active_window_bounds(
    page: usize,
    n_pages: NonZeroUsize,
    window: usize,
) -> (usize, NonZeroUsize) {
    let idx_start = page.saturating_sub(window / 2);
    let idx_end = idx_start.saturating_add(window).min(n_pages.get());
    (idx_start, NonZeroUsize::new(idx_end).unwrap_or(n_pages))
}

fn force_page_rerender(rendered: &mut [PrevRender], page: usize) {
    if let Some(rendered_page) = rendered.get_mut(page) {
        rendered_page.successful = false;
    }
}

fn queue_page_rerender(
    rendered: &mut [PrevRender],
    need_rerender: &mut VecDeque<usize>,
    page: usize,
) {
    if rendered.get(page).is_none() {
        return;
    }

    force_page_rerender(rendered, page);
    if !need_rerender.contains(&page) {
        need_rerender.push_back(page);
    }
}

fn jump_to_page(
    rendered: &mut [PrevRender],
    need_rerender: &mut VecDeque<usize>,
    page: usize,
    n_pages: NonZeroUsize,
) {
    let (start, end) = active_window_bounds(page, n_pages, RENDER_WINDOW);
    need_rerender.retain(|queued| (start..end.get()).contains(queued));
    force_page_rerender(rendered, page);
}

fn active_window_needs_render(
    rendered: &[PrevRender],
    current_page: usize,
    n_pages: NonZeroUsize,
) -> bool {
    let (start, end) = active_window_bounds(current_page, n_pages, RENDER_WINDOW);
    rendered[start..end.get()]
        .iter()
        .any(|page| !page.successful)
}

fn should_continue_rendering(
    rendered: &[PrevRender],
    need_rerender: &VecDeque<usize>,
    current_page: usize,
    n_pages: NonZeroUsize,
    search_term: Option<&str>,
) -> bool {
    !need_rerender.is_empty()
        || active_window_needs_render(rendered, current_page, n_pages)
        || (search_term.is_some() && rendered.iter().any(|page| page.num_search_found.is_none()))
}

fn copy_pixmap_rgb(pixmap: &Pixmap) -> Result<ImageData, RenderError> {
    let width = pixmap.width();
    let height = pixmap.height();
    let components = usize::from(pixmap.n());
    let pixels_per_row = width as usize;

    match components {
        3 => Ok(ImageData {
            pixels: pixmap.samples().to_vec(),
            width,
            height,
            row_stride: pixels_per_row * 3,
        }),
        4 => {
            let mut pixels = Vec::with_capacity(pixels_per_row * height as usize * 3);
            for chunk in pixmap.samples().chunks_exact(4) {
                pixels.extend_from_slice(&chunk[..3]);
            }

            Ok(ImageData {
                pixels,
                width,
                height,
                row_stride: pixels_per_row * 3,
            })
        }
        other => Err(RenderError::Converting(format!(
            "unsupported pixmap format: expected RGB/RGBA, got {other} components"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Render loop (blocking — must run in std::thread::spawn, not tokio::spawn)
// ---------------------------------------------------------------------------

/// Entry point for the render thread.
///
/// `col_h` / `col_w` are the cell pixel dimensions, used to convert rendered pixel sizes back to
/// cell counts for layout.
pub fn start_rendering(
    path: &Path,
    sender: Sender<Result<RenderInfo, RenderError>>,
    receiver: Receiver<RenderNotif>,
    col_h: u16,
    col_w: u16,
    black: i32,
    white: i32,
) -> Result<(), SendError<Result<RenderInfo, RenderError>>> {
    let mut search_term = None;
    let mut invert = false;
    let mut tinted = false;
    let mut rotate = RotateDirection::Deg0;
    let mut preserved_area: Option<(f32, f32)> = None;
    let mut start_point = 0usize;
    let mut reflow_em: Option<f32> = None;
    let mut last_layout: Option<(f32, f32, f32)> = None;
    let mut document_ready = false;

    let mut need_rerender = VecDeque::new();

    #[cfg(windows)]
    let path = path.to_string_lossy();

    'document: loop {
        let (area_w, area_h) = match preserved_area {
            Some(area) => area,
            None => loop {
                match receiver.recv() {
                    Ok(RenderNotif::Area {
                        width_px,
                        height_px,
                    }) => {
                        let area = (width_px, height_px);
                        preserved_area = Some(area);
                        break area;
                    }
                    Ok(RenderNotif::GetLinks(_)) => {}
                    Ok(RenderNotif::AdjustFontSize(delta)) => {
                        if let Some(em) = reflow_em.as_mut() {
                            *em = adjust_epub_em(*em, delta);
                        }
                    }
                    Ok(RenderNotif::SetFontSize(em)) => {
                        reflow_em = Some(clamp_epub_em(em));
                    }
                    Ok(RenderNotif::Refresh) => {}
                    Ok(_) => {}
                    Err(_) => return Ok(()),
                }
            },
        };

        #[cfg_attr(unix, allow(clippy::borrow_deref_ref))]
        let mut doc = match Document::open(&*path) {
            Err(e) => {
                sender.send(Err(RenderError::Mupdf(e)))?;
                if !document_ready {
                    return Ok(());
                }
                // Wait for a Refresh so the user can retry after fixing the file.
                while let Ok(msg) = receiver.recv() {
                    match msg {
                        RenderNotif::Area {
                            width_px,
                            height_px,
                        } => {
                            preserved_area = Some((width_px, height_px));
                        }
                        RenderNotif::Refresh => break,
                        _ => {}
                    }
                }
                continue;
            }
            Ok(d) => d,
        };

        let doc_kind = match doc.is_reflowable() {
            Ok(true) => DocumentKind::Reflowable,
            Ok(false) | Err(_) => DocumentKind::Fixed,
        };
        sender.send(Ok(RenderInfo::DocumentKind(doc_kind)))?;

        if matches!(doc_kind, DocumentKind::Reflowable) {
            let (layout_w, layout_h, default_em) =
                epub_layout_for_area(area_w.max(1.0), area_h.max(1.0), col_w, col_h);
            let layout_em = *reflow_em.get_or_insert(default_em);
            sender.send(Ok(RenderInfo::EpubFontSize(layout_em)))?;

            let start_layout = std::time::Instant::now();
            if let Err(e) = doc.layout(layout_w, layout_h, layout_em) {
                sender.send(Err(RenderError::Mupdf(e)))?;
                if !document_ready {
                    return Ok(());
                }
                while let Ok(msg) = receiver.recv() {
                    match msg {
                        RenderNotif::Area {
                            width_px,
                            height_px,
                        } => {
                            preserved_area = Some((width_px, height_px));
                            break;
                        }
                        RenderNotif::Refresh => break,
                        _ => {}
                    }
                }
                continue 'document;
            }
            let duration = start_layout.elapsed();
            log::info!(
                "EPUB layout took {:?} (w={:.1}, h={:.1}, em={:.1})",
                duration,
                layout_w,
                layout_h,
                layout_em
            );
            last_layout = Some((layout_w, layout_h, layout_em));
        }

        let n_pages = match doc.page_count() {
            Ok(n) => match NonZeroUsize::new(n as usize) {
                Some(n) => n,
                None => {
                    sender.send(Err(RenderError::EmptyDocument))?;
                    return Ok(());
                }
            },
            Err(e) => {
                sender.send(Err(RenderError::Mupdf(e)))?;
                if !document_ready {
                    return Ok(());
                }
                sleep(Duration::from_secs(1));
                continue;
            }
        };

        start_point = start_point.min(n_pages.get().saturating_sub(1));
        sender.send(Ok(RenderInfo::NumPages(n_pages.get())))?;
        document_ready = true;

        // Extract table of contents.
        if let Ok(outlines) = doc.outlines() {
            let mut toc = Vec::new();
            flatten_outlines(&outlines, 0, &mut toc);
            if !toc.is_empty() {
                sender.send(Ok(RenderInfo::Toc(toc)))?;
            }
        }

        // Extract metadata eagerly (cheap, one-time).
        {
            let meta = extract_metadata(&doc);
            if !meta.is_empty() {
                sender.send(Ok(RenderInfo::Metadata(meta)))?;
            }
        }

        let mut rendered = Vec::new();
        need_rerender.retain(|page_num| *page_num < n_pages.get());
        fill_default::<PrevRender>(&mut rendered, n_pages.get());

        'render_pages: loop {
            let (area_w, area_h) = preserved_area.unwrap_or_else(|| {
                let new_area = loop {
                    if let Ok(RenderNotif::Area {
                        width_px,
                        height_px,
                    }) = receiver.recv()
                    {
                        break (width_px, height_px);
                    }
                };
                preserved_area = Some(new_area);
                new_area
            });

            macro_rules! handle_notif {
                ($notif:ident) => {{
                    match $notif {
                        RenderNotif::Refresh => {
                            for page in &mut rendered {
                                page.successful = false;
                            }
                            continue 'render_pages;
                        }
                        RenderNotif::Invert => {
                            invert = !invert;
                            for page in &mut rendered {
                                page.successful = false;
                            }
                            continue 'render_pages;
                        }
                        RenderNotif::Tint => {
                            tinted = !tinted;
                            for page in &mut rendered {
                                page.successful = false;
                            }
                            continue 'render_pages;
                        }
                        RenderNotif::AdjustFontSize(delta) => {
                            if matches!(doc_kind, DocumentKind::Reflowable) {
                                let current = reflow_em.unwrap_or_else(|| {
                                    let (_, _, default_em) =
                                        epub_layout_for_area(area_w, area_h, col_w, col_h);
                                    default_em
                                });
                                let new_em = adjust_epub_em(current, delta);
                                if reflow_em != Some(new_em) {
                                    reflow_em = Some(new_em);
                                    if last_layout.is_some_and(|(_, _, lem)| lem != new_em) {
                                        continue 'document;
                                    }
                                }
                            }
                            continue 'render_pages;
                        }
                        RenderNotif::SetFontSize(em) => {
                            if matches!(doc_kind, DocumentKind::Reflowable) {
                                let new_em = clamp_epub_em(em);
                                if reflow_em != Some(new_em) {
                                    reflow_em = Some(new_em);
                                    if last_layout.is_some_and(|(_, _, lem)| lem != new_em) {
                                        continue 'document;
                                    }
                                }
                            }
                            continue 'render_pages;
                        }
                        RenderNotif::Area {
                            width_px,
                            height_px,
                        } => {
                            preserved_area = Some((width_px as f32, height_px as f32));
                            if matches!(doc_kind, DocumentKind::Reflowable) {
                                let (lw, lh, default_em) = epub_layout_for_area(
                                    width_px as f32,
                                    height_px as f32,
                                    col_w,
                                    col_h,
                                );
                                let lem = reflow_em.unwrap_or(default_em);
                                if last_layout != Some((lw, lh, lem)) {
                                    continue 'document;
                                }
                            }
                            fill_default(&mut rendered, n_pages.get());
                            continue 'render_pages;
                        }
                        RenderNotif::JumpToPage(page) => {
                            start_point = page;
                            jump_to_page(&mut rendered, &mut need_rerender, page, n_pages);
                            continue 'render_pages;
                        }
                        RenderNotif::PageNeedsReRender(page) => {
                            queue_page_rerender(&mut rendered, &mut need_rerender, page);
                            continue 'render_pages;
                        }
                        RenderNotif::Search(term) => {
                            if term.is_empty() {
                                for page in &mut rendered {
                                    if page.num_search_found.is_some_and(|n| n > 0) {
                                        page.num_search_found = Some(0);
                                        page.successful = false;
                                    }
                                }
                                search_term = None;
                            } else {
                                for page in &mut rendered {
                                    page.num_search_found = None;
                                }
                                search_term = Some(term);
                            }
                            continue 'render_pages;
                        }
                        RenderNotif::Rotate => {
                            rotate = match rotate {
                                RotateDirection::Deg0 => RotateDirection::Deg90,
                                RotateDirection::Deg90 => RotateDirection::Deg180,
                                RotateDirection::Deg180 => RotateDirection::Deg270,
                                RotateDirection::Deg270 => RotateDirection::Deg0,
                            };
                            for page in &mut rendered {
                                page.successful = false;
                            }
                            continue 'render_pages;
                        }
                        // One-shot queries handled before this macro; unreachable here.
                        RenderNotif::GetLinks(_) => {
                            continue 'render_pages;
                        }
                    }
                }};
            }

            struct PopOnNext<'a> {
                inner: &'a mut VecDeque<usize>,
            }
            impl Iterator for PopOnNext<'_> {
                type Item = usize;
                fn next(&mut self) -> Option<Self::Item> {
                    self.inner.pop_front()
                }
            }

            let any_not_searched = rendered.iter().any(|r| r.num_search_found.is_none());
            let current_page = start_point.min(n_pages.get().saturating_sub(1));
            let (idx_start, idx_end) = active_window_bounds(current_page, n_pages, RENDER_WINDOW);

            let page_iter = PopOnNext {
                inner: &mut need_rerender,
            }
            .chain(
                InterleavedAroundWithMax::new(current_page, idx_start, idx_end).take(
                    match &search_term {
                        None => RENDER_WINDOW,
                        Some(_) if any_not_searched => 20,
                        Some(_) => RENDER_WINDOW,
                    },
                ),
            );

            for page_num in page_iter {
                let r = &mut rendered[page_num];
                if r.successful && r.num_search_found.is_some() {
                    continue;
                }

                let page = match doc.load_page(page_num as i32) {
                    Err(e) => {
                        sender.send(Err(RenderError::Mupdf(e)))?;
                        continue;
                    }
                    Ok(p) => p,
                };

                let (eff_black, eff_white) = if tinted {
                    (TINT_BLACK, TINT_WHITE)
                } else {
                    (black, white)
                };

                let render_started = std::time::Instant::now();
                match render_single_page(
                    &page,
                    search_term.as_deref(),
                    r,
                    invert,
                    eff_black,
                    eff_white,
                    rotate,
                    (area_w, area_h),
                    col_w,
                    col_h,
                ) {
                    Ok(ctx) => {
                        perf::log_page_timing("render_page", page_num, render_started.elapsed());

                        let copy_started = std::time::Instant::now();
                        let img_data = match copy_pixmap_rgb(&ctx.pixmap) {
                            Ok(img_data) => img_data,
                            Err(e) => {
                                sender.send(Err(e))?;
                                continue;
                            }
                        };
                        perf::log_page_timing("render_rgb_copy", page_num, copy_started.elapsed());

                        log::debug!(
                            "rendered page {page_num} at {}x{}",
                            img_data.width,
                            img_data.height
                        );

                        r.num_search_found = Some(ctx.result_rects.len());
                        r.successful = true;

                        sender.send(Ok(RenderInfo::Page(PageInfo {
                            img_data,
                            page_num,
                            result_rects: ctx.result_rects,
                        })))?;
                    }
                    Err(e) => sender.send(Err(e))?,
                }

                match receiver.try_recv() {
                    Err(TryRecvError::Disconnected) => return Ok(()),
                    Ok(RenderNotif::GetLinks(pn)) => {
                        sender.send(Ok(RenderInfo::Links(extract_links(&doc, pn))))?;
                    }
                    Ok(notif) => handle_notif!(notif),
                    Err(TryRecvError::Empty) => (),
                }
            }

            // Search remaining pages for match counts.
            if let Some(ref term) = search_term {
                let mut search_start = start_point;
                loop {
                    const SEARCH_AT_TIME: usize = 20;

                    let page_idx = rendered[search_start..]
                        .iter_mut()
                        .enumerate()
                        .take(SEARCH_AT_TIME)
                        .filter(|(_, r)| r.num_search_found.is_none())
                        .map(|(idx, r)| (idx + search_start, r));

                    for (page_num, r) in page_idx {
                        let search_res = doc
                            .load_page(page_num as i32)
                            .and_then(|page| search_page(&page, Some(term), 0));

                        let num_results = if let Ok(quads) = search_res {
                            let count = quads.len();
                            r.search_quads = Some(quads);
                            count
                        } else {
                            r.search_quads = None;
                            0
                        };

                        if num_results > 0 {
                            r.successful = false;
                        }
                        r.num_search_found = Some(num_results);

                        sender.send(Ok(RenderInfo::SearchResults {
                            page_num,
                            num_results,
                        }))?;
                    }

                    search_start += SEARCH_AT_TIME;

                    if search_start > n_pages.get() {
                        if start_point == 0 {
                            break;
                        }
                        search_start = 0;
                    } else if ((search_start - SEARCH_AT_TIME) + 1..search_start)
                        .contains(&start_point)
                    {
                        break;
                    }

                    match receiver.try_recv() {
                        Err(TryRecvError::Empty) => (),
                        Err(TryRecvError::Disconnected) => return Ok(()),
                        Ok(RenderNotif::GetLinks(pn)) => {
                            sender.send(Ok(RenderInfo::Links(extract_links(&doc, pn))))?;
                        }
                        Ok(msg) => handle_notif!(msg),
                    }
                }
            }

            if should_continue_rendering(
                &rendered,
                &need_rerender,
                current_page,
                n_pages,
                search_term.as_deref(),
            ) {
                continue;
            }

            loop {
                let Ok(msg) = receiver.recv() else {
                    return Ok(());
                };
                match msg {
                    RenderNotif::GetLinks(pn) => {
                        sender.send(Ok(RenderInfo::Links(extract_links(&doc, pn))))?;
                    }
                    notif => handle_notif!(notif),
                }
            }
        }
    }
}

pub fn probe_document(path: &Path) -> Result<(), RenderError> {
    #[cfg_attr(unix, allow(clippy::borrow_deref_ref))]
    let mut doc = Document::open(&*path)?;

    let doc_kind = match doc.is_reflowable() {
        Ok(true) => DocumentKind::Reflowable,
        Ok(false) | Err(_) => DocumentKind::Fixed,
    };

    if matches!(doc_kind, DocumentKind::Reflowable) {
        let (layout_w, layout_h, layout_em) = epub_layout_for_area(1200.0, 800.0, 10, 20);
        doc.layout(layout_w, layout_h, layout_em)?;
    }

    let n_pages =
        NonZeroUsize::new(doc.page_count()? as usize).ok_or(RenderError::EmptyDocument)?;

    let _ = doc.outlines();
    let _ = extract_metadata(&doc);

    for page_num in 0..n_pages.get().min(3) {
        let page = doc.load_page(page_num as i32)?;
        let _rendered = render_single_page(
            &page,
            None,
            &PrevRender::default(),
            false,
            MUPDF_BLACK,
            MUPDF_WHITE,
            RotateDirection::Deg0,
            (1200.0, 1600.0),
            10,
            20,
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Single-page render
// ---------------------------------------------------------------------------

struct RenderedContext {
    pixmap: Pixmap,
    result_rects: Vec<HighlightRect>,
}

#[allow(clippy::too_many_arguments)]
fn render_single_page(
    page: &Page,
    search_term: Option<&str>,
    prev_render: &PrevRender,
    invert: bool,
    black: i32,
    white: i32,
    rotate: RotateDirection,
    (area_w, area_h): (f32, f32),
    col_w: u16,
    col_h: u16,
) -> Result<RenderedContext, RenderError> {
    let result_rects = if let Some(ref quads) = prev_render.search_quads {
        quads.clone()
    } else {
        match prev_render.num_search_found {
            None => search_page(page, search_term, 0)?,
            Some(0) => Vec::new(),
            Some(count @ 1..) => search_page(page, search_term, count)?,
        }
    };

    let bounds = page.bounds()?;
    let page_dim = match rotate {
        RotateDirection::Deg0 | RotateDirection::Deg180 => {
            (bounds.x1 - bounds.x0, bounds.y1 - bounds.y0)
        }
        RotateDirection::Deg90 | RotateDirection::Deg270 => {
            (bounds.y1 - bounds.y0, bounds.x1 - bounds.x0)
        }
    };

    let (surface_w, surface_h, mut scale_factor) = scale_fit(page_dim, (area_w, area_h));

    // Clamp to Kitty's max image dimension.
    if surface_w > KITTY_MAX_W_OR_H || surface_h > KITTY_MAX_W_OR_H {
        let descale = (surface_w / KITTY_MAX_W_OR_H).max(surface_h / KITTY_MAX_W_OR_H);
        scale_factor /= descale;
    }

    let colorspace = Colorspace::device_rgb();
    let mut matrix = Matrix::new_scale(scale_factor, scale_factor);
    match rotate {
        RotateDirection::Deg0 => {
            matrix.rotate(0.0);
        }
        RotateDirection::Deg90 => {
            matrix.rotate(90.0);
        }
        RotateDirection::Deg180 => {
            matrix.rotate(180.0);
        }
        RotateDirection::Deg270 => {
            matrix.rotate(270.0);
        }
    }

    let mut pixmap = page.to_pixmap(&matrix, &colorspace, false, false)?;

    if invert {
        pixmap.tint(white, black)?;
    } else if black != MUPDF_BLACK || white != MUPDF_WHITE {
        // Custom black/white colors or tint enabled
        pixmap.tint(black, white)?;
    }

    let (x_res, y_res) = pixmap.resolution();
    pixmap.set_resolution(
        (x_res as f32 * scale_factor) as i32,
        (y_res as f32 * scale_factor) as i32,
    );

    let result_rects = result_rects
        .into_iter()
        .map(|quad| HighlightRect {
            ul_x: (quad.ul.x * scale_factor) as u32,
            ul_y: (quad.ul.y * scale_factor) as u32,
            lr_x: (quad.lr.x * scale_factor) as u32,
            lr_y: (quad.lr.y * scale_factor) as u32,
        })
        .collect();

    // Calculate cell size using the col dimensions passed in from main.
    let _ = col_w;
    let _ = col_h;

    Ok(RenderedContext {
        pixmap,
        result_rects,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Scale `(img_w, img_h)` to fit within `(area_w, area_h)` (fit, not fill).
/// Returns `(scaled_w, scaled_h, scale_factor)`.
fn scale_fit((img_w, img_h): (f32, f32), (area_w, area_h): (f32, f32)) -> (f32, f32, f32) {
    let img_ar = img_w / img_h;
    let area_ar = area_w / area_h;
    let scale = if img_ar > area_ar {
        area_w / img_w
    } else {
        area_h / img_h
    };
    (img_w * scale, img_h * scale, scale)
}

fn epub_layout_for_area(
    area_w_px: f32,
    area_h_px: f32,
    col_w_px: u16,
    col_h_px: u16,
) -> (f32, f32, f32) {
    let terminal_cols = (area_w_px / f32::from(col_w_px.max(1))).max(1.0);
    let terminal_rows = (area_h_px / f32::from(col_h_px.max(1))).max(1.0);

    // Reflowed EPUB pages read better as a narrower, book-like column than the
    // full terminal width. Scale the layout width from terminal columns, but
    // keep it within common paperback sizes.
    let layout_w = (terminal_cols * 2.8).clamp(EPUB_LAYOUT_MIN_W, EPUB_LAYOUT_MAX_W);
    let layout_h = (layout_w * EPUB_LAYOUT_ASPECT).min((terminal_rows * 18.0).max(layout_w));

    let layout_em = default_epub_em(layout_w);

    (layout_w, layout_h, layout_em)
}

fn default_epub_em(layout_w: f32) -> f32 {
    clamp_epub_em((layout_w / (EPUB_TARGET_CHARS_PER_LINE * EPUB_APPROX_CHAR_WIDTH_EM)).round())
}

fn clamp_epub_em(em: f32) -> f32 {
    em.clamp(EPUB_EM_MIN, EPUB_EM_MAX)
}

fn adjust_epub_em(em: f32, delta: i8) -> f32 {
    clamp_epub_em(em + f32::from(delta))
}

fn search_page(
    page: &Page,
    search_term: Option<&str>,
    trusted_count: usize,
) -> Result<Vec<Quad>, mupdf::error::Error> {
    search_term
        .map(|term| {
            page.to_text_page(TextPageFlags::empty()).and_then(|tp| {
                let mut v = Vec::with_capacity(trusted_count);
                tp.search_cb(term, &mut v, |v, results| {
                    v.extend(results.iter().cloned());
                    SearchHitResponse::ContinueSearch
                })
                .map(|_| v)
            })
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn extract_metadata(doc: &Document) -> Vec<(String, String)> {
    let keys: &[(&str, MetadataName)] = &[
        ("Format", MetadataName::Format),
        ("Encryption", MetadataName::Encryption),
        ("Title", MetadataName::Title),
        ("Author", MetadataName::Author),
        ("Subject", MetadataName::Subject),
        ("Keywords", MetadataName::Keywords),
        ("Creator", MetadataName::Creator),
        ("Producer", MetadataName::Producer),
        ("Creation Date", MetadataName::CreationDate),
        ("Modification Date", MetadataName::ModDate),
    ];

    let mut meta = Vec::new();
    for (label, key) in keys {
        if let Ok(val) = doc.metadata(*key) {
            let val = val.trim().to_owned();
            if !val.is_empty() {
                meta.push((label.to_string(), val));
            }
        }
    }
    meta
}

fn extract_links(doc: &Document, page_num: usize) -> Vec<LinkInfo> {
    let page = match doc.load_page(page_num as i32) {
        Ok(p) => p,
        Err(_) => return vec![],
    };
    let links = match page.links() {
        Ok(l) => l,
        Err(_) => return vec![],
    };

    links
        .map(|link| {
            let uri = link.uri.clone();
            // dest is Some for internal links, None for external
            let resolved_page = link.dest.as_ref().map(|d| d.loc.page_number as usize);

            let text = if let Some(p) = resolved_page {
                format!("Page {}", p + 1)
            } else {
                uri.clone()
            };

            LinkInfo {
                text,
                uri,
                page: resolved_page,
            }
        })
        .collect()
}

fn flatten_outlines(outlines: &[mupdf::Outline], level: usize, out: &mut Vec<TocEntry>) {
    for o in outlines {
        let page = o
            .dest
            .as_ref()
            .map(|d| d.loc.page_number as usize)
            .unwrap_or(0);
        out.push(TocEntry {
            title: o.title.clone(),
            page,
            level,
        });
        if !o.down.is_empty() {
            flatten_outlines(&o.down, level + 1, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn write_temp_invalid_file(ext: &str, contents: &[u8]) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "treader-invalid-{}-{}-{}.{}",
            std::process::id(),
            unique,
            contents.len(),
            ext
        ));
        fs::write(&path, contents).unwrap();
        path
    }

    fn assert_invalid_document_exits(ext: &str, contents: &[u8]) {
        let path = write_temp_invalid_file(ext, contents);
        let (render_tx, render_rx) = flume::unbounded();
        let (notif_tx, notif_rx) = flume::unbounded();
        notif_tx
            .send(RenderNotif::Area {
                width_px: 800.0,
                height_px: 600.0,
            })
            .unwrap();

        let result = start_rendering(&path, render_tx, notif_rx, 20, 10, MUPDF_BLACK, MUPDF_WHITE);

        fs::remove_file(&path).unwrap();

        assert!(
            result.is_ok(),
            "renderer should exit cleanly for invalid documents"
        );
        assert!(
            matches!(
                render_rx.recv().unwrap(),
                Err(RenderError::Mupdf(_)) | Err(RenderError::EmptyDocument)
            ),
            "renderer should report an invalid-document error",
        );
    }

    // --- scale_fit ---

    #[test]
    fn empty_pdf_reports_error_and_exits() {
        assert_invalid_document_exits("pdf", b"");
    }

    #[test]
    fn empty_epub_reports_error_and_exits() {
        assert_invalid_document_exits("epub", b"");
    }

    #[test]
    fn fake_pdf_reports_error_and_exits() {
        assert_invalid_document_exits("pdf", b"not a real pdf");
    }

    #[test]
    fn fake_epub_reports_error_and_exits() {
        assert_invalid_document_exits("epub", b"not a real epub");
    }

    #[test]
    fn scale_fit_landscape_into_square() {
        let (w, h, s) = scale_fit((200.0, 100.0), (100.0, 100.0));
        assert!((w - 100.0).abs() < 0.01);
        assert!((h - 50.0).abs() < 0.01);
        assert!((s - 0.5).abs() < 0.01);
    }

    #[test]
    fn scale_fit_portrait_into_square() {
        let (w, h, s) = scale_fit((100.0, 200.0), (100.0, 100.0));
        assert!((w - 50.0).abs() < 0.01);
        assert!((h - 100.0).abs() < 0.01);
        assert!((s - 0.5).abs() < 0.01);
    }

    #[test]
    fn scale_fit_already_fits() {
        let (w, h, s) = scale_fit((50.0, 50.0), (100.0, 100.0));
        assert!((w - 100.0).abs() < 0.01);
        assert!((h - 100.0).abs() < 0.01);
        assert!((s - 2.0).abs() < 0.01);
    }

    #[test]
    fn scale_fit_exact_match() {
        let (w, h, s) = scale_fit((100.0, 100.0), (100.0, 100.0));
        assert!((s - 1.0).abs() < 0.01);
        assert!((w - 100.0).abs() < 0.01);
        assert!((h - 100.0).abs() < 0.01);
    }

    #[test]
    fn scale_fit_wide_into_narrow() {
        // 400x100 into 200x300 → width-constrained
        let (w, h, s) = scale_fit((400.0, 100.0), (200.0, 300.0));
        assert!((w - 200.0).abs() < 0.01);
        assert!((h - 50.0).abs() < 0.01);
        assert!((s - 0.5).abs() < 0.01);
    }

    #[test]
    fn scale_fit_tall_into_wide() {
        // 100x400 into 300x200 → height-constrained
        let (w, h, s) = scale_fit((100.0, 400.0), (300.0, 200.0));
        assert!((w - 50.0).abs() < 0.01);
        assert!((h - 200.0).abs() < 0.01);
        assert!((s - 0.5).abs() < 0.01);
    }

    #[test]
    fn epub_layout_uses_book_like_width() {
        let (w, h, em) = epub_layout_for_area(1200.0, 700.0, 10, 20);
        assert!(w >= EPUB_LAYOUT_MIN_W && w <= EPUB_LAYOUT_MAX_W);
        assert!(h > w);
        assert!(em >= EPUB_EM_MIN && em <= EPUB_EM_MAX);
        assert!(
            w < 500.0,
            "epub layout width should not match raw terminal pixels"
        );
    }

    #[test]
    fn adjust_epub_em_clamps_to_bounds() {
        assert!((adjust_epub_em(EPUB_EM_MIN, -1) - EPUB_EM_MIN).abs() < f32::EPSILON);
        assert!((adjust_epub_em(EPUB_EM_MAX, 1) - EPUB_EM_MAX).abs() < f32::EPSILON);
        assert!((adjust_epub_em(10.0, 1) - 11.0).abs() < f32::EPSILON);
        assert!((adjust_epub_em(10.0, -1) - 9.0).abs() < f32::EPSILON);
    }

    // --- fill_default ---

    #[test]
    fn fill_default_initialises_to_size() {
        let mut v: Vec<PrevRender> = vec![];
        fill_default(&mut v, 5);
        assert_eq!(v.len(), 5);
        assert!(v.iter().all(|r| !r.successful));
    }

    #[test]
    fn fill_default_clears_existing() {
        let mut v: Vec<PrevRender> = vec![];
        fill_default(&mut v, 3);
        v[0].successful = true;
        fill_default(&mut v, 2);
        assert_eq!(v.len(), 2);
        assert!(!v[0].successful);
    }

    #[test]
    fn jump_to_page_clears_stale_rerender_backlog() {
        let mut rendered: Vec<PrevRender> = Vec::new();
        fill_default(&mut rendered, 64);
        rendered[40].successful = true;
        let mut need_rerender = VecDeque::from([0, 1, 2, 3]);

        jump_to_page(
            &mut rendered,
            &mut need_rerender,
            40,
            NonZeroUsize::new(64).unwrap(),
        );

        assert!(need_rerender.is_empty());
        assert!(!rendered[40].successful);
    }

    #[test]
    fn jump_to_page_retains_overlapping_window_entries() {
        let mut rendered: Vec<PrevRender> = Vec::new();
        fill_default(&mut rendered, 64);
        let mut need_rerender = VecDeque::from([18, 21, 24, 40]);

        jump_to_page(
            &mut rendered,
            &mut need_rerender,
            30,
            NonZeroUsize::new(64).unwrap(),
        );

        assert_eq!(need_rerender, VecDeque::from([18, 21, 24, 40]));
        assert!(!rendered[30].successful);
    }

    #[test]
    fn queue_page_rerender_marks_page_dirty_once() {
        let mut rendered: Vec<PrevRender> = Vec::new();
        fill_default(&mut rendered, 4);
        rendered[2].successful = true;
        let mut need_rerender = VecDeque::new();

        queue_page_rerender(&mut rendered, &mut need_rerender, 2);
        queue_page_rerender(&mut rendered, &mut need_rerender, 2);

        assert_eq!(need_rerender, VecDeque::from([2]));
        assert!(!rendered[2].successful);
    }

    #[test]
    fn should_not_continue_rendering_for_out_of_window_pages() {
        let mut rendered: Vec<PrevRender> = Vec::new();
        fill_default(&mut rendered, 64);
        for page in rendered.iter_mut().take(RENDER_WINDOW) {
            page.successful = true;
            page.num_search_found = Some(0);
        }
        rendered[40].successful = false;

        assert!(!should_continue_rendering(
            &rendered,
            &VecDeque::new(),
            0,
            NonZeroUsize::new(64).unwrap(),
            None,
        ));
    }

    #[test]
    fn should_continue_rendering_for_pending_current_window_page() {
        let mut rendered: Vec<PrevRender> = Vec::new();
        fill_default(&mut rendered, 64);
        for page in rendered.iter_mut() {
            page.successful = true;
            page.num_search_found = Some(0);
        }
        rendered[12].successful = false;

        assert!(should_continue_rendering(
            &rendered,
            &VecDeque::new(),
            0,
            NonZeroUsize::new(64).unwrap(),
            None,
        ));
    }

    // --- RotateDirection ---

    #[test]
    fn rotate_direction_cycle() {
        let mut r = RotateDirection::Deg0;
        r = match r {
            RotateDirection::Deg0 => RotateDirection::Deg90,
            d => d,
        };
        assert!(matches!(r, RotateDirection::Deg90));
        r = match r {
            RotateDirection::Deg90 => RotateDirection::Deg180,
            d => d,
        };
        assert!(matches!(r, RotateDirection::Deg180));
        r = match r {
            RotateDirection::Deg180 => RotateDirection::Deg270,
            d => d,
        };
        assert!(matches!(r, RotateDirection::Deg270));
        r = match r {
            RotateDirection::Deg270 => RotateDirection::Deg0,
            d => d,
        };
        assert!(matches!(r, RotateDirection::Deg0));
    }
}
