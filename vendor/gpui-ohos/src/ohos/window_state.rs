//! Versioned native geometry snapshot, independent of GPUI's serialization format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeWindowState {
    pub left: i64,
    pub top: i64,
    pub width: i64,
    pub height: i64,
    pub maximized: bool,
    pub fullscreen: bool,
}

impl NativeWindowState {
    pub(crate) fn encode(self) -> Vec<u8> {
        let mut output = Vec::with_capacity(41);
        output.extend_from_slice(b"GPOHWIN1");
        for value in [self.left, self.top, self.width, self.height] {
            output.extend_from_slice(&value.to_le_bytes());
        }
        output.push(u8::from(self.maximized) | u8::from(self.fullscreen) << 1);
        output
    }

    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 41 || &bytes[..8] != b"GPOHWIN1" || bytes[40] & !3 != 0 {
            return None;
        }
        let mut values = [0; 4];
        for (value, chunk) in values.iter_mut().zip(bytes[8..40].as_chunks::<8>().0) {
            *value = i64::from_le_bytes(*chunk);
        }
        if values.iter().any(|value| i32::try_from(*value).is_err())
            || values[2] <= 0
            || values[3] <= 0
        {
            return None;
        }
        Some(Self {
            left: values[0],
            top: values[1],
            width: values[2],
            height: values[3],
            maximized: bytes[40] & 1 != 0,
            fullscreen: bytes[40] & 2 != 0,
        })
    }
}
