//! Direct2D renderer for the result list.
//!
//! Custom-drawn rather than a stock `ListView` because the result set can be
//! hundreds of thousands of rows: nothing is instantiated per row, and only the
//! rows physically on screen are ever touched. A frame is a handful of
//! `FillRectangle` calls plus one `DrawText` per visible cell.

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE,
    D2D1_RENDER_TARGET_PROPERTIES,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_TRAILING,
    DWRITE_WORD_WRAPPING_NO_WRAP,
};

/// One visible row, already formatted for display. Built by the window layer
/// from the engine's `EntryInfo`, so the renderer never touches the index.
pub struct RowData {
    pub name: String,
    pub path: String,
    pub size: String,
    pub modified: String,
    pub is_dir: bool,
}

/// DPI-scaled layout constants.
#[derive(Clone, Copy)]
pub struct Metrics {
    pub scale: f32,
    pub pad: f32,
    pub search_h: f32,
    pub header_h: f32,
    pub row_h: f32,
    pub status_h: f32,
    pub scrollbar_w: f32,
}

impl Metrics {
    pub fn new(dpi: u32) -> Metrics {
        let s = (dpi as f32 / 96.0).max(1.0);
        Metrics {
            scale: s,
            pad: 8.0 * s,
            search_h: 30.0 * s,
            header_h: 26.0 * s,
            row_h: 22.0 * s,
            status_h: 24.0 * s,
            scrollbar_w: 12.0 * s,
        }
    }

    /// Top of the list area (below the search box and the column header).
    pub fn list_top(&self) -> f32 {
        self.pad * 2.0 + self.search_h + self.header_h
    }
    pub fn header_top(&self) -> f32 {
        self.pad * 2.0 + self.search_h
    }
    pub fn list_height(&self, client_h: f32) -> f32 {
        (client_h - self.list_top() - self.status_h).max(0.0)
    }
    pub fn visible_rows(&self, client_h: f32) -> usize {
        (self.list_height(client_h) / self.row_h).floor().max(0.0) as usize
    }
}

/// Column widths as fractions of the list width.
const COL_NAME: f32 = 0.28;
const COL_PATH: f32 = 0.44;
const COL_SIZE: f32 = 0.10;

pub struct Frame<'a> {
    pub rows: &'a [RowData],
    pub first_row: usize,
    /// Absolute index of the selected row, or `usize::MAX` for none.
    pub selected: usize,
    pub status: &'a str,
    pub hint: &'a str,
    pub client: (f32, f32),
    /// `(top, height)` of the scrollbar thumb in client coordinates.
    pub thumb: Option<(f32, f32)>,
    pub busy: bool,
}

fn rgb(hex: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a: 1.0,
    }
}

struct Brushes {
    bg: ID2D1SolidColorBrush,
    row_alt: ID2D1SolidColorBrush,
    selection: ID2D1SolidColorBrush,
    text: ID2D1SolidColorBrush,
    dim: ID2D1SolidColorBrush,
    folder: ID2D1SolidColorBrush,
    chrome: ID2D1SolidColorBrush,
    rule: ID2D1SolidColorBrush,
    track: ID2D1SolidColorBrush,
    thumb: ID2D1SolidColorBrush,
    busy: ID2D1SolidColorBrush,
}

pub struct View {
    _factory: ID2D1Factory,
    _dwrite: IDWriteFactory,
    rt: ID2D1HwndRenderTarget,
    body: IDWriteTextFormat,
    body_right: IDWriteTextFormat,
    header: IDWriteTextFormat,
    brushes: Brushes,
    metrics: Metrics,
    size: (u32, u32),
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

impl View {
    pub fn new(hwnd: HWND, width: u32, height: u32, metrics: Metrics) -> windows::core::Result<View> {
        unsafe {
            let factory: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let rt_props = D2D1_RENDER_TARGET_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT::default(),
                ..Default::default()
            };
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U { width: width.max(1), height: height.max(1) },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let rt = factory.CreateHwndRenderTarget(&rt_props, &hwnd_props)?;

            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let family = wide("Segoe UI");
            let locale = wide("en-us");
            let make = |weight, size: f32| -> windows::core::Result<IDWriteTextFormat> {
                let f = dwrite.CreateTextFormat(
                    PCWSTR(family.as_ptr()),
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    PCWSTR(locale.as_ptr()),
                )?;
                // Rows are single-line and vertically centred; wrapping would
                // silently push text out of its row.
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                Ok(f)
            };
            let body = make(DWRITE_FONT_WEIGHT_NORMAL, 13.0 * metrics.scale)?;
            let body_right = make(DWRITE_FONT_WEIGHT_NORMAL, 13.0 * metrics.scale)?;
            body_right.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;
            let header = make(DWRITE_FONT_WEIGHT_SEMI_BOLD, 12.0 * metrics.scale)?;

            let brushes = Brushes {
                bg: rt.CreateSolidColorBrush(&rgb(0x1E1E1E), None)?,
                row_alt: rt.CreateSolidColorBrush(&rgb(0x242424), None)?,
                selection: rt.CreateSolidColorBrush(&rgb(0x0F5B8C), None)?,
                text: rt.CreateSolidColorBrush(&rgb(0xE8E8E8), None)?,
                dim: rt.CreateSolidColorBrush(&rgb(0x9A9A9A), None)?,
                folder: rt.CreateSolidColorBrush(&rgb(0x4EC9B0), None)?,
                chrome: rt.CreateSolidColorBrush(&rgb(0x2B2B2B), None)?,
                rule: rt.CreateSolidColorBrush(&rgb(0x3A3A3A), None)?,
                track: rt.CreateSolidColorBrush(&rgb(0x262626), None)?,
                thumb: rt.CreateSolidColorBrush(&rgb(0x555555), None)?,
                busy: rt.CreateSolidColorBrush(&rgb(0xD7A23B), None)?,
            };

            Ok(View {
                _factory: factory,
                _dwrite: dwrite,
                rt,
                body,
                body_right,
                header,
                brushes,
                metrics,
                size: (width, height),
            })
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if (width, height) == self.size {
            return;
        }
        self.size = (width, height);
        unsafe {
            let _ = self.rt.Resize(&D2D_SIZE_U { width: width.max(1), height: height.max(1) });
        }
    }

    fn text(&self, s: &str, rect: D2D_RECT_F, fmt: &IDWriteTextFormat, brush: &ID2D1SolidColorBrush) {
        if s.is_empty() || rect.right <= rect.left {
            return;
        }
        let w: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            self.rt.DrawText(
                &w,
                fmt,
                &rect,
                brush,
                // Clip, so a long path cannot bleed into the next column.
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    fn fill(&self, rect: D2D_RECT_F, brush: &ID2D1SolidColorBrush) {
        unsafe { self.rt.FillRectangle(&rect, brush) }
    }

    /// Draw one frame. Returns `false` when the render target was lost and the
    /// caller should rebuild the view.
    pub fn draw(&self, f: &Frame) -> bool {
        let m = self.metrics;
        let (w, h) = f.client;
        let list_top = m.list_top();
        let list_bottom = h - m.status_h;
        let bar_w = if f.thumb.is_some() { m.scrollbar_w } else { 0.0 };
        let content_w = (w - bar_w).max(0.0);

        // Column boundaries.
        let x0 = m.pad;
        let name_w = content_w * COL_NAME;
        let path_w = content_w * COL_PATH;
        let size_w = content_w * COL_SIZE;
        let c_name = x0;
        let c_path = c_name + name_w;
        let c_size = c_path + path_w;
        let c_date = c_size + size_w;

        unsafe {
            self.rt.BeginDraw();
            self.rt.Clear(Some(&rgb(0x1E1E1E)));
        }
        let _ = &self.brushes.bg;

        // Header strip behind the search box.
        self.fill(
            D2D_RECT_F { left: 0.0, top: 0.0, right: w, bottom: m.header_top() },
            &self.brushes.chrome,
        );

        // Column header.
        let hy0 = m.header_top();
        let hy1 = list_top;
        self.fill(
            D2D_RECT_F { left: 0.0, top: hy0, right: w, bottom: hy1 },
            &self.brushes.chrome,
        );
        self.fill(
            D2D_RECT_F { left: 0.0, top: hy1 - 1.0, right: w, bottom: hy1 },
            &self.brushes.rule,
        );
        let hdr = |x: f32, width: f32, label: &str, right: bool| {
            let r = D2D_RECT_F {
                left: x + m.pad * 0.5,
                top: hy0,
                right: x + width - m.pad * 0.5,
                bottom: hy1,
            };
            self.text(
                label,
                r,
                if right { &self.body_right } else { &self.header },
                &self.brushes.dim,
            );
        };
        hdr(c_name, name_w, "Name", false);
        hdr(c_path, path_w, "Path", false);
        hdr(c_size, size_w, "Size", true);
        hdr(c_date, content_w - (c_date - x0), "Modified", false);

        // Rows.
        for (i, row) in f.rows.iter().enumerate() {
            let top = list_top + i as f32 * m.row_h;
            let bottom = top + m.row_h;
            if top >= list_bottom {
                break;
            }
            let abs = f.first_row + i;
            let full = D2D_RECT_F { left: 0.0, top, right: content_w, bottom };
            if abs == f.selected {
                self.fill(full, &self.brushes.selection);
            } else if abs % 2 == 1 {
                self.fill(full, &self.brushes.row_alt);
            }
            let name_brush =
                if row.is_dir { &self.brushes.folder } else { &self.brushes.text };
            let cell = |x: f32, width: f32| D2D_RECT_F {
                left: x + m.pad * 0.5,
                top,
                right: x + width - m.pad * 0.5,
                bottom,
            };
            self.text(&row.name, cell(c_name, name_w), &self.body, name_brush);
            self.text(&row.path, cell(c_path, path_w), &self.body, &self.brushes.dim);
            self.text(&row.size, cell(c_size, size_w), &self.body_right, &self.brushes.dim);
            self.text(
                &row.modified,
                cell(c_date, content_w - (c_date - x0)),
                &self.body,
                &self.brushes.dim,
            );
        }

        // Scrollbar.
        if let Some((thumb_top, thumb_h)) = f.thumb {
            self.fill(
                D2D_RECT_F { left: w - bar_w, top: list_top, right: w, bottom: list_bottom },
                &self.brushes.track,
            );
            self.fill(
                D2D_RECT_F {
                    left: w - bar_w + 2.0,
                    top: thumb_top,
                    right: w - 2.0,
                    bottom: thumb_top + thumb_h,
                },
                &self.brushes.thumb,
            );
        }

        // Status bar.
        let sy = h - m.status_h;
        self.fill(
            D2D_RECT_F { left: 0.0, top: sy, right: w, bottom: h },
            &self.brushes.chrome,
        );
        self.fill(
            D2D_RECT_F { left: 0.0, top: sy, right: w, bottom: sy + 1.0 },
            &self.brushes.rule,
        );
        let status_brush = if f.busy { &self.brushes.busy } else { &self.brushes.dim };
        self.text(
            f.status,
            D2D_RECT_F { left: m.pad, top: sy, right: w * 0.68, bottom: h },
            &self.body,
            status_brush,
        );
        self.text(
            f.hint,
            D2D_RECT_F { left: w * 0.68, top: sy, right: w - m.pad, bottom: h },
            &self.body_right,
            &self.brushes.dim,
        );

        unsafe { self.rt.EndDraw(None, None).is_ok() }
    }
}
