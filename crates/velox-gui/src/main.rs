//! Velox — window, input, and the virtualized result list.
//!
//! The UI thread owns the window and the Direct2D device and does nothing else.
//! Queries go to the engine thread (`worker`), which posts back when it has an
//! answer; the list is drawn from a window of at most a few dozen rows, fetched
//! on demand, no matter how many entries matched.

// A GUI app should not open a console window when launched from Explorer, but
// keeping stdio in debug builds makes panics visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod view;
mod worker;

use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, InvalidateRect, SetBkColor,
    SetTextColor, HBRUSH, HDC, HFONT, PAINTSTRUCT, DEFAULT_CHARSET, DEFAULT_PITCH,
    FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION, FONT_QUALITY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, VIRTUAL_KEY, VK_DOWN, VK_END, VK_ESCAPE, VK_F2, VK_F3,
    VK_F5, VK_HOME, VK_NEXT, VK_PRIOR, VK_RETURN, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use velox_core::query::{MatchMode, Scope, SortKey};
use velox_core::{format_filetime, format_size, Index};
use view::{Frame, Metrics, RowData, View};
use worker::{Command, Shared, WM_APP_INDEXED, WM_APP_RESULTS, WM_APP_STATUS, WM_APP_UPDATED};

/// Search-box colours, matching the Direct2D palette in `view.rs`. `COLORREF` is
/// `0x00BBGGRR`, not RGB.
const EDIT_BG: u32 = 0x0030_3030;
const EDIT_FG: u32 = 0x00E8_E8E8;

const CLASS_NAME: &str = "VeloxMainWindow";
const WINDOW_TITLE: &str = "Velox";
/// Child-window id of the query box.
const ID_SEARCH: isize = 1001;

/// Original `EDIT` window procedure, saved when the search box is subclassed.
static ORIGINAL_EDIT_PROC: AtomicIsize = AtomicIsize::new(0);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct App {
    hwnd: HWND,
    edit: HWND,
    font: HFONT,
    /// Background brush for the search box, owned so it outlives every paint.
    edit_brush: HBRUSH,
    shared: Arc<Shared>,
    tx: Sender<Command>,
    index: Option<Arc<Index>>,
    response: Option<worker::Response>,
    /// Bumped per keystroke so a late reply for an old query is discarded.
    generation: u64,
    query: String,
    mode: MatchMode,
    scope: Scope,
    sort: (SortKey, bool),
    scroll: usize,
    selected: usize,
    dragging: Option<f32>,
    view: Option<View>,
    metrics: Metrics,
}

impl App {
    fn total(&self) -> usize {
        self.response.as_ref().map(|r| r.results.total).unwrap_or(0)
    }

    fn client_size(&self) -> (f32, f32) {
        let mut r = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut r);
        }
        ((r.right - r.left) as f32, (r.bottom - r.top) as f32)
    }

    fn visible_rows(&self) -> usize {
        self.metrics.visible_rows(self.client_size().1)
    }

    fn max_scroll(&self) -> usize {
        self.total().saturating_sub(self.visible_rows())
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// Send the current query to the engine. Returns immediately; the answer
    /// arrives as `WM_APP_RESULTS`.
    fn dispatch_query(&mut self) {
        self.generation += 1;
        let _ = self.tx.send(Command::Search {
            generation: self.generation,
            text: self.query.clone(),
            mode: self.mode,
            scope: self.scope,
            sort: self.sort,
        });
    }

    fn read_query_box(&mut self) {
        let mut buf = [0u16; 1024];
        let n = unsafe { GetWindowTextW(self.edit, &mut buf) } as usize;
        let text = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
        if text != self.query {
            self.query = text;
            self.scroll = 0;
            self.selected = 0;
            self.dispatch_query();
        }
    }

    /// Materialise only the rows that are actually on screen.
    fn visible_row_data(&self) -> Vec<RowData> {
        let (Some(index), Some(resp)) = (&self.index, &self.response) else {
            return Vec::new();
        };
        let count = self.visible_rows() + 1;
        resp.results
            .window(index, self.scroll, count)
            .iter()
            .filter_map(|r| index.describe(*r))
            .map(|info| RowData {
                name: info.name,
                path: info.parent_path,
                size: if info.is_dir { String::new() } else { format_size(info.size) },
                modified: format_filetime(info.mtime),
                is_dir: info.is_dir,
            })
            .collect()
    }

    fn scrollbar_geometry(&self) -> Option<(f32, f32)> {
        let (_, h) = self.client_size();
        let visible = self.visible_rows();
        let total = self.total();
        if total <= visible || visible == 0 {
            return None;
        }
        let track_top = self.metrics.list_top();
        let track_h = self.metrics.list_height(h);
        let thumb_h = (track_h * visible as f32 / total as f32).max(24.0 * self.metrics.scale);
        let frac = self.scroll as f32 / self.max_scroll().max(1) as f32;
        let thumb_top = track_top + frac * (track_h - thumb_h);
        Some((thumb_top, thumb_h))
    }

    fn status_line(&self) -> String {
        if self.shared.indexing.load(Ordering::SeqCst) {
            return self.shared.status_text();
        }
        match &self.response {
            Some(r) => {
                let plan = match r.results.plan {
                    velox_core::Plan::BinarySearch => "binary search",
                    velox_core::Plan::Scan => "scan",
                    velox_core::Plan::Narrowed => "narrowed",
                };
                format!(
                    "{} results in {:.3} ms  [{}]   {} indexed",
                    worker::thousands(r.results.total),
                    r.results.took.as_secs_f64() * 1000.0,
                    plan,
                    worker::thousands(self.shared.entries.load(Ordering::SeqCst) as usize),
                )
            }
            None => self.shared.status_text(),
        }
    }

    fn hint_line(&self) -> String {
        format!(
            "F2 {}   F3 {}   F5 rebuild   Enter reveal",
            match self.mode {
                MatchMode::Prefix => "prefix",
                MatchMode::Substring => "substring",
            },
            match self.scope {
                Scope::Name => "name",
                Scope::Path => "path",
            },
        )
    }

    fn paint(&mut self) {
        let (w, h) = self.client_size();
        if w < 1.0 || h < 1.0 {
            return;
        }
        if self.view.is_none() {
            self.view = View::new(self.hwnd, w as u32, h as u32, self.metrics).ok();
        }
        let rows = self.visible_row_data();
        let status = self.status_line();
        let hint = self.hint_line();
        let thumb = self.scrollbar_geometry();
        let busy = self.shared.indexing.load(Ordering::SeqCst);
        // Read everything the frame needs before borrowing the view mutably.
        let first_row = self.scroll;
        let selected = self.selected;

        let lost = if let Some(v) = self.view.as_mut() {
            v.resize(w as u32, h as u32);
            !v.draw(&Frame {
                rows: &rows,
                first_row,
                selected,
                status: &status,
                hint: &hint,
                client: (w, h),
                thumb,
                busy,
            })
        } else {
            false
        };
        // A lost device (sleep, driver reset) is recovered by rebuilding on the
        // next paint rather than by tearing down the window.
        if lost {
            self.view = None;
        }
    }

    fn layout_children(&self) {
        let (w, _) = self.client_size();
        let m = self.metrics;
        unsafe {
            let _ = SetWindowPos(
                self.edit,
                None,
                m.pad as i32,
                m.pad as i32,
                (w - m.pad * 2.0) as i32,
                m.search_h as i32,
                SWP_NOZORDER,
            );
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        let target = self.scroll as isize + delta;
        self.scroll = target.clamp(0, self.max_scroll() as isize) as usize;
        self.invalidate();
    }

    fn move_selection(&mut self, delta: isize) {
        let total = self.total();
        if total == 0 {
            return;
        }
        let target = self.selected as isize + delta;
        self.selected = target.clamp(0, total as isize - 1) as usize;
        // Keep the selection in view.
        let visible = self.visible_rows();
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if visible > 0 && self.selected >= self.scroll + visible {
            self.scroll = self.selected + 1 - visible;
        }
        self.clamp_scroll();
        self.invalidate();
    }

    /// Show the selected entry in Explorer. Chosen over launching the file
    /// directly: revealing is the useful, non-destructive action.
    fn reveal_selected(&self) {
        let (Some(index), Some(resp)) = (&self.index, &self.response) else { return };
        let Some(r) = resp.results.window(index, self.selected, 1).first().copied() else {
            return;
        };
        let Some(info) = index.describe(r) else { return };
        let path = info.full_path();
        let _ = std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{path}\""))
            .spawn();
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn on_key(&mut self, vk: u16) -> bool {
        let visible = self.visible_rows().max(1) as isize;
        match VIRTUAL_KEY(vk) {
            VK_DOWN => self.move_selection(1),
            VK_UP => self.move_selection(-1),
            VK_NEXT => self.move_selection(visible),
            VK_PRIOR => self.move_selection(-visible),
            VK_HOME => {
                self.selected = 0;
                self.scroll = 0;
                self.invalidate();
            }
            VK_END => {
                self.selected = self.total().saturating_sub(1);
                self.scroll = self.max_scroll();
                self.invalidate();
            }
            VK_RETURN => self.reveal_selected(),
            VK_ESCAPE => {
                let empty = wide("");
                unsafe {
                    let _ = SetWindowTextW(self.edit, PCWSTR(empty.as_ptr()));
                }
                self.read_query_box();
            }
            VK_F2 => {
                self.mode = match self.mode {
                    MatchMode::Prefix => MatchMode::Substring,
                    MatchMode::Substring => MatchMode::Prefix,
                };
                self.dispatch_query();
            }
            VK_F3 => {
                self.scope = match self.scope {
                    Scope::Name => Scope::Path,
                    Scope::Path => Scope::Name,
                };
                self.dispatch_query();
            }
            VK_F5 => {
                self.shared.indexing.store(true, Ordering::SeqCst);
                let _ = self.tx.send(Command::Rebuild);
                self.invalidate();
            }
            _ => return false,
        }
        true
    }

    fn row_at(&self, y: f32) -> Option<usize> {
        let top = self.metrics.list_top();
        if y < top {
            return None;
        }
        let idx = self.scroll + ((y - top) / self.metrics.row_h) as usize;
        if idx < self.total() {
            Some(idx)
        } else {
            None
        }
    }
}

// ------------------------------------------------------------- window plumbing

fn app_from(hwnd: HWND) -> Option<&'static mut App> {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        ptr.as_mut()
    }
}

/// Subclass for the search box: arrow keys and Enter belong to the result list,
/// not to the text field, so they are forwarded to the parent.
unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let original = ORIGINAL_EDIT_PROC.load(Ordering::SeqCst);
    if msg == WM_KEYDOWN {
        let vk = wparam.0 as u16;
        let navigational = matches!(
            VIRTUAL_KEY(vk),
            VK_UP | VK_DOWN | VK_PRIOR | VK_NEXT | VK_RETURN | VK_F2 | VK_F3 | VK_F5 | VK_ESCAPE
        );
        if navigational {
            let parent = unsafe { GetParent(hwnd) };
            if let Ok(parent) = parent {
                if let Some(app) = app_from(parent) {
                    if app.on_key(vk) {
                        return LRESULT(0);
                    }
                }
            }
        }
    }
    // Suppress the beep the EDIT control makes for keys it does not handle.
    if msg == WM_CHAR && (wparam.0 == 0x0D || wparam.0 == 0x1B) {
        return LRESULT(0);
    }
    unsafe {
        let f: WNDPROC = std::mem::transmute(original);
        CallWindowProcW(f, hwnd, msg, wparam, lparam)
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            // Attach the app state before any other message can arrive.
            let cs = lparam.0 as *const CREATESTRUCTW;
            let app = unsafe { (*cs).lpCreateParams as *mut App };
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
                (*app).hwnd = hwnd;
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_CREATE => {
            if let Some(app) = app_from(hwnd) {
                app.metrics = Metrics::new(unsafe { GetDpiForWindow(hwnd) });
                create_search_box(app);
                app.layout_children();
            }
            LRESULT(0)
        }
        WM_SIZE => {
            if let Some(app) = app_from(hwnd) {
                app.layout_children();
                app.clamp_scroll();
                app.invalidate();
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            if let Some(app) = app_from(hwnd) {
                app.metrics = Metrics::new((wparam.0 & 0xFFFF) as u32);
                // Text formats are sized for the old DPI; rebuild on next paint.
                app.view = None;
                app.layout_children();
                app.invalidate();
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe { BeginPaint(hwnd, &mut ps) };
            if let Some(app) = app_from(hwnd) {
                app.paint();
            }
            unsafe {
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // Direct2D paints every pixel
        WM_CTLCOLOREDIT => {
            // The stock EDIT control paints itself with system colours, which
            // look like a bug against the dark list. Recolour it here.
            if let Some(app) = app_from(hwnd) {
                let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
                unsafe {
                    SetTextColor(hdc, COLORREF(EDIT_FG));
                    SetBkColor(hdc, COLORREF(EDIT_BG));
                }
                return LRESULT(app.edit_brush.0 as isize);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_COMMAND => {
            let code = (wparam.0 >> 16) as u32;
            if code == EN_CHANGE {
                if let Some(app) = app_from(hwnd) {
                    app.read_query_box();
                }
            }
            LRESULT(0)
        }
        WM_SETFOCUS => {
            if let Some(app) = app_from(hwnd) {
                unsafe {
                    let _ = SetFocus(Some(app.edit));
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if let Some(app) = app_from(hwnd) {
                if app.on_key(wparam.0 as u16) {
                    return LRESULT(0);
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_MOUSEWHEEL => {
            if let Some(app) = app_from(hwnd) {
                let delta = ((wparam.0 >> 16) as i16) as i32;
                // Three rows per notch, the Windows convention.
                app.scroll_by(-(delta / 120) as isize * 3);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            if let Some(app) = app_from(hwnd) {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                let (w, _) = app.client_size();
                let on_scrollbar = app.scrollbar_geometry().is_some()
                    && x >= w - app.metrics.scrollbar_w;
                if on_scrollbar {
                    if let Some((top, h)) = app.scrollbar_geometry() {
                        // Grab the thumb, or jump to where the user clicked.
                        if y >= top && y <= top + h {
                            app.dragging = Some(y - top);
                        } else {
                            app.dragging = Some(h / 2.0);
                            drag_to(app, y);
                        }
                        unsafe { SetCapture(hwnd) };
                    }
                } else if let Some(row) = app.row_at(y) {
                    app.selected = row;
                    if msg == WM_LBUTTONDBLCLK {
                        app.reveal_selected();
                    }
                    app.invalidate();
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(app) = app_from(hwnd) {
                if app.dragging.is_some() {
                    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                    drag_to(app, y);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(app) = app_from(hwnd) {
                if app.dragging.take().is_some() {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                }
            }
            LRESULT(0)
        }
        WM_APP_RESULTS => {
            if let Some(app) = app_from(hwnd) {
                let incoming = app.shared.response.lock().unwrap().take();
                if let Some(r) = incoming {
                    // Discard a stale answer for a query the user has moved past.
                    // The generation settles races; the text guards against a
                    // reply that raced a rebuild.
                    if r.generation >= app.generation && r.text == app.query {
                        app.response = Some(r);
                        app.clamp_scroll();
                    }
                }
                app.invalidate();
            }
            LRESULT(0)
        }
        WM_APP_INDEXED => {
            if let Some(app) = app_from(hwnd) {
                app.index = app.shared.index.lock().unwrap().clone();
                app.dispatch_query();
                app.invalidate();
            }
            LRESULT(0)
        }
        WM_APP_UPDATED => {
            if let Some(app) = app_from(hwnd) {
                // The journal changed the index under us; re-run the query so the
                // list reflects it.
                app.dispatch_query();
            }
            LRESULT(0)
        }
        WM_APP_STATUS => {
            if let Some(app) = app_from(hwnd) {
                app.invalidate();
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(app) = app_from(hwnd) {
                let _ = app.tx.send(Command::Shutdown);
                unsafe {
                    let _ = DeleteObject(app.font.into());
                    let _ = DeleteObject(app.edit_brush.into());
                }
            }
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn drag_to(app: &mut App, y: f32) {
    let Some(grab) = app.dragging else { return };
    let (_, h) = app.client_size();
    let track_top = app.metrics.list_top();
    let track_h = app.metrics.list_height(h);
    let visible = app.visible_rows();
    let total = app.total();
    if total <= visible {
        return;
    }
    let thumb_h = (track_h * visible as f32 / total as f32).max(24.0 * app.metrics.scale);
    let span = (track_h - thumb_h).max(1.0);
    let frac = ((y - grab - track_top) / span).clamp(0.0, 1.0);
    app.scroll = (frac * app.max_scroll() as f32).round() as usize;
    app.invalidate();
}

fn create_search_box(app: &mut App) {
    let class = wide("EDIT");
    let empty = wide("");
    let hinstance: HINSTANCE = unsafe { GetModuleHandleW(None) }
        .map(|h| HINSTANCE(h.0))
        .unwrap_or_default();
    let edit = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(empty.as_ptr()),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            0,
            0,
            10,
            10,
            Some(app.hwnd),
            Some(HMENU(ID_SEARCH as *mut core::ffi::c_void)),
            Some(hinstance),
            None,
        )
    }
    .expect("create search box");
    app.edit = edit;

    let face = wide("Segoe UI");
    let font = unsafe {
        CreateFontW(
            -(15.0 * app.metrics.scale) as i32,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            FONT_QUALITY(0),
            DEFAULT_PITCH.0 as u32,
            PCWSTR(face.as_ptr()),
        )
    };
    app.font = font;
    app.edit_brush = unsafe { CreateSolidBrush(COLORREF(EDIT_BG)) };
    unsafe {
        SendMessageW(edit, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        // Subclass so navigation keys reach the list instead of the text field.
        let prev = SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as isize);
        ORIGINAL_EDIT_PROC.store(prev, Ordering::SeqCst);
        let _ = SetFocus(Some(edit));
    }
}

fn main() {
    let shared = Arc::new(Shared::default());
    shared.indexing.store(true, Ordering::SeqCst);
    *shared.status.lock().unwrap() = "Starting...".to_string();

    // The worker needs the HWND to post to, and the window needs the sender, so
    // the app is boxed first and the channel filled in immediately after.
    let (placeholder_tx, placeholder_rx) = std::sync::mpsc::channel();
    drop(placeholder_rx);
    let mut app = Box::new(App {
        hwnd: HWND::default(),
        edit: HWND::default(),
        font: HFONT::default(),
        edit_brush: HBRUSH::default(),
        shared: shared.clone(),
        tx: placeholder_tx,
        index: None,
        response: None,
        generation: 0,
        query: String::new(),
        mode: MatchMode::Prefix,
        scope: Scope::Name,
        sort: (SortKey::Name, false),
        scroll: 0,
        selected: 0,
        dragging: None,
        view: None,
        metrics: Metrics::new(96),
    });

    unsafe {
        let hinstance: HINSTANCE =
            GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default();
        let class = wide(CLASS_NAME);
        let title = wide(WINDOW_TITLE);

        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            eprintln!("failed to register the window class");
            return;
        }

        let app_ptr: *mut App = &mut *app;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            // CLIPCHILDREN keeps Direct2D from painting over the search box.
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1100,
            700,
            None,
            None,
            Some(hinstance),
            Some(app_ptr as *const core::ffi::c_void),
        )
        .expect("create main window");

        // Now that the window exists, start the engine and hand the app its sender.
        let tx = worker::spawn(hwnd.0 as isize, shared.clone());
        app.tx = tx;

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetFocus(Some(app.edit));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
