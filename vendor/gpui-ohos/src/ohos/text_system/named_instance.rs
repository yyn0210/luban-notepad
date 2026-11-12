//! Exact fvar coordinates: Swash's public instance values pass through f32,
//! which can discard low 16.16 bits before native metrics and rasterization.
use super::FontDesignCoordinate;
use std::sync::Arc;

pub(super) struct NamedInstance {
    pub coordinates: Arc<[FontDesignCoordinate]>,
}

impl NamedInstance {
    pub fn find(face: swash::FontRef<'_>, name: &str) -> Option<Self> {
        let instance = face.instances().find(|instance| {
            face.localized_strings().any(|value| {
                value.id().to_raw() == instance.name_id().to_raw()
                    && value.to_string().eq_ignore_ascii_case(name)
            })
        })?;
        let table = face.table(u32::from_be_bytes(*b"fvar"))?;
        let values = Self::values(table, instance.index())?;
        if values.len() / 4 != face.variations().count() {
            return None;
        }
        Some(Self {
            coordinates: face
                .variations()
                .zip(values.as_chunks::<4>().0)
                .map(|(axis, bytes)| FontDesignCoordinate {
                    tag: axis.tag().to_be_bytes(),
                    value: i32::from_be_bytes(*bytes),
                })
                .collect(),
        })
    }

    /// The default instance need not have an fvar instance record. Preserve
    /// its exact defaults when the requested name matches the face subfamily.
    pub fn defaults(face: swash::FontRef<'_>) -> Option<Self> {
        let table = face.table(u32::from_be_bytes(*b"fvar"))?;
        let word = |offset| {
            Some(usize::from(u16::from_be_bytes(
                table.get(offset..offset + 2)?.try_into().ok()?,
            )))
        };
        let count = word(8)?;
        let stride = word(10)?;
        if word(0)? != 1
            || !(1..=32).contains(&count)
            || count != face.variations().count()
            || stride < 20
        {
            return None;
        }
        let start = word(4)?;
        let coordinates = face
            .variations()
            .enumerate()
            .map(|(index, axis)| {
                let offset = start
                    .checked_add(index.checked_mul(stride)?)?
                    .checked_add(8)?;
                Some(FontDesignCoordinate {
                    tag: axis.tag().to_be_bytes(),
                    value: i32::from_be_bytes(
                        table.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
                    ),
                })
            })
            .collect::<Option<Arc<[_]>>>()?;
        Some(Self { coordinates })
    }

    fn values(table: &[u8], index: usize) -> Option<&[u8]> {
        let word = |offset| {
            Some(usize::from(u16::from_be_bytes(
                table.get(offset..offset + 2)?.try_into().ok()?,
            )))
        };
        let axis_count = word(8)?;
        let axis_size = word(10)?;
        let instance_size = word(14)?;
        if word(0)? != 1
            || !(1..=32).contains(&axis_count)
            || axis_size < 20
            || instance_size < 4 + axis_count * 4
            || index >= word(12)?
        {
            return None;
        }
        // Records can have compatible extensions; honor both stride fields.
        let start = word(4)?
            .checked_add(axis_count.checked_mul(axis_size)?)?
            .checked_add(index.checked_mul(instance_size)?)?
            .checked_add(4)?;
        table.get(start..start.checked_add(axis_count * 4)?)
    }
}
