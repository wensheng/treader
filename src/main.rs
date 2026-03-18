// treader — terminal PDF viewer using the Kitty graphics protocol
// Architecture:
//   Main thread: async event loop (tokio) — input, display, orchestration
//   Render thread: blocking std::thread — mupdf PDF→pixmap (mupdf is !Send)
//   Image prep: async tokio task — pixmap→Kitty image (CPU-bound image processing)

mod app;
mod error;
mod image_pipeline;
mod kitty;
mod perf;
mod renderer;
mod state;
mod terminal;

use std::{
    io::stdout,
    num::NonZeroU32,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
    execute,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use flume::Sender;
use futures_util::{FutureExt as _, stream::StreamExt as _};
use kittage::{
    action::Action,
    delete::{ClearOrDelete, DeleteConfig, WhichToDelete},
};

use app::{App, InputMode, ScrollAction, SearchJumpResolution, StatusMsg};
use error::RenderError;
use image_pipeline::{ConverterMsg, run_conversion_loop};
use kitty::{
    KittyDisplay, KittyReadyToDisplay, Pos, clear_kitty_image, delete_kitty_images,
    display_kitty_images, do_shms_work, run_action, supports_kitty_graphics,
};
use renderer::{DocumentKind, MUPDF_BLACK, MUPDF_WHITE, RenderInfo, RenderNotif};
use terminal::{
    TerminalGuard, WindowSize, clear_page_area, draw_links, draw_loading, draw_metadata,
    draw_status_bar, draw_toc, get_window_size, reset_terminal,
};

// ---------------------------------------------------------------------------
// Allocator
// ---------------------------------------------------------------------------

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn render_width_px(app: &App, ws: &WindowSize) -> f32 {
    let zf = if app.supports_zoom() {
        app.zoom_factor()
    } else {
        1.0
    };
    ws.page_area_width_px() * zf
}

fn render_height_px(app: &App, ws: &WindowSize) -> f32 {
    let zf = if app.supports_zoom() {
        app.zoom_factor()
    } else {
        1.0
    };
    ws.page_area_height_px() * zf
}

const LOADING_INDICATOR_DELAY: Duration = Duration::from_millis(90);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HorizontalAction {
    NextPage,
    PrevPage,
    PanRight,
    PanLeft,
}

fn horizontal_action(key: KeyCode, zoom_mode: bool) -> Option<HorizontalAction> {
    match key {
        KeyCode::Right => Some(HorizontalAction::NextPage),
        KeyCode::Left => Some(HorizontalAction::PrevPage),
        KeyCode::Char('l') => Some(if zoom_mode {
            HorizontalAction::PanRight
        } else {
            HorizontalAction::NextPage
        }),
        KeyCode::Char('h') => Some(if zoom_mode {
            HorizontalAction::PanLeft
        } else {
            HorizontalAction::PrevPage
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_time()
        .enable_io()
        .build()?;

    rt.block_on(async move { inner_main().await })
}

async fn inner_main() -> anyhow::Result<()> {
    let startup_started = Instant::now();

    // --- CLI args (minimal; expand with clap later) -----------------------
    let args: Vec<String> = std::env::args().collect();

    if let Some(path) = probe_mode_path(&args) {
        renderer::probe_document(&path)?;
        return Ok(());
    }

    // Optional logging (set RUST_LOG=debug)
    let _logger = if std::env::var("RUST_LOG").is_ok() {
        Some(
            flexi_logger::Logger::try_with_env()?
                .log_to_file(flexi_logger::FileSpec::try_from("./treader.log")?)
                .start()?,
        )
    } else {
        None
    };

    // Parse --invert / file positional
    let mut invert_start = false;
    let mut file_path: Option<PathBuf> = None;
    let mut black = MUPDF_BLACK;
    let mut white = MUPDF_WHITE;

    let mut iter = args.into_iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-i" | "--invert" => invert_start = true,
            "-h" | "--help" => {
                print_usage();
                return Ok(());
            }
            "--version" => {
                println!("treader {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-b" | "--black-color" => {
                if let Some(color) = iter.next() {
                    black = parse_color(&color)?;
                }
            }
            "-w" | "--white-color" => {
                if let Some(color) = iter.next() {
                    white = parse_color(&color)?;
                }
            }
            s if !s.starts_with('-') => {
                file_path = Some(PathBuf::from(s));
            }
            other => {
                anyhow::bail!("unknown argument: {other}\nUsage: treader [options] <file>");
            }
        }
    }

    let path = file_path
        .ok_or_else(|| anyhow::anyhow!("Usage: treader [options] <file>"))?
        .canonicalize()?;

    preflight_document(&path)?;

    // --- Terminal setup ---------------------------------------------------
    let ws = get_window_size()?;

    let _guard = TerminalGuard::enter()?;

    // Install a panic hook that restores the terminal before printing the panic.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        reset_terminal();
        original_hook(info);
    }));

    let mut ev_stream = EventStream::new();

    // --- Kitty capability detection ---------------------------------------
    if !supports_kitty_graphics(&mut ev_stream).await {
        anyhow::bail!(
            "Kitty graphics protocol is not supported by this terminal. Use Kitty, Ghostty, or another compatible terminal."
        );
    }

    // Detect whether shared memory transfer works (faster on Linux/macOS with Kitty/Ghostty).
    let shms_work = do_shms_work(&mut ev_stream).await;
    log::info!("shm support: {shms_work}");

    // Clear any leftover Kitty images from a previous session.
    run_action(
        Action::Delete(DeleteConfig {
            effect: ClearOrDelete::Delete,
            which: WhichToDelete::IdRange(NonZeroU32::new(1).unwrap()..=NonZeroU32::MAX),
        }),
        &mut ev_stream,
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to clear previous Kitty images: {e}"))?;

    // --- Channel setup ----------------------------------------------------
    // renderer thread → main
    let (render_tx, render_rx) = flume::unbounded::<Result<RenderInfo, RenderError>>();
    // main → renderer thread
    let (to_renderer, renderer_cmd_rx) = flume::unbounded::<RenderNotif>();
    // main → image pipeline
    let (to_converter, converter_rx) = flume::unbounded::<ConverterMsg>();
    // image pipeline → main
    let (converter_tx, from_converter) = flume::unbounded();

    // --- Application state ------------------------------------------------
    let mut app = App::new();
    let mut current_ws = ws;

    // --- Renderer thread --------------------------------------------------
    let file_for_thread = path.clone();
    let cell_h = ws.cell_height_px;
    let cell_w = ws.cell_width_px;
    std::thread::spawn(move || {
        if let Err(e) = renderer::start_rendering(
            &file_for_thread,
            render_tx,
            renderer_cmd_rx,
            cell_h,
            cell_w,
            black,
            white,
        ) {
            log::error!("render thread send error: {e}");
        }
    });

    // Send initial area so the renderer knows what size to render at.
    to_renderer.send(RenderNotif::Area {
        width_px: render_width_px(&app, &ws),
        height_px: render_height_px(&app, &ws),
    })?;

    // If invert was requested from CLI, send the toggle now.
    if invert_start {
        to_renderer.send(RenderNotif::Invert)?;
    }

    // --- Converter task ---------------------------------------------------
    tokio::spawn(run_conversion_loop(
        converter_tx,
        converter_rx,
        20, // prerender window
        shms_work,
    ));

    // Load persisted state for this file.
    if let Some(saved) = state::load_state(&path) {
        app.page = saved.page;
        app.rotation = saved.rotation;
        app.inverted = saved.inverted;
        app.auto_crop = saved.auto_crop;
        app.tinted = saved.tinted;
        app.epub_font_size = saved.epub_font_size;

        if app.page > 0 {
            to_renderer.send(RenderNotif::JumpToPage(app.page))?;
            to_converter.send(ConverterMsg::GoToPage(app.page))?;
        }
        if let Some(font_size) = saved.epub_font_size {
            to_renderer.send(RenderNotif::SetFontSize(font_size))?;
        }
        // Replay saved rotation
        for _ in 0..(saved.rotation / 90) {
            to_renderer.send(RenderNotif::Rotate)?;
        }
        if saved.inverted {
            to_renderer.send(RenderNotif::Invert)?;
        }
        if saved.tinted {
            to_renderer.send(RenderNotif::Tint)?;
        }
        if saved.auto_crop {
            to_converter.send(ConverterMsg::AutoCrop(true))?;
        }
    }

    let render_rx = render_rx.into_stream();
    let from_converter = from_converter.into_stream();

    // --- Main event loop --------------------------------------------------
    let result = enter_event_loop(
        &mut ev_stream,
        to_renderer,
        render_rx,
        to_converter,
        from_converter,
        &mut app,
        &mut current_ws,
        &path,
        startup_started,
    )
    .await;

    // Save state on exit.
    state::save_state(
        &path,
        &state::SavedState {
            page: app.page,
            rotation: app.rotation,
            inverted: app.inverted,
            auto_crop: app.auto_crop,
            tinted: app.tinted,
            epub_font_size: app.epub_font_size,
        },
    );

    result
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

async fn enter_event_loop(
    ev_stream: &mut EventStream,
    to_renderer: Sender<RenderNotif>,
    mut render_rx: flume::r#async::RecvStream<'_, Result<RenderInfo, RenderError>>,
    to_converter: Sender<ConverterMsg>,
    mut from_converter: flume::r#async::RecvStream<
        '_,
        Result<image_pipeline::ConvertedPage, RenderError>,
    >,
    app: &mut App,
    ws: &mut WindowSize,
    path: &Path,
    startup_started: Instant,
) -> anyhow::Result<()> {
    // Force an initial draw once the first page arrives.
    let mut needs_redraw = false;
    let mut needs_status_redraw = false;
    let mut first_page_logged = false;
    let mut has_displayed_page = false;
    let mut loading_deadline: Option<tokio::time::Instant> = None;
    let mut loading_page: Option<usize> = None;
    let mut show_loading_for_page: Option<usize> = None;
    let mut last_drawn_surface: Option<DrawSurface> = None;
    let mut displayed_image_id: Option<kittage::ImageId> = None;

    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    loop {
        let next_ev = ev_stream.next().fuse();
        let scheduled_loading_deadline = loading_deadline;
        let loading_timer = async {
            if let Some(deadline) = scheduled_loading_deadline {
                tokio::time::sleep_until(deadline).await;
            } else {
                std::future::pending::<()>().await;
            }
        }
        .fuse();
        tokio::pin!(loading_timer);

        tokio::select! {
            // --- SIGINT (external kill -INT) ---
            _ = sigint.recv() => {
                return Ok(());
            }

            // --- Keyboard / mouse input ---
            Some(ev_result) = next_ev => {
                let ev = ev_result?;
                match handle_event(&ev, app, ws, &to_renderer, &to_converter).await? {
                    EventOutcome::Quit => return Ok(()),
                    EventOutcome::Redraw => needs_redraw = true,
                    EventOutcome::RedrawStatus => needs_status_redraw = true,
                    EventOutcome::Nothing => {}
                }
            }

            // --- Messages from render thread ---
            Some(msg) = render_rx.next() => {
                match msg {
                    Ok(RenderInfo::DocumentKind(kind)) => {
                        let had_zoom = app.zoom_mode || app.zoom_level != 0;
                        app.set_document_kind(kind);
                        if matches!(kind, DocumentKind::Reflowable) && had_zoom {
                            to_renderer.send(RenderNotif::Area {
                                width_px: render_width_px(app, ws),
                                height_px: render_height_px(app, ws),
                            })?;
                        }
                    }
                    Ok(RenderInfo::EpubFontSize(size)) => {
                        app.set_epub_font_size(size);
                    }
                    Ok(RenderInfo::NumPages(n)) => {
                        app.set_n_pages(n);
                        to_converter.send(ConverterMsg::NumPages(n))?;
                    }
                    Ok(RenderInfo::Page(info)) => {
                        app.got_search_results(info.page_num, info.result_rects.len());
                        if advance_pending_search_if_ready(app, &to_renderer, &to_converter)? {
                            needs_redraw = true;
                        }
                        to_converter.send(ConverterMsg::AddImg(info))?;
                    }
                    Ok(RenderInfo::SearchResults { page_num, num_results }) => {
                        app.got_search_results(page_num, num_results);
                        if advance_pending_search_if_ready(app, &to_renderer, &to_converter)? {
                            needs_redraw = true;
                        }
                    }
                    Ok(RenderInfo::Toc(toc)) => {
                        app.toc = toc;
                    }
                    Ok(RenderInfo::Metadata(meta)) => {
                        app.metadata = meta;
                    }
                    Ok(RenderInfo::Links(links)) => {
                        app.input_mode = InputMode::Links { links, input: String::new() };
                        needs_redraw = true;
                    }
                    Err(e) => {
                        if app.n_pages == 0 {
                            log::warn!("startup render error: {e}");
                            return Err(anyhow::anyhow!(startup_render_error_message(path, &e)));
                        }
                        app.show_render_error(e);
                    }
                }
            }

            // --- Messages from image pipeline ---
            Some(converted) = from_converter.next() => {
                match converted {
                    Ok(page) => {
                        if !first_page_logged && page.page_num == app.page {
                            perf::log_first_page_ready(startup_started, page.page_num);
                            first_page_logged = true;
                        }
                        app.page_converted(page)
                    }
                    Err(e) => app.show_render_error(e),
                }
            }

            _ = &mut loading_timer => {
                loading_deadline = None;
                if loading_page == Some(app.page)
                    && matches!(app.input_mode, InputMode::Normal)
                    && !current_page_is_ready(app)
                {
                    show_loading_for_page = Some(app.page);
                    needs_redraw = true;
                } else {
                    loading_page = None;
                }
            }
        }

        // Detect terminal resize.
        if let Ok(new_ws) = get_window_size() {
            let size_changed = new_ws.rows != ws.rows || new_ws.cols != ws.cols;
            if size_changed {
                *ws = new_ws;
                to_renderer.send(RenderNotif::Area {
                    width_px: render_width_px(app, ws),
                    height_px: render_height_px(app, ws),
                })?;
                needs_redraw = true;
            }
        }

        if current_page_is_ready(app) || !matches!(app.input_mode, InputMode::Normal) {
            loading_deadline = None;
            loading_page = None;
            show_loading_for_page = None;
        } else if loading_page.is_some_and(|page| page != app.page) {
            loading_deadline = None;
            loading_page = None;
            show_loading_for_page = None;
        }

        if needs_redraw || app.needs_redraw {
            if should_delay_loading_indicator(app, has_displayed_page, show_loading_for_page) {
                if loading_page != Some(app.page) {
                    loading_page = Some(app.page);
                    loading_deadline = Some(tokio::time::Instant::now() + LOADING_INDICATOR_DELAY);
                }
                needs_redraw = false;
                app.needs_redraw = false;
                needs_status_redraw = false;
                continue;
            }

            needs_redraw = false;
            app.needs_redraw = false;
            needs_status_redraw = false;
            draw(
                app,
                ws,
                ev_stream,
                &to_renderer,
                &mut last_drawn_surface,
                &mut displayed_image_id,
            )
            .await?;
            if matches!(app.input_mode, InputMode::Normal) && current_page_is_ready(app) {
                has_displayed_page = true;
                loading_deadline = None;
                loading_page = None;
                show_loading_for_page = None;
            }
        } else if needs_status_redraw {
            needs_status_redraw = false;
            draw_status_only(app, ws)?;
        }
    }
}

// ---------------------------------------------------------------------------
// Input handling
// ---------------------------------------------------------------------------

enum EventOutcome {
    Quit,
    Redraw,
    RedrawStatus,
    Nothing,
}

fn startup_render_error_message(path: &Path, err: &RenderError) -> String {
    match err {
        RenderError::Mupdf(_) | RenderError::EmptyDocument => {
            let kind = match path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
                .as_deref()
            {
                Some("pdf") => "pdf",
                Some("epub") => "epub",
                _ => "document",
            };
            format!("not a valid {kind} file")
        }
        _ => err.to_string(),
    }
}

fn current_page_is_ready(app: &App) -> bool {
    app.rendered
        .get(app.page)
        .and_then(|page| page.converted.as_ref())
        .is_some()
}

fn should_delay_loading_indicator(
    app: &App,
    has_displayed_page: bool,
    show_loading_for_page: Option<usize>,
) -> bool {
    has_displayed_page
        && matches!(app.input_mode, InputMode::Normal)
        && show_loading_for_page != Some(app.page)
        && !current_page_is_ready(app)
}

fn send_page_jump_commands(
    app: &App,
    to_renderer: &Sender<RenderNotif>,
    to_converter: &Sender<ConverterMsg>,
) -> anyhow::Result<()> {
    to_renderer.send(RenderNotif::JumpToPage(app.page))?;
    to_converter.send(ConverterMsg::GoToPage(app.page))?;

    let current_page_missing = app
        .rendered
        .get(app.page)
        .and_then(|page| page.converted.as_ref())
        .is_none();
    if current_page_missing {
        to_renderer.send(RenderNotif::PageNeedsReRender(app.page))?;
    }
    Ok(())
}

fn advance_pending_search_if_ready(
    app: &mut App,
    to_renderer: &Sender<RenderNotif>,
    to_converter: &Sender<ConverterMsg>,
) -> anyhow::Result<bool> {
    let Some(from_page) = app.pending_search_jump_from else {
        return Ok(false);
    };

    match app.resolve_next_search_page(from_page) {
        SearchJumpResolution::Pending => Ok(false),
        SearchJumpResolution::Found(page) => {
            app.pending_search_jump_from = None;
            app.msg = StatusMsg::Hint;
            if app.go_to_page(page) {
                send_page_jump_commands(app, to_renderer, to_converter)?;
            } else {
                app.needs_redraw = true;
            }
            Ok(true)
        }
        SearchJumpResolution::NoResult => {
            app.pending_search_jump_from = None;
            app.show_info("no result".to_owned());
            app.needs_redraw = true;
            Ok(true)
        }
    }
}

fn toc_entry_at_row(toc_len: usize, scroll: usize, row: u16, visible_rows: u16) -> Option<usize> {
    if row >= visible_rows {
        return None;
    }

    let entry_index = scroll + usize::from(row);
    (entry_index < toc_len).then_some(entry_index)
}

async fn handle_event(
    ev: &Event,
    app: &mut App,
    ws: &WindowSize,
    to_renderer: &Sender<RenderNotif>,
    to_converter: &Sender<ConverterMsg>,
) -> anyhow::Result<EventOutcome> {
    let scroll_step = u32::from(ws.cell_height_px) * 3;
    let pan_step = u32::from(ws.cell_width_px) * 3;

    fn is_left_click(mouse: &MouseEvent) -> bool {
        matches!(
            mouse.kind,
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
        )
    }

    if let Event::Mouse(mouse) = ev {
        if let InputMode::Toc { scroll } = app.input_mode
            && is_left_click(mouse)
            && let Some(entry_index) =
                toc_entry_at_row(app.toc.len(), scroll, mouse.row, ws.page_rows())
            && let Some(entry) = app.toc.get(entry_index)
        {
            let target = entry.page;
            app.input_mode = InputMode::Normal;
            app.msg = StatusMsg::Hint;
            if app.go_to_page(target) {
                send_page_jump_commands(app, to_renderer, to_converter)?;
            }
            return Ok(EventOutcome::Redraw);
        }
        return Ok(EventOutcome::Nothing);
    }

    let Event::Key(key) = ev else {
        // Resize handled by the window-size poll in the main loop.
        if matches!(ev, Event::Resize(_, _)) {
            return Ok(EventOutcome::Redraw);
        }
        return Ok(EventOutcome::Nothing);
    };

    // --- Handle special input modes first ---

    match &app.input_mode {
        InputMode::GoToPage(buf) => {
            let buf = buf.clone();
            match key.code {
                KeyCode::Enter => {
                    if let Ok(num) = buf.parse::<usize>() {
                        // User enters 1-indexed page number
                        let target = num.saturating_sub(1);
                        app.input_mode = InputMode::Normal;
                        if app.go_to_page(target) {
                            send_page_jump_commands(app, to_renderer, to_converter)?;
                        }
                        app.msg = StatusMsg::Hint;
                    } else {
                        app.input_mode = InputMode::Normal;
                        app.show_info("invalid page number".to_owned());
                    }
                    return Ok(EventOutcome::Redraw);
                }
                KeyCode::Esc => {
                    app.input_mode = InputMode::Normal;
                    app.msg = StatusMsg::Hint;
                    return Ok(EventOutcome::Redraw);
                }
                KeyCode::Backspace => {
                    let mut new_buf = buf;
                    new_buf.pop();
                    app.msg = StatusMsg::Info(format!("go to page: {new_buf}"));
                    app.input_mode = InputMode::GoToPage(new_buf);
                    return Ok(EventOutcome::RedrawStatus);
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    let mut new_buf = buf;
                    new_buf.push(c);
                    app.msg = StatusMsg::Info(format!("go to page: {new_buf}"));
                    app.input_mode = InputMode::GoToPage(new_buf);
                    return Ok(EventOutcome::RedrawStatus);
                }
                _ => return Ok(EventOutcome::Nothing),
            }
        }

        InputMode::Toc { scroll } => {
            let scroll = *scroll;
            let toc_len = app.toc.len();
            match key.code {
                KeyCode::Esc | KeyCode::Char('t') | KeyCode::Char('q') => {
                    app.input_mode = InputMode::Normal;
                    app.msg = StatusMsg::Hint;
                    app.needs_redraw = true;
                    return Ok(EventOutcome::Redraw);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if scroll + 1 < toc_len {
                        app.input_mode = InputMode::Toc { scroll: scroll + 1 };
                    }
                    return Ok(EventOutcome::Redraw);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.input_mode = InputMode::Toc {
                        scroll: scroll.saturating_sub(1),
                    };
                    return Ok(EventOutcome::Redraw);
                }
                KeyCode::Enter => {
                    if let Some(entry) = app.toc.get(scroll) {
                        let target = entry.page;
                        app.input_mode = InputMode::Normal;
                        app.msg = StatusMsg::Hint;
                        if app.go_to_page(target) {
                            send_page_jump_commands(app, to_renderer, to_converter)?;
                        }
                    }
                    return Ok(EventOutcome::Redraw);
                }
                _ => return Ok(EventOutcome::Nothing),
            }
        }

        InputMode::Search(buf) => {
            let buf = buf.clone();
            match key.code {
                KeyCode::Enter => {
                    app.input_mode = InputMode::Normal;
                    if buf.is_empty() {
                        // Clear search
                        app.search_term = None;
                        app.pending_search_jump_from = None;
                        to_renderer.send(RenderNotif::Search(String::new()))?;
                        app.msg = StatusMsg::Hint;
                    } else {
                        app.search_term = Some(buf.clone());
                        app.clear_search_results();
                        app.pending_search_jump_from = Some(app.page);
                        to_renderer.send(RenderNotif::Search(buf))?;
                        app.show_info("searching...".to_owned());
                    }
                    return Ok(EventOutcome::Redraw);
                }
                KeyCode::Esc => {
                    app.input_mode = InputMode::Normal;
                    app.msg = StatusMsg::Hint;
                    return Ok(EventOutcome::Redraw);
                }
                KeyCode::Backspace => {
                    let mut new_buf = buf;
                    new_buf.pop();
                    app.msg = StatusMsg::Info(format!("/{new_buf}"));
                    app.input_mode = InputMode::Search(new_buf);
                    return Ok(EventOutcome::RedrawStatus);
                }
                KeyCode::Char(c) => {
                    let mut new_buf = buf;
                    new_buf.push(c);
                    app.msg = StatusMsg::Info(format!("/{new_buf}"));
                    app.input_mode = InputMode::Search(new_buf);
                    return Ok(EventOutcome::RedrawStatus);
                }
                _ => return Ok(EventOutcome::Nothing),
            }
        }

        InputMode::Metadata => match key.code {
            KeyCode::Esc | KeyCode::Char('M') | KeyCode::Char('q') => {
                app.input_mode = InputMode::Normal;
                app.msg = StatusMsg::Hint;
                app.needs_redraw = true;
                return Ok(EventOutcome::Redraw);
            }
            _ => return Ok(EventOutcome::Nothing),
        },

        InputMode::Links { links, input } => {
            let links_owned: Vec<_> = links.iter().map(|l| (l.page, l.uri.clone())).collect();
            let input = input.clone();
            match key.code {
                KeyCode::Esc => {
                    app.input_mode = InputMode::Normal;
                    app.msg = StatusMsg::Hint;
                    return Ok(EventOutcome::Redraw);
                }
                KeyCode::Backspace => {
                    let mut new_input = input;
                    new_input.pop();
                    if let InputMode::Links {
                        input: ref mut inp, ..
                    } = app.input_mode
                    {
                        *inp = new_input;
                    }
                    return Ok(EventOutcome::Redraw);
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    let mut new_input = input;
                    new_input.push(c);
                    if let InputMode::Links {
                        input: ref mut inp, ..
                    } = app.input_mode
                    {
                        *inp = new_input;
                    }
                    return Ok(EventOutcome::Redraw);
                }
                KeyCode::Enter => {
                    app.input_mode = InputMode::Normal;
                    if let Ok(num) = input.parse::<usize>() {
                        if num >= 1 && num <= links_owned.len() {
                            let (page, ref uri) = links_owned[num - 1];
                            if let Some(target_page) = page {
                                // Internal link — jump to page
                                if app.go_to_page(target_page) {
                                    send_page_jump_commands(app, to_renderer, to_converter)?;
                                }
                                app.msg = StatusMsg::Hint;
                            } else {
                                // External link — open in browser
                                open_url(uri);
                                app.show_info(format!("opened: {uri}"));
                            }
                        } else {
                            app.show_info("invalid link number".to_owned());
                        }
                    } else {
                        app.msg = StatusMsg::Hint;
                    }
                    return Ok(EventOutcome::Redraw);
                }
                _ => return Ok(EventOutcome::Nothing),
            }
        }

        InputMode::Normal => {} // fall through to normal handling below
    }

    // --- Normal mode ---
    match key.code {
        // Quit
        KeyCode::Char('q') | KeyCode::Esc => return Ok(EventOutcome::Quit),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return Ok(EventOutcome::Quit);
        }
        KeyCode::Char(' ') => {
            if app.next_page() {
                send_page_jump_commands(app, to_renderer, to_converter)?;
            }
        }

        // --- Horizontal navigation ---
        KeyCode::Right | KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('l') => {
            match horizontal_action(key.code, app.zoom_mode).unwrap() {
                HorizontalAction::NextPage => {
                    if app.next_page() {
                        send_page_jump_commands(app, to_renderer, to_converter)?;
                    }
                }
                HorizontalAction::PrevPage => {
                    if app.prev_page() {
                        send_page_jump_commands(app, to_renderer, to_converter)?;
                    }
                }
                HorizontalAction::PanRight => app.pan_right(ws, pan_step),
                HorizontalAction::PanLeft => app.pan_left(ws, pan_step),
            }
        }

        // j/k always turn pages (bypass scroll)
        KeyCode::Char('j') => {
            if app.next_page() {
                send_page_jump_commands(app, to_renderer, to_converter)?;
            }
        }
        KeyCode::Char('k') => {
            if app.prev_page() {
                send_page_jump_commands(app, to_renderer, to_converter)?;
            }
        }

        KeyCode::PageDown => {
            if app.zoom_mode {
                match app.page_down_in_zoom(ws) {
                    ScrollAction::TurnNext => {
                        if app.next_page() {
                            send_page_jump_commands(app, to_renderer, to_converter)?;
                        }
                    }
                    ScrollAction::Scrolled | ScrollAction::Nothing | ScrollAction::TurnPrev => {}
                }
            } else if app.next_page() {
                send_page_jump_commands(app, to_renderer, to_converter)?;
            }
        }
        KeyCode::PageUp => {
            if app.zoom_mode {
                match app.page_up_in_zoom(ws) {
                    ScrollAction::TurnPrev => {
                        if app.prev_page_at_bottom(ws) {
                            send_page_jump_commands(app, to_renderer, to_converter)?;
                        }
                    }
                    ScrollAction::Scrolled | ScrollAction::Nothing | ScrollAction::TurnNext => {}
                }
            } else if app.prev_page() {
                send_page_jump_commands(app, to_renderer, to_converter)?;
            }
        }

        // --- Up/Down: scroll within page, auto-turn at boundaries ---
        KeyCode::Down => match app.scroll_down(ws, scroll_step) {
            ScrollAction::TurnNext => {
                if app.next_page() {
                    send_page_jump_commands(app, to_renderer, to_converter)?;
                }
            }
            ScrollAction::Scrolled | ScrollAction::Nothing | ScrollAction::TurnPrev => {}
        },
        KeyCode::Up => match app.scroll_up(ws, scroll_step) {
            ScrollAction::TurnPrev => {
                if app.prev_page_at_bottom(ws) {
                    send_page_jump_commands(app, to_renderer, to_converter)?;
                }
            }
            ScrollAction::Scrolled | ScrollAction::Nothing | ScrollAction::TurnNext => {}
        },

        // --- Zoom mode ---
        KeyCode::Char('z') => {
            if !app.supports_zoom() {
                app.show_info("zoom is only available for fixed-layout documents".to_owned());
                return Ok(EventOutcome::Redraw);
            }
            app.toggle_zoom_mode();
            app.invalidate_all_pages();
            to_renderer.send(RenderNotif::Area {
                width_px: render_width_px(app, ws),
                height_px: render_height_px(app, ws),
            })?;
            let mode = if app.zoom_mode { "on" } else { "off" };
            app.show_info(format!("zoom mode {mode}"));
        }
        KeyCode::Char('o') if app.zoom_mode => {
            if app.zoom_in() {
                app.invalidate_all_pages();
                to_renderer.send(RenderNotif::Area {
                    width_px: render_width_px(app, ws),
                    height_px: render_height_px(app, ws),
                })?;
                let zf = app.zoom_factor();
                app.show_info(format!("zoom {:.0}%", zf * 100.0));
            }
        }
        KeyCode::Char('O') if app.zoom_mode => {
            if app.zoom_out() {
                app.invalidate_all_pages();
                to_renderer.send(RenderNotif::Area {
                    width_px: render_width_px(app, ws),
                    height_px: render_height_px(app, ws),
                })?;
                let zf = app.zoom_factor();
                let max_pan = app.max_pan_x(ws);
                if app.pan_x > max_pan {
                    app.pan_x = max_pan;
                }
                app.show_info(format!("zoom {:.0}%", zf * 100.0));
            }
        }
        KeyCode::Char('<') => {
            if matches!(app.document_kind, DocumentKind::Reflowable) {
                app.invalidate_all_pages();
                to_renderer.send(RenderNotif::AdjustFontSize(-1))?;
                app.show_info("EPUB font smaller".to_owned());
            } else {
                return Ok(EventOutcome::Nothing);
            }
        }
        KeyCode::Char('>') => {
            if matches!(app.document_kind, DocumentKind::Reflowable) {
                app.invalidate_all_pages();
                to_renderer.send(RenderNotif::AdjustFontSize(1))?;
                app.show_info("EPUB font larger".to_owned());
            } else {
                return Ok(EventOutcome::Nothing);
            }
        }

        // --- TOC ---
        KeyCode::Char('t') => {
            if app.toc.is_empty() {
                app.show_info("no table of contents".to_owned());
            } else {
                app.input_mode = InputMode::Toc { scroll: 0 };
            }
        }

        // --- Go-to-page ---
        KeyCode::Char('g') => {
            app.input_mode = InputMode::GoToPage(String::new());
            app.msg = StatusMsg::Info("go to page: ".to_owned());
        }

        // --- Search ---
        KeyCode::Char('/') => {
            app.input_mode = InputMode::Search(String::new());
            app.msg = StatusMsg::Info("/".to_owned());
        }
        KeyCode::Char('n') => {
            if app.next_search_result() {
                send_page_jump_commands(app, to_renderer, to_converter)?;
            } else if app.search_term.is_some() {
                app.show_info("no more results".to_owned());
            }
        }
        KeyCode::Char('N') => {
            if app.prev_search_result() {
                send_page_jump_commands(app, to_renderer, to_converter)?;
            } else if app.search_term.is_some() {
                app.show_info("no more results".to_owned());
            }
        }

        // Transformations
        KeyCode::Char('i') => {
            to_renderer.send(RenderNotif::Invert)?;
            app.inverted = !app.inverted;
            app.invalidate_all_pages();
            app.show_info("invert toggled".to_owned());
        }
        KeyCode::Char('r') => {
            to_renderer.send(RenderNotif::Rotate)?;
            app.rotation = (app.rotation + 90) % 360;
            app.invalidate_all_pages();
            app.show_info("rotated".to_owned());
        }
        KeyCode::Char('c') => {
            app.auto_crop = !app.auto_crop;
            to_converter.send(ConverterMsg::AutoCrop(app.auto_crop))?;
            to_renderer.send(RenderNotif::Refresh)?;
            app.invalidate_all_pages();
            let mode = if app.auto_crop { "on" } else { "off" };
            app.show_info(format!("auto-crop {mode}"));
        }
        KeyCode::Char('d') => {
            to_renderer.send(RenderNotif::Tint)?;
            app.tinted = !app.tinted;
            app.invalidate_all_pages();
            let mode = if app.tinted { "on" } else { "off" };
            app.show_info(format!("tint {mode}"));
        }

        // Metadata
        KeyCode::Char('M') => {
            if app.metadata.is_empty() {
                app.show_info("no metadata available".to_owned());
            } else {
                app.input_mode = InputMode::Metadata;
            }
        }

        // Links
        KeyCode::Char('f') => {
            to_renderer.send(RenderNotif::GetLinks(app.page))?;
            app.show_info("extracting links...".to_owned());
        }

        // Manual refresh
        KeyCode::Char('R') | KeyCode::F(5) => {
            to_renderer.send(RenderNotif::Refresh)?;
            app.show_info("refreshing...".to_owned());
        }

        // Help
        KeyCode::Char('?') => {
            app.show_info(
                "Space next  PgUp/PgDn page  ←/→ page  ↑/↓ scroll  j/k page  g goto  t toc  / search  n/N result  z zoom  o/O +/-  i invert  r rotate  c crop  d tint  M meta  f links  q quit".to_owned(),
            );
        }

        _ => return Ok(EventOutcome::Nothing),
    }

    Ok(EventOutcome::Redraw)
}

fn probe_mode_path(args: &[String]) -> Option<PathBuf> {
    match args {
        [_, flag, path] if flag == "--probe-document" => Some(PathBuf::from(path)),
        _ => None,
    }
}

fn should_preflight(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("pdf") | Some("epub")
    )
}

fn preflight_document(path: &Path) -> anyhow::Result<()> {
    if !should_preflight(path) {
        return Ok(());
    }

    let current_exe = std::env::current_exe()?;
    let status = Command::new(current_exe)
        .arg("--probe-document")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    if status.success() {
        Ok(())
    } else {
        anyhow::bail!(startup_render_error_message(
            path,
            &RenderError::EmptyDocument
        ));
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlaySurface {
    Toc,
    Metadata,
    Links,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageSurface {
    page_num: usize,
    generation: u64,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
    display_cols: u16,
    display_rows: u16,
    pos_x: u16,
    pos_y: u16,
}

impl PageSurface {
    fn display_location(self) -> kittage::display::DisplayLocation {
        kittage::display::DisplayLocation {
            x: self.src_x,
            y: self.src_y,
            width: self.src_w,
            height: self.src_h,
            columns: self.display_cols,
            rows: self.display_rows,
            ..kittage::display::DisplayLocation::default()
        }
    }

    fn pos(self) -> Pos {
        Pos {
            x: self.pos_x,
            y: self.pos_y,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DrawSurface {
    Overlay(OverlaySurface),
    Loading(usize),
    Page(PageSurface),
}

fn overlay_surface(mode: &InputMode) -> Option<OverlaySurface> {
    match mode {
        InputMode::Toc { .. } => Some(OverlaySurface::Toc),
        InputMode::Metadata => Some(OverlaySurface::Metadata),
        InputMode::Links { .. } => Some(OverlaySurface::Links),
        _ => None,
    }
}

fn compute_page_surface(
    ws: &WindowSize,
    page_num: usize,
    generation: u64,
    pixel_w: u32,
    pixel_h: u32,
    scroll_offset: u32,
    pan_x: u32,
) -> PageSurface {
    let page_area_h_px = ws.page_area_height_px() as u32;
    let page_area_w_px = ws.width_px as u32;

    // Unified source-rect logic: crop if image is larger than terminal,
    // centre if smaller.
    let needs_crop_w = pixel_w > page_area_w_px;
    let needs_crop_h = pixel_h > page_area_h_px;

    let (src_x, src_y, src_w, src_h, display_cols, display_rows, pos_x, pos_y) =
        if needs_crop_w || needs_crop_h {
            // At least one dimension exceeds terminal — crop with scroll/pan offsets.
            let (sx, sw, dcols, px) = if needs_crop_w {
                let max_pan = pixel_w.saturating_sub(page_area_w_px);
                let sx = pan_x.min(max_pan);
                (sx, page_area_w_px, ws.cols, 0u16)
            } else {
                // Image narrower than terminal — centre horizontally, no crop
                let spare = page_area_w_px - pixel_w;
                let px = (spare / 2 / u32::from(ws.cell_width_px)) as u16;
                (
                    0u32,
                    pixel_w,
                    (pixel_w / u32::from(ws.cell_width_px)) as u16,
                    px,
                )
            };

            let (sy, sh, drows, py) = if needs_crop_h {
                let max_off = pixel_h.saturating_sub(page_area_h_px);
                let sy = scroll_offset.min(max_off);
                (sy, page_area_h_px, ws.page_rows(), 0u16)
            } else {
                // Image shorter than terminal — centre vertically, no crop
                let cell_h_u32 = u32::from(ws.cell_height_px);
                let img_rows = (pixel_h / cell_h_u32) as u16;
                let py = ws.page_rows().saturating_sub(img_rows) / 2;
                (0u32, pixel_h, img_rows, py)
            };

            (sx, sy, sw, sh, dcols, drows, px, py)
        } else {
            // Page fits on screen: scale up to fill terminal while
            // preserving aspect ratio (important for auto-crop).
            let cell_w_f = ws.cell_width_px as f32;
            let cell_h_f = ws.cell_height_px as f32;
            let img_aspect = pixel_w as f32 / pixel_h.max(1) as f32;
            let area_aspect = page_area_w_px as f32 / page_area_h_px.max(1) as f32;

            let (disp_w_px, disp_h_px) = if img_aspect > area_aspect {
                (page_area_w_px as f32, page_area_w_px as f32 / img_aspect)
            } else {
                (page_area_h_px as f32 * img_aspect, page_area_h_px as f32)
            };

            let display_cols = (disp_w_px / cell_w_f).round() as u16;
            let display_rows = (disp_h_px / cell_h_f).round() as u16;
            let pos_x = ws.cols.saturating_sub(display_cols) / 2;
            let pos_y = ws.page_rows().saturating_sub(display_rows) / 2;

            (
                0u32,
                0u32,
                pixel_w,
                pixel_h,
                display_cols,
                display_rows,
                pos_x,
                pos_y,
            )
        };

    PageSurface {
        page_num,
        generation,
        src_x,
        src_y,
        src_w,
        src_h,
        display_cols,
        display_rows,
        pos_x,
        pos_y,
    }
}

async fn clear_visible_image(
    displayed_image_id: &mut Option<kittage::ImageId>,
    ev_stream: &mut EventStream,
    app: &mut App,
) {
    let Some(image_id) = displayed_image_id.take() else {
        return;
    };

    if let Err(err) = clear_kitty_image(image_id, ev_stream).await {
        use kittage::error::{TerminalError, TransmitError};
        if !matches!(err, TransmitError::Terminal(TerminalError::NoEntity(_))) {
            app.show_render_error(RenderError::Converting(err.to_string()));
        }
    }
}

fn draw_status_only(app: &App, ws: &WindowSize) -> anyhow::Result<()> {
    execute!(stdout(), BeginSynchronizedUpdate)?;
    draw_status_bar(ws, &app.msg.text(app.page, app.n_pages, app.zoom_mode))?;
    execute!(stdout(), EndSynchronizedUpdate)?;
    Ok(())
}

async fn draw(
    app: &mut App,
    ws: &WindowSize,
    ev_stream: &mut EventStream,
    to_renderer: &Sender<RenderNotif>,
    last_drawn_surface: &mut Option<DrawSurface>,
    displayed_image_id: &mut Option<kittage::ImageId>,
) -> anyhow::Result<()> {
    let draw_started = Instant::now();
    execute!(stdout(), BeginSynchronizedUpdate)?;

    draw_status_bar(ws, &app.msg.text(app.page, app.n_pages, app.zoom_mode))?;

    let stale_image_ids = std::mem::take(&mut app.pending_image_deletes);
    if let Err(err) = delete_kitty_images(stale_image_ids, ev_stream).await {
        use kittage::error::{TerminalError, TransmitError};
        if !matches!(err, TransmitError::Terminal(TerminalError::NoEntity(_))) {
            app.show_render_error(RenderError::Converting(err.to_string()));
        }
    }

    // Full-screen overlays take precedence over page image redraws.
    if let Some(overlay) = overlay_surface(&app.input_mode) {
        clear_page_area(ws)?;
        clear_visible_image(displayed_image_id, ev_stream, app).await;
        match &app.input_mode {
            InputMode::Toc { scroll } => draw_toc(ws, &app.toc, *scroll, app.page)?,
            InputMode::Metadata => draw_metadata(ws, &app.metadata)?,
            InputMode::Links { links, input } => draw_links(ws, links, input, app.page)?,
            _ => {}
        }
        *last_drawn_surface = Some(DrawSurface::Overlay(overlay));
        execute!(stdout(), EndSynchronizedUpdate)?;
        perf::log_page_timing("draw", app.page, draw_started.elapsed());
        return Ok(());
    }

    let page_num = app.page;
    let maybe_converted = app
        .rendered
        .get(page_num)
        .and_then(|p| p.converted.as_ref());
    let Some(converted) = maybe_converted else {
        if !matches!(last_drawn_surface, Some(DrawSurface::Loading(p)) if *p == page_num) {
            clear_page_area(ws)?;
        }
        clear_visible_image(displayed_image_id, ev_stream, app).await;
        draw_loading(ws)?;
        *last_drawn_surface = Some(DrawSurface::Loading(page_num));
        execute!(stdout(), EndSynchronizedUpdate)?;
        perf::log_page_timing("draw", page_num, draw_started.elapsed());
        return Ok(());
    };

    let target_surface = compute_page_surface(
        ws,
        page_num,
        converted.generation,
        converted.pixel_w,
        converted.pixel_h,
        app.scroll_offset,
        app.pan_x,
    );

    if matches!(last_drawn_surface, Some(DrawSurface::Page(surface)) if *surface == target_surface)
    {
        execute!(stdout(), EndSynchronizedUpdate)?;
        perf::log_page_timing("draw", page_num, draw_started.elapsed());
        return Ok(());
    }

    if !matches!(last_drawn_surface, Some(DrawSurface::Page(_))) {
        clear_page_area(ws)?;
    }

    let target_image_id = converted.img.image_id_hint();
    let old_image_id = *displayed_image_id;
    if old_image_id.is_some() && old_image_id != target_image_id {
        clear_visible_image(displayed_image_id, ev_stream, app).await;
    }

    let maybe_converted = app
        .rendered
        .get_mut(page_num)
        .and_then(|page| page.converted.as_mut());
    let Some(converted) = maybe_converted else {
        clear_page_area(ws)?;
        clear_visible_image(displayed_image_id, ev_stream, app).await;
        draw_loading(ws)?;
        *last_drawn_surface = Some(DrawSurface::Loading(page_num));
        execute!(stdout(), EndSynchronizedUpdate)?;
        perf::log_page_timing("draw", page_num, draw_started.elapsed());
        return Ok(());
    };
    let (maybe_err, new_image_id) = {
        let to_display = KittyDisplay::DisplayImages(vec![KittyReadyToDisplay {
            img: &mut converted.img,
            page_num,
            pos: target_surface.pos(),
            display_loc: target_surface.display_location(),
        }]);

        let display_started = Instant::now();
        let maybe_err = display_kitty_images(to_display, ev_stream).await;
        perf::log_page_timing("kitty_display", page_num, display_started.elapsed());
        (maybe_err, converted.img.image_id_hint())
    };

    let mut had_display_error = false;
    if let Err((failed_pages, _desc, err)) = maybe_err {
        had_display_error = true;
        use kittage::error::{TerminalError, TransmitError};
        match err {
            // Kitty/Ghostty evicted the image under memory pressure — silently re-render.
            TransmitError::Terminal(TerminalError::NoEntity(_)) => {}
            _ => {
                app.show_render_error(RenderError::Converting(err.to_string()));
            }
        }
        for failed_page in failed_pages {
            if failed_page == page_num {
                *last_drawn_surface = None;
                *displayed_image_id = None;
            }
            app.page_display_failed(failed_page);
            to_renderer.send(RenderNotif::PageNeedsReRender(failed_page))?;
        }
    }

    if !had_display_error {
        *displayed_image_id = new_image_id;
        *last_drawn_surface = Some(DrawSurface::Page(target_surface));
    }

    execute!(stdout(), EndSynchronizedUpdate)?;
    perf::log_page_timing("draw", page_num, draw_started.elapsed());
    Ok(())
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let cmd = "xdg-open";

    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn print_usage() {
    println!(
        "\
treader {} — terminal PDF/EPUB viewer (Kitty graphics protocol)

USAGE:
    treader [options] <file>

ARGUMENTS:
    <file>                    PDF or EPUB file to open

OPTIONS:
    -i, --invert              Start with inverted colours
    -b, --black-color <css>   Custom black colour (e.g. '#1a1a2e')
    -w, --white-color <css>   Custom white colour (e.g. '#e0e0e0')
    -h, --help                Show this help message and exit
        --version             Print version and exit

KEYBOARD:
    Left/Right               Turn page
    Space                    Next page
    PageUp/PageDown          Previous/next page (viewport jump in zoom mode)
    h/l                      Turn page (pan horizontally in zoom mode)
    Up/Down                  Scroll within page (auto-turns at boundary)
    j/k                      Next/previous page (bypass scroll)
    g + number + Enter       Go to page
    t                        Table of contents
    / + text + Enter         Search (empty search clears)
    n/N                      Next/previous search result
    z                        Toggle zoom mode
    o/O                      Zoom in/out (fixed-layout docs)
    </>                      Smaller/larger font (EPUB)
    i                        Invert colours
    r                        Rotate 90° clockwise
    c                        Toggle auto-crop (remove whitespace margins)
    d                        Toggle warm tint (sepia)
    M                        Document metadata
    f                        Show links (type number + Enter to follow)
    R, F5                    Refresh
    ?                        Help summary in status bar
    q, Esc                   Quit

ENVIRONMENT:
    RUST_LOG=debug            Enable debug logging to ./treader.log

See docs/help.md for the full reference.",
        env!("CARGO_PKG_VERSION")
    );
}

fn parse_color(cs: &str) -> anyhow::Result<i32> {
    let color =
        csscolorparser::parse(cs).map_err(|e| anyhow::anyhow!("invalid color {cs:?}: {e}"))?;
    let [r, g, b, _] = color.to_rgba8();
    Ok(i32::from_be_bytes([0, r, g, b]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn startup_render_error_message_for_pdf() {
        assert_eq!(
            startup_render_error_message(Path::new("fake.pdf"), &RenderError::EmptyDocument),
            "not a valid pdf file"
        );
    }

    #[test]
    fn startup_render_error_message_for_epub() {
        assert_eq!(
            startup_render_error_message(Path::new("fake.epub"), &RenderError::EmptyDocument),
            "not a valid epub file"
        );
    }

    #[test]
    fn startup_render_error_message_for_other_extension() {
        assert_eq!(
            startup_render_error_message(Path::new("fake.txt"), &RenderError::EmptyDocument),
            "not a valid document file"
        );
    }

    #[test]
    fn toc_entry_at_row_maps_top_visible_entry() {
        assert_eq!(toc_entry_at_row(10, 3, 0, 20), Some(3));
    }

    #[test]
    fn toc_entry_at_row_maps_scrolled_row() {
        assert_eq!(toc_entry_at_row(10, 3, 4, 20), Some(7));
    }

    #[test]
    fn toc_entry_at_row_rejects_status_bar_row() {
        assert_eq!(toc_entry_at_row(10, 0, 20, 20), None);
    }

    #[test]
    fn toc_entry_at_row_rejects_past_end_of_toc() {
        assert_eq!(toc_entry_at_row(5, 3, 3, 20), None);
    }

    #[test]
    fn parse_color_hex() {
        let c = parse_color("#ff0000").unwrap();
        assert_eq!(c, i32::from_be_bytes([0, 0xff, 0, 0]));
    }

    #[test]
    fn parse_color_white_hex() {
        let c = parse_color("#ffffff").unwrap();
        assert_eq!(c, i32::from_be_bytes([0, 0xff, 0xff, 0xff]));
    }

    #[test]
    fn parse_color_black_hex() {
        let c = parse_color("#000000").unwrap();
        assert_eq!(c, i32::from_be_bytes([0, 0, 0, 0]));
    }

    #[test]
    fn parse_color_sepia() {
        let c = parse_color("#704214").unwrap();
        assert_eq!(c, i32::from_be_bytes([0, 0x70, 0x42, 0x14]));
    }

    #[test]
    fn parse_color_invalid() {
        assert!(parse_color("not_a_color").is_err());
    }

    #[test]
    fn parse_color_rgb_func() {
        let c = parse_color("rgb(128, 64, 32)").unwrap();
        assert_eq!(c, i32::from_be_bytes([0, 128, 64, 32]));
    }

    #[test]
    fn horizontal_action_keeps_arrow_keys_as_page_turns_in_zoom_mode() {
        assert_eq!(
            horizontal_action(KeyCode::Right, true),
            Some(HorizontalAction::NextPage)
        );
        assert_eq!(
            horizontal_action(KeyCode::Left, true),
            Some(HorizontalAction::PrevPage)
        );
    }

    #[test]
    fn horizontal_action_uses_h_and_l_for_pan_in_zoom_mode() {
        assert_eq!(
            horizontal_action(KeyCode::Char('l'), true),
            Some(HorizontalAction::PanRight)
        );
        assert_eq!(
            horizontal_action(KeyCode::Char('h'), true),
            Some(HorizontalAction::PanLeft)
        );
    }
}
