//! Fontconfig-backed font selection and FreeType rasterization helpers.

use fontconfig_sys as fc;
use freetype::freetype as ft;
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

const DEFAULT_FONT_QUERY: &str = "sans";

#[derive(Clone, PartialEq, Eq)]
struct FontMatch {
    path: PathBuf,
    index: i32,
}

struct LoadedFont {
    query: String,
    face: ft::FT_Face,
    size_px: u32,
}

struct LoadedFallbackFont {
    query: String,
    matched: FontMatch,
    face: ft::FT_Face,
    size_px: u32,
}

struct FontCache {
    library: ft::FT_Library,
    loaded: Option<LoadedFont>,
    fallback: Option<LoadedFallbackFont>,
}

impl FontCache {
    fn new() -> Option<Self> {
        let mut library: ft::FT_Library = ptr::null_mut();
        let ok = unsafe { freetype::succeeded(ft::FT_Init_FreeType(&mut library)) };
        if !ok || library.is_null() {
            return None;
        }
        Some(Self {
            library,
            loaded: None,
            fallback: None,
        })
    }

    fn face_for(&mut self, query: Option<&str>, size_px: u32) -> Option<FaceHandle> {
        let query = query.unwrap_or(DEFAULT_FONT_QUERY);
        let needs_reload = match &self.loaded {
            Some(loaded) => loaded.query != query,
            None => true,
        };

        if needs_reload {
            let matched = match_font(query)?;
            let face = self.load_face(&matched)?;
            if let Some(loaded) = self.loaded.take() {
                unsafe {
                    ft::FT_Done_Face(loaded.face);
                }
            }
            let size_px = size_px.max(1);
            let ok = set_pixel_size(face, size_px);
            if !ok {
                unsafe {
                    ft::FT_Done_Face(face);
                }
                return None;
            }
            self.loaded = Some(LoadedFont {
                query: query.to_string(),
                face,
                size_px,
            });
        }

        let loaded = self.loaded.as_mut()?;
        let size_px = size_px.max(1);
        if loaded.size_px != size_px {
            let ok = set_pixel_size(loaded.face, size_px);
            if !ok {
                return None;
            }
            loaded.size_px = size_px;
        }
        Some(FaceHandle { face: loaded.face })
    }

    fn load_face(&self, matched: &FontMatch) -> Option<ft::FT_Face> {
        let c_path = CString::new(matched.path.to_string_lossy().as_bytes()).ok()?;
        let mut face: ft::FT_Face = ptr::null_mut();
        let ok = unsafe {
            freetype::succeeded(ft::FT_New_Face(
                self.library,
                c_path.as_ptr(),
                matched.index as ft::FT_Long,
                &mut face,
            ))
        };
        if ok && !face.is_null() {
            Some(face)
        } else {
            None
        }
    }

    fn cached_fallback_face(&mut self, query: &str, size_px: u32) -> Option<FaceHandle> {
        let fallback = self.fallback.as_mut()?;
        if fallback.query != query {
            return None;
        }

        let size_px = size_px.max(1);
        if fallback.size_px != size_px {
            if !set_pixel_size(fallback.face, size_px) {
                return None;
            }
            fallback.size_px = size_px;
        }

        Some(FaceHandle {
            face: fallback.face,
        })
    }

    fn fallback_face_for_match(
        &mut self,
        query: &str,
        matched: FontMatch,
        size_px: u32,
    ) -> Option<FaceHandle> {
        let size_px = size_px.max(1);
        let needs_reload = match &self.fallback {
            Some(fallback) => fallback.query != query || fallback.matched != matched,
            None => true,
        };

        if needs_reload {
            let face = self.load_face(&matched)?;
            if !set_pixel_size(face, size_px) {
                unsafe {
                    ft::FT_Done_Face(face);
                }
                return None;
            }
            if let Some(fallback) = self.fallback.take() {
                unsafe {
                    ft::FT_Done_Face(fallback.face);
                }
            }
            self.fallback = Some(LoadedFallbackFont {
                query: query.to_string(),
                matched,
                face,
                size_px,
            });
        } else if let Some(fallback) = self.fallback.as_mut() {
            if fallback.size_px != size_px {
                if !set_pixel_size(fallback.face, size_px) {
                    return None;
                }
                fallback.size_px = size_px;
            }
        }

        Some(FaceHandle {
            face: self.fallback.as_ref()?.face,
        })
    }

    fn with_glyph<R>(
        &mut self,
        query: Option<&str>,
        size_px: u32,
        ch: char,
        f: impl FnOnce(GlyphBitmap<'_>) -> R,
    ) -> Option<R> {
        let size_px = size_px.max(1);
        if let Some(mut face) = self.face_for(query, size_px) {
            if let Some(glyph) = face.load_char(ch) {
                return Some(f(glyph));
            }
        }

        let query = query.unwrap_or(DEFAULT_FONT_QUERY);
        if let Some(mut face) = self.cached_fallback_face(query, size_px) {
            if let Some(glyph) = face.load_char(ch) {
                return Some(f(glyph));
            }
        }

        let matched = match_font_for_char(query, ch)?;
        let mut face = self.fallback_face_for_match(query, matched, size_px)?;
        face.load_char(ch).map(f)
    }
}

impl Drop for FontCache {
    fn drop(&mut self) {
        if let Some(loaded) = self.loaded.take() {
            unsafe {
                ft::FT_Done_Face(loaded.face);
            }
        }
        if let Some(fallback) = self.fallback.take() {
            unsafe {
                ft::FT_Done_Face(fallback.face);
            }
        }
        if !self.library.is_null() {
            unsafe {
                ft::FT_Done_FreeType(self.library);
            }
        }
    }
}

thread_local! {
    static FONT_CACHE: RefCell<Option<FontCache>> = RefCell::new(FontCache::new());
}

pub fn with_face<R>(
    query: Option<&str>,
    size_px: u32,
    f: impl FnOnce(&mut FaceHandle) -> R,
) -> Option<R> {
    FONT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let cache = cache.as_mut()?;
        let mut face = cache.face_for(query, size_px)?;
        Some(f(&mut face))
    })
}

pub struct FaceHandle {
    face: ft::FT_Face,
}

impl FaceHandle {
    pub fn line_metrics(&self) -> Option<LineMetrics> {
        unsafe {
            let size = (*self.face).size;
            if size.is_null() {
                return None;
            }
            let metrics = (*size).metrics;
            Some(LineMetrics {
                ascender: (metrics.ascender >> 6) as i32,
                descender: (metrics.descender >> 6) as i32,
            })
        }
    }

    pub fn load_char(&mut self, ch: char) -> Option<GlyphBitmap<'_>> {
        if unsafe { ft::FT_Get_Char_Index(self.face, ch as ft::FT_ULong) } == 0 {
            return None;
        }
        let ok = unsafe {
            freetype::succeeded(ft::FT_Load_Char(
                self.face,
                ch as ft::FT_ULong,
                (ft::FT_LOAD_RENDER | ft::FT_LOAD_COLOR) as ft::FT_Int32,
            ))
        };
        if !ok {
            return None;
        }

        unsafe {
            let glyph = (*self.face).glyph;
            if glyph.is_null() {
                return None;
            }
            let bitmap = &(*glyph).bitmap;
            let pitch = bitmap.pitch;
            let rows = bitmap.rows as i32;
            let width = bitmap.width as i32;
            let abs_pitch = pitch.unsigned_abs() as usize;
            let len = abs_pitch.saturating_mul(rows.max(0) as usize);
            let pixel_mode = match bitmap.pixel_mode {
                mode if mode == ft::FT_Pixel_Mode::FT_PIXEL_MODE_GRAY as u8 => GlyphPixelMode::Gray,
                mode if mode == ft::FT_Pixel_Mode::FT_PIXEL_MODE_BGRA as u8 => GlyphPixelMode::Bgra,
                _ => return None,
            };
            if width == 0 || rows == 0 || len == 0 {
                return Some(GlyphBitmap {
                    width,
                    rows,
                    pitch,
                    pixel_mode,
                    buffer: &[],
                    left: (*glyph).bitmap_left,
                    top: (*glyph).bitmap_top,
                    advance: ((*glyph).advance.x >> 6) as i32,
                });
            }
            if bitmap.buffer.is_null() {
                return None;
            }
            let buffer = std::slice::from_raw_parts(bitmap.buffer, len);

            Some(GlyphBitmap {
                width,
                rows,
                pitch,
                pixel_mode,
                buffer,
                left: (*glyph).bitmap_left,
                top: (*glyph).bitmap_top,
                advance: ((*glyph).advance.x >> 6) as i32,
            })
        }
    }
}

pub struct LineMetrics {
    pub ascender: i32,
    pub descender: i32,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum GlyphPixelMode {
    Gray,
    Bgra,
}

pub struct GlyphBitmap<'a> {
    pub width: i32,
    pub rows: i32,
    pub pitch: i32,
    pub pixel_mode: GlyphPixelMode,
    pub buffer: &'a [u8],
    pub left: i32,
    pub top: i32,
    pub advance: i32,
}

impl GlyphBitmap<'_> {
    pub fn display_advance(&self, target_height: i32) -> i32 {
        if self.pixel_mode == GlyphPixelMode::Bgra && self.rows > target_height.max(1) {
            return scale_positive(self.advance, target_height.max(1), self.rows);
        }
        self.advance
    }
}

pub fn measure_text(query: Option<&str>, font_size: f32, text: &str) -> Option<f32> {
    let size_px = font_size.round().max(1.0) as u32;
    line_metrics(query, size_px)?;
    let mut width = 0i32;
    for ch in text.chars() {
        width += with_glyph(query, size_px, ch, |glyph| {
            glyph.display_advance(size_px as i32)
        })
        .unwrap_or(0);
    }
    Some(width as f32)
}

pub fn line_metrics(query: Option<&str>, size_px: u32) -> Option<LineMetrics> {
    with_face(query, size_px, |face| face.line_metrics()).flatten()
}

pub fn with_glyph<R>(
    query: Option<&str>,
    size_px: u32,
    ch: char,
    f: impl FnOnce(GlyphBitmap<'_>) -> R,
) -> Option<R> {
    FONT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let cache = cache.as_mut()?;
        cache.with_glyph(query, size_px, ch, f)
    })
}

fn match_font(pattern: &str) -> Option<FontMatch> {
    let path = Path::new(pattern);
    if path.is_file() {
        return Some(FontMatch {
            path: path.to_path_buf(),
            index: 0,
        });
    }

    let config = fontconfig_config()?;
    let c_pattern = CString::new(pattern).ok()?;
    unsafe {
        let pat = fc::FcNameParse(c_pattern.as_ptr() as *const fc::FcChar8);
        if pat.is_null() {
            return None;
        }
        fc::FcConfigSubstitute(config, pat, fc::FcMatchPattern);
        fc::FcDefaultSubstitute(pat);
        let mut result = fc::FcResultMatch;
        let font = fc::FcFontMatch(config, pat, &mut result);
        fc::FcPatternDestroy(pat);
        font_match_from_pattern(font)
    }
}

fn match_font_for_char(pattern: &str, ch: char) -> Option<FontMatch> {
    let config = fontconfig_config()?;
    let c_pattern = CString::new(pattern).ok()?;
    unsafe {
        let pat = fc::FcNameParse(c_pattern.as_ptr() as *const fc::FcChar8);
        if pat.is_null() {
            return None;
        }

        let charset = fc::FcCharSetCreate();
        if charset.is_null() {
            fc::FcPatternDestroy(pat);
            return None;
        }
        if fc::FcCharSetAddChar(charset, ch as fc::FcChar32) == 0 {
            fc::FcCharSetDestroy(charset);
            fc::FcPatternDestroy(pat);
            return None;
        }
        fc::FcPatternAddCharSet(pat, fc::constants::FC_CHARSET.as_ptr(), charset);
        fc::FcCharSetDestroy(charset);

        fc::FcConfigSubstitute(config, pat, fc::FcMatchPattern);
        fc::FcDefaultSubstitute(pat);
        let mut result = fc::FcResultMatch;
        let font = fc::FcFontMatch(config, pat, &mut result);
        fc::FcPatternDestroy(pat);
        font_match_from_pattern(font)
    }
}

fn font_match_from_pattern(font: *mut fc::FcPattern) -> Option<FontMatch> {
    unsafe {
        if font.is_null() {
            return None;
        }

        let mut file_ptr: *mut fc::FcChar8 = ptr::null_mut();
        let file_result =
            fc::FcPatternGetString(font, fc::constants::FC_FILE.as_ptr(), 0, &mut file_ptr);
        if file_result != fc::FcResultMatch || file_ptr.is_null() {
            fc::FcPatternDestroy(font);
            return None;
        }

        let mut index: c_int = 0;
        if fc::FcPatternGetInteger(font, fc::constants::FC_INDEX.as_ptr(), 0, &mut index)
            != fc::FcResultMatch
        {
            index = 0;
        }

        let path = CStr::from_ptr(file_ptr as *const c_char)
            .to_string_lossy()
            .into_owned();
        fc::FcPatternDestroy(font);

        Some(FontMatch {
            path: PathBuf::from(path),
            index,
        })
    }
}

fn scale_positive(value: i32, num: i32, den: i32) -> i32 {
    if value <= 0 || num <= 0 || den <= 0 {
        return value;
    }
    (((value as i64 * num as i64) + den as i64 - 1) / den as i64).max(1) as i32
}

fn set_pixel_size(face: ft::FT_Face, size_px: u32) -> bool {
    if unsafe { freetype::succeeded(ft::FT_Set_Pixel_Sizes(face, 0, size_px)) } {
        return true;
    }

    unsafe {
        let count = (*face).num_fixed_sizes;
        let sizes = (*face).available_sizes;
        if count <= 0 || sizes.is_null() {
            return false;
        }

        let mut best = 0;
        let mut best_delta = i64::MAX;
        for i in 0..count {
            let size = *sizes.add(i as usize);
            let y_px = ((size.y_ppem + 32) >> 6).max(size.height as i64);
            let delta = (y_px - size_px as i64).abs();
            if delta < best_delta {
                best = i;
                best_delta = delta;
            }
        }
        freetype::succeeded(ft::FT_Select_Size(face, best))
    }
}

fn fontconfig_config() -> Option<*mut fc::FcConfig> {
    #[derive(Clone, Copy)]
    struct ConfigPtr(*mut fc::FcConfig);

    unsafe impl Send for ConfigPtr {}
    unsafe impl Sync for ConfigPtr {}

    static CONFIG: OnceLock<ConfigPtr> = OnceLock::new();
    let config = CONFIG.get_or_init(|| unsafe { ConfigPtr(fc::FcInitLoadConfigAndFonts()) });
    if config.0.is_null() {
        None
    } else {
        Some(config.0)
    }
}
