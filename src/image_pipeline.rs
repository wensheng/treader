// Async image pipeline: receives raw RGB pages from the renderer, converts them to
// kittage Kitty images, and sends them to the main loop for display.

use std::{
    collections::HashMap,
    num::NonZeroUsize,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use flume::{Receiver, SendError, Sender, TryRecvError};
use futures_util::stream::StreamExt as _;
use image::{DynamicImage, RgbImage};
use kittage::{ImageId, NumberOrId, action::NONZERO_ONE};
use rayon::prelude::*;

use crate::{
    error::RenderError,
    perf,
    renderer::{HighlightRect, PageInfo},
};

const CROP_WHITESPACE_THRESHOLD: u8 = 240;
const CROP_MARGIN: u32 = 8;
/// Images smaller than this (in total pixels) use the sequential path to avoid
/// Rayon thread-pool overhead.  256 × 256 = 65 536 pixels.
const PAR_PIXEL_CUTOFF: u32 = 256 * 256;
static NEXT_CONVERTED_GENERATION: AtomicU64 = AtomicU64::new(1);

enum PlusOrMinus {
    Plus,
    Minus,
}

/// Iterates outward from `around`, alternating above and below, clipping at the bounds.
/// Used to prerender pages in priority order: current page first, then +/-1, +/-2, etc.
pub struct InterleavedAroundWithMax {
    around: usize,
    inclusive_min: usize,
    exclusive_max: NonZeroUsize,
    remaining: usize,
    next_change: usize,
    next_op: PlusOrMinus,
    yielded_current: bool,
}

impl InterleavedAroundWithMax {
    /// Preconditions: `inclusive_min < exclusive_max` and `inclusive_min <= around < exclusive_max`.
    #[must_use]
    pub fn new(around: usize, inclusive_min: usize, exclusive_max: NonZeroUsize) -> Self {
        Self {
            around,
            inclusive_min,
            exclusive_max,
            remaining: exclusive_max.get() - inclusive_min,
            next_change: 1,
            next_op: PlusOrMinus::Plus,
            yielded_current: false,
        }
    }
}

impl Iterator for InterleavedAroundWithMax {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }

        if !self.yielded_current {
            self.yielded_current = true;
            self.remaining -= 1;
            return Some(self.around);
        }

        while self.remaining > 0 {
            let candidate = match self.next_op {
                PlusOrMinus::Plus => self.around.checked_add(self.next_change),
                PlusOrMinus::Minus => self.around.checked_sub(self.next_change),
            };

            self.next_op = match self.next_op {
                PlusOrMinus::Plus => PlusOrMinus::Minus,
                PlusOrMinus::Minus => {
                    self.next_change += 1;
                    PlusOrMinus::Plus
                }
            };

            if let Some(next_val) = candidate
                && next_val >= self.inclusive_min
                && next_val < self.exclusive_max.get()
            {
                self.remaining -= 1;
                return Some(next_val);
            }
        }

        None
    }
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum MaybeTransferred {
    /// Image data ready to transmit for the first time.
    NotYet(kittage::image::Image<'static>),
    /// Image already in Kitty's memory; use this ID to re-display.
    Transferred(kittage::ImageId),
}

impl MaybeTransferred {
    pub fn image_id_hint(&self) -> Option<ImageId> {
        match self {
            Self::NotYet(image) => match image.num_or_id {
                NumberOrId::Id(id) => Some(id),
                NumberOrId::Number(_) => None,
            },
            Self::Transferred(id) => Some(*id),
        }
    }
}

pub struct ConvertedPage {
    pub page_num: usize,
    pub img: MaybeTransferred,
    pub pixel_w: u32,
    pub pixel_h: u32,
    pub generation: u64,
    pub num_results: usize,
}

// ---------------------------------------------------------------------------
// Input message type
// ---------------------------------------------------------------------------

pub enum ConverterMsg {
    NumPages(usize),
    GoToPage(usize),
    AddImg(PageInfo),
    AutoCrop(bool),
}

fn active_window_bounds(
    page: usize,
    n_pages: usize,
    prerender: usize,
) -> Option<(usize, NonZeroUsize)> {
    if n_pages == 0 {
        return None;
    }

    let idx_start = page.saturating_sub(prerender / 2);
    let idx_end = idx_start.saturating_add(prerender).min(n_pages);
    NonZeroUsize::new(idx_end).map(|end| (idx_start, end))
}

fn page_is_in_active_window(
    page_num: usize,
    page: usize,
    n_pages: usize,
    prerender: usize,
) -> bool {
    active_window_bounds(page, n_pages, prerender)
        .map(|(start, end)| (start..end.get()).contains(&page_num))
        .unwrap_or(false)
}

fn pending_bytes(images: &HashMap<usize, PageInfo>) -> u64 {
    images
        .values()
        .map(|page| page.img_data.pixels.len() as u64)
        .sum()
}

fn image_from_rgb_data(img_data: crate::renderer::ImageData) -> Result<RgbImage, RenderError> {
    let crate::renderer::ImageData {
        pixels,
        width,
        height,
        row_stride,
    } = img_data;

    let tight_stride = width as usize * 3;
    if row_stride < tight_stride {
        return Err(RenderError::Converting(format!(
            "invalid RGB row stride: {row_stride} for width {width}"
        )));
    }

    let required_len = row_stride
        .checked_mul(height as usize)
        .ok_or_else(|| RenderError::Converting("RGB buffer size overflow".to_owned()))?;
    if pixels.len() < required_len {
        return Err(RenderError::Converting(format!(
            "RGB buffer too short: got {}, need {required_len}",
            pixels.len()
        )));
    }

    let packed = if row_stride == tight_stride {
        pixels
    } else {
        let mut tight = Vec::with_capacity(tight_stride * height as usize);
        for row in pixels.chunks(row_stride).take(height as usize) {
            tight.extend_from_slice(&row[..tight_stride]);
        }
        tight
    };

    RgbImage::from_raw(width, height, packed).ok_or_else(|| {
        RenderError::Converting(format!(
            "cannot build RGB image from raw buffer {width}x{height}"
        ))
    })
}

fn apply_search_highlights(img: &mut RgbImage, rects: &[HighlightRect]) {
    if rects.is_empty() {
        return;
    }

    let width = img.width();
    let height = img.height();
    if width == 0 || height == 0 {
        return;
    }

    let row_intervals = highlight_intervals_per_row(rects, width, height);
    let row_stride = width as usize * 3;
    const BLUE_DIM: u8 = u8::MAX / 2;

    let apply_row = |y: usize, row: &mut [u8]| {
        let intervals = &row_intervals[y];
        for &(x_start, x_end) in intervals {
            for x in x_start..x_end {
                let blue_idx = x as usize * 3 + 2;
                row[blue_idx] = row[blue_idx].saturating_sub(BLUE_DIM);
            }
        }
    };

    let pixels = img.as_mut();
    if width * height < PAR_PIXEL_CUTOFF {
        for (y, row) in pixels.chunks_mut(row_stride).enumerate() {
            apply_row(y, row);
        }
    } else {
        pixels
            .par_chunks_mut(row_stride)
            .enumerate()
            .for_each(|(y, row)| apply_row(y, row));
    }
}

fn highlight_intervals_per_row(
    rects: &[HighlightRect],
    width: u32,
    height: u32,
) -> Vec<Vec<(u32, u32)>> {
    let mut rows = vec![Vec::<(u32, u32)>::new(); height as usize];

    for rect in rects {
        let x_start = rect.ul_x.saturating_add(1).min(width);
        let x_end = rect.lr_x.min(width);
        let y_start = rect.ul_y.saturating_add(1).min(height);
        let y_end = rect.lr_y.min(height);
        if x_start >= x_end || y_start >= y_end {
            continue;
        }

        for y in y_start..y_end {
            rows[y as usize].push((x_start, x_end));
        }
    }

    for row in &mut rows {
        if row.len() <= 1 {
            continue;
        }

        row.sort_unstable_by_key(|(start, end)| (*start, *end));
        let mut merged = Vec::with_capacity(row.len());
        let mut cur = row[0];
        for &(start, end) in row.iter().skip(1) {
            if start <= cur.1 {
                cur.1 = cur.1.max(end);
            } else {
                merged.push(cur);
                cur = (start, end);
            }
        }
        merged.push(cur);
        *row = merged;
    }

    rows
}

/// Per-row crop-bounds computed by the parallel highlight+crop pass.
struct RowBounds {
    min_x: u32,
    max_x: u32,
    has_content: bool,
}

fn apply_search_highlights_and_crop(img: RgbImage, rects: &[HighlightRect]) -> RgbImage {
    let (width, height) = img.dimensions();
    if width == 0 || height == 0 {
        return img;
    }

    let row_intervals = highlight_intervals_per_row(rects, width, height);
    let row_stride = width as usize * 3;
    let mut img = img;

    // Process one row: apply highlight tinting and compute crop bounds.
    let process_row = |y: usize, row: &mut [u8]| -> RowBounds {
        let intervals = &row_intervals[y];
        let mut interval_idx = 0usize;
        let mut local_min_x = width;
        let mut local_max_x = 0u32;
        let mut has_content = false;

        for x in 0..width {
            while interval_idx < intervals.len() && x >= intervals[interval_idx].1 {
                interval_idx += 1;
            }

            let blue_idx = x as usize * 3 + 2;
            if interval_idx < intervals.len() && x >= intervals[interval_idx].0 {
                row[blue_idx] = row[blue_idx].saturating_sub(u8::MAX / 2);
            }

            let r = row[x as usize * 3];
            let g = row[x as usize * 3 + 1];
            let b = row[blue_idx];
            if r < CROP_WHITESPACE_THRESHOLD
                || g < CROP_WHITESPACE_THRESHOLD
                || b < CROP_WHITESPACE_THRESHOLD
            {
                has_content = true;
                local_min_x = local_min_x.min(x);
                local_max_x = local_max_x.max(x);
            }
        }

        RowBounds {
            min_x: local_min_x,
            max_x: local_max_x,
            has_content,
        }
    };

    // Collect per-row bounds (parallel or sequential depending on size).
    let row_bounds: Vec<RowBounds> = {
        let pixels = img.as_mut();
        if width * height < PAR_PIXEL_CUTOFF {
            pixels
                .chunks_mut(row_stride)
                .enumerate()
                .map(|(y, row)| process_row(y, row))
                .collect()
        } else {
            pixels
                .par_chunks_mut(row_stride)
                .enumerate()
                .map(|(y, row)| process_row(y, row))
                .collect()
        }
    };

    // Reduce per-row bounds into global crop rectangle.
    let mut min_x = width;
    let mut max_x = 0u32;
    let mut min_y = height;
    let mut max_y = 0u32;
    let mut saw_content = false;

    for (y, rb) in row_bounds.iter().enumerate() {
        if rb.has_content {
            saw_content = true;
            min_x = min_x.min(rb.min_x);
            max_x = max_x.max(rb.max_x);
            min_y = min_y.min(y as u32);
            max_y = max_y.max(y as u32);
        }
    }

    if !saw_content {
        return img;
    }

    let x0 = min_x.saturating_sub(CROP_MARGIN);
    let y0 = min_y.saturating_sub(CROP_MARGIN);
    let x1 = (max_x + CROP_MARGIN + 1).min(width);
    let y1 = (max_y + CROP_MARGIN + 1).min(height);

    if x0 == 0 && y0 == 0 && x1 == width && y1 == height {
        img
    } else {
        image::imageops::crop_imm(&img, x0, y0, x1 - x0, y1 - y0).to_image()
    }
}

fn process_rgb_image(img: RgbImage, rects: &[HighlightRect], auto_crop: bool) -> RgbImage {
    if rects.is_empty() {
        if auto_crop {
            return crop_whitespace(DynamicImage::ImageRgb8(img)).to_rgb8();
        }
        return img;
    }

    if auto_crop {
        apply_search_highlights_and_crop(img, rects)
    } else {
        let mut img = img;
        apply_search_highlights(&mut img, rects);
        img
    }
}

fn prune_pending_pages(
    images: &mut HashMap<usize, PageInfo>,
    page: usize,
    n_pages: usize,
    prerender: usize,
) {
    let before = images.len();

    let mut distances: Vec<(usize, u64)> = images
        .iter()
        .map(|(&k, v)| {
            let bytes = u64::from(v.img_data.width) * u64::from(v.img_data.height) * 3;
            (k, bytes)
        })
        .collect();

    distances.sort_by_key(|&(k, _)| k.abs_diff(page));

    let max_bytes = 250 * 1024 * 1024; // 250 MB
    let mut keep = std::collections::HashSet::new();
    let mut resident_bytes = 0u64;
    let mut resident_pages = 0usize;

    for (k, bytes) in distances {
        if resident_bytes + bytes <= max_bytes || resident_pages == 0 {
            keep.insert(k);
            resident_bytes += bytes;
            resident_pages += 1;
        }
    }

    match active_window_bounds(page, n_pages, prerender) {
        Some((start, end)) => {
            images.retain(|page_num, _| {
                (start..end.get()).contains(page_num) && keep.contains(page_num)
            });
        }
        None => images.clear(),
    }

    let evicted = before.saturating_sub(images.len());
    if evicted > 0 || log::log_enabled!(log::Level::Debug) {
        perf::log_cache_state(
            "converter_pending",
            images.len(),
            pending_bytes(images),
            evicted,
        );
    }
}

fn apply_converter_msg(
    msg: ConverterMsg,
    images: &mut HashMap<usize, PageInfo>,
    page: &mut usize,
    n_pages: &mut usize,
    auto_crop: &mut bool,
    prerender: usize,
) {
    match msg {
        ConverterMsg::AddImg(img) => {
            if img.page_num < *n_pages
                && page_is_in_active_window(img.page_num, *page, *n_pages, prerender)
            {
                images.insert(img.page_num, img);
            }
        }
        ConverterMsg::NumPages(total_pages) => {
            images.clear();
            *n_pages = total_pages;
            *page = (*page).min(total_pages.saturating_sub(1));
        }
        ConverterMsg::GoToPage(new_page) => *page = new_page,
        ConverterMsg::AutoCrop(v) => *auto_crop = v,
    }

    prune_pending_pages(images, *page, *n_pages, prerender);
}

fn dequeue_next_page(
    images: &mut HashMap<usize, PageInfo>,
    page: usize,
    n_pages: usize,
    iteration: &mut usize,
    prerender: usize,
) -> Option<PageInfo> {
    if images.is_empty() || n_pages == 0 || *iteration >= prerender {
        return None;
    }

    let (idx_start, idx_end) = active_window_bounds(page, n_pages, prerender)?;
    let current_page = page.min(n_pages.saturating_sub(1));
    let (page_info, new_iter) = InterleavedAroundWithMax::new(current_page, idx_start, idx_end)
        .enumerate()
        .take(prerender)
        .find_map(|(i_idx, p_idx)| images.remove(&p_idx).map(|p| (p, i_idx)))?;

    *iteration = new_iter;
    Some(page_info)
}

fn convert_page_info(
    page_info: PageInfo,
    pid: u32,
    shms_work: bool,
    auto_crop: bool,
) -> Result<ConvertedPage, RenderError> {
    let convert_started = std::time::Instant::now();
    let PageInfo {
        page_num,
        img_data,
        result_rects,
    } = page_info;
    let rgb_img = image_from_rgb_data(img_data)?;
    let final_img = process_rgb_image(rgb_img, &result_rects, auto_crop);
    let final_w = final_img.width();
    let final_h = final_img.height();

    // Generate a unique name for shared memory segments.
    let rn = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        % 1_000_000;

    let mut img = if shms_work {
        let shm_name = format!("/treader_{pid}_{rn}_{page_num}");
        #[cfg(unix)]
        let shm_name = &*shm_name;
        kittage::image::Image::shm_from(DynamicImage::ImageRgb8(final_img), shm_name)
            .map_err(|e| RenderError::Converting(format!("cannot create shm: {e:?}")))?
    } else {
        kittage::image::Image::from(DynamicImage::ImageRgb8(final_img))
    };

    // Assign a stable per-page ID (1-indexed since 0 is invalid).
    img.num_or_id = NumberOrId::Id(NONZERO_ONE.saturating_add(page_num as u32));

    Ok(ConvertedPage {
        page_num,
        img: MaybeTransferred::NotYet(img),
        pixel_w: final_w,
        pixel_h: final_h,
        generation: NEXT_CONVERTED_GENERATION.fetch_add(1, Ordering::Relaxed),
        num_results: result_rects.len(),
    })
    .inspect(|_| perf::log_page_timing("converter_page", page_num, convert_started.elapsed()))
}

// ---------------------------------------------------------------------------
// Conversion loop
// ---------------------------------------------------------------------------

pub async fn run_conversion_loop(
    sender: Sender<Result<ConvertedPage, RenderError>>,
    receiver: Receiver<ConverterMsg>,
    prerender: usize,
    shms_work: bool,
) -> Result<(), SendError<Result<ConvertedPage, RenderError>>> {
    let mut images: HashMap<usize, PageInfo> = HashMap::new();
    let mut n_pages = 0usize;
    let mut page: usize = 0;
    let pid = std::process::id();
    let mut auto_crop = false;

    'outer: loop {
        let mut iteration = 0;
        loop {
            match receiver.try_recv() {
                Ok(msg) => {
                    apply_converter_msg(
                        msg,
                        &mut images,
                        &mut page,
                        &mut n_pages,
                        &mut auto_crop,
                        prerender,
                    );
                    continue 'outer;
                }
                Err(TryRecvError::Empty) => (),
                Err(TryRecvError::Disconnected) => return Ok(()),
            }

            let Some(page_info) =
                dequeue_next_page(&mut images, page, n_pages, &mut iteration, prerender)
            else {
                break;
            };

            let converted = tokio::task::spawn_blocking({
                let page_info = page_info;
                move || convert_page_info(page_info, pid, shms_work, auto_crop)
            })
            .await
            .map_err(|e| RenderError::Converting(format!("converter worker failed to join: {e}")))
            .and_then(|res| res);

            sender.send(converted)?;
        }

        let Some(msg) = receiver.stream().next().await else {
            break;
        };
        apply_converter_msg(
            msg,
            &mut images,
            &mut page,
            &mut n_pages,
            &mut auto_crop,
            prerender,
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Auto-crop helper
// ---------------------------------------------------------------------------

/// Detect whitespace margins and crop the image to the content bounding box
/// with a small margin.
fn crop_whitespace(img: DynamicImage) -> DynamicImage {
    let rgb = img.to_rgb8();
    let (w, h) = (rgb.width(), rgb.height());
    if w == 0 || h == 0 {
        return img;
    }

    let row_stride = w as usize * 3;
    let pixels: &[u8] = rgb.as_ref();

    // Compute per-row x-bounds.
    let scan_row = |row: &[u8]| -> (u32, u32, bool) {
        let mut local_min = w;
        let mut local_max = 0u32;
        let mut has = false;
        for x in 0..w {
            let off = x as usize * 3;
            let r = row[off];
            let g = row[off + 1];
            let b = row[off + 2];
            if r < CROP_WHITESPACE_THRESHOLD
                || g < CROP_WHITESPACE_THRESHOLD
                || b < CROP_WHITESPACE_THRESHOLD
            {
                has = true;
                local_min = local_min.min(x);
                local_max = local_max.max(x);
            }
        }
        (local_min, local_max, has)
    };

    let row_bounds: Vec<(u32, u32, bool)> = if w * h < PAR_PIXEL_CUTOFF {
        pixels.chunks(row_stride).map(scan_row).collect()
    } else {
        pixels.par_chunks(row_stride).map(scan_row).collect()
    };

    let mut min_x = w;
    let mut max_x = 0u32;
    let mut min_y = h;
    let mut max_y = 0u32;

    for (y, &(rmin, rmax, has)) in row_bounds.iter().enumerate() {
        if has {
            min_x = min_x.min(rmin);
            max_x = max_x.max(rmax);
            min_y = min_y.min(y as u32);
            max_y = max_y.max(y as u32);
        }
    }

    if min_x > max_x || min_y > max_y {
        // All white — return as-is
        return img;
    }

    // Add margin, clamped to image bounds
    let x0 = min_x.saturating_sub(CROP_MARGIN);
    let y0 = min_y.saturating_sub(CROP_MARGIN);
    let x1 = (max_x + CROP_MARGIN + 1).min(w);
    let y1 = (max_y + CROP_MARGIN + 1).min(h);

    img.crop_imm(x0, y0, x1 - x0, y1 - y0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage};

    fn raw_image_data(width: u32, height: u32, row_stride: usize) -> crate::renderer::ImageData {
        crate::renderer::ImageData {
            pixels: vec![0; row_stride * height as usize],
            width,
            height,
            row_stride,
        }
    }

    /// Create a white image with a coloured rectangle drawn on it.
    fn white_with_rect(w: u32, h: u32, rx: u32, ry: u32, rw: u32, rh: u32) -> DynamicImage {
        let mut img = RgbImage::from_pixel(w, h, Rgb([255, 255, 255]));
        for y in ry..(ry + rh).min(h) {
            for x in rx..(rx + rw).min(w) {
                img.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
        DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn crop_whitespace_removes_margins() {
        // 200x200 image with a 50x50 black rectangle at (75, 75)
        let img = white_with_rect(200, 200, 75, 75, 50, 50);
        let cropped = crop_whitespace(img);
        let (w, h) = (cropped.width(), cropped.height());
        // Content is 50x50 + 8px margin on each side = 66x66 (clamped to image bounds)
        assert!(w <= 66 + 2 && w >= 60, "unexpected width: {w}");
        assert!(h <= 66 + 2 && h >= 60, "unexpected height: {h}");
    }

    #[test]
    fn crop_whitespace_all_white_returns_unchanged() {
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(100, 100, Rgb([255, 255, 255])));
        let cropped = crop_whitespace(img);
        assert_eq!(cropped.width(), 100);
        assert_eq!(cropped.height(), 100);
    }

    #[test]
    fn crop_whitespace_content_at_edges() {
        // Content pixel at (0, 0) — margin extends to clamped bounds
        let img = white_with_rect(100, 100, 0, 0, 1, 1);
        let cropped = crop_whitespace(img);
        // margin: left=0 (clamped), top=0 (clamped), right=0+1+8=9, bottom=0+1+8=9
        assert!(cropped.width() <= 10, "w={}", cropped.width());
        assert!(cropped.height() <= 10, "h={}", cropped.height());
    }

    #[test]
    fn crop_whitespace_full_content() {
        // Entire image is dark — no cropping should occur
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(100, 100, Rgb([0, 0, 0])));
        let cropped = crop_whitespace(img);
        // margin extends to edges, so result should be original size
        assert_eq!(cropped.width(), 100);
        assert_eq!(cropped.height(), 100);
    }

    #[test]
    fn crop_whitespace_empty_image() {
        let img = DynamicImage::ImageRgb8(RgbImage::new(0, 0));
        let cropped = crop_whitespace(img);
        assert_eq!(cropped.width(), 0);
        assert_eq!(cropped.height(), 0);
    }

    #[test]
    fn crop_whitespace_near_threshold() {
        // Pixel at (50,50) with value 239 (just below threshold 240 — counts as content)
        let mut img = RgbImage::from_pixel(100, 100, Rgb([255, 255, 255]));
        img.put_pixel(50, 50, Rgb([239, 239, 239]));
        let cropped = crop_whitespace(DynamicImage::ImageRgb8(img));
        assert!(cropped.width() < 100, "should have cropped");
    }

    #[test]
    fn crop_whitespace_at_threshold_not_content() {
        // All pixels at exactly 240 — should be treated as white
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(100, 100, Rgb([240, 240, 240])));
        let cropped = crop_whitespace(img);
        // All white → unchanged
        assert_eq!(cropped.width(), 100);
        assert_eq!(cropped.height(), 100);
    }

    #[test]
    fn interleaved_iter_works() {
        let got =
            InterleavedAroundWithMax::new(5, 2, NonZeroUsize::new(21).unwrap()).collect::<Vec<_>>();

        assert_eq!(
            got,
            vec![
                5, 6, 4, 7, 3, 8, 2, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20
            ]
        );
    }

    #[test]
    fn active_window_bounds_matches_prerender_window() {
        let (start, end) = active_window_bounds(10, 100, 20).unwrap();
        assert_eq!(start, 0);
        assert_eq!(end.get(), 20);

        let (start, end) = active_window_bounds(40, 100, 20).unwrap();
        assert_eq!(start, 30);
        assert_eq!(end.get(), 50);
    }

    #[test]
    fn prune_pending_pages_drops_out_of_window_entries() {
        let mut pending = HashMap::new();
        for page in 0..40 {
            pending.insert(
                page,
                PageInfo {
                    page_num: page,
                    img_data: raw_image_data(1, 2, 3),
                    result_rects: vec![],
                },
            );
        }

        prune_pending_pages(&mut pending, 20, 100, 12);

        assert!(pending.keys().all(|page| (14..26).contains(page)));
    }

    #[test]
    fn go_to_page_before_num_pages_is_preserved_until_document_is_known() {
        let mut pending = HashMap::new();
        let mut page = 0usize;
        let mut n_pages = 0usize;
        let mut auto_crop = false;

        apply_converter_msg(
            ConverterMsg::GoToPage(148),
            &mut pending,
            &mut page,
            &mut n_pages,
            &mut auto_crop,
            20,
        );
        assert_eq!(page, 148);

        apply_converter_msg(
            ConverterMsg::NumPages(200),
            &mut pending,
            &mut page,
            &mut n_pages,
            &mut auto_crop,
            20,
        );
        assert_eq!(page, 148);
    }

    #[test]
    fn image_from_rgb_data_accepts_tightly_packed_rows() {
        let img = image_from_rgb_data(crate::renderer::ImageData {
            pixels: vec![255, 0, 0, 0, 255, 0],
            width: 2,
            height: 1,
            row_stride: 6,
        })
        .unwrap();

        assert_eq!(img.width(), 2);
        assert_eq!(img.height(), 1);
    }

    #[test]
    fn image_from_rgb_data_repacks_padded_rows() {
        let rgb = image_from_rgb_data(crate::renderer::ImageData {
            pixels: vec![255, 0, 0, 9, 0, 255, 0, 8],
            width: 1,
            height: 2,
            row_stride: 4,
        })
        .unwrap();

        assert_eq!(rgb.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(rgb.get_pixel(0, 1).0, [0, 255, 0]);
    }

    #[test]
    fn apply_search_highlights_updates_only_matching_rect_pixels() {
        let mut img = RgbImage::from_pixel(4, 4, Rgb([20, 30, 200]));
        let rects = vec![HighlightRect {
            ul_x: 0,
            ul_y: 0,
            lr_x: 3,
            lr_y: 3,
        }];

        apply_search_highlights(&mut img, &rects);

        for y in 0..4 {
            for x in 0..4 {
                let blue = img.get_pixel(x, y).0[2];
                let is_highlighted = (1..3).contains(&x) && (1..3).contains(&y);
                if is_highlighted {
                    assert_eq!(blue, 73);
                } else {
                    assert_eq!(blue, 200);
                }
            }
        }
    }

    #[test]
    fn apply_search_highlights_clips_rects_to_image_bounds() {
        let mut img = RgbImage::from_pixel(2, 2, Rgb([20, 30, 120]));
        let rects = vec![HighlightRect {
            ul_x: 0,
            ul_y: 0,
            lr_x: 10,
            lr_y: 10,
        }];

        apply_search_highlights(&mut img, &rects);

        assert_eq!(img.get_pixel(0, 0).0[2], 120);
        assert_eq!(img.get_pixel(1, 0).0[2], 120);
        assert_eq!(img.get_pixel(0, 1).0[2], 120);
        assert_eq!(img.get_pixel(1, 1).0[2], 0);
    }

    #[test]
    fn process_rgb_image_matches_sequential_highlight_then_crop() {
        let mut img = RgbImage::from_pixel(12, 12, Rgb([255, 255, 255]));
        for y in 3..9 {
            for x in 3..9 {
                img.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
        let rects = vec![HighlightRect {
            ul_x: 2,
            ul_y: 2,
            lr_x: 10,
            lr_y: 10,
        }];

        let mut expected = img.clone();
        apply_search_highlights(&mut expected, &rects);
        let expected = crop_whitespace(DynamicImage::ImageRgb8(expected)).to_rgb8();

        let fused = process_rgb_image(img, &rects, true);
        assert_eq!(fused.dimensions(), expected.dimensions());
        assert_eq!(fused.as_raw(), expected.as_raw());
    }

    #[test]
    fn interleaved_single_element() {
        let got =
            InterleavedAroundWithMax::new(0, 0, NonZeroUsize::new(1).unwrap()).collect::<Vec<_>>();
        assert_eq!(got, vec![0]);
    }

    #[test]
    fn interleaved_two_elements_from_start() {
        let got =
            InterleavedAroundWithMax::new(0, 0, NonZeroUsize::new(2).unwrap()).collect::<Vec<_>>();
        assert_eq!(got, vec![0, 1]);
    }

    #[test]
    fn interleaved_around_at_min() {
        let got =
            InterleavedAroundWithMax::new(0, 0, NonZeroUsize::new(5).unwrap()).collect::<Vec<_>>();
        assert_eq!(got, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn interleaved_around_at_max_minus_one() {
        let got =
            InterleavedAroundWithMax::new(4, 0, NonZeroUsize::new(5).unwrap()).collect::<Vec<_>>();
        assert_eq!(got[0], 4);
        assert_eq!(got[1], 3);
        assert_eq!(got[2], 2);
    }

    #[test]
    fn interleaved_covers_all_indices() {
        let n = 10;
        let got: std::collections::HashSet<usize> =
            InterleavedAroundWithMax::new(5, 0, NonZeroUsize::new(n).unwrap())
                .take(n)
                .collect();
        assert_eq!(got.len(), n);
        for i in 0..n {
            assert!(got.contains(&i), "missing index {i}");
        }
    }

    #[test]
    fn interleaved_nonzero_inclusive_min() {
        let got =
            InterleavedAroundWithMax::new(5, 3, NonZeroUsize::new(8).unwrap()).collect::<Vec<_>>();
        assert_eq!(got, vec![5, 6, 4, 7, 3]);
    }

    /// Byte-for-byte parity test on a large image that exercises the parallel
    /// path (512×512 > PAR_PIXEL_CUTOFF).
    #[test]
    fn parallel_highlight_and_crop_parity_with_sequential() {
        let w = 512u32;
        let h = 512u32;
        assert!(w * h >= PAR_PIXEL_CUTOFF, "test must exceed cutoff");

        // White image with a dark rectangle at (100,100)-(400,400).
        let mut img = RgbImage::from_pixel(w, h, Rgb([255, 255, 255]));
        for y in 100..400 {
            for x in 100..400 {
                img.put_pixel(x, y, Rgb([30, 40, 50]));
            }
        }

        let rects = vec![
            HighlightRect {
                ul_x: 120,
                ul_y: 120,
                lr_x: 300,
                lr_y: 300,
            },
            HighlightRect {
                ul_x: 250,
                ul_y: 250,
                lr_x: 380,
                lr_y: 380,
            },
        ];

        // Sequential reference: manually apply highlights then crop.
        let mut seq_img = img.clone();
        {
            let intervals = highlight_intervals_per_row(&rects, w, h);
            let row_stride = w as usize * 3;
            let pixels = seq_img.as_mut();
            for y in 0..h {
                let row_base = y as usize * row_stride;
                let ivs = &intervals[y as usize];
                let mut idx = 0usize;
                for x in 0..w {
                    while idx < ivs.len() && x >= ivs[idx].1 {
                        idx += 1;
                    }
                    let blue_idx = row_base + x as usize * 3 + 2;
                    if idx < ivs.len() && x >= ivs[idx].0 {
                        pixels[blue_idx] = pixels[blue_idx].saturating_sub(u8::MAX / 2);
                    }
                }
            }
        }
        let seq_cropped = crop_whitespace(DynamicImage::ImageRgb8(seq_img)).to_rgb8();

        // Parallel path via process_rgb_image.
        let par_result = process_rgb_image(img, &rects, true);

        assert_eq!(
            par_result.dimensions(),
            seq_cropped.dimensions(),
            "dimension mismatch"
        );
        assert_eq!(
            par_result.as_raw(),
            seq_cropped.as_raw(),
            "pixel data mismatch"
        );
    }
}
