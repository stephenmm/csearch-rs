//! LEB128-style unsigned varints, used for delta-encoded posting lists.

#[inline]
pub fn put(buf: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

/// Decode one varint at `*pos`, advancing `*pos`. Returns `None` on truncation.
#[inline]
pub fn get(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let mut result: u32 = 0;
    let mut shift = 0;
    loop {
        let b = *buf.get(*pos)?;
        *pos += 1;
        result |= u32::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let vals = [0u32, 1, 127, 128, 300, 16_383, 16_384, u32::MAX];
        let mut buf = Vec::new();
        for &v in &vals {
            put(&mut buf, v);
        }
        let mut pos = 0;
        for &v in &vals {
            assert_eq!(get(&buf, &mut pos), Some(v));
        }
        assert_eq!(pos, buf.len());
        assert_eq!(get(&buf, &mut pos), None);
    }
}
