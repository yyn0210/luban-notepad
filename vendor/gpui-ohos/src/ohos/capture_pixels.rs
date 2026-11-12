/// Row-local conversion avoids zeroing and then rescanning a complete capture frame.
#[derive(Clone, Copy)]
pub(crate) enum CaptureFormat {
    Rgba,
    Rgbx,
    Bgra,
    Bgrx,
}
pub(crate) struct CapturePixels {
    bytes: Vec<u8>,
    row_bytes: usize,
    height: usize,
    format: CaptureFormat,
}
impl CapturePixels {
    pub(crate) fn new(width: usize, height: usize, format: CaptureFormat) -> Option<Self> {
        let row_bytes = width.checked_mul(4)?;
        let size = row_bytes.checked_mul(height)?;
        if width == 0 || height == 0 || size > 256 * 1024 * 1024 {
            return None;
        }
        Some(Self {
            bytes: Vec::with_capacity(size),
            row_bytes,
            height,
            format,
        })
    }
    pub(crate) fn push_row(&mut self, source: &[u8]) -> bool {
        if source.len() != self.row_bytes || self.bytes.len() / self.row_bytes >= self.height {
            return false;
        }
        let start = self.bytes.len();
        self.bytes.extend_from_slice(source);
        if matches!(self.format, CaptureFormat::Rgba) {
            return true;
        }
        let swap = matches!(self.format, CaptureFormat::Bgra | CaptureFormat::Bgrx);
        let opaque = matches!(self.format, CaptureFormat::Rgbx | CaptureFormat::Bgrx);
        for pixel in self.bytes[start..].as_chunks_mut::<4>().0 {
            if swap {
                pixel.swap(0, 2);
            }
            if opaque {
                pixel[3] = 255;
            }
        }
        true
    }
    pub(crate) fn finish(self) -> Option<Vec<u8>> {
        (self.bytes.len() == self.row_bytes * self.height).then_some(self.bytes)
    }
}
