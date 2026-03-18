// Application state — page tracking, scroll/zoom, status messages, and converted image cache.

use crate::{
    error::RenderError,
    image_pipeline::{ConvertedPage, MaybeTransferred},
    perf,
    renderer::{DocumentKind, LinkInfo, TocEntry, fill_default},
    terminal::WindowSize,
};
use kittage::ImageId;

// ---------------------------------------------------------------------------
// Status bar message
// ---------------------------------------------------------------------------

pub enum StatusMsg {
    /// Default hint shown when idle.
    Hint,
    Info(String),
    Error(String),
}

impl StatusMsg {
    pub fn text(&self, page: usize, n_pages: usize, zoom_mode: bool) -> String {
        let page_str = format!("{} / {}  ", page + 1, n_pages.max(1));
        let zoom_str = if zoom_mode { "[ZOOM] " } else { "" };
        let detail = match self {
            Self::Hint => {
                "? help  q quit  ←/→ page  ↑/↓ scroll  i invert  r rotate  z zoom".to_owned()
            }
            Self::Info(s) => s.clone(),
            Self::Error(s) => format!("ERROR: {s}"),
        };
        format!("{page_str}{zoom_str}{detail}")
    }
}

// ---------------------------------------------------------------------------
// Per-page display state
// ---------------------------------------------------------------------------

pub struct RenderedPageInfo {
    /// The converted Kitty image ready for display, if available.
    pub converted: Option<ConvertedPage>,
    /// Number of search results on this page, if known.
    pub num_results: Option<usize>,
}

impl Default for RenderedPageInfo {
    fn default() -> Self {
        Self {
            converted: None,
            num_results: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Scroll outcome
// ---------------------------------------------------------------------------

pub enum ScrollAction {
    /// Scroll position changed within page.
    Scrolled,
    /// At boundary — turn to next page.
    TurnNext,
    /// At boundary — turn to previous page.
    TurnPrev,
    /// No change (already at boundary and no page to turn to).
    Nothing,
}

// ---------------------------------------------------------------------------
// Input mode
// ---------------------------------------------------------------------------

pub enum InputMode {
    /// Normal viewing mode.
    Normal,
    /// Entering a page number (go-to-page).
    GoToPage(String),
    /// Entering a search term.
    Search(String),
    /// Displaying the table of contents.
    Toc { scroll: usize },
    /// Displaying document metadata.
    Metadata,
    /// Displaying links on the current page; user types number to follow.
    Links { links: Vec<LinkInfo>, input: String },
}

pub enum SearchJumpResolution {
    Pending,
    Found(usize),
    NoResult,
}

// ---------------------------------------------------------------------------
// Zoom state
// ---------------------------------------------------------------------------

/// Zoom level: 0 = fit-to-screen, positive = zoomed in.
const ZOOM_STEP: f32 = 1.2;
const MAX_ZOOM_LEVEL: i16 = 20;

// ---------------------------------------------------------------------------
// Main application state
// ---------------------------------------------------------------------------

pub struct App {
    pub page: usize,
    pub n_pages: usize,
    pub msg: StatusMsg,
    pub rendered: Vec<RenderedPageInfo>,
    /// Set to true when the current page's image changed and needs a screen update.
    pub needs_redraw: bool,

    // --- Scroll state ---
    /// Vertical scroll offset in pixels from the top of the page image.
    pub scroll_offset: u32,

    // --- Zoom state ---
    pub zoom_mode: bool,
    /// Zoom level: 0 = fit, positive = zoom in, negative = zoom out.
    pub zoom_level: i16,
    /// Horizontal scroll offset in pixels (like scroll_offset but horizontal).
    pub pan_x: u32,

    // --- TOC and input mode ---
    pub toc: Vec<TocEntry>,
    pub input_mode: InputMode,

    // --- Search state ---
    pub search_term: Option<String>,
    pub pending_search_jump_from: Option<usize>,

    // --- Document transformations ---
    pub auto_crop: bool,
    pub tinted: bool,
    pub inverted: bool,
    /// Rotation in degrees (0, 90, 180, 270) — tracked for state persistence.
    pub rotation: u16,

    // --- Metadata ---
    pub metadata: Vec<(String, String)>,
    pub document_kind: DocumentKind,
    pub epub_font_size: Option<f32>,
    pub pending_image_deletes: Vec<ImageId>,
}

impl App {
    pub fn new() -> Self {
        Self {
            page: 0,
            n_pages: 0,
            msg: StatusMsg::Hint,
            rendered: vec![],
            needs_redraw: false,
            scroll_offset: 0,
            zoom_mode: false,
            zoom_level: 0,
            pan_x: 0,
            toc: vec![],
            input_mode: InputMode::Normal,
            search_term: None,
            pending_search_jump_from: None,
            auto_crop: false,
            tinted: false,
            inverted: false,
            rotation: 0,
            metadata: vec![],
            document_kind: DocumentKind::Fixed,
            epub_font_size: None,
            pending_image_deletes: vec![],
        }
    }

    /// Current zoom factor (1.0 = no zoom).
    pub fn zoom_factor(&self) -> f32 {
        ZOOM_STEP.powi(self.zoom_level as i32)
    }

    /// Invalidate all cached converted pages (e.g. after area/zoom change).
    pub fn invalidate_all_pages(&mut self) {
        let mut deleted = 0usize;
        for info in &mut self.rendered {
            deleted +=
                take_converted_for_delete(&mut info.converted, &mut self.pending_image_deletes)
                    as usize;
        }
        if deleted > 0 || log::log_enabled!(log::Level::Debug) {
            perf::log_cache_state("app_cache_drop", 0, 0, deleted);
        }
        self.needs_redraw = true;
    }

    /// Called when the renderer reports the total page count.
    pub fn set_n_pages(&mut self, n: usize) {
        fill_default(&mut self.rendered, n);
        self.n_pages = n;
        self.page = self.page.min(n.saturating_sub(1));
        self.prune_converted_pages();
    }

    pub fn set_document_kind(&mut self, kind: DocumentKind) {
        self.document_kind = kind;
        if matches!(kind, DocumentKind::Fixed) {
            self.epub_font_size = None;
        }
        if matches!(kind, DocumentKind::Reflowable) && self.zoom_mode {
            self.zoom_mode = false;
            self.zoom_level = 0;
            self.pan_x = 0;
            self.scroll_offset = 0;
        }
        self.needs_redraw = true;
    }

    pub fn supports_zoom(&self) -> bool {
        matches!(self.document_kind, DocumentKind::Fixed)
    }

    pub fn set_epub_font_size(&mut self, size: f32) {
        self.epub_font_size = Some(size);
    }

    /// Called when the renderer reports search result counts for a page.
    pub fn got_search_results(&mut self, page_num: usize, num_results: usize) {
        if let Some(info) = self.rendered.get_mut(page_num) {
            info.num_results = Some(num_results);
        }
    }

    /// Called when the image pipeline produces a display-ready image.
    pub fn page_converted(&mut self, converted: ConvertedPage) {
        let page_num = converted.page_num;
        if let Some(info) = self.rendered.get_mut(page_num) {
            info.num_results = Some(converted.num_results);
            info.converted = Some(converted);
            self.prune_converted_pages();
            if page_num == self.page {
                self.needs_redraw = true;
            }
        }
    }

    /// Mark a page's image as gone (e.g. Kitty evicted it under memory pressure).
    pub fn page_display_failed(&mut self, page_num: usize) {
        if let Some(info) = self.rendered.get_mut(page_num) {
            take_converted_for_delete(&mut info.converted, &mut self.pending_image_deletes);
        }
    }

    /// Show a render error in the status bar.
    pub fn show_render_error(&mut self, err: RenderError) {
        self.msg = StatusMsg::Error(err.to_string());
    }

    pub fn show_info(&mut self, s: String) {
        self.msg = StatusMsg::Info(s);
    }

    // --- Page navigation ---

    /// Navigate to a page, returning `true` if the page actually changed.
    pub fn go_to_page(&mut self, new_page: usize) -> bool {
        let clamped = new_page.min(self.n_pages.saturating_sub(1));
        if clamped != self.page {
            self.page = clamped;
            self.scroll_offset = 0;
            self.pan_x = 0;
            self.prune_converted_pages();
            self.needs_redraw = true;
            true
        } else {
            false
        }
    }

    /// Go to next page, scroll offset reset to 0 (top).
    pub fn next_page(&mut self) -> bool {
        self.go_to_page(self.page.saturating_add(1))
    }

    /// Go to previous page, scroll offset set to max (bottom) so up-scroll
    /// at boundary feels natural.
    pub fn prev_page_at_bottom(&mut self, ws: &WindowSize) -> bool {
        let new_page = self.page.saturating_sub(1);
        if self.go_to_page(new_page) {
            // Set scroll to bottom of the previous page
            self.scroll_offset = self.max_scroll(ws);
            true
        } else {
            false
        }
    }

    pub fn prev_page(&mut self) -> bool {
        self.go_to_page(self.page.saturating_sub(1))
    }

    // --- Scrolling ---

    /// Maximum scroll offset for the current page (0 if the page fits on screen).
    pub fn max_scroll(&self, ws: &WindowSize) -> u32 {
        let page_height_px = self.current_page_pixel_h();
        let visible_px = ws.page_area_height_px() as u32;
        page_height_px.saturating_sub(visible_px)
    }

    /// Pixel height of the current page's rendered image.
    fn current_page_pixel_h(&self) -> u32 {
        self.rendered
            .get(self.page)
            .and_then(|r| r.converted.as_ref())
            .map(|c| c.pixel_h)
            .unwrap_or(0)
    }

    /// Pixel width of the current page's rendered image.
    fn current_page_pixel_w(&self) -> u32 {
        self.rendered
            .get(self.page)
            .and_then(|r| r.converted.as_ref())
            .map(|c| c.pixel_w)
            .unwrap_or(0)
    }

    /// Scroll down by `step_px` pixels. Returns what happened.
    pub fn scroll_down(&mut self, ws: &WindowSize, step_px: u32) -> ScrollAction {
        let max = self.max_scroll(ws);
        if max == 0 {
            // Page fits on screen — treat as page turn
            if self.next_page() {
                return ScrollAction::TurnNext;
            }
            return ScrollAction::Nothing;
        }
        if self.scroll_offset >= max {
            ScrollAction::TurnNext
        } else {
            self.scroll_offset = (self.scroll_offset + step_px).min(max);
            self.needs_redraw = true;
            ScrollAction::Scrolled
        }
    }

    /// Scroll up by `step_px` pixels. Returns what happened.
    pub fn scroll_up(&mut self, ws: &WindowSize, step_px: u32) -> ScrollAction {
        let max = self.max_scroll(ws);
        if max == 0 {
            // Page fits on screen — treat as page turn
            if self.prev_page() {
                return ScrollAction::TurnPrev;
            }
            return ScrollAction::Nothing;
        }
        if self.scroll_offset == 0 {
            ScrollAction::TurnPrev
        } else {
            self.scroll_offset = self.scroll_offset.saturating_sub(step_px);
            self.needs_redraw = true;
            ScrollAction::Scrolled
        }
    }

    /// A viewport-sized vertical step with a one-line overlap.
    pub fn viewport_page_scroll_step(ws: &WindowSize) -> u32 {
        (ws.page_area_height_px() as u32)
            .saturating_sub(u32::from(ws.cell_height_px))
            .max(1)
    }

    /// Move down by one viewport in zoom mode, keeping one line of overlap.
    pub fn page_down_in_zoom(&mut self, ws: &WindowSize) -> ScrollAction {
        self.scroll_down(ws, Self::viewport_page_scroll_step(ws))
    }

    /// Move up by one viewport in zoom mode, keeping one line of overlap.
    pub fn page_up_in_zoom(&mut self, ws: &WindowSize) -> ScrollAction {
        self.scroll_up(ws, Self::viewport_page_scroll_step(ws))
    }

    // --- Zoom ---

    pub fn toggle_zoom_mode(&mut self) {
        self.zoom_mode = !self.zoom_mode;
        if !self.zoom_mode {
            // Reset zoom when leaving zoom mode
            self.zoom_level = 0;
            self.pan_x = 0;
            self.scroll_offset = 0;
        }
        self.needs_redraw = true;
    }

    pub fn zoom_in(&mut self) -> bool {
        if self.zoom_level < MAX_ZOOM_LEVEL {
            self.zoom_level += 1;
            self.needs_redraw = true;
            true
        } else {
            false
        }
    }

    pub fn zoom_out(&mut self) -> bool {
        if self.zoom_level > 0 {
            self.zoom_level -= 1;
            self.needs_redraw = true;
            true
        } else {
            false
        }
    }

    /// Maximum horizontal pan offset (0 if image fits horizontally).
    pub fn max_pan_x(&self, ws: &WindowSize) -> u32 {
        let img_w = self.current_page_pixel_w();
        let visible_w = ws.width_px as u32;
        img_w.saturating_sub(visible_w)
    }

    /// Pan right by `step_px` pixels.
    pub fn pan_right(&mut self, ws: &WindowSize, step_px: u32) {
        let max = self.max_pan_x(ws);
        if self.pan_x < max {
            self.pan_x = (self.pan_x + step_px).min(max);
            self.needs_redraw = true;
        }
    }

    /// Pan left by `step_px` pixels.
    pub fn pan_left(&mut self, _ws: &WindowSize, step_px: u32) {
        if self.pan_x > 0 {
            self.pan_x = self.pan_x.saturating_sub(step_px);
            self.needs_redraw = true;
        }
    }

    // --- Search ---

    /// Find next page with search results (forward from current page).
    pub fn next_search_result(&mut self) -> bool {
        if self.search_term.is_none() {
            return false;
        }
        for i in 1..self.n_pages {
            let page = (self.page + i) % self.n_pages;
            if let Some(info) = self.rendered.get(page) {
                if info.num_results.is_some_and(|n| n > 0) {
                    return self.go_to_page(page);
                }
            }
        }
        false
    }

    /// Find previous page with search results (backward from current page).
    pub fn prev_search_result(&mut self) -> bool {
        if self.search_term.is_none() {
            return false;
        }
        for i in 1..self.n_pages {
            let page = (self.page + self.n_pages - i) % self.n_pages;
            if let Some(info) = self.rendered.get(page) {
                if info.num_results.is_some_and(|n| n > 0) {
                    return self.go_to_page(page);
                }
            }
        }
        false
    }

    pub fn clear_search_results(&mut self) {
        for info in &mut self.rendered {
            info.num_results = None;
        }
    }

    pub fn resolve_next_search_page(&self, from_page: usize) -> SearchJumpResolution {
        if self.n_pages == 0 {
            return SearchJumpResolution::NoResult;
        }

        let mut saw_unknown = false;
        for i in 1..=self.n_pages {
            let page = (from_page + i) % self.n_pages;
            match self.rendered.get(page).and_then(|info| info.num_results) {
                Some(n) if n > 0 => {
                    if saw_unknown {
                        return SearchJumpResolution::Pending;
                    }
                    return SearchJumpResolution::Found(page);
                }
                Some(_) => {}
                None => saw_unknown = true,
            }
        }

        if saw_unknown {
            SearchJumpResolution::Pending
        } else {
            SearchJumpResolution::NoResult
        }
    }

    fn prune_converted_pages(&mut self) {
        if self.rendered.is_empty() {
            return;
        }

        let mut distances: Vec<(usize, u64)> = Vec::new();
        for (idx, info) in self.rendered.iter().enumerate() {
            if let Some(converted) = info.converted.as_ref() {
                let bytes = u64::from(converted.pixel_w) * u64::from(converted.pixel_h) * 3;
                distances.push((idx, bytes));
            }
        }

        distances.sort_by_key(|&(idx, _)| idx.abs_diff(self.page));

        let max_bytes = 250 * 1024 * 1024; // 250 MB
        let mut keep = std::collections::HashSet::new();
        let mut resident_bytes = 0u64;
        let mut resident_pages = 0usize;

        for (idx, bytes) in distances {
            if resident_bytes + bytes <= max_bytes || resident_pages == 0 {
                keep.insert(idx);
                resident_bytes += bytes;
                resident_pages += 1;
            }
        }

        let mut evicted = 0usize;
        for (idx, info) in self.rendered.iter_mut().enumerate() {
            if !keep.contains(&idx) {
                if take_converted_for_delete(&mut info.converted, &mut self.pending_image_deletes) {
                    evicted += 1;
                }
            }
        }

        if evicted > 0 || log::log_enabled!(log::Level::Debug) {
            perf::log_cache_state("app_cache", resident_pages, resident_bytes, evicted);
        }
    }
}

fn take_converted_for_delete(
    slot: &mut Option<ConvertedPage>,
    pending_image_deletes: &mut Vec<ImageId>,
) -> bool {
    let Some(converted) = slot.take() else {
        return false;
    };

    if let MaybeTransferred::Transferred(image_id) = converted.img {
        pending_image_deletes.push(image_id);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_pipeline::MaybeTransferred;
    use std::num::NonZeroU32;

    fn test_ws() -> WindowSize {
        WindowSize {
            rows: 25,
            cols: 80,
            width_px: 800,
            cell_width_px: 10,
            cell_height_px: 20,
        }
    }

    fn make_converted(page_num: usize, pixel_w: u32, pixel_h: u32) -> ConvertedPage {
        ConvertedPage {
            page_num,
            img: MaybeTransferred::Transferred(NonZeroU32::new(1).unwrap()),
            pixel_w,
            pixel_h,
            generation: 1,
            num_results: 0,
        }
    }

    #[test]
    fn resolve_next_search_page_finds_next_known_hit() {
        let mut app = App::new();
        app.set_n_pages(5);
        app.rendered[0].num_results = Some(0);
        app.rendered[1].num_results = Some(0);
        app.rendered[2].num_results = Some(2);
        app.rendered[3].num_results = Some(0);
        app.rendered[4].num_results = Some(0);

        match app.resolve_next_search_page(0) {
            SearchJumpResolution::Found(page) => assert_eq!(page, 2),
            _ => panic!("expected Found(2)"),
        }
    }

    #[test]
    fn resolve_next_search_page_waits_when_earlier_pages_unknown() {
        let mut app = App::new();
        app.set_n_pages(4);
        app.rendered[1].num_results = None;
        app.rendered[2].num_results = Some(1);
        app.rendered[3].num_results = Some(0);
        app.rendered[0].num_results = Some(0);

        assert!(matches!(
            app.resolve_next_search_page(0),
            SearchJumpResolution::Pending
        ));
    }

    #[test]
    fn resolve_next_search_page_reports_no_result_when_complete() {
        let mut app = App::new();
        app.set_n_pages(3);
        app.rendered[0].num_results = Some(0);
        app.rendered[1].num_results = Some(0);
        app.rendered[2].num_results = Some(0);

        assert!(matches!(
            app.resolve_next_search_page(1),
            SearchJumpResolution::NoResult
        ));
    }

    /// Set up an app with n pages, current page at `page`, each page rendered
    /// at the given pixel dimensions.
    fn app_with_pages(n: usize, page: usize, pixel_w: u32, pixel_h: u32) -> App {
        let mut app = App::new();
        app.set_n_pages(n);
        app.page = page;
        for i in 0..n {
            app.rendered[i].converted = Some(make_converted(i, pixel_w, pixel_h));
        }
        app
    }

    // --- Zoom factor ---

    #[test]
    fn zoom_factor_at_zero() {
        let app = App::new();
        assert!((app.zoom_factor() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn zoom_factor_increases() {
        let mut app = App::new();
        app.zoom_level = 1;
        assert!((app.zoom_factor() - 1.2).abs() < 0.001);
        app.zoom_level = 2;
        assert!((app.zoom_factor() - 1.44).abs() < 0.01);
    }

    #[test]
    fn zoom_in_increments_level() {
        let mut app = App::new();
        assert!(app.zoom_in());
        assert_eq!(app.zoom_level, 1);
        assert!(app.needs_redraw);
    }

    #[test]
    fn zoom_in_capped_at_max() {
        let mut app = App::new();
        app.zoom_level = 20;
        assert!(!app.zoom_in());
        assert_eq!(app.zoom_level, 20);
    }

    #[test]
    fn zoom_out_decrements_level() {
        let mut app = App::new();
        app.zoom_level = 3;
        assert!(app.zoom_out());
        assert_eq!(app.zoom_level, 2);
    }

    #[test]
    fn zoom_out_stops_at_zero() {
        let mut app = App::new();
        assert!(!app.zoom_out());
        assert_eq!(app.zoom_level, 0);
    }

    #[test]
    fn toggle_zoom_mode_resets_state() {
        let mut app = App::new();
        app.zoom_mode = true;
        app.zoom_level = 5;
        app.pan_x = 100;
        app.scroll_offset = 50;
        app.toggle_zoom_mode();
        assert!(!app.zoom_mode);
        assert_eq!(app.zoom_level, 0);
        assert_eq!(app.pan_x, 0);
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn set_document_kind_reflowable_disables_zoom() {
        let mut app = App::new();
        app.zoom_mode = true;
        app.zoom_level = 2;
        app.pan_x = 50;
        app.scroll_offset = 25;

        app.set_document_kind(DocumentKind::Reflowable);

        assert_eq!(app.document_kind, DocumentKind::Reflowable);
        assert!(!app.zoom_mode);
        assert_eq!(app.zoom_level, 0);
        assert_eq!(app.pan_x, 0);
        assert_eq!(app.scroll_offset, 0);
    }

    // --- Page navigation ---

    #[test]
    fn set_n_pages_initialises_rendered() {
        let mut app = App::new();
        app.set_n_pages(10);
        assert_eq!(app.n_pages, 10);
        assert_eq!(app.rendered.len(), 10);
    }

    #[test]
    fn set_n_pages_clamps_current_page() {
        let mut app = App::new();
        app.set_n_pages(10);
        app.page = 15;
        app.set_n_pages(5);
        assert_eq!(app.page, 4);
    }

    #[test]
    fn go_to_page_clamps_to_max() {
        let mut app = App::new();
        app.set_n_pages(5);
        assert!(app.go_to_page(100));
        assert_eq!(app.page, 4);
    }

    #[test]
    fn go_to_page_resets_scroll_and_pan() {
        let mut app = App::new();
        app.set_n_pages(5);
        app.scroll_offset = 100;
        app.pan_x = 50;
        app.go_to_page(3);
        assert_eq!(app.scroll_offset, 0);
        assert_eq!(app.pan_x, 0);
    }

    #[test]
    fn go_to_same_page_returns_false() {
        let mut app = App::new();
        app.set_n_pages(5);
        app.page = 2;
        assert!(!app.go_to_page(2));
    }

    #[test]
    fn next_page_at_last_returns_false() {
        let mut app = App::new();
        app.set_n_pages(3);
        app.page = 2;
        assert!(!app.next_page());
        assert_eq!(app.page, 2);
    }

    #[test]
    fn prev_page_at_first_returns_false() {
        let mut app = App::new();
        app.set_n_pages(3);
        app.page = 0;
        assert!(!app.prev_page());
        assert_eq!(app.page, 0);
    }

    #[test]
    fn next_prev_page_basic() {
        let mut app = App::new();
        app.set_n_pages(5);
        app.page = 2;
        assert!(app.next_page());
        assert_eq!(app.page, 3);
        assert!(app.prev_page());
        assert_eq!(app.page, 2);
    }

    // --- Scrolling ---

    #[test]
    fn max_scroll_zero_when_page_fits() {
        let ws = test_ws();
        // page_area_height_px = (25-1)*20 = 480
        let app = app_with_pages(1, 0, 800, 400);
        assert_eq!(app.max_scroll(&ws), 0);
    }

    #[test]
    fn max_scroll_positive_when_page_taller() {
        let ws = test_ws();
        // page_area_height_px = 480, image = 1000
        let app = app_with_pages(1, 0, 800, 1000);
        assert_eq!(app.max_scroll(&ws), 520);
    }

    #[test]
    fn scroll_down_turns_page_when_fits_on_screen() {
        let ws = test_ws();
        let mut app = app_with_pages(3, 0, 800, 400);
        let action = app.scroll_down(&ws, 60);
        assert!(matches!(action, ScrollAction::TurnNext));
        assert_eq!(app.page, 1);
    }

    #[test]
    fn scroll_down_scrolls_within_tall_page() {
        let ws = test_ws();
        let mut app = app_with_pages(3, 0, 800, 1000);
        let action = app.scroll_down(&ws, 60);
        assert!(matches!(action, ScrollAction::Scrolled));
        assert_eq!(app.scroll_offset, 60);
    }

    #[test]
    fn scroll_down_at_bottom_signals_turn() {
        let ws = test_ws();
        let mut app = app_with_pages(3, 0, 800, 1000);
        app.scroll_offset = 520; // at max
        let action = app.scroll_down(&ws, 60);
        assert!(matches!(action, ScrollAction::TurnNext));
    }

    #[test]
    fn scroll_up_at_top_signals_turn() {
        let ws = test_ws();
        let mut app = app_with_pages(3, 1, 800, 1000);
        app.scroll_offset = 0;
        let action = app.scroll_up(&ws, 60);
        assert!(matches!(action, ScrollAction::TurnPrev));
    }

    #[test]
    fn scroll_up_scrolls_within_page() {
        let ws = test_ws();
        let mut app = app_with_pages(3, 0, 800, 1000);
        app.scroll_offset = 200;
        let action = app.scroll_up(&ws, 60);
        assert!(matches!(action, ScrollAction::Scrolled));
        assert_eq!(app.scroll_offset, 140);
    }

    #[test]
    fn scroll_offset_clamped_to_max() {
        let ws = test_ws();
        let mut app = app_with_pages(1, 0, 800, 1000);
        app.scroll_offset = 500;
        let action = app.scroll_down(&ws, 100);
        assert!(matches!(action, ScrollAction::Scrolled));
        assert_eq!(app.scroll_offset, 520); // clamped to max
    }

    #[test]
    fn viewport_page_scroll_step_keeps_one_line_overlap() {
        let ws = test_ws();
        assert_eq!(App::viewport_page_scroll_step(&ws), 460);
    }

    #[test]
    fn page_down_in_zoom_scrolls_by_viewport_minus_one_line() {
        let ws = test_ws();
        let mut app = app_with_pages(3, 0, 800, 1400);
        let action = app.page_down_in_zoom(&ws);
        assert!(matches!(action, ScrollAction::Scrolled));
        assert_eq!(app.scroll_offset, 460);
    }

    #[test]
    fn page_down_in_zoom_at_bottom_turns_next() {
        let ws = test_ws();
        let mut app = app_with_pages(3, 0, 800, 1000);
        app.scroll_offset = app.max_scroll(&ws);
        let action = app.page_down_in_zoom(&ws);
        assert!(matches!(action, ScrollAction::TurnNext));
    }

    #[test]
    fn page_up_in_zoom_scrolls_by_viewport_minus_one_line() {
        let ws = test_ws();
        let mut app = app_with_pages(3, 1, 800, 1400);
        app.scroll_offset = 900;
        let action = app.page_up_in_zoom(&ws);
        assert!(matches!(action, ScrollAction::Scrolled));
        assert_eq!(app.scroll_offset, 440);
    }

    #[test]
    fn page_up_in_zoom_at_top_turns_prev() {
        let ws = test_ws();
        let mut app = app_with_pages(3, 1, 800, 1000);
        let action = app.page_up_in_zoom(&ws);
        assert!(matches!(action, ScrollAction::TurnPrev));
    }

    #[test]
    fn prev_page_at_bottom_sets_max_scroll() {
        let ws = test_ws();
        let mut app = app_with_pages(3, 1, 800, 1000);
        assert!(app.prev_page_at_bottom(&ws));
        assert_eq!(app.page, 0);
        assert_eq!(app.scroll_offset, 520);
    }

    // --- Panning ---

    #[test]
    fn pan_right_within_bounds() {
        let ws = test_ws();
        // width_px = 800, image = 1200
        let mut app = app_with_pages(1, 0, 1200, 480);
        app.pan_right(&ws, 100);
        assert_eq!(app.pan_x, 100);
    }

    #[test]
    fn pan_right_clamped_to_max() {
        let ws = test_ws();
        let mut app = app_with_pages(1, 0, 1200, 480);
        app.pan_right(&ws, 500);
        assert_eq!(app.pan_x, 400); // max_pan_x = 1200 - 800
    }

    #[test]
    fn pan_left_from_zero_stays() {
        let ws = test_ws();
        let mut app = app_with_pages(1, 0, 1200, 480);
        app.pan_left(&ws, 100);
        assert_eq!(app.pan_x, 0);
    }

    #[test]
    fn pan_left_reduces_offset() {
        let ws = test_ws();
        let mut app = app_with_pages(1, 0, 1200, 480);
        app.pan_x = 200;
        app.pan_left(&ws, 50);
        assert_eq!(app.pan_x, 150);
    }

    #[test]
    fn max_pan_x_zero_when_fits() {
        let ws = test_ws();
        let app = app_with_pages(1, 0, 600, 480);
        assert_eq!(app.max_pan_x(&ws), 0);
    }

    // --- Invalidation ---

    #[test]
    fn invalidate_clears_all_converted() {
        let mut app = app_with_pages(3, 0, 800, 480);
        assert!(app.rendered[0].converted.is_some());
        app.invalidate_all_pages();
        assert!(app.rendered.iter().all(|r| r.converted.is_none()));
        assert_eq!(app.pending_image_deletes.len(), 3);
        assert!(app.needs_redraw);
    }

    // --- Search navigation ---

    #[test]
    fn next_search_result_without_term_returns_false() {
        let mut app = App::new();
        app.set_n_pages(5);
        assert!(!app.next_search_result());
    }

    #[test]
    fn next_search_result_wraps_around() {
        let mut app = App::new();
        app.set_n_pages(5);
        app.page = 3;
        app.search_term = Some("test".to_owned());
        // Mark page 1 as having results
        app.rendered[1].num_results = Some(2);
        assert!(app.next_search_result());
        assert_eq!(app.page, 1);
    }

    #[test]
    fn prev_search_result_wraps_backward() {
        let mut app = App::new();
        app.set_n_pages(5);
        app.page = 1;
        app.search_term = Some("test".to_owned());
        // Mark page 4 as having results
        app.rendered[4].num_results = Some(3);
        assert!(app.prev_search_result());
        assert_eq!(app.page, 4);
    }

    #[test]
    fn search_no_results_returns_false() {
        let mut app = App::new();
        app.set_n_pages(5);
        app.page = 0;
        app.search_term = Some("test".to_owned());
        // All pages have 0 results
        for r in &mut app.rendered {
            r.num_results = Some(0);
        }
        assert!(!app.next_search_result());
    }

    // --- page_converted ---

    #[test]
    fn page_converted_triggers_redraw_for_current() {
        let mut app = App::new();
        app.set_n_pages(3);
        app.page = 1;
        app.needs_redraw = false;
        let converted = make_converted(1, 800, 480);
        app.page_converted(converted);
        assert!(app.needs_redraw);
        assert!(app.rendered[1].converted.is_some());
    }

    #[test]
    fn page_converted_no_redraw_for_other_page() {
        let mut app = App::new();
        app.set_n_pages(3);
        app.page = 0;
        app.needs_redraw = false;
        let converted = make_converted(2, 800, 480);
        app.page_converted(converted);
        assert!(!app.needs_redraw);
        assert!(app.rendered[2].converted.is_some());
    }

    #[test]
    fn go_to_page_prunes_far_converted_pages() {
        // Create 256 pages to exceed the 250MB budget (256 * 1.1MB > 250MB)
        let mut app = app_with_pages(256, 0, 800, 480);
        assert_eq!(
            app.rendered
                .iter()
                .filter(|page| page.converted.is_some())
                .count(),
            256
        );

        app.go_to_page(128);

        assert!(app.rendered[128].converted.is_some());
        assert!(app.rendered[0].converted.is_none());
        assert!(!app.pending_image_deletes.is_empty());
        assert!(
            app.rendered
                .iter()
                .filter(|page| page.converted.is_some())
                .count()
                < 250
        );
    }

    #[test]
    fn page_converted_outside_residency_is_not_retained() {
        // Create an app that already exceeds the budget to force pruning of distant pages
        let mut app = app_with_pages(256, 0, 800, 480);
        app.page_converted(make_converted(255, 800, 480));

        assert!(app.rendered[255].converted.is_none());
    }

    // --- StatusMsg ---

    #[test]
    fn status_msg_hint_format() {
        let msg = StatusMsg::Hint;
        let text = msg.text(0, 10, false);
        assert!(text.contains("1 / 10"));
        assert!(text.contains("? help"));
    }

    #[test]
    fn status_msg_zoom_indicator() {
        let msg = StatusMsg::Hint;
        let text = msg.text(0, 10, true);
        assert!(text.contains("[ZOOM]"));
    }

    #[test]
    fn status_msg_error_format() {
        let msg = StatusMsg::Error("bad thing".to_owned());
        let text = msg.text(0, 1, false);
        assert!(text.contains("ERROR: bad thing"));
    }
}
