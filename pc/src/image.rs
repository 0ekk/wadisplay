//! Clipboard images through WIC (Windows Imaging Component): a clipboard bitmap to PNG for the
//! tablet, and the tablet's PNG or JPEG to a CF_DIB block.

use windows::core::Result;
use windows::Win32::Foundation::{E_FAIL, HGLOBAL};
use windows::Win32::Graphics::Gdi::{BITMAPINFOHEADER, BI_RGB, HBITMAP, HPALETTE};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream, IPropertyBag2};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
                                  STATFLAG_NONAME, STATSTG};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};

/// Larger images from the tablet are refused (a small file can decode to gigabytes).
const MAX_PIXELS: u64 = 1 << 25;

fn factory() -> Result<IWICImagingFactory> {
    unsafe {
        // Every calling thread needs COM; calling again on the same thread is harmless.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
    }
}

/// A copy of a bitmap the clipboard owns, taken while it is open. Opaque: clipboard bitmaps
/// rarely carry real alpha (apps that have it also offer "PNG").
pub fn copy_hbitmap(h: HBITMAP) -> Result<IWICBitmap> {
    unsafe { factory()?.CreateBitmapFromHBITMAP(h, HPALETTE::default(), WICBitmapIgnoreAlpha) }
}

/// The bitmap as a 24-bit PNG file.
pub fn png(b: &IWICBitmap) -> Result<Vec<u8>> {
    unsafe {
        let f = factory()?;
        let rgb = f.CreateFormatConverter()?;
        rgb.Initialize(b, &GUID_WICPixelFormat24bppBGR, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
        let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
        let enc = f.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        enc.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.ok_or(E_FAIL)?;
        frame.Initialize(None::<&IPropertyBag2>)?;
        let mut fmt = GUID_WICPixelFormat24bppBGR;
        frame.SetPixelFormat(&mut fmt)?;
        frame.WriteSource(&rgb, std::ptr::null())?;
        frame.Commit()?;
        enc.Commit()?;
        let mut st = STATSTG::default();
        stream.Stat(&mut st, STATFLAG_NONAME)?;
        let g = GetHGlobalFromStream(&stream)?;
        let p = GlobalLock(g) as *const u8;
        if p.is_null() {
            return Err(E_FAIL.into());
        }
        let out = std::slice::from_raw_parts(p, st.cbSize as usize).to_vec();
        let _ = GlobalUnlock(g);
        Ok(out)
    }
}

/// An image file (PNG, JPEG, anything WIC decodes) as a CF_DIB block: BITMAPINFOHEADER, then
/// bottom-up 32-bit BGRA rows.
pub fn dib(file: &[u8]) -> Result<Vec<u8>> {
    unsafe {
        let f = factory()?;
        let s = f.CreateStream()?;
        s.InitializeFromMemory(file)?;
        let dec = f.CreateDecoderFromStream(&s, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        let bgra = f.CreateFormatConverter()?;
        bgra.Initialize(&dec.GetFrame(0)?, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None, 0.0,
                        WICBitmapPaletteTypeCustom)?;
        let (mut w, mut h) = (0, 0);
        bgra.GetSize(&mut w, &mut h)?;
        if w as u64 * h as u64 > MAX_PIXELS {
            return Err(E_FAIL.into());
        }
        let stride = w as usize * 4;
        let mut px = vec![0u8; stride * h as usize];
        bgra.CopyPixels(std::ptr::null(), stride as u32, &mut px)?;
        let hdr = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: w as i32, biHeight: h as i32, biPlanes: 1,
            biBitCount: 32, biCompression: BI_RGB.0, biSizeImage: px.len() as u32, ..Default::default()
        };
        let mut out = std::slice::from_raw_parts(&hdr as *const _ as *const u8, hdr.biSize as usize).to_vec();
        out.reserve(px.len());
        for row in px.chunks(stride.max(1)).rev() {
            out.extend_from_slice(row);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};

    #[test]
    fn bitmap_to_png_to_dib() {
        // 3x2, top-down rows of 0x00RRGGBB.
        let px: [u32; 6] = [0xff0000, 0x00ff00, 0x0000ff, 0xffffff, 0x000000, 0x808080];
        let h = unsafe { CreateBitmap(3, 2, 1, 32, Some(px.as_ptr().cast())) };
        let png = png(&copy_hbitmap(h).unwrap()).unwrap();
        unsafe { let _ = DeleteObject(h.into()); }
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let d = dib(&png).unwrap();
        let at = |i: usize| i32::from_le_bytes(d[i..i + 4].try_into().unwrap());
        assert_eq!((at(4), at(8), d[14]), (3, 2, 32), "width, height, bits");
        assert_eq!(d.len(), 40 + 3 * 2 * 4);
        assert_eq!(&d[40..44], &[0xff, 0xff, 0xff, 0xff], "bottom row first: white");
        assert_eq!(&d[52..56], &[0x00, 0x00, 0xff, 0xff], "then the top row: red, opaque");
        assert!(dib(b"not an image").is_err());
    }
}
